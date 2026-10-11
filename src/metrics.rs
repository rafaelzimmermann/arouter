//! Prometheus metrics (text exposition format 0.0.4), hand-rolled to keep
//! the dependency tree small. Every routed request/attempt is counted with
//! caller/project labels so multiple developers can share one arouter and
//! track usage per user and project. See docs/architecture.md §Metrics.

use crate::usage::UsageRecord;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

// ─────────────────────────── families ───────────────────────────

pub const N_REQUESTS_TOTAL: &str = "arouter_requests_total";
pub const N_TOKENS_TOTAL: &str = "arouter_tokens_total";
pub const N_COST_TOTAL: &str = "arouter_cost_usd_total";
pub const N_DEGRADED_TOTAL: &str = "arouter_requests_degraded_total";
pub const N_IN_FLIGHT: &str = "arouter_requests_in_flight";
pub const N_UPTIME: &str = "arouter_uptime_seconds";
pub const N_BUILD_INFO: &str = "arouter_build_info";
pub const N_MODELS: &str = "arouter_models";
pub const N_LATENCY: &str = "arouter_request_duration_seconds";
pub const N_TTFB: &str = "arouter_stream_ttfb_seconds";

const H_REQUESTS_TOTAL: &str = "Routed requests and upstream attempts by outcome.";
const H_TOKENS_TOTAL: &str = "Tokens counted from upstream usage, by direction.";
const H_COST_TOTAL: &str = "Estimated spend in USD (priced models).";
const H_DEGRADED_TOTAL: &str = "Requests served at a lower grade than requested.";
const H_IN_FLIGHT: &str = "Requests currently being routed.";
const H_LATENCY: &str = "End-to-end request latency (includes failover attempts).";
const H_TTFB: &str = "Stream time-to-first-byte.";

// Help strings for gauge families set from the proxy layer.
pub const H_BUILD_INFO: &str = "Build information.";
pub const H_UPTIME: &str = "Seconds since daemon start.";
pub const H_MODELS: &str = "Models visible per profile/pool/grade.";
pub const H_QUOTA_WINDOW: &str = "Quota window utilization percent from aitop snapshots.";
pub const H_QUOTA_STALE: &str =
    "1 when the provider's quota numbers are the last good ones, not live.";
pub const H_QUOTA_DRAIN_RESET: &str =
    "Seconds until the draining window resets (use-it-or-lose-it); -1 when no window is draining.";

pub const N_QUOTA_WINDOW: &str = "arouter_quota_window_pct";
pub const N_QUOTA_STALE: &str = "arouter_quota_stale";
pub const N_QUOTA_DRAIN_RESET: &str = "arouter_quota_drain_reset_secs";

/// Default buckets (seconds) for request latency.
pub const LATENCY_BUCKETS: &[f64] = &[0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0];
/// Default buckets (seconds) for streaming TTFB.
pub const TTFB_BUCKETS: &[f64] = &[0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0];

// ─────────────────────────── storage ───────────────────────────

#[derive(Default)]
struct CounterFamily {
    help: &'static str,
    series: BTreeMap<String, f64>,
}

#[derive(Default)]
struct GaugeFamily {
    help: &'static str,
    series: BTreeMap<String, f64>,
}

#[derive(Default)]
struct HistFamily {
    help: &'static str,
    buckets: &'static [f64],
    series: BTreeMap<String, HistSeries>,
}

#[derive(Default, Clone)]
struct HistSeries {
    /// Cumulative counts per bucket (len == buckets.len()).
    counts: Vec<u64>,
    count: u64,
    sum: f64,
}

#[derive(Default)]
struct Inner {
    counters: BTreeMap<&'static str, CounterFamily>,
    gauges: BTreeMap<&'static str, GaugeFamily>,
    hists: BTreeMap<&'static str, HistFamily>,
}

/// Thread-safe metrics registry. Shared as `Arc<Metrics>` across all
/// concurrent requests; critical sections are tiny (map update + f64 add),
/// never held across `.await` points.
#[derive(Default)]
pub struct Metrics {
    inner: Mutex<Inner>,
}

/// Render `caller="x",project="y"` label strings, skipping empty values and
/// sorting by label name for stable output.
fn label_str(pairs: &[(&str, String)]) -> String {
    let mut kept: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(k, v)| (*k, v.as_str()))
        .filter(|(_, v)| !v.is_empty())
        .collect();
    kept.sort_unstable();
    if kept.is_empty() {
        return String::new();
    }
    kept.iter()
        .map(|(k, v)| format!("{k}=\"{}\"", escape(v)))
        .collect::<Vec<_>>()
        .join(",")
}

/// Escape a label value per the Prometheus text format.
fn escape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

/// Format a float the way Prometheus likes (never scientific notation).
fn fmt_f64(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn counter_inc(
        &self,
        name: &'static str,
        help: &'static str,
        labels: &[(&str, String)],
        by: f64,
    ) {
        let key = label_str(labels);
        let mut inner = self.inner.lock().unwrap();
        let fam = inner.counters.entry(name).or_insert_with(|| CounterFamily {
            help,
            series: BTreeMap::new(),
        });
        *fam.series.entry(key).or_insert(0.0) += by;
    }

    pub fn gauge_set(
        &self,
        name: &'static str,
        help: &'static str,
        labels: &[(&str, String)],
        value: f64,
    ) {
        let key = label_str(labels);
        let mut inner = self.inner.lock().unwrap();
        let fam = inner.gauges.entry(name).or_insert_with(|| GaugeFamily {
            help,
            series: BTreeMap::new(),
        });
        fam.series.insert(key, value);
    }

    pub fn gauge_add(
        &self,
        name: &'static str,
        help: &'static str,
        labels: &[(&str, String)],
        by: f64,
    ) {
        let key = label_str(labels);
        let mut inner = self.inner.lock().unwrap();
        let fam = inner.gauges.entry(name).or_insert_with(|| GaugeFamily {
            help,
            series: BTreeMap::new(),
        });
        *fam.series.entry(key).or_insert(0.0) += by;
    }

    pub fn histogram_observe(
        &self,
        name: &'static str,
        help: &'static str,
        buckets: &'static [f64],
        labels: &[(&str, String)],
        value: f64,
    ) {
        let key = label_str(labels);
        let mut inner = self.inner.lock().unwrap();
        let fam = inner.hists.entry(name).or_insert_with(|| HistFamily {
            help,
            buckets,
            series: BTreeMap::new(),
        });
        let s = fam.series.entry(key).or_default();
        if s.counts.len() != buckets.len() {
            s.counts.resize(buckets.len(), 0);
        }
        for (i, b) in buckets.iter().enumerate() {
            if value <= *b {
                s.counts[i] += 1;
            }
        }
        s.count += 1;
        s.sum += value;
    }

    /// RAII in-flight counter: increment on create, decrement on drop.
    pub fn in_flight(self: &Arc<Self>) -> InFlightGuard {
        self.gauge_add(N_IN_FLIGHT, H_IN_FLIGHT, &[], 1.0);
        InFlightGuard(self.clone())
    }

    // ───────────────── record helpers (one per UsageRecord) ─────────────────

    /// Feed a completed request/attempt record into all relevant families.
    pub fn record(&self, rec: &UsageRecord) {
        let caller = rec.caller.as_str();
        let project = rec.project.clone().unwrap_or_default();
        self.counter_inc(
            N_REQUESTS_TOTAL,
            H_REQUESTS_TOTAL,
            &[
                ("pool", rec.pool.clone()),
                ("profile", rec.profile.clone()),
                ("caller", caller.to_string()),
                ("project", project.clone()),
                ("grade_requested", rec.grade_requested.clone()),
                ("grade_served", rec.grade_served.clone()),
                ("model", rec.model.clone()),
                ("status", rec.status.clone()),
            ],
            1.0,
        );
        let tok_labels = |direction: &str| {
            vec![
                ("direction", direction.to_string()),
                ("pool", rec.pool.clone()),
                ("caller", caller.to_string()),
                ("project", project.clone()),
                ("model", rec.model.clone()),
            ]
        };
        if let Some(ti) = rec.tokens_in {
            self.counter_inc(N_TOKENS_TOTAL, H_TOKENS_TOTAL, &tok_labels("in"), ti as f64);
        }
        if let Some(to) = rec.tokens_out {
            self.counter_inc(
                N_TOKENS_TOTAL,
                H_TOKENS_TOTAL,
                &tok_labels("out"),
                to as f64,
            );
        }
        if rec.cost_usd > 0.0 {
            self.counter_inc(
                N_COST_TOTAL,
                H_COST_TOTAL,
                &[
                    ("pool", rec.pool.clone()),
                    ("profile", rec.profile.clone()),
                    ("caller", caller.to_string()),
                    ("project", project),
                    ("model", rec.model.clone()),
                ],
                rec.cost_usd,
            );
        }
        if rec.degraded {
            self.counter_inc(
                N_DEGRADED_TOTAL,
                H_DEGRADED_TOTAL,
                &[
                    ("pool", rec.pool.clone()),
                    ("profile", rec.profile.clone()),
                    ("caller", caller.to_string()),
                ],
                1.0,
            );
        }
        if rec.status == "ok" || rec.status == "aborted" {
            self.histogram_observe(
                N_LATENCY,
                H_LATENCY,
                LATENCY_BUCKETS,
                &[("pool", rec.pool.clone()), ("caller", caller.to_string())],
                rec.latency_ms as f64 / 1000.0,
            );
            if let Some(ttfb) = rec.ttfb_ms {
                self.histogram_observe(
                    N_TTFB,
                    H_TTFB,
                    TTFB_BUCKETS,
                    &[("pool", rec.pool.clone()), ("caller", caller.to_string())],
                    ttfb as f64 / 1000.0,
                );
            }
        }
    }

    /// A grade request found no candidates at all (429 arouter_exhausted).
    pub fn record_exhausted(
        &self,
        pool: &str,
        profile: &str,
        caller: &str,
        project: Option<&str>,
        requested: &str,
    ) {
        self.counter_inc(
            N_REQUESTS_TOTAL,
            H_REQUESTS_TOTAL,
            &[
                ("pool", pool.to_string()),
                ("profile", profile.to_string()),
                ("caller", caller.to_string()),
                ("project", project.unwrap_or("").to_string()),
                ("grade_requested", requested.to_string()),
                ("status", "exhausted".to_string()),
            ],
            1.0,
        );
    }

    // ─────────────────────────── exposition ───────────────────────────

    /// Render the registry in Prometheus text format 0.0.4.
    pub fn render(&self) -> String {
        let inner = self.inner.lock().unwrap();
        let mut out = String::new();
        for (name, fam) in &inner.counters {
            emit_header(&mut out, name, fam.help, "counter");
            for (k, v) in &fam.series {
                emit_sample(&mut out, name, k, &fmt_f64(*v));
            }
        }
        for (name, fam) in &inner.gauges {
            emit_header(&mut out, name, fam.help, "gauge");
            for (k, v) in &fam.series {
                emit_sample(&mut out, name, k, &fmt_f64(*v));
            }
        }
        for (name, fam) in &inner.hists {
            emit_header(&mut out, name, fam.help, "histogram");
            for (k, s) in &fam.series {
                for (i, b) in fam.buckets.iter().enumerate() {
                    let lk = with_le(k, &fmt_f64(*b));
                    emit_sample(
                        &mut out,
                        &format!("{name}_bucket"),
                        &lk,
                        &s.counts[i].to_string(),
                    );
                }
                emit_sample(
                    &mut out,
                    &format!("{name}_bucket"),
                    &with_le(k, "+Inf"),
                    &s.count.to_string(),
                );
                emit_sample(&mut out, &format!("{name}_sum"), k, &fmt_f64(s.sum));
                emit_sample(&mut out, &format!("{name}_count"), k, &s.count.to_string());
            }
        }
        out
    }
}

fn emit_header(out: &mut String, name: &str, help: &str, kind: &str) {
    out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n"));
}

fn emit_sample(out: &mut String, name: &str, key: &str, value: &str) {
    if key.is_empty() {
        out.push_str(&format!("{name} {value}\n"));
    } else {
        out.push_str(&format!("{name}{{{key}}} {value}\n"));
    }
}

fn with_le(key: &str, le: &str) -> String {
    // insert le alongside existing labels, keeping them sorted by name
    let mut pairs: Vec<String> = key
        .split(',')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    pairs.push(format!("le=\"{le}\""));
    pairs.sort_by(|a, b| a.split('=').next().cmp(&b.split('=').next()));
    pairs.join(",")
}

/// Decrements `arouter_requests_in_flight` when dropped.
pub struct InFlightGuard(Arc<Metrics>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.gauge_add(N_IN_FLIGHT, H_IN_FLIGHT, &[], -1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(status: &str) -> UsageRecord {
        UsageRecord {
            ts_unix: 1,
            pool: "ondemand".into(),
            profile: "default".into(),
            caller: "alice".into(),
            project: Some("arouter".into()),
            grade_requested: "A+".into(),
            grade_served: "A".into(),
            model: "or/gpt-5.2".into(),
            status: status.into(),
            http_status: Some(200),
            tokens_in: Some(1000),
            tokens_out: Some(3000),
            cost_usd: 0.013,
            latency_ms: 2500,
            ttfb_ms: Some(300),
            degraded: true,
        }
    }

    #[test]
    fn labels_skip_empty_and_sort() {
        let l = label_str(&[
            ("model", "m".into()),
            ("project", String::new()),
            ("caller", "x".into()),
        ]);
        assert_eq!(l, "caller=\"x\",model=\"m\"");
        assert_eq!(label_str(&[]), "");
    }

    #[test]
    fn label_escaping() {
        let l = label_str(&[("caller", "a\"b\\c\nd".into())]);
        assert_eq!(l, "caller=\"a\\\"b\\\\c\\nd\"");
    }

    #[test]
    fn counter_render_format() {
        let m = Metrics::new();
        m.counter_inc(
            N_REQUESTS_TOTAL,
            H_REQUESTS_TOTAL,
            &[("caller", "alice".into()), ("status", "ok".into())],
            1.0,
        );
        m.counter_inc(
            N_REQUESTS_TOTAL,
            H_REQUESTS_TOTAL,
            &[("caller", "alice".into()), ("status", "ok".into())],
            1.0,
        );
        let out = m.render();
        assert!(out.contains("# HELP arouter_requests_total Routed requests"));
        assert!(out.contains("# TYPE arouter_requests_total counter"));
        assert!(out.contains("arouter_requests_total{caller=\"alice\",status=\"ok\"} 2\n"));
    }

    #[test]
    fn histogram_render_format() {
        let m = Metrics::new();
        m.histogram_observe(
            N_TTFB,
            H_TTFB,
            TTFB_BUCKETS,
            &[("pool", "quota".into())],
            0.3,
        );
        let out = m.render();
        assert!(out.contains("# TYPE arouter_stream_ttfb_seconds histogram\n"));
        // 0.3 falls in the 0.5 bucket (and everything above)
        assert!(out.contains("arouter_stream_ttfb_seconds_bucket{le=\"0.5\",pool=\"quota\"} 1\n"));
        assert!(out.contains("arouter_stream_ttfb_seconds_bucket{le=\"+Inf\",pool=\"quota\"} 1\n"));
        assert!(out.contains("arouter_stream_ttfb_seconds_sum{pool=\"quota\"} 0.3\n"));
        assert!(out.contains("arouter_stream_ttfb_seconds_count{pool=\"quota\"} 1\n"));
        // smaller buckets stay 0
        assert!(out.contains("arouter_stream_ttfb_seconds_bucket{le=\"0.25\",pool=\"quota\"} 0\n"));
    }

    #[test]
    fn record_maps_fields_to_families() {
        let m = Metrics::new();
        m.record(&rec("ok"));
        let out = m.render();
        assert!(out.contains(
            "arouter_requests_total{caller=\"alice\",grade_requested=\"A+\",grade_served=\"A\",model=\"or/gpt-5.2\",pool=\"ondemand\",profile=\"default\",project=\"arouter\",status=\"ok\"} 1"
        ));
        assert!(out.contains("arouter_tokens_total{caller=\"alice\",direction=\"in\",model=\"or/gpt-5.2\",pool=\"ondemand\",project=\"arouter\"} 1000\n"));
        assert!(out.contains("arouter_tokens_total{caller=\"alice\",direction=\"out\",model=\"or/gpt-5.2\",pool=\"ondemand\",project=\"arouter\"} 3000\n"));
        assert!(out.contains("arouter_cost_usd_total{caller=\"alice\",model=\"or/gpt-5.2\",pool=\"ondemand\",profile=\"default\",project=\"arouter\"} 0.013\n"));
        assert!(out.contains("arouter_requests_degraded_total{caller=\"alice\",pool=\"ondemand\",profile=\"default\"} 1\n"));
        assert!(out.contains(
            "arouter_request_duration_seconds_sum{caller=\"alice\",pool=\"ondemand\"} 2.5\n"
        ));
        assert!(out
            .contains("arouter_stream_ttfb_seconds_sum{caller=\"alice\",pool=\"ondemand\"} 0.3\n"));
    }

    #[test]
    fn exhausted_has_no_model_label() {
        let m = Metrics::new();
        m.record_exhausted("quota", "default", "ci", None, "A+");
        let out = m.render();
        assert!(out.contains(
            "arouter_requests_total{caller=\"ci\",grade_requested=\"A+\",pool=\"quota\",profile=\"default\",status=\"exhausted\"} 1\n"
        ));
        assert!(!out.contains("status=\"exhausted\",model"));
    }

    #[test]
    fn in_flight_guard_increments_and_drops() {
        let m = Arc::new(Metrics::new());
        assert!(!m.render().contains(N_IN_FLIGHT));
        {
            let _g = m.in_flight();
            let _g2 = m.in_flight();
            assert!(m.render().contains("arouter_requests_in_flight 2\n"));
        }
        assert!(m.render().contains("arouter_requests_in_flight 0\n"));
    }

    #[test]
    fn families_render_sorted() {
        let m = Metrics::new();
        m.counter_inc(
            N_DEGRADED_TOTAL,
            H_DEGRADED_TOTAL,
            &[("caller", "a".into())],
            1.0,
        );
        m.counter_inc(N_REQUESTS_TOTAL, H_REQUESTS_TOTAL, &[], 1.0);
        let out = m.render();
        // "arouter_requests_degraded_total" sorts before "arouter_requests_total"
        let i_deg = out.find("arouter_requests_degraded_total{").unwrap();
        let i_req = out.find("arouter_requests_total ").unwrap();
        assert!(i_deg < i_req, "families must be sorted by name");
    }

    #[test]
    fn float_formatting_avoids_scientific() {
        let m = Metrics::new();
        m.counter_inc(N_COST_TOTAL, H_COST_TOTAL, &[("m", "x".into())], 1e-7);
        assert!(m.render().contains(" 0.0000001\n"));
    }
}
