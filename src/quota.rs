//! Quota-aware routing (docs/routing.md §3/§7): ingests `aitop --json`
//! snapshots into a [`QuotaState`], scores per-provider availability,
//! and orders attempt lists. Also owns the RFC3339 parsing (no chrono
//! dependency) and the provider-id → panel-name mapping.

use crate::registry::Model;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

// ───────────────────────── thresholds (routing.md §3) ─────────────────────────

/// Window is "scarce" above this pct.
pub const SCARCE_PCT: f64 = 80.0;
/// Scarce penalty floor: ×(1 − 0.7·overshoot).
pub const SCARCE_DEPTH: f64 = 0.7;
/// Drain horizon ("use it or lose it", §7): a window with remaining
/// quota that resets within this makes its provider a draining target —
/// the leftover quota is a wasting asset, so routing prefers it (no
/// scarcity/pace penalties; drain wins availability ties within its
/// grade, soonest reset first). Supersedes the old "preserve the last
/// bit before reset" behavior, which just wasted the quota instead.
pub const DRAIN_SECS: u64 = 4 * 3600;
/// Pace-ahead penalty.
pub const PACE_AHEAD_PENALTY: f64 = 0.75;
/// Snapshot older than this is "unknown" (×0.8, not excluded).
pub const STALE_AFTER_SECS: u64 = 300;
pub const STALE_PENALTY: f64 = 0.8;
/// Exhausted windows are skipped until reset − safety margin.
pub const RESET_SAFETY_SECS: u64 = 120;
/// Fairness headroom applies when availability exceeds this (§7).
pub const FAIRNESS_MIN_AVAIL: f64 = 0.9;

// ───────────────────────── RFC3339 (subset, no chrono) ─────────────────────────

/// Parse the RFC3339 subset aitop emits (`2026-10-11T09:35:42.280Z` /
/// `+02:00`) into unix seconds. Uses Howard Hinnant's civil-from-days.
pub fn parse_rfc3339_unix(s: &str) -> Option<u64> {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    if bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: i64 = s.get(5..7)?.parse().ok()?;
    let day: i64 = s.get(8..10)?.parse().ok()?;
    let hour: i64 = s.get(11..13)?.parse().ok()?;
    let min: i64 = s.get(14..16)?.parse().ok()?;
    let sec: i64 = s.get(17..19)?.parse().ok()?;

    // optional fraction, then Z or ±HH:MM
    let rest = &s[19..];
    let rest = rest.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let offset_secs: i64 = match rest {
        "" | "Z" | "z" => 0,
        r if r.len() == 6 && (r.starts_with('+') || r.starts_with('-')) => {
            let sign = if r.starts_with('-') { -1 } else { 1 };
            let oh: i64 = r.get(1..3)?.parse().ok()?;
            let om: i64 = r.get(4..6)?.parse().ok()?;
            sign * (oh * 3600 + om * 60)
        }
        _ => return None,
    };

    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + min * 60 + sec - offset_secs;
    u64::try_from(secs).ok()
}

/// Days since unix epoch for a civil date (Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ───────────────────────── aitop JSON → QuotaState ─────────────────────────

#[derive(Debug, Deserialize)]
struct RawSnapshot {
    #[serde(default)]
    fetched_at: Option<String>,
    #[serde(default)]
    panels: Vec<RawPanel>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawPanel {
    name: String,
    error: Option<String>,
    stale: Option<bool>,
    rows: Vec<RawRow>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawRow {
    label: String,
    pct: Option<f64>,
    pace: Option<String>,
    reset_at: Option<String>,
}

/// Parsed pace string ("pace behind 14%", "pace ahead 5%").
#[derive(Debug, Clone, PartialEq)]
pub enum Pace {
    /// burning slower than the window refills — good
    Behind(f64),
    /// burning faster than the window refills — penalized
    Ahead(f64),
}

impl Pace {
    fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let (dir, pct) = s
            .strip_prefix("pace behind ")
            .map(|p| (Pace::Behind as fn(f64) -> Pace, p))
            .or_else(|| {
                s.strip_prefix("pace ahead ")
                    .map(|p| (Pace::Ahead as fn(f64) -> Pace, p))
            })?;
        let pct: f64 = pct.trim_end_matches('%').trim().parse().ok()?;
        Some(dir(pct))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuotaWindow {
    pub label: String,
    pub pct: f64,
    pub reset_unix: Option<u64>,
    pub pace: Option<Pace>,
}

#[derive(Debug, Clone)]
pub struct ProviderState {
    pub panel: String,
    pub error: Option<String>,
    /// the source says these numbers are the last good ones, not live.
    pub stale: bool,
    pub windows: Vec<QuotaWindow>,
    /// Last per-provider update (native fetch / header harvest).
    /// `None` for whole-snapshot (command) sources → fall back to
    /// snapshot age for staleness.
    pub updated_unix: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct QuotaState {
    pub fetched_unix: Option<u64>,
    /// key: aitop panel name (e.g. "z.ai", "codex")
    pub providers: BTreeMap<String, ProviderState>,
    /// 0 = use `STALE_AFTER_SECS`. main sets `max(STALE_AFTER_SECS,
    /// 2 × poll interval)` so a healthy snapshot isn't penalized as
    /// stale between polls (review checkpoint: freshness ↔ polling).
    pub stale_after_secs: u64,
}

impl QuotaState {
    /// Schema-tolerant parse: missing panels/rows/fetch time never fail;
    /// a panel with `error` and no usable rows is kept as an error marker.
    pub fn parse(raw: &str) -> Self {
        let snap: RawSnapshot = match serde_json::from_str(raw) {
            Ok(s) => s,
            Err(_) => return Self::default(),
        };
        let fetched_unix = snap.fetched_at.as_deref().and_then(parse_rfc3339_unix);
        let mut providers = BTreeMap::new();
        for p in snap.panels {
            let windows = p
                .rows
                .into_iter()
                .filter_map(|r| {
                    let pct = r.pct?;
                    Some(QuotaWindow {
                        label: r.label,
                        pct,
                        reset_unix: r.reset_at.as_deref().and_then(parse_rfc3339_unix),
                        pace: r.pace.as_deref().and_then(Pace::parse),
                    })
                })
                .collect();
            providers.insert(
                p.name.clone(),
                ProviderState {
                    panel: p.name,
                    error: p.error,
                    stale: p.stale.unwrap_or(false),
                    windows,
                    updated_unix: None,
                },
            );
        }
        Self {
            fetched_unix,
            providers,
            stale_after_secs: 0,
        }
    }

    /// Whole snapshot age (None = never fetched).
    pub fn age_secs(&self, now: u64) -> Option<u64> {
        self.fetched_unix.map(|t| now.saturating_sub(t))
    }

    /// Effective staleness cutoff (configured override or const floor).
    fn stale_cut(&self) -> u64 {
        if self.stale_after_secs > 0 {
            self.stale_after_secs
        } else {
            STALE_AFTER_SECS
        }
    }

    pub fn is_stale(&self, now: u64) -> bool {
        self.age_secs(now).is_none_or(|a| a > self.stale_cut())
    }

    /// Per-provider staleness: the newest signal wins — windows
    /// harvested 30s ago keep a provider fresh even when the snapshot
    /// behind them is old.
    pub fn provider_stale(&self, p: &ProviderState, now: u64) -> bool {
        match p.updated_unix {
            Some(u) => now.saturating_sub(u) > self.stale_cut(),
            None => self.is_stale(now) || p.stale,
        }
    }

    /// Option-taking variant for call sites that resolve a provider
    /// that may never have been seen (None → snapshot age; availability
    /// treats "no data" separately anyway).
    pub fn provider_stale_soft(&self, p: Option<&ProviderState>, now: u64) -> bool {
        match p {
            Some(p) => self.provider_stale(p, now),
            None => self.is_stale(now),
        }
    }
}

// ───────────────────────── availability score (§3) ─────────────────────────

/// Per-provider availability in 0..=1 plus, when excluded by an exhausted
/// window, the unix time it becomes usable again (reset − safety margin).
#[derive(Debug, Clone)]
pub struct Availability {
    pub score: f64,
    pub retry_after_unix: Option<u64>,
    pub notes: Vec<String>,
}

impl Default for Availability {
    fn default() -> Self {
        Self {
            score: 1.0,
            retry_after_unix: None,
            notes: vec![],
        }
    }
}

pub fn provider_availability(
    p: Option<&ProviderState>,
    // per-provider staleness (QuotaState::provider_stale) — the newest
    // signal wins: recently harvested windows keep a provider fresh
    // even when the snapshot behind them is old
    stale: bool,
    now: u64,
) -> Availability {
    let mut a = Availability::default();
    let Some(p) = p else {
        // never seen: unknown ≠ unavailable
        a.score = STALE_PENALTY;
        a.notes.push("no quota data".into());
        return a;
    };
    if p.error.as_deref().is_some_and(|e| !e.is_empty()) && p.windows.is_empty() {
        a.score = STALE_PENALTY;
        a.notes.push("panel error".into());
        return a;
    }
    // exhausted: any window ≥ 100% excludes until nearest reset − safety
    if let Some(w) = p.windows.iter().find(|w| w.pct >= 100.0) {
        let retry = p
            .windows
            .iter()
            .filter_map(|w| w.reset_unix)
            .min()
            .map(|t| t.saturating_sub(RESET_SAFETY_SECS));
        a.score = 0.0;
        a.retry_after_unix = retry;
        a.notes
            .push(format!("exhausted ({} {:.0}%)", w.label, w.pct));
        return a;
    }
    // drain ("use it or lose it"): a window with leftover quota resetting
    // within DRAIN_SECS turns every preservation penalty off — the quota
    // is about to evaporate, so the only wrong move is not spending it.
    if let Some(w) = draining(Some(p), now) {
        a.notes
            .push(format!("draining ({} {:.0}%)", w.label, w.pct));
        if stale || p.stale {
            a.score *= STALE_PENALTY;
            a.notes.push("stale".into());
        }
        a.score = a.score.clamp(0.0, 1.0);
        return a;
    }
    // tightest window drives scarcity
    if let Some(w) = p.windows.iter().max_by(|x, y| x.pct.total_cmp(&y.pct)) {
        if w.pct > SCARCE_PCT {
            let over = (w.pct - SCARCE_PCT) / (100.0 - SCARCE_PCT);
            a.score *= 1.0 - over * SCARCE_DEPTH;
            a.notes.push(format!("scarce ({} {:.0}%)", w.label, w.pct));
        }
    }
    // pace ahead on any window → burning faster than the window refills
    if p.windows
        .iter()
        .any(|w| matches!(w.pace, Some(Pace::Ahead(_))))
    {
        a.score *= PACE_AHEAD_PENALTY;
        a.notes.push("pace ahead".into());
    }
    if stale || p.stale {
        a.score *= STALE_PENALTY;
        a.notes.push("stale".into());
    }
    a.score = a.score.clamp(0.0, 1.0);
    a
}

// ───────────────────── headroom & drain (§7) ─────────────────────

/// "Use it or lose it": the soonest-resetting window that still has
/// leftover quota and resets within DRAIN_SECS. `None` = not draining.
pub fn draining(p: Option<&ProviderState>, now: u64) -> Option<QuotaWindow> {
    let p = p?;
    p.windows
        .iter()
        .filter(|w| {
            w.pct < 100.0 && matches!(w.reset_unix, Some(t) if t > now && t - now <= DRAIN_SECS)
        })
        .min_by_key(|w| w.reset_unix.unwrap_or(u64::MAX))
        .cloned()
}

/// "quota-hours" left: (100 − tightest pct) × hours until that window
/// resets (windows without reset → 1h assumed; unobserved → 0 — it can
/// never win a fairness tie anyway, since unobserved availability is
/// capped at 0.8 by the stale penalty).
///
/// Deliberately *not* drain-aware: drain is its own ordering axis that
/// beats headroom (a dying window has the least to preserve and the
/// most to spend, so nearness-to-reset must help it, not hurt it).
pub fn headroom(p: Option<&ProviderState>, now: u64) -> f64 {
    let Some(p) = p else {
        return 0.0;
    };
    let Some(tightest) = p.windows.iter().max_by(|x, y| x.pct.total_cmp(&y.pct)) else {
        return 0.0;
    };
    let hours = match tightest.reset_unix {
        Some(t) if t > now => ((t - now) as f64 / 3600.0).max(0.25),
        _ => 1.0,
    };
    (100.0 - tightest.pct).max(0.0) * hours
}

// ───────────────────────── provider mapping ─────────────────────────

/// Resolve a registry provider prefix (`zai`, `or`) to an aitop panel
/// name. Explicit `[quota_watcher] provider_map` wins; else heuristics:
/// exact, dot/space-stripped case-insensitive match (`zai` ↔ `z.ai`).
#[derive(Debug, Clone, Default)]
pub struct ProviderMap {
    pub explicit: BTreeMap<String, String>,
}

impl ProviderMap {
    pub fn resolve<'a>(
        &self,
        provider: &str,
        providers: &'a BTreeMap<String, ProviderState>,
    ) -> Option<&'a ProviderState> {
        if let Some(name) = self.explicit.get(provider) {
            return providers.get(name);
        }
        if let Some(p) = providers.get(provider) {
            return Some(p);
        }
        let norm = |s: &str| s.replace(['.', ' ', '-', '_'], "").to_lowercase();
        let want = norm(provider);
        providers.values().find(|p| norm(&p.panel) == want)
    }
}

// ───────────────────────── watcher ─────────────────────────

/// Shared quota state, swapped by the watcher task (E3) and read by
/// every request. `RwLock<Arc<…>>`, never held across `.await`.
pub struct QuotaWatcher {
    state: RwLock<Arc<QuotaState>>,
}

impl Default for QuotaWatcher {
    fn default() -> Self {
        Self {
            state: RwLock::new(Arc::new(QuotaState::default())),
        }
    }
}

impl QuotaWatcher {
    pub fn new(initial: QuotaState) -> Arc<Self> {
        Arc::new(Self {
            state: RwLock::new(Arc::new(initial)),
        })
    }

    pub fn current(&self) -> Arc<QuotaState> {
        self.state.read().unwrap().clone()
    }

    /// Freshness cutoff, derived from the poll interval at startup.
    pub fn set_stale_after(&self, secs: u64) {
        let cur = self.current();
        let mut qs = QuotaState::clone(&cur);
        qs.stale_after_secs = secs;
        self.set(qs);
    }

    fn set(&self, qs: QuotaState) {
        *self.state.write().unwrap() = Arc::new(qs);
    }

    /// Incremental per-provider update (E3b): replace one provider's
    /// windows, keeping everyone else's. Used by native fetches and
    /// header harvest — the whole snapshot is only ever swapped by the
    /// legacy command watcher.
    pub fn apply(&self, provider: &str, windows: Vec<QuotaWindow>, now: u64) {
        let current = self.current();
        let mut qs = QuotaState::clone(&current);
        let entry = qs
            .providers
            .entry(provider.to_string())
            .or_insert_with(|| ProviderState {
                panel: provider.to_string(),
                error: None,
                stale: false,
                windows: vec![],
                updated_unix: None,
            });
        entry.windows = windows;
        entry.error = None;
        entry.stale = false;
        entry.updated_unix = Some(now);
        qs.fetched_unix = Some(qs.fetched_unix.map_or(now, |t| t.max(now)));
        self.set(qs);
    }

    /// Whole-snapshot swap for the legacy command watcher — but a
    /// provider whose last signal is NEWER than the snapshot (native
    /// fetch, header harvest, manual window) is preserved: the command
    /// never erases fresher knowledge (review checkpoint).
    pub fn merge_snapshot(&self, mut qs: QuotaState) {
        let cur = self.current();
        qs.stale_after_secs = cur.stale_after_secs;
        let snap_time = qs.fetched_unix;
        for (k, existing) in &cur.providers {
            if existing
                .updated_unix
                .is_some_and(|t| snap_time.is_none_or(|s| t > s))
            {
                qs.providers.insert(k.clone(), existing.clone());
            }
        }
        self.set(qs);
    }
}

/// Spawn the poll loop: runs `command` every `interval`, parses one
/// JSON snapshot, swaps it in. Failures keep the old snapshot (stale
/// penalties apply); missing binary disables after the first warning.
pub async fn run_watcher(watcher: Arc<QuotaWatcher>, command: &str, interval: Duration) {
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut warned_missing = false;
    loop {
        tick.tick().await;
        let parts: Vec<&str> = command.split_whitespace().collect();
        let Some((prog, args)) = parts.split_first() else {
            return; // empty command = disabled
        };
        let out = tokio::time::timeout(Duration::from_secs(30), async {
            tokio::process::Command::new(prog)
                .args(args)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .output()
                .await
        })
        .await;
        match out {
            Ok(Ok(o)) if o.status.success() => {
                let text = String::from_utf8_lossy(&o.stdout);
                let qs = QuotaState::parse(&text);
                let n = qs.providers.len();
                watcher.merge_snapshot(qs);
                tracing::debug!("quota snapshot: {n} providers");
            }
            Ok(Ok(o)) => {
                tracing::warn!(
                    "aitop exited {}: keeping last snapshot",
                    o.status.code().unwrap_or(-1)
                );
            }
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                if !warned_missing {
                    tracing::warn!(
                        "quota watcher: {prog:?} not found — routing degrades to health-only (install aitop or clear [quota_watcher] command)"
                    );
                    warned_missing = true;
                }
            }
            Ok(Err(e)) => {
                tracing::warn!("quota watcher spawn failed: {e}");
            }
            Err(_) => tracing::warn!("quota watcher: aitop timed out (30s), keeping last snapshot"),
        }
    }
}

// ───────────────────────── attempt ordering (§4/§5/§7) ─────────────────────────

/// What the ordering needs to know about the request (ondemand cost est).
#[derive(Debug, Clone, Copy)]
pub struct RequestEstimate {
    /// prompt tokens, chars/4 heuristic
    pub est_in: f64,
    /// response cap used for cost estimate: min(max_tokens, 8192) × 0.5
    pub est_out: f64,
    /// max_tokens as sent (context filter)
    pub max_tokens: Option<u64>,
}

pub fn estimate_request(payload: &serde_json::Value) -> RequestEstimate {
    let mut chars: usize = 0;
    if let Some(msgs) = payload.get("messages").and_then(|m| m.as_array()) {
        for m in msgs {
            if let Some(c) = m.get("content").and_then(|c| c.as_str()) {
                chars += c.len();
            }
        }
    }
    let max_tokens = payload
        .get("max_tokens")
        .and_then(|v| v.as_u64())
        .or_else(|| {
            payload
                .get("max_completion_tokens")
                .and_then(|v| v.as_u64())
        });
    RequestEstimate {
        est_in: chars as f64 / 4.0,
        est_out: max_tokens.unwrap_or(8192).min(8192) as f64 * 0.5,
        max_tokens,
    }
}

/// Context-window filter (routing.md §1): models whose declared context
/// cannot fit the estimate are skipped. Unknown context = never filtered.
pub fn context_fits(m: &Model, est: &RequestEstimate) -> bool {
    let Some(ctx) = m.context else {
        return true;
    };
    let need = est.est_in.ceil() as u64 + est.max_tokens.unwrap_or(0);
    need <= ctx
}

/// Estimated cost in $ for this request (§4/§6): tokens_in × cost_in +
/// expected_out × cost_out. Shared by ordering and budget preflight.
pub fn est_cost(m: &Model, est: &RequestEstimate) -> f64 {
    (est.est_in / 1e6) * m.cost_in + (est.est_out / 1e6) * m.cost_out
}

/// Sort mode — the proxy derives it from pool + `[pool.*] preference`.
/// `QuotaPool`: availability × weight, then headroom (§7), then id.
/// `Cheapest`: availability, then est_cost asc, then weight, then id.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OrderMode {
    QuotaPool,
    Cheapest,
    /// observed speed first within the grade (F11): latency EMA from
    /// health, unseen models last
    Fastest,
}

/// Sort candidates for the request. Consumes and returns a sorted Vec.
pub fn order_candidates<'a>(
    quota: &QuotaState,
    health: &crate::health::Health,
    provider_map: &ProviderMap,
    mode: OrderMode,
    est: &RequestEstimate,
    candidates: Vec<&'a Model>,
) -> Vec<&'a Model> {
    let now = unix_now();

    let scored: Vec<_> = candidates
        .into_iter()
        .map(|m| {
            let provider = m.id.split('/').next().unwrap_or(&m.id);
            let ps = provider_map.resolve(provider, &quota.providers);
            let avail = provider_availability(ps, quota.provider_stale_soft(ps, now), now).score;
            let health_penalty = health.penalty(&m.id);
            let hr = headroom(ps, now);
            // §7 drain: reset unix of the dying window, soonest first;
            // u64::MAX = not draining (sorts after every drainer)
            let drain = draining(ps, now)
                .and_then(|w| w.reset_unix)
                .unwrap_or(u64::MAX);
            (m, avail, health_penalty, hr, drain)
        })
        .collect();

    let mut out = scored;
    if mode == OrderMode::Fastest {
        // F11: grade primary, then observed latency EMA ascending —
        // the preference `fastest` finally means something
        let speed = |s: &(&Model, f64, f64, f64, u64)| {
            health.speed_ema_ms(&s.0.id).unwrap_or(f64::INFINITY)
        };
        out.sort_by(|a, b| {
            a.0.grade_rank.cmp(&b.0.grade_rank).then_with(|| {
                speed(a)
                    .partial_cmp(&speed(b))
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| (b.1 * b.2).total_cmp(&(a.1 * a.2)))
                    .then_with(|| a.0.id.cmp(&b.0.id))
            })
        });
    } else if mode == OrderMode::Cheapest {
        // grade is always primary (§4/§5): cheapest-first orders *within*
        // each allowed grade — never silently drops a grade to save money.
        out.sort_by(|a, b| {
            a.0.grade_rank
                .cmp(&b.0.grade_rank)
                .then_with(|| {
                    // availability × health descending, then est_cost ascending
                    (b.1 * b.2).total_cmp(&(a.1 * a.2))
                })
                .then_with(|| est_cost(a.0, est).total_cmp(&est_cost(b.0, est)))
                .then_with(|| b.0.weight.total_cmp(&a.0.weight))
                .then_with(|| a.0.id.cmp(&b.0.id))
        });
    } else {
        // §5: grade primary, then availability × health × weight
        // (descending); §7: a draining window ("use it or lose it") wins
        // availability ties within its grade, soonest reset first — only
        // then does headroom spread load among the rest.
        let fair = |s: &(&Model, f64, f64, f64, u64)| (s.1 > FAIRNESS_MIN_AVAIL).then_some(s.3);
        out.sort_by(|a, b| {
            a.0.grade_rank.cmp(&b.0.grade_rank).then_with(|| {
                (b.1 * b.2 * b.0.weight)
                    .total_cmp(&(a.1 * a.2 * a.0.weight))
                    .then_with(|| a.4.cmp(&b.4)) // drain: soonest-dying first
                    .then_with(|| {
                        fair(b)
                            .partial_cmp(&fair(a))
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .then_with(|| a.0.id.cmp(&b.0.id))
            })
        });
    }
    out.into_iter().map(|(m, ..)| m).collect()
}

/// Whether a model's provider is usable right now (availability > 0 —
/// only an exhausted quota window zeroes it).
pub fn model_available(quota: &QuotaState, map: &ProviderMap, m: &Model) -> bool {
    let provider = m.id.split('/').next().unwrap_or(&m.id);
    let ps = map.resolve(provider, &quota.providers);
    provider_availability(ps, quota.provider_stale_soft(ps, unix_now()), unix_now()).score > 0.0
}

/// Nearest quota-driven retry point among candidates (unix secs), for
/// `Retry-After` on arouter's own exhausted 429 (§5).
pub fn nearest_retry_unix(
    quota: &QuotaState,
    provider_map: &ProviderMap,
    candidates: &[&Model],
) -> Option<u64> {
    let now = unix_now();
    candidates
        .iter()
        .filter_map(|m| {
            let provider = m.id.split('/').next().unwrap_or(&m.id);
            let ps = provider_map.resolve(provider, &quota.providers);
            provider_availability(ps, quota.provider_stale_soft(ps, now), now).retry_after_unix
        })
        .min()
}

// ───────────────────────── tests ─────────────────────────

#[cfg(test)]
#[allow(clippy::useless_vec)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
        "fetched_at": "2026-10-11T08:52:58.354Z",
        "panels": [
            {"name": "codex", "subtitle": "plus", "rows": [
                {"label": "5h window", "pct": 100.0, "detail": "resets in 42m",
                 "pace": "pace behind 14%", "cap": null,
                 "reset_at": "2026-10-11T09:35:42Z"},
                {"label": "7d window", "pct": 16.0, "detail": "", "pace": null,
                 "cap": null, "reset_at": "2026-10-18T04:35:42Z"}],
             "lines": [], "spark": [], "error": null, "stale": false},
            {"name": "z.ai", "rows": [
                {"label": "5h window", "pct": 30.0, "detail": "3.6k / 12.0k",
                 "pace": "resets in 3h47m", "cap": "12.0k",
                 "reset_at": "2026-10-11T12:40:08Z"}],
             "error": null, "stale": false},
            {"name": "broken", "rows": [], "error": "auth failed", "stale": true},
            {"name": "empty"}
        ]
    }"#;

    #[test]
    fn rfc3339_parsing() {
        assert_eq!(parse_rfc3339_unix("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_unix("2026-10-11T08:52:58.354649478+00:00"),
            parse_rfc3339_unix("2026-10-11T08:52:58Z")
        );
        // offset shifts
        let z = parse_rfc3339_unix("2026-10-11T10:00:00Z").unwrap();
        let plus2 = parse_rfc3339_unix("2026-10-11T12:00:00+02:00").unwrap();
        let minus5 = parse_rfc3339_unix("2026-10-11T05:00:00-05:00").unwrap();
        assert_eq!(z, plus2);
        assert_eq!(z, minus5);
        // leap-ish date sanity: 2024-02-29 exists
        assert_eq!(
            parse_rfc3339_unix("2024-03-01T00:00:00Z").unwrap()
                - parse_rfc3339_unix("2024-02-29T00:00:00Z").unwrap(),
            86_400
        );
        assert_eq!(parse_rfc3339_unix("garbage"), None);
        assert_eq!(parse_rfc3339_unix("2026-10-11"), None);
    }

    #[test]
    fn fixture_parse_tolerant() {
        let qs = QuotaState::parse(FIXTURE);
        assert_eq!(qs.providers.len(), 4, "empty panel kept, not fatal");
        assert!(qs.fetched_unix.is_some());
        let codex = &qs.providers["codex"];
        assert_eq!(codex.windows.len(), 2);
        assert_eq!(codex.windows[0].pace, Some(Pace::Behind(14.0)));
        let zai = &qs.providers["z.ai"];
        // "resets in 3h47m" is not a pace string → None
        assert_eq!(zai.windows[0].pace, None);
        assert!(qs.providers["broken"].windows.is_empty());
        assert_eq!(qs.providers["broken"].error.as_deref(), Some("auth failed"));

        // garbage input → empty, never panics
        let bad = QuotaState::parse("not json at all");
        assert!(bad.providers.is_empty());
        assert!(bad.fetched_unix.is_none());
    }

    #[test]
    fn exhausted_excludes_with_retry_after() {
        let qs = QuotaState::parse(FIXTURE);
        let now = parse_rfc3339_unix("2026-10-11T08:52:58Z").unwrap();
        let codex = provider_availability(qs.providers.get("codex"), false, now);
        assert_eq!(codex.score, 0.0);
        // 09:35:42 reset − 120s safety
        assert_eq!(
            codex.retry_after_unix,
            Some(parse_rfc3339_unix("2026-10-11T09:33:42Z").unwrap())
        );
        // z.ai 30%: fully available
        let zai = provider_availability(qs.providers.get("z.ai"), false, now);
        assert!((zai.score - 1.0).abs() < 1e-9, "got {}", zai.score);
        // error panel with no rows → unknown, not excluded
        let broken = provider_availability(qs.providers.get("broken"), false, now);
        assert!((broken.score - STALE_PENALTY).abs() < 1e-9);
    }

    fn panel(pcts_resets: &[(f64, Option<u64>)], pace: Option<Pace>) -> ProviderState {
        ProviderState {
            panel: "x".into(),
            error: None,
            stale: false,
            updated_unix: None,
            windows: pcts_resets
                .iter()
                .enumerate()
                .map(|(i, (pct, reset))| QuotaWindow {
                    label: format!("w{i}"),
                    pct: *pct,
                    reset_unix: *reset,
                    pace: pace.clone(),
                })
                .collect(),
        }
    }

    #[test]
    fn scarcity_penalties_and_drain() {
        let now = 1_000_000u64;
        // 90% scarce with days left: ×(1 − 0.5·0.7) = 0.65
        let p = panel(&[(90.0, Some(now + 25 * 3600))], None);
        let a = provider_availability(Some(&p), false, now);
        assert!((a.score - 0.65).abs() < 1e-9, "scarce: {}", a.score);
        // 90% but the window resets within DRAIN_SECS → use it or lose it:
        // no scarcity penalty, drain note instead (was ×0.65 ×0.6)
        let p = panel(&[(90.0, Some(now + 600))], None);
        let a = provider_availability(Some(&p), false, now);
        assert!((a.score - 1.0).abs() < 1e-9, "drain: {}", a.score);
        assert!(a.notes.iter().any(|n| n.starts_with("draining")));
        // 50% resetting in 2h34m (the z.ai weekly case): fully available
        let p = panel(&[(50.0, Some(now + 9240))], None);
        let a = provider_availability(Some(&p), false, now);
        assert!((a.score - 1.0).abs() < 1e-9);
        // pace ahead is meaningless for a dying window → penalty off
        let p = panel(&[(85.0, Some(now + 3600))], Some(Pace::Ahead(3.0)));
        let a = provider_availability(Some(&p), false, now);
        assert!(
            (a.score - 1.0).abs() < 1e-9,
            "drain beats pace: {}",
            a.score
        );
        // pace ahead on a surviving window still penalizes
        let p = panel(&[(10.0, None)], Some(Pace::Ahead(3.0)));
        let a = provider_availability(Some(&p), false, now);
        assert!((a.score - PACE_AHEAD_PENALTY).abs() < 1e-9);
        // panel flagged stale by aitop
        let p = ProviderState {
            stale: true,
            ..panel(&[(10.0, None)], None)
        };
        let a = provider_availability(Some(&p), false, now);
        assert!((a.score - STALE_PENALTY).abs() < 1e-9);
    }

    #[test]
    fn drain_selects_soonest_dying_window() {
        let now = 1_000_000u64;
        let p = panel(
            &[(50.0, Some(now + 3 * 3600)), (40.0, Some(now + 3600))],
            None,
        );
        let w = draining(Some(&p), now).unwrap();
        assert_eq!(w.reset_unix, Some(now + 3600), "soonest reset wins");
        // exhausted window never drains; past resets don't count
        let p = panel(&[(100.0, Some(now + 600)), (10.0, Some(now - 60))], None);
        assert!(draining(Some(&p), now).is_none());
        // just past the horizon → not draining
        let p = panel(&[(50.0, Some(now + DRAIN_SECS + 1))], None);
        assert!(draining(Some(&p), now).is_none());
        assert!(draining(None, now).is_none());
    }

    #[test]
    fn drain_orders_dying_provider_first() {
        // z.ai weekly at 50% resetting in 2h34m vs a competitor with the
        // same numbers but days left: the wasted-quota provider serves.
        // (order_candidates uses wall-clock now, so resets must be real.)
        let now = unix_now();
        let mut zai_p = panel(&[(50.0, Some(now + 9240))], None);
        zai_p.panel = "z.ai".into();
        let mut slow_p = panel(&[(50.0, Some(now + 5 * 86400))], None);
        slow_p.panel = "slow".into();
        let quota = QuotaState {
            stale_after_secs: 0,
            fetched_unix: Some(now),
            providers: BTreeMap::from([("z.ai".into(), zai_p), ("slow".into(), slow_p)]),
        };
        let map = ProviderMap::default();
        let health = crate::health::Health::new(crate::health::HealthPolicy::default());
        let est = RequestEstimate {
            est_in: 10.0,
            est_out: 100.0,
            max_tokens: None,
        };
        let models = vec![
            Model {
                wire: "openai".into(),
                id: "slow/m".into(),
                grade_rank: 1,
                ..Default::default()
            },
            Model {
                id: "zai/m".into(),
                grade_rank: 1,
                ..Default::default()
            },
        ];
        let refs: Vec<&Model> = models.iter().collect();
        let out = order_candidates(&quota, &health, &map, OrderMode::QuotaPool, &est, refs);
        assert_eq!(out[0].id, "zai/m", "draining provider wins the tie");

        // among two drainers, the sooner reset goes first
        let mut a_p = panel(&[(50.0, Some(now + 3 * 3600))], None);
        a_p.panel = "a".into();
        let mut b_p = panel(&[(50.0, Some(now + 1800))], None);
        b_p.panel = "b".into();
        let quota = QuotaState {
            stale_after_secs: 0,
            fetched_unix: Some(now),
            providers: BTreeMap::from([("a".into(), a_p), ("b".into(), b_p)]),
        };
        let models = vec![
            Model {
                wire: "openai".into(),
                id: "a/m".into(),
                grade_rank: 1,
                ..Default::default()
            },
            Model {
                id: "b/m".into(),
                grade_rank: 1,
                ..Default::default()
            },
        ];
        let refs: Vec<&Model> = models.iter().collect();
        let out = order_candidates(&quota, &health, &map, OrderMode::QuotaPool, &est, refs);
        assert_eq!(out[0].id, "b/m", "sooner reset drains first");

        // grade stays primary: an A+ drainer never serves a B-only request
        // (covered by grade_rank ordering upstream; sanity here)
        let models = vec![
            Model {
                id: "zai/ap".into(),
                grade_rank: 0,
                ..Default::default()
            },
            Model {
                id: "slow/b".into(),
                grade_rank: 2,
                ..Default::default()
            },
        ];
        let refs: Vec<&Model> = models.iter().collect();
        let out = order_candidates(&quota, &health, &map, OrderMode::QuotaPool, &est, refs);
        assert_eq!(out[0].id, "zai/ap");
    }

    #[test]
    fn headroom_spreads_load() {
        let now = 1_000_000u64;
        // 30% with 4h left vs 30% with 24h left → latter has more quota-hours
        let short = panel(&[(30.0, Some(now + 4 * 3600))], None);
        let long = panel(&[(30.0, Some(now + 24 * 3600))], None);
        assert!(headroom(Some(&long), now) > headroom(Some(&short), now));
        assert_eq!(headroom(None, now), 0.0, "unobserved = conservative");
    }

    #[test]
    fn provider_map_resolution() {
        let qs = QuotaState::parse(FIXTURE);
        let mut map = ProviderMap::default();
        assert!(map.resolve("zai", &qs.providers).is_some(), "zai ↔ z.ai");
        assert!(map.resolve("codex", &qs.providers).is_some());
        assert!(map.resolve("nope", &qs.providers).is_none());
        map.explicit.insert("or".into(), "z.ai".into());
        assert!(map.resolve("or", &qs.providers).is_some());
        // explicit wins even when wrong
        map.explicit.insert("codex".into(), "missing".into());
        assert!(map.resolve("codex", &qs.providers).is_none());
    }

    #[test]
    fn stale_snapshot_deprioritizes() {
        let mut qs = QuotaState::parse(FIXTURE);
        qs.fetched_unix = Some(unix_now() - STALE_AFTER_SECS - 10);
        let now = unix_now();
        let a = provider_availability(qs.providers.get("z.ai"), qs.is_stale(now), now);
        assert!((a.score - STALE_PENALTY).abs() < 1e-9, "stale ×0.8");
    }

    #[test]
    fn unknown_quota_still_honors_weight() {
        // unobserved providers all score 0.8 — config weight must still
        // order them (regression: ascending sort once inverted this).
        let quota = QuotaState::default();
        let health = crate::health::Health::new(crate::health::HealthPolicy::default());
        let est = RequestEstimate {
            est_in: 10.0,
            est_out: 100.0,
            max_tokens: None,
        };
        let primary = Model {
            wire: "openai".into(),
            weight: 2.0,
            ..model_stub_with_id("zai/primary")
        };
        let backup = Model {
            wire: "openai".into(),
            weight: 1.0,
            ..model_stub_with_id("zai/backup")
        };
        let ordered = order_candidates(
            &quota,
            &health,
            &ProviderMap::default(),
            OrderMode::QuotaPool,
            &est,
            vec![&primary, &backup],
        );
        assert_eq!(ordered[0].id, "zai/primary");
        assert_eq!(ordered[1].id, "zai/backup");
    }

    #[test]
    fn ordering_quota_pool_prefers_headroom() {
        let now = unix_now();
        let mut quota = QuotaState {
            fetched_unix: Some(now),
            ..QuotaState::default()
        };
        // resets beyond DRAIN_SECS so §7 headroom (not drain) decides
        quota.providers.insert(
            "a".into(),
            panel(&[(30.0, Some(now + DRAIN_SECS + 3600))], None),
        );
        quota.providers.insert(
            "b".into(),
            panel(&[(30.0, Some(now + DRAIN_SECS + 20 * 3600))], None),
        );
        let m = |id: &str| Model {
            wire: "openai".into(),
            id: id.into(),
            grade_rank: 1,
            pools: vec!["quota".into()],
            base_url: "http://x".into(),
            api_key: None,
            weight: 1.0,
            context: None,
            cost_in: 0.0,
            cost_out: 0.0,
            blended_cost: 0.0,
            upstream_name: "x".into(),
            profiles: vec![],
            provenance: "manual".into(),
            role_map: Default::default(),
        };
        let ma = m("a/m");
        let mb = m("b/m");
        let cands: Vec<&Model> = vec![&ma, &mb];
        let health = crate::health::Health::new(crate::health::HealthPolicy::default());
        let est = RequestEstimate {
            est_in: 100.0,
            est_out: 4096.0,
            max_tokens: None,
        };
        let ordered = order_candidates(
            &quota,
            &health,
            &ProviderMap::default(),
            OrderMode::QuotaPool,
            &est,
            cands,
        );
        assert_eq!(
            ordered[0].id, "b/m",
            "same avail+weight → more headroom first"
        );
        assert_eq!(ordered[1].id, "a/m");
    }

    #[test]
    fn grade_is_primary_even_when_cheaper_grade_exists() {
        // regression (E4 smoke): an A request with allow [A, B] must try
        // the A model first even when a B model is far cheaper.
        let quota = QuotaState::default();
        let health = crate::health::Health::new(crate::health::HealthPolicy::default());
        let est = RequestEstimate {
            est_in: 1_000_000.0,
            est_out: 4096.0,
            max_tokens: None,
        };
        let a_model = Model {
            wire: "openai".into(),
            grade_rank: 0, // A (ladder A+ first? ranks are 0-based best)
            cost_in: 5.0,
            cost_out: 25.0,
            ..model_stub_with_id("or/expensive-a")
        };
        let b_model = Model {
            wire: "openai".into(),
            grade_rank: 1,
            cost_in: 0.01,
            cost_out: 0.02,
            ..model_stub_with_id("or/cheap-b")
        };
        let ordered = order_candidates(
            &quota,
            &health,
            &ProviderMap::default(),
            OrderMode::Cheapest,
            &est,
            vec![&b_model, &a_model],
        );
        assert_eq!(ordered[0].id, "or/expensive-a", "grade outranks cost");
        assert_eq!(ordered[1].id, "or/cheap-b");
    }

    #[test]
    fn ordering_ondemand_cheapest_first() {
        let quota = QuotaState::default();
        let health = crate::health::Health::new(crate::health::HealthPolicy::default());
        let est = RequestEstimate {
            est_in: 1_000_000.0, // 1Mtok in
            est_out: 4096.0,
            max_tokens: None,
        };
        let m = |id: &str, cin: f64, cout: f64| Model {
            wire: "openai".into(),
            id: id.into(),
            grade_rank: 1,
            pools: vec!["ondemand".into()],
            base_url: "http://x".into(),
            api_key: None,
            weight: 1.0,
            context: None,
            cost_in: cin,
            cost_out: cout,
            blended_cost: 0.0,
            upstream_name: "x".into(),
            profiles: vec![],
            provenance: "manual".into(),
            role_map: Default::default(),
        };
        let mp = m("x/pricey", 9.0, 27.0);
        let mm = m("x/mid", 1.0, 4.0);
        let mc = m("x/cheap", 0.3, 0.5);
        let cands: Vec<&Model> = vec![&mp, &mm, &mc];
        let ordered = order_candidates(
            &quota,
            &health,
            &ProviderMap::default(),
            OrderMode::Cheapest,
            &est,
            cands,
        );
        let ids: Vec<&str> = ordered.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["x/cheap", "x/mid", "x/pricey"]);
    }

    #[test]
    fn fastest_orders_within_grade_by_observed_latency() {
        // F11: preference `fastest` = grade primary, then latency EMA
        let quota = QuotaState::default();
        let health = crate::health::Health::new(crate::health::HealthPolicy::default());
        let est = RequestEstimate {
            est_in: 100.0,
            est_out: 100.0,
            max_tokens: None,
        };
        let m = |id: &str, rank: usize| Model {
            id: id.into(),
            grade_rank: rank,
            pools: vec!["quota".into()],
            base_url: "http://x".into(),
            api_key: None,
            weight: 1.0,
            context: None,
            cost_in: 1.0,
            cost_out: 4.0,
            blended_cost: 0.0,
            upstream_name: "x".into(),
            profiles: vec![],
            provenance: "manual".into(),
            role_map: Default::default(),
            wire: "openai".into(),
        };
        let slow = m("x/slow", 1);
        let fast = m("x/fast", 1);
        let unseen = m("x/unseen", 1);
        let top = m("x/top-grade", 0); // grade always primary
        health.record_success_latency("x/slow", Some(900));
        health.record_success_latency("x/fast", Some(40));
        let cands: Vec<&Model> = vec![&slow, &unseen, &top, &fast];
        let ordered = order_candidates(
            &quota,
            &health,
            &ProviderMap::default(),
            OrderMode::Fastest,
            &est,
            cands,
        );
        let ids: Vec<&str> = ordered.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["x/top-grade", "x/fast", "x/slow", "x/unseen"]);
    }

    #[test]
    fn estimate_and_context_filter() {
        let payload = serde_json::json!({
            "model": "A",
            "max_tokens": 100000,
            "messages": [
                {"role": "user", "content": "x".repeat(4000)}
            ]
        });
        let est = estimate_request(&payload);
        assert!((est.est_in - 1000.0).abs() < 1e-9);
        assert_eq!(est.max_tokens, Some(100_000));
        assert!((est.est_out - 4096.0).abs() < 1e-9, "min(100k,8192)×0.5");

        let small = Model {
            wire: "openai".into(),
            context: Some(8192),
            ..model_stub()
        };
        assert!(!context_fits(&small, &est), "100k max_tokens > 8k ctx");
        let big = Model {
            wire: "openai".into(),
            context: Some(200_000),
            ..model_stub()
        };
        assert!(context_fits(&big, &est));
        let unknown = Model {
            wire: "openai".into(),
            context: None,
            ..model_stub()
        };
        assert!(context_fits(&unknown, &est), "unknown ctx never filtered");
    }

    fn model_stub() -> Model {
        model_stub_with_id("x/m")
    }

    fn model_stub_with_id(id: &str) -> Model {
        Model {
            id: id.into(),
            grade_rank: 1,
            wire: "openai".into(),
            pools: vec!["quota".into()],
            base_url: "http://x".into(),
            api_key: None,
            weight: 1.0,
            context: None,
            cost_in: 0.0,
            cost_out: 0.0,
            blended_cost: 0.0,
            upstream_name: "x".into(),
            profiles: vec![],
            provenance: "manual".into(),
            role_map: Default::default(),
        }
    }

    #[test]
    fn nearest_retry_across_candidates() {
        let mut quota = QuotaState::default();
        let now = unix_now();
        quota.fetched_unix = Some(now);
        quota
            .providers
            .insert("a".into(), panel(&[(100.0, Some(now + 600))], None));
        quota
            .providers
            .insert("b".into(), panel(&[(100.0, Some(now + 3000))], None));
        let m1 = model_stub();
        let m2 = Model {
            id: "b/m".into(),
            ..model_stub()
        };
        let retry = nearest_retry_unix(&quota, &ProviderMap::default(), &[&m1, &m2]);
        assert_eq!(retry, Some(now + 600 - RESET_SAFETY_SECS));
    }
}
