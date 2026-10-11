//! Native quota intake (E3b): the router asks its upstreams directly —
//! no external quota tool in the loop.
//!
//! Sources, resolved per provider prefix:
//! - `native:openrouter` — documented `GET /api/v1/auth/key` (default for
//!   openrouter.ai upstreams)
//! - `native:zai` — `GET api.z.ai/api/monitor/usage/quota/limit`
//!   (undocumented endpoint: opt-in, see PLAN E3b etiquette)
//! - `headers` — harvest `x-ratelimit-*` from responses arouter already
//!   proxies, plus a synthetic exhausted window on 429 `Retry-After`
//!   (default for everything else)
//! - `manual` — windows pinned in config (web overlay later)
//! - `off` — no quota knowledge; routing is health-only for this provider
//! - `command:<cmd>` — legacy escape hatch (aitop-shaped JSON)
//!
//! Everything normalizes into the existing `QuotaWindow` shape, so the
//! scoring path (drain, pace, scarcity, headroom) is unchanged.

use crate::quota::{unix_now, QuotaWatcher, QuotaWindow};
use reqwest::header::HeaderMap;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

pub const DEFAULT_POLL_SECS: u64 = 900;

/// Sent with every quota poll — be identifiable, don't look like a
/// browser scraping (review checkpoint: UA policy).
pub const USER_AGENT: &str = concat!(
    "arouter/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/rafaelzimmermann/arouter)"
);

#[derive(Debug, Clone, PartialEq)]
pub enum SourceKind {
    Headers,
    Off,
    Native(NativeKind),
    Manual(Vec<crate::config::QuotaManualWindow>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeKind {
    OpenRouter,
    Zai,
}

/// Parse a source spec from config (`"headers"`, `"off"`,
/// `"native:zai"`, `"manual"`). Command specs are handled by the legacy
/// watcher and never reach here.
pub fn parse_spec(spec: &str) -> Option<SourceKind> {
    match spec.trim() {
        "headers" => Some(SourceKind::Headers),
        "off" => Some(SourceKind::Off),
        "native:openrouter" => Some(SourceKind::Native(NativeKind::OpenRouter)),
        "native:zai" => Some(SourceKind::Native(NativeKind::Zai)),
        "manual" => Some(SourceKind::Manual(vec![])), // windows pinned separately
        _ => None,
    }
}

/// Group `[[quota.manual]]` entries by provider — duplicates are kept
/// as separate windows of the same provider (a naive map would keep
/// only the last; review checkpoint).
/// Pure scan (review F6): per-prefix quota source resolution —
/// (harvest-headers? map, source-name map) for the runtime snapshot.
pub fn scan_sources(
    cfg: &crate::config::Config,
    registry: &crate::registry::Registry,
) -> (
    std::collections::BTreeMap<String, bool>,
    std::collections::BTreeMap<String, String>,
) {
    let manual = manual_map(&cfg.quota.manual);
    let mut harvest = std::collections::BTreeMap::new();
    let mut sources = std::collections::BTreeMap::new();
    let mut seen = std::collections::BTreeSet::new();
    for m in &registry.models {
        let prefix = m.id.split('/').next().unwrap_or(&m.id).to_string();
        if !seen.insert(prefix.clone()) {
            continue;
        }
        let explicit = cfg.quota.sources.get(&prefix).map(|s| s.as_str());
        if explicit.is_some_and(|s| s == "command" || s.starts_with("command:")) {
            continue; // legacy path, not a headers source
        }
        let kind = resolve(
            explicit,
            &m.base_url,
            manual.get(&prefix).map(|v| v.as_slice()),
        );
        harvest.insert(
            prefix.clone(),
            matches!(kind, crate::quotaeng::SourceKind::Headers),
        );
        sources.insert(prefix, describe_kind(&kind).to_string());
    }
    (harvest, sources)
}

pub fn manual_map(
    specs: &[crate::config::QuotaManualWindow],
) -> BTreeMap<String, Vec<crate::config::QuotaManualWindow>> {
    let mut out: BTreeMap<String, Vec<crate::config::QuotaManualWindow>> = BTreeMap::new();
    for m in specs {
        out.entry(m.provider.clone()).or_default().push(m.clone());
    }
    out
}

/// Realize manual specs into windows at apply time, so relative resets
/// (`resets_in_secs`) stay in the future even after weeks of uptime.
pub fn manual_windows(specs: &[crate::config::QuotaManualWindow], now: u64) -> Vec<QuotaWindow> {
    specs
        .iter()
        .map(|m| QuotaWindow {
            label: m.label.clone(),
            pct: m.pct.clamp(0.0, 100.0),
            reset_unix: m.resets_in_secs.filter(|s| *s > 0).map(|s| now + s),
            pace: None,
        })
        .collect()
}

/// Auto-detect for providers without an explicit source: the one
/// documented endpoint we trust by default; everything else learns from
/// the responses it already proxies. Hosts are compared parsed, not by
/// substring (`notopenrouter.example` must stay `Headers`).
pub fn auto_kind(base_url: &str) -> SourceKind {
    let is_or = reqwest::Url::parse(base_url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .is_some_and(|h| h == "openrouter.ai" || h.ends_with(".openrouter.ai"));
    if is_or {
        SourceKind::Native(NativeKind::OpenRouter)
    } else {
        SourceKind::Headers
    }
}

/// Resolve the effective source for a provider prefix. Explicit config
/// wins; `off` beats everything except an explicit spec (it must be
/// possible to silence an auto-detected native source).
pub fn resolve(
    explicit: Option<&str>,
    base_url: &str,
    manual: Option<&[crate::config::QuotaManualWindow]>,
) -> SourceKind {
    if let Some(spec) = explicit {
        if let Some(kind) = parse_spec(spec) {
            if matches!(kind, SourceKind::Manual(_)) {
                return manual
                    .map(|w| SourceKind::Manual(w.to_vec()))
                    .unwrap_or(kind);
            }
            return kind;
        }
    }
    match manual {
        Some(w) if !w.is_empty() => SourceKind::Manual(w.to_vec()),
        _ => auto_kind(base_url),
    }
}

// ───────────────────────── native fetchers ─────────────────────────

/// Typed fetch failure: the engine backs off exponentially on 429 and
/// keeps the last snapshot otherwise.
#[derive(Debug, Clone, PartialEq)]
pub enum FetchErr {
    RateLimited,
    Http(u16, String),
    Other(String),
}

impl std::fmt::Display for FetchErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchErr::RateLimited => write!(f, "http 429 (rate limited)"),
            FetchErr::Http(s, e) => write!(f, "http {s}: {e}"),
            FetchErr::Other(e) => write!(f, "{e}"),
        }
    }
}

impl FetchErr {
    fn from_reqwest(e: reqwest::Error) -> Self {
        match e.status() {
            Some(s) if s.as_u16() == 429 => FetchErr::RateLimited,
            Some(s) => FetchErr::Http(s.as_u16(), e.to_string()),
            None => FetchErr::Other(e.to_string()),
        }
    }
}

/// One native poll: fetch + normalize. Failures are typed and handled
/// by the engine — they never change routing state (the last snapshot
/// stays).
pub async fn fetch_native(
    kind: NativeKind,
    base_url: &str,
    api_key: Option<&str>,
    http: &reqwest::Client,
) -> Result<Vec<QuotaWindow>, FetchErr> {
    let key = api_key.ok_or_else(|| FetchErr::Other("no api key configured".into()))?;
    match kind {
        NativeKind::OpenRouter => fetch_openrouter(base_url, key, http).await,
        NativeKind::Zai => fetch_zai(base_url, key, http).await,
    }
}

/// Documented: `GET {base}/auth/key` → credits limit/remaining/reset.
async fn fetch_openrouter(
    base_url: &str,
    key: &str,
    http: &reqwest::Client,
) -> Result<Vec<QuotaWindow>, FetchErr> {
    let url = format!("{}/auth/key", base_url.trim_end_matches('/'));
    let v: serde_json::Value = http
        .get(&url)
        .bearer_auth(key)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(FetchErr::from_reqwest)?
        .error_for_status()
        .map_err(FetchErr::from_reqwest)?
        .json()
        .await
        .map_err(|e| FetchErr::Other(format!("parse: {e}")))?;
    parse_openrouter(&v).ok_or_else(|| FetchErr::Other("no usable windows".into()))
}

/// Pure parse (unit-tested): credits window + optional guardrail budget.
pub fn parse_openrouter(v: &serde_json::Value) -> Option<Vec<QuotaWindow>> {
    let d = v.get("data")?;
    let mut windows = vec![];
    let f = |k: &str| d.get(k).and_then(|x| x.as_f64());
    if let (Some(limit), Some(remaining)) = (f("limit"), f("limit_remaining")) {
        if limit > 0.0 {
            let pct = ((limit - remaining) / limit * 100.0).clamp(0.0, 100.0);
            let reset = d
                .get("limit_reset")
                .and_then(|x| x.as_str())
                .and_then(crate::quota::parse_rfc3339_unix);
            windows.push(QuotaWindow {
                label: "credits".into(),
                pct,
                reset_unix: reset,
                pace: None,
            });
        }
    }
    // Optional guardrail budget: a second, tighter window if present.
    if let Some(b) = d.get("effective_budget") {
        if let (Some(lim), Some(spend)) = (
            b.get("limit_usd").and_then(|x| x.as_f64()),
            b.get("spend_usd").and_then(|x| x.as_f64()),
        ) {
            if lim > 0.0 {
                let reset = b
                    .get("resets_at")
                    .and_then(|x| x.as_str())
                    .and_then(crate::quota::parse_rfc3339_unix);
                let scope = b.get("scope").and_then(|x| x.as_str()).unwrap_or("budget");
                windows.push(QuotaWindow {
                    label: format!("budget:{scope}"),
                    pct: (spend / lim * 100.0).clamp(0.0, 100.0),
                    reset_unix: reset,
                    pace: None,
                });
            }
        }
    }
    (!windows.is_empty()).then_some(windows)
}

/// Undocumented (opt-in): `GET {host}/api/monitor/usage/quota/limit` →
/// per-window used-% and epoch-millis reset. The host is taken from the
/// model's own base URL — the credential never leaves its provider.
async fn fetch_zai(
    base_url: &str,
    key: &str,
    http: &reqwest::Client,
) -> Result<Vec<QuotaWindow>, FetchErr> {
    let base =
        reqwest::Url::parse(base_url).map_err(|e| FetchErr::Other(format!("base url: {e}")))?;
    // The monitor endpoint lives at the host root; any port the model's
    // base URL carries is preserved.
    let mut url = base;
    url.set_path("/api/monitor/usage/quota/limit");
    url.set_query(None);
    let v: serde_json::Value = http
        .get(url)
        .bearer_auth(key)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(FetchErr::from_reqwest)?
        .error_for_status()
        .map_err(FetchErr::from_reqwest)?
        .json()
        .await
        .map_err(|e| FetchErr::Other(format!("parse: {e}")))?;
    parse_zai(&v).ok_or_else(|| FetchErr::Other("no usable windows".into()))
}

/// Pure parse (unit-tested): one window per `data.limits[]` entry.
pub fn parse_zai(v: &serde_json::Value) -> Option<Vec<QuotaWindow>> {
    let limits = v.pointer("/data/limits")?.as_array()?;
    let windows: Vec<QuotaWindow> = limits
        .iter()
        .filter_map(|l| {
            let pct = l.get("percentage").and_then(|x| x.as_f64())?;
            let reset = l
                .get("nextResetTime")
                .and_then(|x| x.as_i64())
                .map(|ms| (ms / 1000).max(0) as u64);
            let label = l
                .get("type")
                .and_then(|x| x.as_str())
                .unwrap_or("window")
                .to_string();
            Some(QuotaWindow {
                label,
                pct: pct.clamp(0.0, 100.0),
                reset_unix: reset,
                pace: None,
            })
        })
        .collect();
    (!windows.is_empty()).then_some(windows)
}

// ───────────────────────── header harvest ─────────────────────────

/// Harvest quota knowledge from a response arouter already proxied.
/// Supports the OpenAI-ish trio (`x-ratelimit-remaining/limit/reset`),
/// the requests/tokens variants, and RFC3339 / epoch / duration resets.
/// Returns at most two windows (`requests`, `tokens`).
pub fn harvest(headers: &HeaderMap) -> Vec<QuotaWindow> {
    let mut out = vec![];
    for label in ["requests", "tokens"] {
        let (rem, lim, res) = match label {
            "requests" => (
                hdr_f64(headers, "x-ratelimit-remaining")
                    .or_else(|| hdr_f64(headers, "x-ratelimit-requests-remaining")),
                hdr_f64(headers, "x-ratelimit-limit")
                    .or_else(|| hdr_f64(headers, "x-ratelimit-requests-limit")),
                headers
                    .get("x-ratelimit-reset")
                    .or_else(|| headers.get("x-ratelimit-reset-requests"))
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_reset),
            ),
            _ => (
                hdr_f64(headers, "x-ratelimit-tokens-remaining"),
                hdr_f64(headers, "x-ratelimit-tokens-limit"),
                headers
                    .get("x-ratelimit-reset-tokens")
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_reset),
            ),
        };
        let Some(remaining) = rem else { continue };
        let Some(limit) = lim else { continue };
        if limit <= 0.0 {
            continue;
        }
        out.push(QuotaWindow {
            label: label.into(),
            pct: ((limit - remaining) / limit * 100.0).clamp(0.0, 100.0),
            reset_unix: res,
            pace: None,
        });
    }
    out
}

/// A provider 429'd: until `Retry-After` it is exhausted, whatever its
/// quota API says. Returns a synthetic 100% window.
pub fn harvest_429(retry_after_secs: u64) -> QuotaWindow {
    QuotaWindow {
        label: "429".into(),
        pct: 100.0,
        reset_unix: Some(unix_now() + retry_after_secs),
        pace: None,
    }
}

fn hdr_f64(headers: &HeaderMap, name: &str) -> Option<f64> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse().ok())
}

/// Reset as unix seconds: epoch (OpenRouter), RFC3339, or a relative
/// duration like `12s` / `1m30s` / `1h` (Anthropic-style).
pub fn parse_reset(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Ok(n) = s.parse::<u64>() {
        // epoch seconds vs a small relative count — anything past 2001
        // is an absolute timestamp
        return if n > 1_000_000_000 { Some(n) } else { None };
    }
    if let Some(t) = crate::quota::parse_rfc3339_unix(s) {
        return Some(t);
    }
    parse_duration_secs(s).map(|d| unix_now() + d)
}

/// `10s` / `90s` / `1m30s` / `2h` / `1h0m0s` → seconds.
pub fn parse_duration_secs(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // h/m/s compound (Anthropic: "1h0m0s")
    let mut total = 0u64;
    let mut num = String::new();
    let mut matched = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            let unit = match c {
                'h' => 3600,
                'm' => 60,
                's' => 1,
                _ => return None,
            };
            let n: u64 = num.parse().ok()?;
            total += n * unit;
            num.clear();
            matched = true;
        }
    }
    if !num.is_empty() || !matched {
        return None;
    }
    Some(total)
}

// ───────────────────────── engine ─────────────────────────

/// One provider's resolved intake configuration.
#[derive(Debug, Clone)]
pub struct ProviderSource {
    pub prefix: String,
    pub kind: SourceKind,
    pub base_url: String,
    pub api_key: Option<String>,
}

/// The poll loop: every `poll_secs`, fetch every native source and
/// apply manual windows. Header sources are fed from the proxy, not
/// here. A failing fetch keeps the last windows (stale penalties
/// apply) and backs off: one skipped cycle for a normal failure,
/// exponential (2, 4, … capped 8) after a 429 — politeness towards
/// endpoints we may be the only caller of.
pub async fn run_engine(
    watcher: Arc<QuotaWatcher>,
    http: reqwest::Client,
    sources: Vec<ProviderSource>,
    poll_secs: u64,
) {
    if poll_secs == 0 || sources.is_empty() {
        return;
    }
    let mut tick = tokio::time::interval(Duration::from_secs(poll_secs.max(1)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // tokio's first tick completes immediately → the loop below is the
    // one and only startup poll (an extra explicit poll here would hit
    // every endpoint twice at boot; review checkpoint).
    let mut cooldown: Vec<u32> = vec![0; sources.len()];
    loop {
        tick.tick().await;
        for (i, s) in sources.iter().enumerate() {
            if cooldown[i] > 0 {
                cooldown[i] -= 1;
                tracing::debug!(provider = %s.prefix, "quota poll skipped (backoff)");
                continue;
            }
            match &s.kind {
                SourceKind::Native(kind) => {
                    match fetch_native(*kind, &s.base_url, s.api_key.as_deref(), &http).await {
                        Ok(w) if !w.is_empty() => {
                            cooldown[i] = 0;
                            tracing::debug!(provider = %s.prefix, windows = w.len(), "native quota snapshot");
                            watcher.apply(&s.prefix, w, unix_now());
                        }
                        Ok(_) => {}
                        Err(e) => {
                            cooldown[i] =
                                next_cooldown(cooldown[i], matches!(e, FetchErr::RateLimited));
                            tracing::warn!(
                                provider = %s.prefix,
                                "native quota fetch failed ({e}), keeping last; skipping {} cycle(s)",
                                cooldown[i]
                            );
                        }
                    }
                }
                SourceKind::Manual(specs) if !specs.is_empty() => {
                    let now = unix_now();
                    let w = manual_windows(specs, now);
                    watcher.apply(&s.prefix, w, now);
                }
                _ => {}
            }
        }
    }
}

/// Backoff policy (pure, unit-tested): any failure skips the next
/// cycle; a 429 doubles the previous skip, capped at 8.
pub fn next_cooldown(prev: u32, rate_limited: bool) -> u32 {
    let next = if rate_limited {
        prev.max(1).saturating_mul(2)
    } else {
        1
    };
    next.min(8)
}

/// One-word source name for status output (provenance without
/// credentials — review checkpoint: failures must be diagnosable).
pub fn describe_kind(k: &SourceKind) -> String {
    match k {
        SourceKind::Headers => "headers".into(),
        SourceKind::Off => "off".into(),
        SourceKind::Native(NativeKind::OpenRouter) => "native:openrouter".into(),
        SourceKind::Native(NativeKind::Zai) => "native:zai".into(),
        SourceKind::Manual(specs) => format!(
            "manual ({} window{})",
            specs.len(),
            if specs.len() == 1 { "" } else { "s" }
        ),
    }
}

/// Convenience for tests / status: describe the engine in one line.
pub fn describe(sources: &[ProviderSource]) -> BTreeMap<String, String> {
    sources
        .iter()
        .map(|s| (s.prefix.clone(), describe_kind(&s.kind)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::{ProviderState, QuotaState};

    fn win(label: &str, pct: f64, reset: Option<u64>) -> QuotaWindow {
        QuotaWindow {
            label: label.into(),
            pct,
            reset_unix: reset,
            pace: None,
        }
    }

    #[test]
    fn parses_zai_monitor_shape() {
        // shape probed live 2026-10-11 (strings redacted, numbers real):
        // two windows, epoch-millis resets
        let v: serde_json::Value = serde_json::json!({
            "code": 200, "success": true,
            "data": { "limits": [
                { "type": "five_hour", "percentage": 3, "usage": 12000,
                  "currentValue": 409, "remaining": 11590,
                  "nextResetTime": 1791751202624i64 },
                { "type": "month", "percentage": 1, "usage": 60000,
                  "currentValue": 409, "remaining": 59590,
                  "nextResetTime": 1792337120975i64 }
            ], "level": "l4" }
        });
        let w = parse_zai(&v).unwrap();
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].label, "five_hour");
        assert_eq!(w[0].pct, 3.0);
        assert_eq!(w[0].reset_unix, Some(1791751202)); // millis → secs
        assert_eq!(w[1].label, "month");
    }

    #[test]
    fn parses_openrouter_key_shape() {
        let v: serde_json::Value = serde_json::json!({
            "data": {
                "label": "k", "limit": 50.0, "limit_remaining": 37.5,
                "limit_reset": "2026-10-12T00:00:00Z",
                "usage_daily": 1.2,
                "effective_budget": {
                    "scope": "key", "limit_usd": 10.0, "spend_usd": 2.5,
                    "resets_at": "2026-10-12T00:00:00Z"
                }
            }
        });
        let w = parse_openrouter(&v).unwrap();
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].label, "credits");
        assert!((w[0].pct - 25.0).abs() < 1e-9); // (50-37.5)/50
        assert_eq!(w[0].reset_unix, Some(1791763200));
        assert_eq!(w[1].label, "budget:key");
        assert!((w[1].pct - 25.0).abs() < 1e-9);
    }

    #[test]
    fn openrouter_without_limit_yields_none() {
        let v: serde_json::Value = serde_json::json!({
            "data": { "limit": null, "limit_remaining": null, "usage": 1.0 }
        });
        assert!(parse_openrouter(&v).is_none());
    }

    #[test]
    fn harvests_openrouter_style_trio() {
        let mut h = HeaderMap::new();
        h.insert("x-ratelimit-remaining", "18".parse().unwrap());
        h.insert("x-ratelimit-limit", "20".parse().unwrap());
        h.insert("x-ratelimit-reset", "1791751202".parse().unwrap());
        let w = harvest(&h);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].label, "requests");
        assert!((w[0].pct - 10.0).abs() < 1e-9);
        assert_eq!(w[0].reset_unix, Some(1791751202));
    }

    #[test]
    fn harvests_token_windows_and_duration_resets() {
        let mut h = HeaderMap::new();
        h.insert("x-ratelimit-tokens-remaining", "50".parse().unwrap());
        h.insert("x-ratelimit-tokens-limit", "100".parse().unwrap());
        h.insert("x-ratelimit-reset-tokens", "1m30s".parse().unwrap());
        let w = harvest(&h);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].label, "tokens");
        assert!((w[0].pct - 50.0).abs() < 1e-9);
        let reset = w[0].reset_unix.unwrap();
        let now = unix_now();
        assert!(reset > now && reset <= now + 91, "relative reset");
    }

    #[test]
    fn harvest_ignores_limitless_headers() {
        let mut h = HeaderMap::new();
        h.insert("x-ratelimit-remaining", "5".parse().unwrap());
        assert!(harvest(&h).is_empty());
    }

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration_secs("10s"), Some(10));
        assert_eq!(parse_duration_secs("1m30s"), Some(90));
        assert_eq!(parse_duration_secs("2h"), Some(7200));
        assert_eq!(parse_duration_secs("1h0m0s"), Some(3600));
        assert_eq!(parse_duration_secs("soon"), None);
        assert_eq!(parse_duration_secs(""), None);
    }

    #[test]
    fn reset_parser_classifies_epoch_vs_relative() {
        assert_eq!(parse_reset("1791751202"), Some(1791751202));
        assert_eq!(parse_reset("42"), None); // small relative count, unusable
        assert_eq!(parse_reset("2026-10-12T00:00:00Z"), Some(1791763200));
        assert!(parse_reset("12s").is_some());
    }

    #[test]
    fn apply_replaces_one_provider_keeps_the_rest() {
        let w = QuotaWatcher::new(QuotaState::default());
        let now = unix_now();
        w.apply("zai", vec![win("5h", 33.0, Some(now + 3600))], now);
        w.apply("or", vec![win("credits", 25.0, None)], now + 5);
        let q = w.current();
        assert_eq!(q.providers.len(), 2);
        assert_eq!(q.providers["zai"].windows.len(), 1);
        assert_eq!(q.providers["zai"].windows[0].label, "5h");
        assert_eq!(q.providers["or"].windows[0].label, "credits");
        assert_eq!(q.fetched_unix, Some(now + 5));
        // fresh by per-provider update, even with no command snapshot
        assert!(!q.provider_stale(&q.providers["zai"], now));
        assert!(q.provider_stale(&q.providers["zai"], now + 301));
    }

    #[test]
    fn apply_upgrades_command_snapshot_provider() {
        // provider first seen via legacy command (updated_unix None)
        let mut qs = QuotaState::default();
        qs.providers.insert(
            "zai".into(),
            ProviderState {
                panel: "z.ai".into(),
                error: None,
                stale: false,
                windows: vec![win("old", 90.0, None)],
                updated_unix: None,
            },
        );
        let w = QuotaWatcher::new(qs);
        let now = unix_now();
        w.apply("zai", vec![win("fresh", 10.0, None)], now);
        let q = w.current();
        assert_eq!(q.providers["zai"].windows.len(), 1);
        assert_eq!(q.providers["zai"].windows[0].label, "fresh");
        assert_eq!(q.providers["zai"].updated_unix, Some(now));
    }

    #[test]
    fn resolve_explicit_beats_auto_and_manual() {
        assert!(matches!(
            resolve(Some("off"), "https://openrouter.ai/api/v1", None),
            SourceKind::Off
        ));
        assert!(matches!(
            resolve(None, "https://openrouter.ai/api/v1", None),
            SourceKind::Native(NativeKind::OpenRouter)
        ));
        assert!(matches!(
            resolve(None, "https://api.z.ai/api/coding/paas/v4", None),
            SourceKind::Headers // undocumented → opt-in, never auto
        ));
        let manual = vec![crate::config::QuotaManualWindow {
            provider: "x".into(),
            label: "weekly".into(),
            pct: 5.0,
            resets_in_secs: None,
        }];
        assert!(matches!(
            resolve(None, "https://strata:8081/v1", Some(&manual)),
            SourceKind::Manual(_)
        ));
        assert!(matches!(
            resolve(Some("manual"), "https://x/v1", Some(&manual)),
            SourceKind::Manual(_)
        ));
    }

    #[test]
    fn synthetic_429_window_is_exhausted_until_retry() {
        let now = unix_now();
        let w = harvest_429(120);
        assert_eq!(w.pct, 100.0);
        assert!(w.reset_unix.unwrap() > now + 110);
        // scoring path: availability must zero it out
        let p = ProviderState {
            panel: "x".into(),
            error: None,
            stale: false,
            windows: vec![w],
            updated_unix: Some(now),
        };
        let a = crate::quota::provider_availability(Some(&p), false, now);
        assert_eq!(a.score, 0.0);
        assert!(a.retry_after_unix.is_some());
    }

    // ── review checkpoints (astra-project-review) ──

    #[test]
    fn auto_kind_matches_parsed_hosts_only() {
        use SourceKind::*;
        assert_eq!(
            auto_kind("https://openrouter.ai/api/v1"),
            Native(NativeKind::OpenRouter)
        );
        assert_eq!(
            auto_kind("https://api.openrouter.ai/api/v1"),
            Native(NativeKind::OpenRouter)
        );
        // substring match would wrongly fire here (old impl)
        assert_eq!(auto_kind("https://notopenrouter.ai.example/v1"), Headers);
        assert_eq!(auto_kind("https://openrouter.com.evil.example/v1"), Headers);
        assert_eq!(auto_kind("http://localhost:8080/v1"), Headers);
    }

    #[test]
    fn manual_map_keeps_duplicate_provider_windows() {
        let specs = [
            spec_win("strata", "daily", 10.0, Some(3600)),
            spec_win("strata", "weekly", 50.0, Some(86400)),
            spec_win("or", "credits", 1.0, None),
        ];
        let m = manual_map(&specs);
        assert_eq!(m.len(), 2);
        assert_eq!(m["strata"].len(), 2, "both strata windows survive");
        // realized windows: relative resets become absolute
        let w = manual_windows(&m["strata"], 1_000);
        assert_eq!(w[0].label, "daily");
        assert_eq!(w[0].reset_unix, Some(4_600));
        assert_eq!(w[1].reset_unix, Some(87_400));
        assert_eq!(manual_windows(&m["or"], 1_000)[0].reset_unix, None);
    }

    fn spec_win(
        provider: &str,
        label: &str,
        pct: f64,
        resets_in_secs: Option<u64>,
    ) -> crate::config::QuotaManualWindow {
        crate::config::QuotaManualWindow {
            provider: provider.into(),
            label: label.into(),
            pct,
            resets_in_secs,
        }
    }

    #[test]
    fn backoff_policy_skips_and_doubles_on_429() {
        assert_eq!(next_cooldown(0, false), 1); // any failure skips 1 cycle
        assert_eq!(next_cooldown(3, false), 1); // reset after success is caller's job
        assert_eq!(next_cooldown(0, true), 2);
        assert_eq!(next_cooldown(2, true), 4);
        assert_eq!(next_cooldown(4, true), 8);
        assert_eq!(next_cooldown(8, true), 8, "capped");
    }

    #[test]
    fn stale_after_tracks_poll_interval() {
        let now = 1_000_000u64;
        let qs = crate::quota::QuotaState {
            fetched_unix: Some(now - 1_500),
            providers: BTreeMap::new(),
            stale_after_secs: 1_800, // 2 × a 900s poll
        };
        assert!(!qs.is_stale(now), "1500s old is fresh under a 900s poll");
        let qs = crate::quota::QuotaState {
            fetched_unix: Some(now - 1_900),
            providers: BTreeMap::new(),
            stale_after_secs: 1_800,
        };
        assert!(qs.is_stale(now));
    }

    #[test]
    fn command_snapshot_never_erases_fresher_signals() {
        let now = unix_now();
        let w = Arc::new(crate::quota::QuotaWatcher::default());
        // header harvest wrote a fresh window for "zai" 30s ago
        w.apply(
            "zai",
            vec![QuotaWindow {
                label: "requests".into(),
                pct: 5.0,
                reset_unix: Some(now + 600),
                pace: None,
            }],
            now - 30,
        );
        // a legacy command snapshot fetched 10 minutes ago says zai is 99% used
        let snap = crate::quota::QuotaState {
            fetched_unix: Some(now - 600),
            providers: BTreeMap::from([(
                "zai".into(),
                crate::quota::ProviderState {
                    panel: "z.ai".into(),
                    error: None,
                    stale: false,
                    windows: vec![QuotaWindow {
                        label: "weekly".into(),
                        pct: 99.0,
                        reset_unix: None,
                        pace: None,
                    }],
                    updated_unix: None,
                },
            )]),
            stale_after_secs: 0,
        };
        w.merge_snapshot(snap);
        let cur = w.current();
        let z = cur.providers.get("zai").expect("provider kept");
        assert_eq!(
            z.windows[0].label, "requests",
            "harvested window survives the swap"
        );
        assert_eq!(z.updated_unix, Some(now - 30));
        // but a snapshot NEWER than the harvest wins
        let snap2 = crate::quota::QuotaState {
            fetched_unix: Some(now),
            providers: BTreeMap::from([(
                "zai".into(),
                crate::quota::ProviderState {
                    panel: "z.ai".into(),
                    error: None,
                    stale: false,
                    windows: vec![QuotaWindow {
                        label: "weekly".into(),
                        pct: 40.0,
                        reset_unix: None,
                        pace: None,
                    }],
                    updated_unix: None,
                },
            )]),
            stale_after_secs: 0,
        };
        w.merge_snapshot(snap2);
        assert_eq!(w.current().providers["zai"].windows[0].label, "weekly");
    }
}
