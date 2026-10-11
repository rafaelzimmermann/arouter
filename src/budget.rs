//! On-demand budget enforcement (docs/routing.md §6): stacked caps
//! (pool / profile / caller, effective = the tightest), preflight cost
//! estimates per candidate, and spend headers.
//!
//! Windows: `daily` = UTC calendar day, `monthly` = rolling 30d,
//! `pace` = rolling 1h. JSONL keeps the audit trail; the in-memory
//! ring (usage.rs) provides the aggregates.

use crate::config::{BudgetConfig, CallerIdentity, Config};
use crate::usage::{unix_now, UsageLog};

#[derive(Debug, Clone)]
pub struct Denial {
    /// which stacked layer denied: pool | profile | caller
    pub layer: &'static str,
    /// daily | monthly | pace | per_request
    pub window: &'static str,
    pub cap: f64,
    pub spent: f64,
    pub est: f64,
}

impl Denial {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "layer": self.layer,
            "window": self.window,
            "cap_usd": self.cap,
            "spent_usd": (self.spent * 1e6).round() / 1e6,
            "est_usd": (self.est * 1e6).round() / 1e6,
        })
    }
}

fn day_start(now: u64) -> u64 {
    now - (now % 86_400)
}

fn month_start(now: u64) -> u64 {
    now.saturating_sub(30 * 86_400) // rolling 30d
}

fn hour_start(now: u64) -> u64 {
    now.saturating_sub(3_600)
}

/// Preflight a candidate against every stacked budget layer
/// (routing.md §6). `est_usd` is the request estimate for *this* model;
/// a per-request denial tells the caller to try cheaper candidates,
/// window denials are the pool/profile/caller being out of money.
pub fn preflight(
    usage: &UsageLog,
    cfg: &Config,
    profile: &str,
    caller: &CallerIdentity,
    est_usd: f64,
) -> Result<(), Denial> {
    let pool_b = cfg.pool.get("ondemand").and_then(|p| p.budget.as_ref());
    let prof_b = cfg.profiles.get(profile).and_then(|p| p.budget.as_ref());
    let caller_b = cfg
        .callers
        .iter()
        .find(|c| c.name == caller.name)
        .and_then(|c| c.budget_usd_day);

    let now = unix_now();

    // per-request cap (pool level): skip this candidate, cheaper may pass
    if let Some(b) = pool_b.filter(|b| b.per_request_usd > 0.0) {
        if est_usd > b.per_request_usd {
            return Err(Denial {
                layer: "pool",
                window: "per_request",
                cap: b.per_request_usd,
                spent: 0.0,
                est: est_usd,
            });
        }
    }

    // pool layer
    if let Some(b) = pool_b {
        check(usage, b, None, None, now, est_usd, "pool")?;
    }
    // profile layer (spend within this profile)
    if let Some(b) = prof_b {
        check(usage, b, Some(profile), None, now, est_usd, "profile")?;
    }
    // caller layer (spend by this caller, any profile)
    if let Some(day) = caller_b.filter(|d| *d > 0.0) {
        let spent = usage.spend_since("ondemand", None, Some(&caller.name), day_start(now));
        if spent + est_usd > day {
            return Err(Denial {
                layer: "caller",
                window: "daily",
                cap: day,
                spent,
                est: est_usd,
            });
        }
    }
    Ok(())
}

fn check(
    usage: &UsageLog,
    b: &BudgetConfig,
    profile: Option<&str>,
    _caller: Option<&str>,
    now: u64,
    est: f64,
    layer: &'static str,
) -> Result<(), Denial> {
    if b.daily_usd > 0.0 {
        let spent = usage.spend_since("ondemand", profile, None, day_start(now));
        if spent + est > b.daily_usd {
            return Err(Denial {
                layer,
                window: "daily",
                cap: b.daily_usd,
                spent,
                est,
            });
        }
    }
    if b.monthly_usd > 0.0 {
        let spent = usage.spend_since("ondemand", profile, None, month_start(now));
        if spent + est > b.monthly_usd {
            return Err(Denial {
                layer,
                window: "monthly",
                cap: b.monthly_usd,
                spent,
                est,
            });
        }
    }
    if b.pace_hourly_usd > 0.0 {
        let spent = usage.spend_since("ondemand", profile, None, hour_start(now));
        if spent + est > b.pace_hourly_usd {
            return Err(Denial {
                layer,
                window: "pace",
                cap: b.pace_hourly_usd,
                spent,
                est,
            });
        }
    }
    Ok(())
}

/// `X-Arouter-Budget-Remaining-Today`: tightest remaining across the
/// stacked layers, or None when no daily cap is configured anywhere.
pub fn remaining_today(
    usage: &UsageLog,
    cfg: &Config,
    profile: &str,
    caller: &CallerIdentity,
) -> Option<f64> {
    let now = unix_now();
    let since = day_start(now);
    let mut caps: Vec<f64> = vec![];
    if let Some(b) = cfg.pool.get("ondemand").and_then(|p| p.budget.as_ref()) {
        if b.daily_usd > 0.0 {
            let spent = usage.spend_since("ondemand", None, None, since);
            caps.push(b.daily_usd - spent);
        }
    }
    if let Some(b) = cfg.profiles.get(profile).and_then(|p| p.budget.as_ref()) {
        if b.daily_usd > 0.0 {
            let spent = usage.spend_since("ondemand", Some(profile), None, since);
            caps.push(b.daily_usd - spent);
        }
    }
    if let Some(day) = cfg
        .callers
        .iter()
        .find(|c| c.name == caller.name)
        .and_then(|c| c.budget_usd_day)
        .filter(|d| *d > 0.0)
    {
        let spent = usage.spend_since("ondemand", None, Some(&caller.name), since);
        caps.push(day - spent);
    }
    caps.iter().cloned().fold(None, |acc: Option<f64>, x| {
        Some(match acc {
            Some(a) => a.min(x),
            None => x,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CallerConfig;
    use crate::usage::UsageRecord;

    fn rec(cost: f64, ts: u64, caller: &str, profile: &str) -> UsageRecord {
        UsageRecord {
            ts_unix: ts,
            pool: "ondemand".into(),
            profile: profile.into(),
            caller: caller.into(),
            project: None,
            grade_requested: "A".into(),
            grade_served: "A".into(),
            model: "or/x".into(),
            status: "ok".into(),
            http_status: Some(200),
            tokens_in: None,
            tokens_out: None,
            cost_usd: cost,
            latency_ms: 1,
            ttfb_ms: None,
            degraded: false,
        }
    }

    fn cfg_with(budget: BudgetConfig, caller_day: Option<f64>) -> Config {
        let mut c = Config::default();
        c.pool.insert(
            "ondemand".into(),
            crate::config::PoolConfig {
                preference: "cheapest".into(),
                budget: Some(budget),
            },
        );
        if let Some(d) = caller_day {
            c.callers.push(CallerConfig {
                name: "alice".into(),
                project: None,
                key: None,
                budget_usd_day: Some(d),
            });
        }
        c
    }

    fn alice() -> CallerIdentity {
        CallerIdentity {
            name: "alice".into(),
            project: None,
        }
    }

    #[test]
    fn no_budget_configured_allows() {
        let usage = UsageLog::in_memory();
        let cfg = Config::default();
        assert!(preflight(&usage, &cfg, "default", &alice(), 5.0).is_ok());
        assert_eq!(remaining_today(&usage, &cfg, "default", &alice()), None);
    }

    #[test]
    fn daily_cap_denies_with_details() {
        let usage = UsageLog::in_memory();
        let now = unix_now();
        usage.record(rec(0.40, now - 60, "bob", "default"));
        let cfg = cfg_with(
            BudgetConfig {
                daily_usd: 0.50,
                ..Default::default()
            },
            None,
        );
        let d = preflight(&usage, &cfg, "default", &alice(), 0.20).unwrap_err();
        assert_eq!((d.layer, d.window), ("pool", "daily"));
        assert_eq!(d.cap, 0.50);
        assert!((d.spent - 0.40).abs() < 1e-9);
        assert_eq!(d.est, 0.20);
        // old spend outside the day doesn't count
        usage.record(rec(10.0, now - 86_400 * 2, "bob", "default"));
        let d2 = preflight(&usage, &cfg, "default", &alice(), 0.05);
        assert!(d2.is_ok(), "yesterday's spend must not count");
    }

    #[test]
    fn per_request_cap_denies_but_cheaper_passes() {
        let usage = UsageLog::in_memory();
        let cfg = cfg_with(
            BudgetConfig {
                per_request_usd: 0.10,
                ..Default::default()
            },
            None,
        );
        assert!(preflight(&usage, &cfg, "default", &alice(), 0.20).is_err());
        assert!(preflight(&usage, &cfg, "default", &alice(), 0.05).is_ok());
    }

    #[test]
    fn caller_budget_stacks_below_pool() {
        let usage = UsageLog::in_memory();
        let now = unix_now();
        // alice already burned 0.08 of her 0.10 today
        usage.record(rec(0.08, now - 60, "alice", "default"));
        let cfg = cfg_with(
            BudgetConfig {
                daily_usd: 5.0,
                ..Default::default()
            },
            Some(0.10),
        );
        let d = preflight(&usage, &cfg, "default", &alice(), 0.05).unwrap_err();
        assert_eq!((d.layer, d.window), ("caller", "daily"));
        // remaining = tightest of (5.0 - 0.08 pool, 0.10 - 0.08 caller)
        let rem = remaining_today(&usage, &cfg, "default", &alice()).unwrap();
        assert!((rem - 0.02).abs() < 1e-9, "remaining {rem}");
        // bob isn't caller-budgeted: pool cap applies
        let bob = CallerIdentity {
            name: "bob".into(),
            project: None,
        };
        assert!(preflight(&usage, &cfg, "default", &bob, 0.05).is_ok());
    }

    #[test]
    fn hourly_pace_and_monthly() {
        let usage = UsageLog::in_memory();
        let now = unix_now();
        usage.record(rec(0.90, now - 600, "bob", "default")); // 10min ago
        let cfg = cfg_with(
            BudgetConfig {
                pace_hourly_usd: 1.0,
                ..Default::default()
            },
            None,
        );
        let d = preflight(&usage, &cfg, "default", &alice(), 0.20).unwrap_err();
        assert_eq!(d.window, "pace");

        let usage2 = UsageLog::in_memory();
        usage2.record(rec(4.0, now - 20 * 86_400, "bob", "default")); // 20d ago
        let cfg2 = cfg_with(
            BudgetConfig {
                monthly_usd: 4.0,
                ..Default::default()
            },
            None,
        );
        let d2 = preflight(&usage2, &cfg2, "default", &alice(), 0.10).unwrap_err();
        assert_eq!(d2.window, "monthly");
    }

    #[test]
    fn failed_attempts_without_usage_do_not_spend() {
        let usage = UsageLog::in_memory();
        let now = unix_now();
        // an upstream 500 bills nothing: no tokens, no cost — free
        let mut r = rec(0.0, now - 60, "bob", "default");
        r.status = "upstream_error".into();
        r.http_status = Some(500);
        usage.record(r);
        let cfg = cfg_with(
            BudgetConfig {
                daily_usd: 0.10,
                ..Default::default()
            },
            None,
        );
        assert!(preflight(&usage, &cfg, "default", &alice(), 0.05).is_ok());
    }

    #[test]
    fn billed_usage_spends_even_when_the_stream_died() {
        // review F4: a stream that broke mid-flight AFTER emitting
        // billable tokens still spends — the upstream will bill them
        let usage = UsageLog::in_memory();
        let now = unix_now();
        let mut r = rec(0.09, now - 60, "bob", "default");
        r.status = "aborted".into();
        r.tokens_out = Some(1_000); // scanned before the stream died
        usage.record(r);
        let cfg = cfg_with(
            BudgetConfig {
                daily_usd: 0.10,
                ..Default::default()
            },
            None,
        );
        let d = preflight(&usage, &cfg, "default", &alice(), 0.05).unwrap_err();
        assert_eq!(d.window, "daily");
    }
}
