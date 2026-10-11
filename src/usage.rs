//! Usage log: one JSON line per attempt/request in
//! `<state_dir>/usage.jsonl` plus in-memory aggregates for `/status.json`
//! and future budget enforcement (E4).

use serde::Serialize;
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct UsageRecord {
    pub ts_unix: u64,
    pub pool: String,
    pub profile: String,
    /// Who called arouter (key-mapped identity or `X-Arouter-User` header;
    /// "anonymous" when unidentified) — enables per-user metrics.
    pub caller: String,
    /// Optional project tag (per-project usage tracking).
    pub project: Option<String>,
    pub grade_requested: String,
    pub grade_served: String,
    pub model: String,
    /// ok | aborted | upstream_error | connect_error
    pub status: String,
    pub http_status: Option<u16>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub cost_usd: f64,
    pub latency_ms: u64,
    pub ttfb_ms: Option<u64>,
    pub degraded: bool,
}

impl UsageRecord {
    pub fn cost_for(
        tokens_in: Option<u64>,
        tokens_out: Option<u64>,
        cost_in: f64,
        cost_out: f64,
    ) -> f64 {
        let tin = tokens_in.unwrap_or(0) as f64;
        let tout = tokens_out.unwrap_or(0) as f64;
        (tin * cost_in + tout * cost_out) / 1_000_000.0
    }
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct ModelStats {
    pub requests: u64,
    pub ok: u64,
    pub failed: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: f64,
    pub last_error: Option<String>,
    pub last_ts_unix: Option<u64>,
}

/// Aggregates keyed by caller (`name` or `name/project`).
#[derive(Debug, Default, Clone, Serialize)]
pub struct CallerStats {
    pub requests: u64,
    pub ok: u64,
    pub failed: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: f64,
    pub last_ts_unix: Option<u64>,
}

struct Inner {
    file: Option<std::fs::File>,
    path: Option<PathBuf>,
    stats: BTreeMap<String, ModelStats>,
    callers: BTreeMap<String, CallerStats>,
    total: u64,
    /// Raw records retained for time-windowed budget queries (E4).
    /// Bounded to [`RETENTION_SECS`]; the JSONL file keeps the full audit
    /// trail.
    ring: std::collections::VecDeque<UsageRecord>,
    /// Cache for the 24h hourly series (E5 monitor): rebuild only when
    /// the ring changed (`total` moved) or the UTC hour rolled.
    hour_cache: Option<HourCache>,
}

struct HourCache {
    built_for_total: u64,
    built_for_hour: u64,
    by_model: BTreeMap<String, [u64; 24]>,
    by_caller: BTreeMap<String, [u64; 24]>,
}

/// Last-24h token buckets per model and per caller (E5 monitor
/// sparklines). Cached on `Inner`: rebuilt only when the ring grew or
/// the UTC hour rolled. Bucket 23 = current hour.
fn hourly_series(inner: &Inner) -> (BTreeMap<String, [u64; 24]>, BTreeMap<String, [u64; 24]>) {
    let now = unix_now();
    let cur_hour = now / 3600;
    let cache_ok = inner
        .hour_cache
        .as_ref()
        .is_some_and(|c| c.built_for_total == inner.total && c.built_for_hour == cur_hour);
    if cache_ok {
        let c = inner.hour_cache.as_ref().unwrap();
        return (c.by_model.clone(), c.by_caller.clone());
    }
    let first = cur_hour.saturating_sub(23);
    let mut by_model: BTreeMap<String, [u64; 24]> = BTreeMap::new();
    let mut by_caller: BTreeMap<String, [u64; 24]> = BTreeMap::new();
    for r in &inner.ring {
        if r.status != "ok" {
            continue;
        }
        let hour = r.ts_unix / 3600;
        if hour < first {
            continue;
        }
        let idx = (hour - first) as usize; // ≤ 23 by construction
        let toks = r.tokens_in.unwrap_or(0) + r.tokens_out.unwrap_or(0);
        by_model.entry(r.model.clone()).or_insert([0; 24])[idx] += toks;
        let ckey = match &r.project {
            Some(p) if !p.is_empty() => format!("{}/{}", r.caller, p),
            _ => r.caller.clone(),
        };
        by_caller.entry(ckey).or_insert([0; 24])[idx] += toks;
    }
    (by_model, by_caller)
}

/// In-memory retention for budget queries: 32 days covers the monthly
/// (rolling 30d) window with margin. JSONL stays the full audit log.
const RETENTION_SECS: u64 = 32 * 86_400;

pub struct UsageLog {
    inner: Mutex<Inner>,
}

impl UsageLog {
    /// Open (creating dirs if needed) `<dir>/usage.jsonl`. On failure, log to
    /// memory only — usage tracking must never take routing down.
    pub fn open(dir: &str) -> anyhow::Result<Self> {
        let dir = crate::config::expand_tilde(dir);
        let path = PathBuf::from(&dir).join("usage.jsonl");
        let file = std::fs::create_dir_all(&dir).ok().and_then(|_| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .ok()
        });
        if file.is_none() {
            tracing::warn!(
                "cannot open {} — usage logging to memory only",
                path.display()
            );
        }
        let ring = load_ring(&path);
        Ok(Self {
            inner: Mutex::new(Inner {
                file,
                path: Some(path),
                stats: BTreeMap::new(),
                callers: BTreeMap::new(),
                total: 0,
                ring,
                hour_cache: None,
            }),
        })
    }

    /// Memory-only log (tests, dry runs).
    pub fn in_memory() -> Self {
        Self {
            inner: Mutex::new(Inner {
                file: None,
                path: None,
                stats: BTreeMap::new(),
                callers: BTreeMap::new(),
                total: 0,
                ring: std::collections::VecDeque::new(),
                hour_cache: None,
            }),
        }
    }

    pub fn record(&self, rec: UsageRecord) {
        let mut inner = self.inner.lock().unwrap();
        inner.total += 1;
        // budget ring: append + prune (ring is time-ordered)
        inner.ring.push_back(rec.clone());
        let cutoff = rec.ts_unix.saturating_sub(RETENTION_SECS);
        while inner.ring.front().is_some_and(|r| r.ts_unix < cutoff) {
            inner.ring.pop_front();
        }
        let entry = inner.stats.entry(rec.model.clone()).or_default();
        entry.requests += 1;
        entry.last_ts_unix = Some(rec.ts_unix);
        if rec.status == "ok" {
            entry.ok += 1;
            entry.tokens_in += rec.tokens_in.unwrap_or(0);
            entry.tokens_out += rec.tokens_out.unwrap_or(0);
            entry.cost_usd += rec.cost_usd;
        } else {
            entry.failed += 1;
            entry.last_error = Some(format!("{} (http {:?})", rec.status, rec.http_status));
        }
        let ckey = match &rec.project {
            Some(p) if !p.is_empty() => format!("{}/{}", rec.caller, p),
            _ => rec.caller.clone(),
        };
        let c = inner.callers.entry(ckey).or_default();
        c.requests += 1;
        c.last_ts_unix = Some(rec.ts_unix);
        if rec.status == "ok" {
            c.ok += 1;
            c.tokens_in += rec.tokens_in.unwrap_or(0);
            c.tokens_out += rec.tokens_out.unwrap_or(0);
            c.cost_usd += rec.cost_usd;
        } else {
            c.failed += 1;
        }
        if let Some(f) = &mut inner.file {
            let line = serde_json::to_string(&rec).unwrap_or_default();
            if writeln!(f, "{line}").is_err() {
                // don't spam; switch to memory-only
                inner.file = None;
            }
        }
    }

    /// Snapshot for `/status.json`.
    pub fn snapshot(&self) -> serde_json::Value {
        let mut inner = self.inner.lock().unwrap();
        let (by_model, by_caller) = hourly_series(&inner);
        if inner.hour_cache.as_ref().is_none_or(|c| {
            c.built_for_total != inner.total || c.built_for_hour != unix_now() / 3600
        }) {
            inner.hour_cache = Some(HourCache {
                built_for_total: inner.total,
                built_for_hour: unix_now() / 3600,
                by_model: by_model.clone(),
                by_caller: by_caller.clone(),
            });
        }
        serde_json::json!({
            "requests_total": inner.total,
            // `usage_by_model` — must not collide with the registry's
            // `models` array that /status.json also carries
            "usage_by_model": inner.stats,
            "callers": inner.callers,
            // E5 monitor: tokens per UTC hour for the last 24h
            // (in+out, successful attempts only)
            "hourly_by_model": by_model,
            "hourly_by_caller": by_caller,
        })
    }

    pub fn records(&self) -> Vec<UsageRecord> {
        // read back from file (best effort) — used by tests and `status`.
        // In-memory logs return the live ring instead of nothing.
        let path = {
            let inner = self.inner.lock().unwrap();
            if inner.path.is_none() {
                return inner.ring.iter().rev().cloned().collect();
            }
            inner.path.clone()
        };
        let Some(path) = path else { return vec![] };
        std::fs::read_to_string(path)
            .map(|raw| {
                raw.lines()
                    .filter(|l| !l.trim().is_empty())
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// $ spent since `since_unix`, filtered by pool (required), profile
    /// and caller (optional). Bills what the upstream billed: cost is
    /// counted for EVERY record with a cost — a stream that died
    /// mid-flight after emitting billable tokens still spends (review
    /// F4); failed attempts with no usage carry cost 0 and stay free.
    pub fn spend_since(
        &self,
        pool: &str,
        profile: Option<&str>,
        caller: Option<&str>,
        since_unix: u64,
    ) -> f64 {
        let inner = self.inner.lock().unwrap();
        inner
            .ring
            .iter()
            .filter(|r| r.ts_unix >= since_unix)
            .filter(|r| r.pool == pool)
            .filter(|r| profile.is_none_or(|p| r.profile == p))
            .filter(|r| caller.is_none_or(|c| r.caller == c))
            .filter(|r| r.cost_usd > 0.0)
            .map(|r| r.cost_usd)
            .sum()
    }
}

/// Load the recent window of an existing JSONL file into the ring.
fn load_ring(path: &Path) -> std::collections::VecDeque<UsageRecord> {
    let cutoff = unix_now().saturating_sub(RETENTION_SECS);
    std::fs::read_to_string(path)
        .map(|raw| {
            raw.lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str::<UsageRecord>(l).ok())
                .filter(|r| r.ts_unix >= cutoff)
                .collect()
        })
        .unwrap_or_default()
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(model: &str, status: &str, tin: Option<u64>, tout: Option<u64>) -> UsageRecord {
        UsageRecord {
            ts_unix: 1,
            pool: "quota".into(),
            profile: "default".into(),
            caller: "alice".into(),
            project: Some("arouter".into()),
            grade_requested: "A+".into(),
            grade_served: "A".into(),
            model: model.into(),
            status: status.into(),
            http_status: Some(200),
            tokens_in: tin,
            tokens_out: tout,
            cost_usd: UsageRecord::cost_for(tin, tout, 1.0, 3.0),
            latency_ms: 12,
            ttfb_ms: Some(5),
            degraded: true,
        }
    }

    #[test]
    fn hourly_series_buckets() {
        let log = UsageLog::in_memory();
        let now = unix_now();
        let hour = now / 3600;
        let rec = |model: &str, ts: u64, tin: u64, tout: u64, status: &str| UsageRecord {
            ts_unix: ts,
            pool: "ondemand".into(),
            profile: "default".into(),
            caller: "alice".into(),
            project: Some("arouter".into()),
            grade_requested: "A".into(),
            grade_served: "A".into(),
            model: model.into(),
            status: status.into(),
            http_status: Some(200),
            tokens_in: Some(tin),
            tokens_out: Some(tout),
            cost_usd: 0.0,
            latency_ms: 5,
            ttfb_ms: None,
            degraded: false,
        };
        log.record(rec("m1", now - 10, 100, 50, "ok")); // current hour → 23
        log.record(rec("m1", (hour - 1) * 3600, 10, 10, "ok")); // → 22
        log.record(rec("m1", (hour - 30) * 3600, 999, 0, "ok")); // >24h → dropped
        log.record(rec("m2", now - 5, 7, 3, "upstream_error")); // failed → dropped
        let snap = log.snapshot();
        let m1 = snap["hourly_by_model"]["m1"].as_array().unwrap();
        assert_eq!(m1.len(), 24);
        assert_eq!(m1[23], 150, "current hour bucket");
        assert_eq!(m1[22], 20, "previous hour bucket");
        assert!(snap["hourly_by_model"]["m2"].is_null());
        let c = snap["hourly_by_caller"]["alice/arouter"]
            .as_array()
            .unwrap();
        assert_eq!(c[23], 150);
    }

    #[test]
    fn aggregates_and_file_roundtrip() {
        let dir = std::env::temp_dir().join(format!("arouter-usage-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let log = UsageLog::open(dir.to_str().unwrap()).unwrap();

        log.record(rec("zai/glm-5.3", "ok", Some(1000), Some(3000)));
        log.record(rec("zai/glm-5.3", "upstream_error", None, None));
        log.record(rec("strata/qwen", "ok", Some(10), Some(10)));

        let snap = log.snapshot();
        assert_eq!(snap["requests_total"], 3);
        let glm = &snap["usage_by_model"]["zai/glm-5.3"];
        assert_eq!(glm["requests"], 2);
        assert_eq!(glm["ok"], 1);
        assert_eq!(glm["failed"], 1);
        // cost = (1000×1 + 3000×3)/1e6 = 0.01
        assert!((glm["cost_usd"].as_f64().unwrap() - 0.01).abs() < 1e-9);

        let records = log.records();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].model, "zai/glm-5.3");
        assert_eq!(records[0].caller, "alice");
        assert_eq!(records[0].project.as_deref(), Some("arouter"));
        assert_eq!(records[1].status, "upstream_error");
        assert!(records[1].tokens_in.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn per_caller_aggregation() {
        let log = UsageLog::in_memory();
        let mut r = rec("zai/glm-5.3", "ok", Some(100), Some(50));
        log.record(r.clone());
        r.caller = "ci".into();
        r.project = None;
        r.status = "upstream_error".into();
        r.tokens_in = None;
        r.tokens_out = None;
        log.record(r);

        let snap = log.snapshot();
        let alice = &snap["callers"]["alice/arouter"];
        assert_eq!(alice["requests"], 1);
        assert_eq!(alice["ok"], 1);
        assert_eq!(alice["tokens_in"], 100);
        let ci = &snap["callers"]["ci"];
        assert_eq!(ci["requests"], 1);
        assert_eq!(ci["failed"], 1);
        assert_eq!(ci["ok"], 0);
    }

    #[test]
    fn cost_formula() {
        assert!((UsageRecord::cost_for(Some(1_000_000), None, 2.0, 6.0) - 2.0).abs() < 1e-9);
        assert!((UsageRecord::cost_for(None, Some(1_000_000), 2.0, 6.0) - 6.0).abs() < 1e-9);
        assert_eq!(UsageRecord::cost_for(None, None, 2.0, 6.0), 0.0);
    }

    #[test]
    fn memory_only_never_panics() {
        let log = UsageLog::in_memory();
        log.record(rec("x/y", "ok", None, None));
        assert_eq!(log.snapshot()["requests_total"], 1);
        // in-memory logs now serve the live ring (was: always empty)
        assert_eq!(log.records().len(), 1);
    }
}
