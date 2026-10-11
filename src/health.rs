//! Per-model health: cooldowns with exponential backoff and a circuit
//! breaker (docs/routing.md §3). Updated synchronously from proxy paths;
//! consulted when building attempt lists (cooling / circuit-open models
//! are excluded — "× 0 for cooldown duration").
//!
//! Locking: one `std::sync::Mutex` over a map; critical sections are map
//! updates only (never held across `.await`).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Exponential cooldown ladder + circuit thresholds. Defaults follow
/// docs/routing.md (30s → 2m → 8m → 30m; open for 5m after > 30% failure
/// ratio over 1h with ≥ 8 samples).
#[derive(Clone, Debug)]
pub struct HealthPolicy {
    pub cooldown_steps: Vec<Duration>,
    pub circuit_open: Duration,
    pub circuit_window: Duration,
    pub circuit_min_samples: usize,
    pub circuit_ratio: f64,
    /// Samples needed before the ×0.5 penalty applies (less evidence than
    /// a circuit trip, so mild degradation is felt before exclusion).
    pub penalty_min_samples: usize,
}

impl Default for HealthPolicy {
    fn default() -> Self {
        Self {
            cooldown_steps: vec![
                Duration::from_secs(30),
                Duration::from_secs(120),
                Duration::from_secs(480),
                Duration::from_secs(1800),
            ],
            circuit_open: Duration::from_secs(300),
            circuit_window: Duration::from_secs(3600),
            circuit_min_samples: 8,
            circuit_ratio: 0.30,
            penalty_min_samples: 3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthState {
    /// Serving (or probing after a circuit reopened).
    Healthy,
    /// Backing off after a retryable failure (429/5xx/connect).
    Cooldown,
    /// Failure ratio over the window exceeded the threshold.
    CircuitOpen,
}

impl HealthState {
    pub fn excluded(self) -> bool {
        !matches!(self, HealthState::Healthy)
    }
}

#[derive(Clone, Debug)]
pub struct HealthSnapshot {
    pub state: HealthState,
    /// Seconds until the current cooldown/circuit expires (0 if healthy).
    pub secs_left: u64,
    /// (samples, failure ratio) over the window, for /status.json.
    pub samples_1h: usize,
    pub fail_ratio_1h: f64,
}

#[derive(Debug, Default)]
struct Entry {
    step: usize,
    cooldown_until: Option<Instant>,
    circuit_until: Option<Instant>,
    samples: VecDeque<(Instant, bool)>,
    /// observed latency EMA in ms (`fastest` preference, review F11);
    /// None until the first success — unseen models tie at the end.
    speed_ema_ms: Option<f64>,
}

impl Entry {
    fn state(&self, now: Instant) -> HealthState {
        if let Some(t) = self.circuit_until {
            if t > now {
                return HealthState::CircuitOpen;
            }
        }
        if let Some(t) = self.cooldown_until {
            if t > now {
                return HealthState::Cooldown;
            }
        }
        HealthState::Healthy
    }
}

pub struct Health {
    policy: HealthPolicy,
    entries: Mutex<HashMap<String, Entry>>,
}

impl Health {
    pub fn new(policy: HealthPolicy) -> Arc<Self> {
        Arc::new(Self {
            policy,
            entries: Mutex::new(HashMap::new()),
        })
    }

    fn prune(&self, e: &mut Entry, now: Instant) {
        let window = self.policy.circuit_window;
        while let Some(&(t, _)) = e.samples.front() {
            if now.duration_since(t) > window {
                e.samples.pop_front();
            } else {
                break;
            }
        }
    }

    fn ratio(&self, e: &Entry) -> f64 {
        if e.samples.is_empty() {
            return 0.0;
        }
        let fails = e.samples.iter().filter(|(_, ok)| !ok).count();
        fails as f64 / e.samples.len() as f64
    }

    /// Record a served request: clears the cooldown ladder and counts a
    /// success sample (which closes an expired circuit for good).
    pub fn record_success(&self, id: &str) {
        self.record_success_latency(id, None);
    }

    /// Success with observed latency (ms) — feeds the `fastest`
    /// preference (F11); buffered relays and stream finalization both
    /// know their wall time.
    pub fn record_success_latency(&self, id: &str, latency_ms: Option<u64>) {
        let now = Instant::now();
        let mut map = self.entries.lock().unwrap();
        let e = map.entry(id.to_string()).or_default();
        e.step = 0;
        e.cooldown_until = None;
        if let Some(ms) = latency_ms {
            e.speed_ema_ms = Some(match e.speed_ema_ms {
                Some(prev) => prev * 0.75 + ms as f64 * 0.25,
                None => ms as f64,
            });
        }
        // an expired circuit is closed by a healthy probe
        if e.circuit_until.map(|t| t <= now).unwrap_or(true) {
            e.circuit_until = None;
        }
        e.samples.push_back((now, true));
        self.prune(e, now);
    }

    /// Record a failure. `retryable` (429/5xx/connect errors) also steps
    /// the cooldown ladder; every failure counts toward the circuit ratio.
    pub fn record_failure(&self, id: &str, retryable: bool) {
        let now = Instant::now();
        let mut map = self.entries.lock().unwrap();
        let e = map.entry(id.to_string()).or_default();

        if retryable && !self.policy.cooldown_steps.is_empty() {
            let dur = self.policy.cooldown_steps[e.step.min(self.policy.cooldown_steps.len() - 1)];
            e.cooldown_until = Some(now + dur);
            e.step = (e.step + 1).min(self.policy.cooldown_steps.len() - 1);
        }

        e.samples.push_back((now, false));
        self.prune(e, now);
        if e.samples.len() >= self.policy.circuit_min_samples
            && self.ratio(e) > self.policy.circuit_ratio
        {
            e.circuit_until = Some(now + self.policy.circuit_open);
            // fresh window after the circuit reopens
            e.samples.clear();
            e.step = 0;
            e.cooldown_until = None;
        }
    }

    /// Current state (cooldown/circuit expiry is checked lazily).
    pub fn state(&self, id: &str) -> HealthState {
        let now = Instant::now();
        let map = self.entries.lock().unwrap();
        map.get(id)
            .map(|e| e.state(now))
            .unwrap_or(HealthState::Healthy)
    }

    /// True when the model must be excluded from attempt lists.
    pub fn is_excluded(&self, id: &str) -> bool {
        self.state(id).excluded()
    }

    /// Ordering penalty for E3 quota scoring: × 0.5 while the recent
    /// failure ratio is above the circuit threshold but the circuit has
    /// not tripped yet (docs/routing.md §3).
    /// Observed latency EMA (ms) for `fastest` ordering (F11).
    pub fn speed_ema_ms(&self, id: &str) -> Option<f64> {
        self.entries
            .lock()
            .unwrap()
            .get(id)
            .and_then(|e| e.speed_ema_ms)
    }

    pub fn penalty(&self, id: &str) -> f64 {
        let map = self.entries.lock().unwrap();
        match map.get(id) {
            Some(e)
                if e.samples.len() >= self.policy.penalty_min_samples
                    && self.ratio(e) > self.policy.circuit_ratio =>
            {
                0.5
            }
            _ => 1.0,
        }
    }

    /// Snapshot for `/status.json` and the monitor (E5).
    pub fn snapshot(&self) -> BTreeMap<String, HealthSnapshot> {
        let now = Instant::now();
        let map = self.entries.lock().unwrap();
        map.iter()
            .filter_map(|(id, e)| {
                let state = e.state(now);
                let relevant = state != HealthState::Healthy || !e.samples.is_empty();
                relevant.then(|| {
                    let until = match state {
                        HealthState::CircuitOpen => e.circuit_until,
                        HealthState::Cooldown => e.cooldown_until,
                        HealthState::Healthy => None,
                    };
                    (
                        id.clone(),
                        HealthSnapshot {
                            state,
                            secs_left: until
                                .filter(|t| *t > now)
                                .map(|t| t.duration_since(now).as_secs())
                                .unwrap_or(0),
                            samples_1h: e.samples.len(),
                            fail_ratio_1h: self.ratio(e),
                        },
                    )
                })
            })
            .collect()
    }
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny() -> HealthPolicy {
        HealthPolicy {
            cooldown_steps: vec![Duration::from_millis(50), Duration::from_millis(200)],
            circuit_open: Duration::from_millis(150),
            circuit_window: Duration::from_secs(60),
            circuit_min_samples: 3,
            circuit_ratio: 0.5,
            penalty_min_samples: 2,
        }
    }

    #[test]
    fn cooldown_exponential_and_recovery() {
        // circuit effectively disabled here — its own test covers it
        let h = Health::new(HealthPolicy {
            circuit_min_samples: 100,
            ..tiny()
        });
        assert_eq!(h.state("m"), HealthState::Healthy);
        h.record_failure("m", true);
        assert_eq!(h.state("m"), HealthState::Cooldown);
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(h.state("m"), HealthState::Healthy);
        // second failure steps up (200ms)
        h.record_failure("m", true);
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(h.state("m"), HealthState::Cooldown, "step 2 still cooling");
        // success resets the ladder
        std::thread::sleep(Duration::from_millis(130));
        assert_eq!(h.state("m"), HealthState::Healthy);
        h.record_failure("m", true);
        std::thread::sleep(Duration::from_millis(30));
        h.record_success("m");
        assert_eq!(h.state("m"), HealthState::Healthy);
    }

    #[test]
    fn nonretryable_failures_do_not_cooldown() {
        let h = Health::new(tiny());
        h.record_failure("m", false);
        assert_eq!(h.state("m"), HealthState::Healthy);
    }

    #[test]
    fn circuit_opens_on_ratio_and_reopens() {
        let h = Health::new(tiny());
        // 3 samples, ratio 2/3 > 0.5 → circuit opens on the 3rd failure
        h.record_success("m");
        h.record_failure("m", false);
        h.record_failure("m", false);
        assert_eq!(h.state("m"), HealthState::CircuitOpen);
        // expires after 150ms → healthy probe allowed
        std::thread::sleep(Duration::from_millis(160));
        assert_eq!(h.state("m"), HealthState::Healthy);
        // success closes it for good; failure in the fresh window just cools
        h.record_failure("m", false);
        assert_eq!(h.state("m"), HealthState::Healthy);
    }

    #[test]
    fn penalty_half_above_threshold_before_circuit() {
        let h = Health::new(tiny());
        // 2 samples, ratio 0.5 — exactly at threshold, not above
        h.record_success("m");
        h.record_failure("m", false);
        assert!((h.penalty("m") - 1.0).abs() < 1e-9, "ratio == threshold");
        // one more failure: ratio 2/3 > 0.5, 3 samples < circuit min…
        // but circuit min == 3 here, so it trips; use fresh model for the band
        h.record_success("m2");
        h.record_failure("m2", false);
        h.record_failure("m2", false); // ratio 2/3 > 0.5 → circuit (min 3)
        assert!(h.is_excluded("m2"));
        // band check with a policy where circuit needs more evidence
        let band = HealthPolicy {
            penalty_min_samples: 2,
            circuit_min_samples: 5,
            ..tiny()
        };
        let h = Health::new(band);
        h.record_success("m");
        h.record_failure("m", false);
        h.record_failure("m", false); // ratio 2/3 > 0.5, 3 samples < 5
        assert!(!h.is_excluded("m"), "circuit not tripped yet");
        assert!((h.penalty("m") - 0.5).abs() < 1e-9, "penalty band active");
    }

    #[test]
    fn snapshot_reports_cooldown() {
        let h = Health::new(tiny());
        h.record_failure("m", true);
        let snap = h.snapshot();
        let s = &snap["m"];
        assert_eq!(s.state, HealthState::Cooldown);
        assert!(s.secs_left <= 1);
    }
}
