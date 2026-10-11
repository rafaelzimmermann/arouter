//! HTTP layer: axum server, OpenAI-completions proxy with SSE passthrough,
//! per-caller identity, failover across the attempt list, Prometheus
//! `/metrics`, and `X-Arouter-*` observability headers.

use crate::budget;
use crate::config::{CallerIdentity, CallerMap, Config};
use crate::health::Health;
use crate::metrics::{self, Metrics};
use crate::quota;
use crate::quotaeng;
use crate::registry::{Model, Registry};
use crate::usage::{unix_now, UsageLog, UsageRecord};
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{Stream, TryStreamExt};
use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

// Response header names (lowercase wire form; keep the 'r' in "arouter"!).
mod hdr {
    use axum::http::HeaderName;

    pub const UPSTREAM: HeaderName = HeaderName::from_static("x-arouter-upstream");
    pub const POOL: HeaderName = HeaderName::from_static("x-arouter-pool");
    pub const GRADE: HeaderName = HeaderName::from_static("x-arouter-grade");
    pub const DEGRADED: HeaderName = HeaderName::from_static("x-arouter-degraded");
    pub const PROFILE: HeaderName = HeaderName::from_static("x-arouter-profile");
    pub const CALLER: HeaderName = HeaderName::from_static("x-arouter-caller");
    pub const PROJECT: HeaderName = HeaderName::from_static("x-arouter-project");
    pub const COST: HeaderName = HeaderName::from_static("x-arouter-cost-usd");
    pub const BUDGET_REMAINING: HeaderName =
        HeaderName::from_static("x-arouter-budget-remaining-today");
    pub const CROSS_POOL: HeaderName = HeaderName::from_static("x-arouter-cross-pool");
}

// Request headers for open-mode caller identification.
mod req_hdr {
    use axum::http::HeaderName;

    pub const USER: HeaderName = HeaderName::from_static("x-arouter-user");
    pub const PROJECT: HeaderName = HeaderName::from_static("x-arouter-project");
    pub const API_KEY: HeaderName = HeaderName::from_static("x-api-key");
}

pub const METRICS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

#[derive(Clone)]
/// Everything derived from one config load (review F6): registry,
/// callers, budgets/policy and quota wiring. Swapped ATOMICICALLY on
/// hot-reload — a request that started on config N finishes on config
/// N (candidates, caps and callers never mix generations).
pub struct Runtime {
    pub cfg: Arc<Config>,
    pub registry: Arc<Registry>,
    pub callers: Arc<CallerMap>,
    /// registry provider prefix → quota panel name.
    pub provider_map: Arc<quota::ProviderMap>,
    /// provider prefixes whose source is `headers` (E3b): the proxy
    /// harvests x-ratelimit-*/429 Retry-After into quota state.
    pub quota_harvest: Arc<std::collections::BTreeMap<String, bool>>,
    /// prefix → quota source name ("native:zai", "headers", …) for
    /// status provenance.
    pub quota_sources: Arc<std::collections::BTreeMap<String, String>>,
}

impl Runtime {
    /// Build the full snapshot from a (possibly discovery-merged)
    /// config. Pure — spawns nothing; pollers bind at startup.
    pub fn build(cfg: Config, keys: &crate::config::KeyStore) -> Result<Runtime, String> {
        let registry =
            Arc::new(Registry::from_config(&cfg, keys).map_err(|e| format!("registry: {e}"))?);
        let callers = Arc::new(CallerMap::from_config(&cfg, keys));
        let provider_map = Arc::new(quota::ProviderMap {
            explicit: cfg.quota.provider_map.clone(),
        });
        let (quota_harvest, quota_sources) = crate::quotaeng::scan_sources(&cfg, &registry);
        Ok(Runtime {
            cfg: Arc::new(cfg),
            registry,
            callers,
            provider_map,
            quota_harvest: Arc::new(quota_harvest),
            quota_sources: Arc::new(quota_sources),
        })
    }
}

#[derive(Clone)]
pub struct AppState {
    /// Immutable runtime snapshot — hot-reload (SIGHUP / config mtime /
    /// discovery) swaps the whole Arc; readers bind
    /// `let rt = st.runtime();` for a consistent view.
    pub runtime: Arc<RwLock<Arc<Runtime>>>,
    pub usage: Arc<UsageLog>,
    pub metrics: Arc<Metrics>,
    pub health: Arc<Health>,
    /// aitop-derived quota state — replaced by E3b native intake
    /// (fetchers + header harvest); still swapped by the legacy command
    /// watcher when one is configured.
    pub quota: Arc<quota::QuotaWatcher>,
    /// ollama/llama.cpp residency + busy state (E8).
    pub local: Arc<crate::localeng::LocalEngines>,
    /// warm-cache stickiness for the local pool (E8).
    pub sticky: Arc<crate::localeng::StickyMap>,
    pub http: reqwest::Client,
    pub started: Instant,
}

impl AppState {
    /// Current runtime snapshot — bind once per request (review F6).
    pub fn runtime(&self) -> Arc<Runtime> {
        self.runtime.read().unwrap().clone()
    }
}

pub fn client(t: &crate::config::TimeoutsConfig) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(t.connect_secs))
        // review F5: read gaps are bounded — this covers stalled
        // headers, stalled buffered bodies, AND SSE chunk gaps (a
        // stalled stream surfaces as an error in TapStream)
        .read_timeout(Duration::from_secs(t.idle_secs))
        .build()
        .expect("reqwest client")
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_base))
        .route("/local/v1/chat/completions", post(chat_local))
        .route("/ondemand/v1/chat/completions", post(chat_ondemand))
        .route("/{profile}/v1/chat/completions", post(chat_profile))
        .route(
            "/{profile}/{pool}/v1/chat/completions",
            post(chat_profile_pool),
        )
        .route("/v1/messages", post(messages_base))
        .route("/local/v1/messages", post(messages_local))
        .route("/ondemand/v1/messages", post(messages_ondemand))
        .route("/{profile}/v1/messages", post(messages_profile))
        .route("/{profile}/{pool}/v1/messages", post(messages_profile_pool))
        .route("/v1/models", get(models_base))
        .route("/local/v1/models", get(models_local))
        .route("/ondemand/v1/models", get(models_ondemand))
        .route("/{profile}/v1/models", get(models_profile))
        .route("/{profile}/{pool}/v1/models", get(models_profile_pool))
        .route("/status.json", get(status))
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics_scrape))
        .with_state(state)
}

// ─────────────────────────── thin handlers ───────────────────────────

async fn chat_base(State(st): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    handle_chat(st, "default", "quota", &headers, body, Wire::OpenAi).await
}
async fn chat_local(State(st): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    handle_chat(st, "default", "local", &headers, body, Wire::OpenAi).await
}
async fn chat_ondemand(State(st): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    handle_chat(st, "default", "ondemand", &headers, body, Wire::OpenAi).await
}
async fn chat_profile(
    State(st): State<AppState>,
    Path(profile): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(resp) = path_profile_guard(&profile) {
        return resp;
    }
    handle_chat(st, &profile, "quota", &headers, body, Wire::OpenAi).await
}
async fn chat_profile_pool(
    State(st): State<AppState>,
    Path((profile, pool)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(resp) = path_profile_guard(&profile) {
        return resp;
    }
    handle_chat(st, &profile, &pool, &headers, body, Wire::OpenAi).await
}

async fn messages_base(State(st): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    handle_chat(st, "default", "quota", &headers, body, Wire::Anthropic).await
}
async fn messages_local(State(st): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    handle_chat(st, "default", "local", &headers, body, Wire::Anthropic).await
}
async fn messages_ondemand(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle_chat(st, "default", "ondemand", &headers, body, Wire::Anthropic).await
}
async fn messages_profile(
    State(st): State<AppState>,
    Path(profile): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(resp) = path_profile_guard(&profile) {
        return resp;
    }
    handle_chat(st, &profile, "quota", &headers, body, Wire::Anthropic).await
}
async fn messages_profile_pool(
    State(st): State<AppState>,
    Path((profile, pool)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(resp) = path_profile_guard(&profile) {
        return resp;
    }
    handle_chat(st, &profile, &pool, &headers, body, Wire::Anthropic).await
}

async fn models_base(State(st): State<AppState>, headers: HeaderMap) -> Response {
    list_models(st, "default", "quota", &headers).await
}
async fn models_local(State(st): State<AppState>, headers: HeaderMap) -> Response {
    list_models(st, "default", "local", &headers).await
}
async fn models_ondemand(State(st): State<AppState>, headers: HeaderMap) -> Response {
    list_models(st, "default", "ondemand", &headers).await
}
async fn models_profile(
    State(st): State<AppState>,
    Path(profile): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Some(resp) = path_profile_guard(&profile) {
        return resp;
    }
    list_models(st, &profile, "quota", &headers).await
}
async fn models_profile_pool(
    State(st): State<AppState>,
    Path((profile, pool)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Some(resp) = path_profile_guard(&profile) {
        return resp;
    }
    list_models(st, &profile, &pool, &headers).await
}

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// F12: observation endpoints (status/metrics/model listings) honor
/// caller auth whenever completions do — operational detail (callers,
/// upstreams, spend) is not open in auth-required mode.
fn obs_auth(st: &AppState, headers: &HeaderMap) -> Option<Response> {
    let rt = st.runtime();
    if !rt.cfg.auth.require {
        return None;
    }
    match resolve_caller(&rt.callers, headers) {
        Ok(_) => None,
        Err(msg) => Some(api_error(401, "unauthorized", msg)),
    }
}

async fn status(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = obs_auth(&st, &headers) {
        return resp;
    }
    let rt = st.runtime();
    let reg = &rt.registry;
    let snap = st.usage.snapshot();
    let health: serde_json::Map<String, serde_json::Value> = st
        .health
        .snapshot()
        .into_iter()
        .map(|(id, s)| {
            (
                id,
                serde_json::json!({
                    "state": format!("{:?}", s.state).to_lowercase(),
                    "secs_left": s.secs_left,
                    "samples_1h": s.samples_1h,
                    "fail_ratio_1h": (s.fail_ratio_1h * 100.0).round() / 100.0,
                }),
            )
        })
        .collect();
    let mut root = serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": st.started.elapsed().as_secs(),
        "bind": rt.cfg.server.bind,
        "auth_required": rt.cfg.auth.require,
        "pools": crate::config::POOLS,
        "profiles": reg
            .profiles
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::json!({ "description": v.description })))
            .collect::<serde_json::Map<String, serde_json::Value>>(),
        "models": reg
            .models
            .iter()
            .map(|m| serde_json::json!({
                "id": m.id,
                "grade": reg.ladder.token(m.grade_rank),
                "pools": m.pools,
                "base_url": m.base_url,
                "weight": m.weight,
                "profiles": m.profiles,
                "provenance": m.provenance,
            }))
            .collect::<Vec<_>>(),
        "health": health,
    });
    // E3b: quota — panel windows + computed availability (per-provider
    // staleness: the newest signal wins).
    let q = st.quota.current();
    let now = crate::quota::unix_now();
    // panel key → source name: engine/harvest write under the registry
    // prefix, legacy snapshots under panel names — try both, then the
    // explicit map in reverse (zai → "z.ai").
    let source_for = |panel: &str| -> Option<String> {
        rt.quota_sources.get(panel).cloned().or_else(|| {
            rt.provider_map
                .explicit
                .iter()
                .find(|(_, p)| p == &panel)
                .and_then(|(prefix, _)| rt.quota_sources.get(prefix).cloned())
        })
    };
    let providers: serde_json::Map<String, serde_json::Value> = q
        .providers
        .iter()
        .map(|(panel, ps)| {
            let avail =
                crate::quota::provider_availability(Some(ps), q.provider_stale(ps, now), now);
            (
                panel.clone(),
                serde_json::json!({
                    "stale": ps.stale,
                    "error": ps.error,
                    "source": source_for(panel),
                    "updated_unix": ps.updated_unix,
                    "availability": (avail.score * 1000.0).round() / 1000.0,
                    "notes": avail.notes,
                    "windows": ps.windows.iter().map(|w| serde_json::json!({
                        "label": w.label,
                        "pct": (w.pct * 1000.0).round() / 1000.0,
                        "reset_in_secs": w.reset_unix.map(|t| t.saturating_sub(now)),
                    })).collect::<Vec<_>>(),
                }),
            )
        })
        .collect();
    if let Some(root_obj) = root.as_object_mut() {
        root_obj.insert(
            "quota".into(),
            serde_json::json!({
                "fetched_unix": q.fetched_unix,
                "stale": q.is_stale(now),
                "providers": providers,
                // explicit aliases (config `[quota].provider_map`,
                // legacy `[quota_watcher]`); the monitor uses them to
                // attach models to quota panels
                "provider_map": rt.provider_map.explicit,
            }),
        );
    }
    if let (Some(root_obj), Some(snap_obj)) = (root.as_object_mut(), snap.as_object()) {
        for (k, v) in snap_obj {
            root_obj.insert(k.clone(), v.clone());
        }
    }
    // E8: local-engine residency snapshot (monitor + debugging)
    {
        let l = st.local.current();
        if let Some(root_obj) = root.as_object_mut() {
            root_obj.insert(
                "local_engines".into(),
                serde_json::json!({
                    "fetched_unix": l.fetched_unix,
                    "stale": l.stale,
                    "engines": l.engines.iter().map(|e| serde_json::json!({
                        "url": e.url,
                        "kind": e.kind.as_str(),
                        "busy": e.busy,
                        "loaded": e.loaded.iter().map(|(name, lm)| (
                            name.clone(),
                            serde_json::json!({
                                "size_vram": lm.size_vram,
                                "expires_unix": lm.expires_unix,
                            })
                        )).collect::<serde_json::Map<String, serde_json::Value>>(),
                    })).collect::<Vec<_>>(),
                }),
            );
        }
    }
    Json(root).into_response()
}

async fn metrics_scrape(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = obs_auth(&st, &headers) {
        return resp;
    }
    let rt = st.runtime();
    let m = &st.metrics;
    m.gauge_set(
        metrics::N_BUILD_INFO,
        metrics::H_BUILD_INFO,
        &[("version", env!("CARGO_PKG_VERSION").to_string())],
        1.0,
    );
    m.gauge_set(
        metrics::N_UPTIME,
        metrics::H_UPTIME,
        &[],
        st.started.elapsed().as_secs_f64(),
    );
    let reg = &rt.registry;
    let mut profiles: Vec<&str> = vec!["default"];
    profiles.extend(reg.profiles.keys().map(|s| s.as_str()));
    for p in profiles {
        for pool in crate::config::POOLS {
            for rank in 0..reg.ladder.grades.len() {
                m.gauge_set(
                    metrics::N_MODELS,
                    metrics::H_MODELS,
                    &[
                        ("profile", p.to_string()),
                        ("pool", pool.to_string()),
                        ("grade", reg.ladder.token(rank).to_string()),
                    ],
                    reg.count_exact(p, pool, rank) as f64,
                );
            }
        }
    }
    // E3: quota gauges — window pct per provider + staleness.
    let q = st.quota.current();
    for (panel, ps) in &q.providers {
        for w in &ps.windows {
            m.gauge_set(
                metrics::N_QUOTA_WINDOW,
                metrics::H_QUOTA_WINDOW,
                &[("provider", panel.clone()), ("window", w.label.clone())],
                w.pct,
            );
        }
        m.gauge_set(
            metrics::N_QUOTA_STALE,
            metrics::H_QUOTA_STALE,
            &[("provider", panel.clone())],
            f64::from(u8::from(ps.stale)),
        );
        let now = quota::unix_now();
        let secs = quota::draining(Some(ps), now)
            .map(|w| (w.reset_unix.unwrap_or(now).saturating_sub(now)) as f64)
            .unwrap_or(-1.0);
        m.gauge_set(
            metrics::N_QUOTA_DRAIN_RESET,
            metrics::H_QUOTA_DRAIN_RESET,
            &[("provider", panel.clone())],
            secs,
        );
    }
    ([(header::CONTENT_TYPE, METRICS_CONTENT_TYPE)], m.render()).into_response()
}

// ───────────────────────────── core proxy ─────────────────────────────

/// Profiles in the URL path may not use reserved names (`default` is the
/// base path; `local`/`ondemand` are pool paths).
fn path_profile_guard(profile: &str) -> Option<Response> {
    crate::config::RESERVED_PROFILES
        .contains(&profile)
        .then(|| {
            api_error(
                404,
                "reserved_profile",
                format!("{profile:?} is reserved; use the base paths"),
            )
        })
}

/// Extract the bearer token from `Authorization: Bearer …` or `X-Api-Key`.
fn bearer_of(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        let tok = v
            .strip_prefix("Bearer ")
            .or_else(|| v.strip_prefix("bearer "))
            .map(str::trim);
        if let Some(t) = tok.filter(|t| !t.is_empty()) {
            return Some(t.to_string());
        }
    }
    headers
        .get(&req_hdr::API_KEY)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(String::from)
}

fn hdr_str<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
}

/// Resolve the caller identity for a request (see docs/architecture.md
/// §Callers): bearer key → `[[callers]]` entry; open mode falls back to
/// `X-Arouter-User`/`X-Arouter-Project` headers, else anonymous.
/// Err = rejection message (401).
fn resolve_caller(callers: &CallerMap, headers: &HeaderMap) -> Result<CallerIdentity, String> {
    let bearer = bearer_of(headers);
    callers.resolve(
        bearer.as_deref(),
        hdr_str(headers, &req_hdr::USER),
        hdr_str(headers, &req_hdr::PROJECT),
    )
}

#[allow(clippy::too_many_arguments)]
async fn handle_chat(
    st: AppState,
    profile: &str,
    pool: &str,
    headers: &HeaderMap,
    body: Bytes,
    wire: Wire,
) -> Response {
    let t0 = Instant::now();
    let _in_flight = st.metrics.in_flight();
    // review F6: one snapshot for the whole request — reloads swap the
    // Arc under us, this request still finishes on the config it saw
    let rt = st.runtime();
    let caller = match resolve_caller(&rt.callers, headers) {
        Ok(c) => c,
        Err(msg) => return wire_error(wire, 401, "unauthorized", msg),
    };
    if !crate::config::POOLS.contains(&pool) {
        return wire_error(
            wire,
            404,
            "unknown_pool",
            format!("{pool:?} (valid: {:?})", crate::config::POOLS),
        );
    }
    let reg = rt.registry.clone();
    if profile != "default" && !reg.profiles.contains_key(profile) {
        let known: Vec<&str> = reg.profiles.keys().map(|s| s.as_str()).collect();
        return wire_error(
            wire,
            404,
            "unknown_profile",
            format!("{profile:?} (configured: {known:?})"),
        );
    }

    let mut payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return wire_error(wire, 400, "invalid_json", format!("request body: {e}")),
    };
    // E7: anthropic-wire clients get translated in, translated back out;
    // routing/quota/budget/health all operate on the openai payload.
    if wire == Wire::Anthropic {
        payload = match crate::translate::anthropic_to_openai(&payload) {
            Ok(v) => v,
            Err(e) => return wire_error(wire, 400, "invalid_request", e),
        };
    }
    let raw_model = payload
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let Some(req) = reg.ladder.parse_request(&raw_model) else {
        return wire_error(
            wire,
            400,
            "unknown_grade",
            format!(
                "model {raw_model:?}: not a grade; valid: {}",
                reg.ladder.list()
            ),
        );
    };
    let is_stream = payload
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // review F3: ASK for usage on streams — many upstreams omit token
    // counts from SSE unless stream_options.include_usage is set, and
    // missing usage silently became zero spend.
    if is_stream {
        match payload
            .get_mut("stream_options")
            .and_then(|v| v.as_object_mut())
        {
            Some(o) => {
                o.insert("include_usage".into(), serde_json::Value::Bool(true));
            }
            None => {
                payload["stream_options"] = serde_json::json!({ "include_usage": true });
            }
        }
    }

    // E3/E4: quota + context + budget aware candidate set
    // (routing.md §1/§3). `pre` keeps every health-ok candidate (even
    // quota-exhausted) so the 429 can quote a reset-aware Retry-After.
    let est = quota::estimate_request(&payload);
    let qstate = st.quota.current();
    let build_candidates = |pool: &str| -> (Vec<&Model>, Vec<&Model>) {
        let pre: Vec<&Model> = reg
            .attempt_list(profile, pool, &req)
            .into_iter()
            .filter(|m| !st.health.is_excluded(&m.id))
            .filter(|m| quota::context_fits(m, &est))
            .collect();
        // F11: preference resolved ONCE per request — profile overlay
        // beats pool default; every recognized value maps to an ordering
        let mode = match reg.preference_for(profile, pool) {
            "cheapest" if pool == "ondemand" => quota::OrderMode::Cheapest,
            "fastest" => quota::OrderMode::Fastest,
            _ => quota::OrderMode::QuotaPool,
        };
        let ordered = quota::order_candidates(
            &qstate,
            &st.health,
            &rt.provider_map,
            mode,
            &est,
            pre.clone(),
        )
        .into_iter()
        .filter(|m| quota::model_available(&qstate, &rt.provider_map, m))
        .collect();
        // E8: residency boost (loaded local models float within their
        // grade), then warm-cache stickiness for the local pool.
        let mut ordered = crate::localeng::boost_residency(ordered, &st.local.current());
        if pool == "local" {
            ordered = crate::localeng::boost_sticky(ordered, &st.sticky, &caller.name);
        }
        (ordered, pre)
    };

    let (candidates, pre) = build_candidates(pool);
    let mut final_err: Option<AttemptErr> = None;
    if !candidates.is_empty() {
        match try_candidates(
            &st,
            &rt,
            &caller,
            profile,
            pool,
            &raw_model,
            &req,
            is_stream,
            &mut payload,
            &est,
            &candidates,
            None,
            wire,
            t0,
        )
        .await
        {
            Ok(r) => return r,
            Err(e) => final_err = Some(e),
        }
    }

    // Cross-pool fallback: explicit opt-in only (routing.md §2) — the
    // guard against surprise charges. Targets still pass their own
    // budget preflight inside try_candidates.
    let cross: Vec<String> = reg
        .profiles
        .get(profile)
        .map(|p| p.fallback.cross_pool.clone())
        .unwrap_or_else(|| reg.base_fallback.cross_pool.clone())
        .unwrap_or_default();
    for target in cross {
        if target == pool || !crate::config::POOLS.contains(&target.as_str()) {
            continue;
        }
        let (tcands, _) = build_candidates(&target);
        if tcands.is_empty() {
            continue;
        }
        tracing::info!(from = pool, to = %target, "cross-pool fallback");
        match try_candidates(
            &st,
            &rt,
            &caller,
            profile,
            &target,
            &raw_model,
            &req,
            is_stream,
            &mut payload,
            &est,
            &tcands,
            Some(pool),
            wire,
            t0,
        )
        .await
        {
            Ok(r) => return r,
            Err(e) => final_err = Some(e),
        }
    }

    st.metrics.record_exhausted(
        pool,
        profile,
        &caller.name,
        caller.project.as_deref(),
        &raw_model,
    );
    // Budget denial outranks generic exhaustion (E4): the caller asked
    // for a chargeable pool and the guard said no.
    if let Some(e) = &final_err {
        if let Some(d) = &e.denial {
            if e.denials == e.attempts {
                return budget_error(d, profile, &raw_model, wire);
            }
        }
    }
    let retry = quota::nearest_retry_unix(&qstate, &rt.provider_map, &pre)
        .map(|t| t.saturating_sub(quota::unix_now()).max(1))
        .filter(|s| *s > 0);
    let mut extra: Vec<(header::HeaderName, String)> = vec![];
    if let Some(secs) = retry {
        extra.push((header::RETRY_AFTER, secs.to_string()));
    }
    let detail = final_err
        .map(|e| e.last_error)
        .unwrap_or_else(|| "no candidates".into());
    if wire == Wire::Anthropic {
        let mut resp = wire_error(
            wire,
            429,
            "arouter_exhausted",
            format!("grade {raw_model} in pool {pool} (profile {profile}) exhausted: {detail}"),
        );
        if let Some(secs) = retry {
            if let Ok(hv) = header::HeaderValue::from_str(&secs.to_string()) {
                resp.headers_mut().insert(header::RETRY_AFTER, hv);
            }
        }
        return resp;
    }
    api_error_with(
        429,
        "arouter_exhausted",
        format!("grade {raw_model} in pool {pool} (profile {profile}) exhausted: {detail}"),
        &extra,
    )
}

/// Why an attempt list produced no response.
struct AttemptErr {
    last_error: String,
    /// Last budget denial, if any (only reported as 402 when it was the
    /// reason every attempt was skipped).
    denial: Option<budget::Denial>,
    attempts: usize,
    denials: usize,
}

/// Walk an ordered candidate list: budget preflight (ondemand), then one
/// upstream attempt per candidate with health recording and failover.
/// `pool` is the *serving* pool (differs from the request pool during
/// cross-pool fallback; `cross_from` names the origin then).
#[allow(clippy::too_many_arguments)]
async fn try_candidates(
    st: &AppState,
    rt: &Runtime,
    caller: &CallerIdentity,
    profile: &str,
    pool: &str,
    raw_model: &str,
    req: &crate::grades::GradeRequest,
    is_stream: bool,
    payload: &mut serde_json::Value,
    est: &quota::RequestEstimate,
    candidates: &[&Model],
    cross_from: Option<&str>,
    wire: Wire,
    t0: Instant,
) -> Result<Response, AttemptErr> {
    let mut err = AttemptErr {
        last_error: String::from("all candidates failed"),
        denial: None,
        attempts: 0,
        denials: 0,
    };
    // Role rewrites are per-model (an upstream may reject a role a
    // sibling accepts), so restore the caller's original `messages`
    // before applying each candidate's map — a failed attempt with a
    // mapped payload must not leak into the next candidate.
    let original_messages = payload.get("messages").cloned();
    for m in candidates
        .iter()
        // review F5: bound candidates per request (documented top-3)
        .take(rt.cfg.timeouts.max_attempts.max(1))
    {
        // Settled into relay_success / TapStream on success; dropped
        // (hold released) when this candidate fails (review F2).
        let mut reserve: Option<budget::ReserveGuard> = None;
        // E4: on-demand spend guards before every chargeable attempt.
        if pool == "ondemand" {
            // review F3: unknown price must not pass as "free" — a
            // candidate with no cost metadata cannot enter the paid pool
            if m.cost_in <= 0.0 && m.cost_out <= 0.0 {
                tracing::warn!(
                    model = m.id,
                    "unpriced model in ondemand pool; refusing (set cost_in/cost_out)"
                );
                err.last_error = format!("{}: unpriced models cannot enter the paid pool", m.id);
                err.attempts += 1;
                err.denials += 1;
                continue;
            }
            let est_usd = quota::est_cost(m, est);
            match budget::admit(&st.usage, &rt.cfg, profile, caller, est_usd) {
                Ok(g) => reserve = Some(g),
                Err(d) => {
                    tracing::info!(
                        model = m.id,
                        layer = d.layer,
                        window = d.window,
                        "budget preflight skip (est ${:.4}, spent ${:.4}, cap ${:.4})",
                        d.est,
                        d.spent,
                        d.cap
                    );
                    // keep the FIRST (cheapest-candidate) denial: it names the
                    // guard that stopped the model the caller would have gotten
                    // — later, pricier candidates' denials are less actionable
                    if err.denial.is_none() {
                        err.denial = Some(d);
                    }
                    err.last_error = format!(
                        "{}: budget {} {}",
                        m.id,
                        err.denial.as_ref().unwrap().layer,
                        err.denial.as_ref().unwrap().window
                    );
                    err.attempts += 1;
                    err.denials += 1;
                    continue;
                }
            }
        }
        if let Some(orig) = &original_messages {
            payload["messages"] = orig.clone();
        }
        apply_role_map(payload, &m.role_map);
        payload["model"] = serde_json::Value::String(m.upstream_name.clone());
        // E7: native Codex subscription wire — translate to the Codex
        // Responses endpoint under the CLI login, translate back.
        if m.wire == "codex" {
            match codex_attempt(
                st, rt, m, caller, profile, pool, raw_model, req, is_stream, payload, est,
                cross_from, wire, t0, reserve,
            )
            .await
            {
                Ok(r) => return Ok(r),
                Err(e) => {
                    if let Some(resp) = e.fatal {
                        return Ok(resp);
                    }
                    err.last_error = format!("{}: {}", m.id, e.last_error);
                    err.attempts += 1;
                    continue;
                }
            }
        }
        let url = format!("{}/chat/completions", m.base_url);
        let mut rb = st.http.post(&url).json(&payload);
        if let Some(key) = &m.api_key {
            rb = rb.bearer_auth(key);
        }
        let prefix = m.id.split('/').next().unwrap_or("").to_string();
        let harvest = rt.quota_harvest.get(&prefix).copied().unwrap_or(true);
        // review F5: headers deadline — a connected upstream that never
        // answers fails over instead of hanging the request
        let resp = match tokio::time::timeout(
            Duration::from_secs(rt.cfg.timeouts.headers_secs),
            rb.send(),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => {
                record_failure(
                    st,
                    rt,
                    m,
                    caller,
                    profile,
                    pool,
                    raw_model,
                    "timeout",
                    None,
                    "no response headers within headers_secs",
                    t0.elapsed().as_millis() as u64,
                );
                tracing::warn!(model = m.id, "headers deadline exceeded");
                st.health.record_failure(&m.id, true);
                err.last_error = format!("{}: headers timeout", m.id);
                err.attempts += 1;
                continue;
            }
        };
        match resp {
            Ok(resp) if resp.status().is_success() => {
                let degraded = m.grade_rank != req.ceiling;
                if harvest {
                    let w = quotaeng::harvest(resp.headers());
                    if !w.is_empty() {
                        st.quota.apply(&prefix, w, unix_now());
                    }
                }
                if !is_stream {
                    // review F5: read the FULL body before committing.
                    // Nothing has reached the client yet, so a stall or
                    // truncation fails over to the next candidate — the
                    // cheapest legal form of transparent retry.
                    let ct = resp
                        .headers()
                        .get(header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("application/json")
                        .to_string();
                    let idle = Duration::from_secs(rt.cfg.timeouts.idle_secs);
                    let bytes = match tokio::time::timeout(idle, resp.bytes()).await {
                        Ok(Ok(b)) => b,
                        Ok(Err(e)) => {
                            let msg = format!("body read failed: {e}");
                            record_failure(
                                st,
                                rt,
                                m,
                                caller,
                                profile,
                                pool,
                                raw_model,
                                "upstream_error",
                                Some(200),
                                &msg,
                                t0.elapsed().as_millis() as u64,
                            );
                            tracing::warn!(model = m.id, "{msg}");
                            st.health.record_failure(&m.id, true);
                            err.last_error = format!("{}: {}", m.id, msg);
                            err.attempts += 1;
                            continue;
                        }
                        Err(_) => {
                            let msg = "buffered body stalled past idle_secs";
                            record_failure(
                                st,
                                rt,
                                m,
                                caller,
                                profile,
                                pool,
                                raw_model,
                                "timeout",
                                Some(200),
                                msg,
                                t0.elapsed().as_millis() as u64,
                            );
                            tracing::warn!(model = m.id, "{msg}");
                            st.health.record_failure(&m.id, true);
                            err.last_error = format!("{}: {}", m.id, msg);
                            err.attempts += 1;
                            continue;
                        }
                    };
                    st.health
                        .record_success_latency(&m.id, Some(t0.elapsed().as_millis() as u64));
                    return Ok(relay_success(
                        st,
                        rt,
                        m,
                        caller,
                        profile,
                        pool,
                        raw_model,
                        degraded,
                        is_stream,
                        Upstream::Buffered(ct, bytes),
                        t0,
                        est,
                        cross_from,
                        wire,
                        reserve,
                    )
                    .await);
                }
                st.health.record_success(&m.id); // EMA lands at stream finish
                return Ok(relay_success(
                    st,
                    rt,
                    m,
                    caller,
                    profile,
                    pool,
                    raw_model,
                    degraded,
                    is_stream,
                    Upstream::Resp(resp),
                    t0,
                    est,
                    cross_from,
                    wire,
                    reserve,
                )
                .await);
            }
            Ok(resp) => {
                let code = resp.status().as_u16();
                // E3b: a 429 is ground truth — the provider is exhausted
                // until Retry-After says otherwise, whatever its quota
                // API last claimed.
                if harvest && code == 429 {
                    let ra = resp
                        .headers()
                        .get(header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.trim().parse::<u64>().ok())
                        .unwrap_or(60);
                    st.quota
                        .apply(&prefix, vec![quotaeng::harvest_429(ra)], unix_now());
                } else if harvest {
                    let w = quotaeng::harvest(resp.headers());
                    if !w.is_empty() {
                        st.quota.apply(&prefix, w, unix_now());
                    }
                }
                let body_text = truncate(&resp.text().await.unwrap_or_default(), 300);
                let msg = format!("upstream {code}: {body_text}");
                record_failure(
                    st,
                    rt,
                    m,
                    caller,
                    profile,
                    pool,
                    raw_model,
                    "upstream_error",
                    Some(code),
                    &msg,
                    t0.elapsed().as_millis() as u64,
                );
                if retryable(code) {
                    tracing::warn!(model = m.id, "retryable failure: {msg}");
                    st.health.record_failure(&m.id, true);
                    err.last_error = format!("{}: {msg}", m.id);
                    err.attempts += 1;
                    continue;
                }
                st.health.record_failure(&m.id, false);
                return Ok(wire_error(
                    wire,
                    code,
                    "upstream_error",
                    format!("{}: {msg}", m.id),
                ));
            }
            Err(e) => {
                let msg = e.to_string();
                record_failure(
                    st,
                    rt,
                    m,
                    caller,
                    profile,
                    pool,
                    raw_model,
                    "connect_error",
                    None,
                    &msg,
                    t0.elapsed().as_millis() as u64,
                );
                tracing::warn!(model = m.id, "connect error: {msg}");
                st.health.record_failure(&m.id, true);
                err.last_error = format!("{}: {msg}", m.id);
                err.attempts += 1;
            }
        }
    }
    Err(err)
}

/// 402-style budget refusal with a machine-readable breakdown
/// (routing.md §6 guard 1).
fn budget_error(d: &budget::Denial, profile: &str, requested: &str, wire: Wire) -> Response {
    let message = format!(
        "on-demand budget exceeded: {layer} {window} cap ${cap:.2} (spent ${spent:.4}, this request est ${est:.4}); cheapest candidates were tried first",
        layer = d.layer,
        window = d.window,
        cap = d.cap,
        spent = d.spent,
        est = d.est,
    );
    if wire == Wire::Anthropic {
        let body = serde_json::json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": message,
                "budget": d.to_json(),
                "profile": profile,
                "requested": requested,
            }
        });
        return (StatusCode::PAYMENT_REQUIRED, Json(body)).into_response();
    }
    let body = serde_json::json!({
        "error": {
            "type": "budget_exceeded",
            "message": message,
            "budget": d.to_json(),
            "profile": profile,
            "requested": requested,
        }
    });
    (StatusCode::PAYMENT_REQUIRED, Json(body)).into_response()
}

fn retryable(code: u16) -> bool {
    matches!(code, 402 | 408 | 429) || code >= 500
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

#[allow(clippy::too_many_arguments)]
fn record_failure(
    st: &AppState,
    rt: &Runtime,
    m: &Model,
    caller: &CallerIdentity,
    profile: &str,
    pool: &str,
    requested: &str,
    status: &str,
    http_status: Option<u16>,
    _msg: &str,
    latency_ms: u64,
) {
    let rec = UsageRecord {
        ts_unix: unix_now(),
        pool: pool.into(),
        profile: profile.into(),
        caller: caller.name.clone(),
        project: caller.project.clone(),
        grade_requested: requested.into(),
        grade_served: rt.registry.ladder.token(m.grade_rank).into(),
        model: m.id.clone(),
        status: status.into(),
        http_status,
        tokens_in: None,
        tokens_out: None,
        cost_usd: 0.0,
        latency_ms,
        ttfb_ms: None,
        degraded: false,
    };
    st.usage.record(rec.clone());
    st.metrics.record(&rec);
}

/// What a successful attempt hands to [`relay_success`]: the raw
/// openai-wire response (streaming), an already-translated byte stream
/// (E7 codex wire, streaming), or a body read ahead of time — either
/// pre-drained by `try_candidates` (F5) or translated from a Codex
/// stream (E7 codex wire, non-streaming) — as (content-type, bytes).
enum Upstream {
    Resp(reqwest::Response),
    Stream(Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>),
    Buffered(String, Bytes),
}

/// Why a codex attempt failed: `fatal` carries a client-facing error
/// response to return unchanged (invalid request — the caller's
/// fault, so no failover); otherwise the message feeds AttemptErr and
/// the next candidate gets a chance.
struct CodexFail {
    last_error: String,
    fatal: Option<Response>,
}

/// One attempt against the Codex subscription backend (E7). Mirrors the
/// openai attempt path: headers/body deadlines (F5), a budget hold
/// (F2/F3) and quota windows harvested from real inference responses;
/// retryable failures fail over, and the login is re-read per request
/// (the Codex CLI owns renewal — arouter never caches it).
#[allow(clippy::too_many_arguments)]
async fn codex_attempt(
    st: &AppState,
    rt: &Runtime,
    m: &Model,
    caller: &CallerIdentity,
    profile: &str,
    pool: &str,
    raw_model: &str,
    req: &crate::grades::GradeRequest,
    is_stream: bool,
    payload: &serde_json::Value,
    est: &quota::RequestEstimate,
    cross_from: Option<&str>,
    wire: Wire,
    t0: Instant,
    reserve: Option<budget::ReserveGuard>,
) -> Result<Response, CodexFail> {
    let fail = |msg: String| CodexFail {
        last_error: msg,
        fatal: None,
    };
    let login = crate::codex::read_login().await.map_err(|e| fail(e.message))?;
    let body = crate::codex::openai_to_codex(payload, &m.upstream_name).map_err(|e| {
        // invalid request is the client's fault: no failover, plain 400
        CodexFail {
            last_error: format!("invalid request: {}", e.message),
            fatal: Some(wire_error(wire, e.status, "invalid_request", e.message)),
        }
    })?;
    let url = crate::codex::responses_url(&m.base_url);
    let request = st
        .http
        .post(&url)
        .header(header::AUTHORIZATION, format!("Bearer {}", login.token))
        .header("chatgpt-account-id", login.account_id.as_str())
        .header("OpenAI-Beta", "responses=experimental")
        .header(header::ACCEPT, "text/event-stream")
        .header("originator", concat!("arouter/", env!("CARGO_PKG_VERSION")))
        .json(&body);
    let prefix = m.id.split('/').next().unwrap_or("").to_string();
    let harvest = rt.quota_harvest.get(&prefix).copied().unwrap_or(true);
    // review F5: bound the wait for response headers, same as the openai wire
    let resp = match tokio::time::timeout(
        Duration::from_secs(rt.cfg.timeouts.headers_secs),
        request.send(),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err(fail(format!("connect error: {e}"))),
        Err(_) => return Err(fail("headers timeout".into())),
    };
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        // E3b (codex flavor): quota headers on failures are ground truth
        // too; a bare 429 without them still pins a synthetic window.
        let now = unix_now();
        let windows = crate::codex::quota_windows(resp.headers(), now);
        if harvest && !windows.is_empty() {
            st.quota.apply(&prefix, windows, now);
        } else if harvest && code == 429 {
            let ra = resp
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(60);
            st.quota.apply(&prefix, vec![quotaeng::harvest_429(ra)], now);
        }
        let body_text = truncate(&resp.text().await.unwrap_or_default(), 300);
        let msg = format!("codex upstream {code}: {body_text}");
        record_failure(
            st,
            rt,
            m,
            caller,
            profile,
            pool,
            raw_model,
            "upstream_error",
            Some(code),
            &msg,
            t0.elapsed().as_millis() as u64,
        );
        // 401/403 = login expired or wrong account: behaves like a 503
        // so the next candidate is tried instead of hard-failing
        let effective = if code == 401 || code == 403 { 503 } else { code };
        if retryable(effective) {
            st.health.record_failure(&m.id, true);
            return Err(fail(msg));
        }
        st.health.record_failure(&m.id, false);
        return Ok(api_error_with(
            code,
            "upstream_error",
            msg,
            &[(hdr::UPSTREAM, m.id.clone())],
        ));
    }
    let degraded = m.grade_rank != req.ceiling;
    let now = unix_now();
    let windows = crate::codex::quota_windows(resp.headers(), now);
    if harvest && !windows.is_empty() {
        st.quota.apply(&prefix, windows, now);
    }
    if !is_stream {
        // upstream always streams: buffer (deadline-bounded, F5),
        // translate, relay a plain chat-completions JSON body. A
        // truncated stream (no terminal event) is an upstream failure —
        // fail over, never an empty 200.
        let idle = Duration::from_secs(rt.cfg.timeouts.idle_secs);
        let bytes = match tokio::time::timeout(idle, resp.bytes()).await {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => {
                let msg = format!("body read failed: {e}");
                record_failure(
                    st,
                    rt,
                    m,
                    caller,
                    profile,
                    pool,
                    raw_model,
                    "upstream_error",
                    Some(200),
                    &msg,
                    t0.elapsed().as_millis() as u64,
                );
                st.health.record_failure(&m.id, true);
                return Err(fail(msg));
            }
            Err(_) => {
                let msg = "codex body stalled past idle_secs".to_string();
                record_failure(
                    st,
                    rt,
                    m,
                    caller,
                    profile,
                    pool,
                    raw_model,
                    "timeout",
                    Some(200),
                    &msg,
                    t0.elapsed().as_millis() as u64,
                );
                st.health.record_failure(&m.id, true);
                return Err(fail(msg));
            }
        };
        let completion =
            crate::codex::parse_buffered_sse(&bytes, &m.upstream_name).map_err(|e| {
                let msg = e.to_string();
                record_failure(
                    st,
                    rt,
                    m,
                    caller,
                    profile,
                    pool,
                    raw_model,
                    "upstream_error",
                    Some(200),
                    &msg,
                    t0.elapsed().as_millis() as u64,
                );
                st.health.record_failure(&m.id, true);
                fail(msg)
            })?;
        st.health.record_success(&m.id);
        return Ok(relay_success(
            st,
            rt,
            m,
            caller,
            profile,
            pool,
            raw_model,
            degraded,
            is_stream,
            Upstream::Buffered("application/json".into(), Bytes::from(completion.to_string())),
            t0,
            est,
            cross_from,
            wire,
            reserve,
        )
        .await);
    }
    st.health.record_success(&m.id);
    let stream = CodexSse::new(
        resp.bytes_stream(),
        crate::codex::SseTranslator::new(&m.upstream_name),
    );
    Ok(relay_success(
        st,
        rt,
        m,
        caller,
        profile,
        pool,
        raw_model,
        degraded,
        is_stream,
        Upstream::Stream(Box::pin(stream)),
        t0,
        est,
        cross_from,
        wire,
        reserve,
    )
    .await)
}

#[allow(clippy::too_many_arguments)]
async fn relay_success(
    st: &AppState,
    rt: &Runtime,
    m: &Model,
    caller: &CallerIdentity,
    profile: &str,
    pool: &str,
    requested: &str,
    degraded: bool,
    is_stream: bool,
    upstream: Upstream,
    t0: Instant,
    est: &quota::RequestEstimate,
    cross_from: Option<&str>,
    wire: Wire,
    reserve: Option<budget::ReserveGuard>,
) -> Response {
    let reg = &rt.registry;
    let served = reg.ladder.token(m.grade_rank);
    let default_ct = if is_stream {
        "text/event-stream"
    } else {
        "application/json"
    };
    let ct = match &upstream {
        Upstream::Resp(r) => r
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .unwrap_or_else(|| default_ct.to_string()),
        Upstream::Stream(_) => "text/event-stream".to_string(),
        Upstream::Buffered(ct, _) => ct.clone(),
    };

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, ct)
        .header(hdr::UPSTREAM, m.id.as_str())
        .header(hdr::POOL, pool)
        .header(hdr::GRADE, served)
        .header(hdr::PROFILE, profile);
    if let Some(from) = cross_from {
        builder = builder.header(hdr::CROSS_POOL, from);
    }
    if !caller.is_anonymous() {
        builder = builder.header(hdr::CALLER, caller.name.as_str());
        if let Some(p) = &caller.project {
            builder = builder.header(hdr::PROJECT, p.as_str());
        }
    }
    if degraded {
        builder = builder.header(hdr::DEGRADED, served);
    }

    // E8: warm-cache stickiness — local-pool successes pin the caller's
    // next same-grade request to this model so its KV cache stays hot.
    if pool == "local" {
        st.sticky.record(&caller.name, m.grade_rank, &m.id);
    }

    let rec_meta = |tokens_in, tokens_out, ttfb_ms, latency_ms| UsageRecord {
        ts_unix: unix_now(),
        pool: pool.into(),
        profile: profile.into(),
        caller: caller.name.clone(),
        project: caller.project.clone(),
        grade_requested: requested.into(),
        grade_served: served.into(),
        model: m.id.clone(),
        status: "ok".into(),
        http_status: Some(200),
        tokens_in,
        tokens_out,
        cost_usd: UsageRecord::cost_for(tokens_in, tokens_out, m.cost_in, m.cost_out),
        latency_ms,
        ttfb_ms,
        degraded,
    };

    if is_stream {
        // E4 spend headers: estimate now — actual usage only arrives at
        // the end of the stream (recorded in the usage log / metrics).
        if pool == "ondemand" {
            builder = builder.header(hdr::COST, format!("{:.6}", quota::est_cost(m, est)));
            if let Some(rem) = budget::remaining_today(&st.usage, &rt.cfg, profile, caller) {
                builder = builder.header(hdr::BUDGET_REMAINING, format!("{:.6}", rem));
            }
        }
        // E7 codex wire: translate upstream Responses-SSE into
        // chat-completions-SSE inside the tap, so usage accounting,
        // health and metrics see the openai wire they already parse.
        let bytes_stream: Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>> =
            match upstream {
                Upstream::Resp(r) => Box::pin(
                    r.bytes_stream()
                        .map_err(|e: reqwest::Error| std::io::Error::other(e.to_string())),
                ),
                Upstream::Stream(s) => s,
                Upstream::Buffered(..) => unreachable!("buffered relay is not streaming"),
            };
        let tap = TapStream::new(
            bytes_stream,
            TapMeta {
                pool: pool.into(),
                profile: profile.into(),
                requested: requested.into(),
                served: served.into(),
                caller: caller.name.clone(),
                project: caller.project.clone(),
                model: m.id.clone(),
                cost_in: m.cost_in,
                cost_out: m.cost_out,
                degraded,
            },
            st.usage.clone(),
            st.metrics.clone(),
            st.health.clone(),
            t0,
            reserve,
        );
        if wire == Wire::Anthropic {
            // re-emit as anthropic SSE; insert() replaces the upstream
            // content-type (which may carry openai-specific parameters)
            let mut resp = builder
                .body(Body::from_stream(AnthropicSse::new(tap)))
                .unwrap();
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("text/event-stream"),
            );
            return resp;
        }
        builder.body(Body::from_stream(tap)).unwrap()
    } else {
        // The body was fully read (and deadline-bounded) by
        // try_candidates — stalls/truncations already failed over there.
        // E7 codex wire arrives here already translated (Upstream::Buffered).
        let bytes = match upstream {
            Upstream::Buffered(_, b) => b,
            _ => unreachable!("non-streaming relay always has a buffered body"),
        };
        let (tin, tout) = extract_usage(&bytes);
        // F2: the actual record replaces the estimate hold first —
        // settle-then-record keeps spend from double-counting
        if let Some(g) = reserve {
            g.settle();
        }
        let rec = rec_meta(tin, tout, None, t0.elapsed().as_millis() as u64);
        if pool == "ondemand" {
            builder = builder.header(hdr::COST, format!("{:.6}", rec.cost_usd));
            if let Some(rem) = budget::remaining_today(&st.usage, &rt.cfg, profile, caller) {
                builder = builder.header(hdr::BUDGET_REMAINING, format!("{:.6}", rem));
            }
        }
        st.usage.record(rec.clone());
        st.metrics.record(&rec);
        if wire == Wire::Anthropic {
            let translated = serde_json::from_slice::<serde_json::Value>(&bytes)
                .map(|v| crate::translate::openai_to_anthropic(&v))
                .map(|v| v.to_string().into_bytes())
                .unwrap_or_else(|_| bytes.to_vec());
            let mut resp = builder.body(Body::from(translated)).unwrap();
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            );
            return resp;
        }
        builder.body(Body::from(bytes)).unwrap()
    }
}

/// Extract `usage` from a non-streaming chat completion body.
fn extract_usage(bytes: &[u8]) -> (Option<u64>, Option<u64>) {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|v| {
            let u = v.get("usage")?;
            Some((
                u.get("prompt_tokens").and_then(|x| x.as_u64()),
                u.get("completion_tokens").and_then(|x| x.as_u64()),
            ))
        })
        .unwrap_or((None, None))
}

async fn list_models(st: AppState, profile: &str, pool: &str, headers: &HeaderMap) -> Response {
    if let Some(resp) = obs_auth(&st, headers) {
        return resp;
    }
    let rt = st.runtime();
    let reg = &rt.registry;
    if profile != "default" && !reg.profiles.contains_key(profile) {
        return api_error(404, "unknown_profile", profile.to_string());
    }
    if !crate::config::POOLS.contains(&pool) {
        return api_error(404, "unknown_pool", pool.to_string());
    }
    let data: Vec<serde_json::Value> = (0..reg.ladder.grades.len())
        .filter(|&rank| reg.has_exact(profile, pool, rank))
        .map(|rank| {
            serde_json::json!({
                "id": reg.ladder.token(rank),
                "object": "model",
                "owned_by": "arouter",
            })
        })
        .collect();
    Json(serde_json::json!({ "object": "list", "data": data })).into_response()
}

/// Client-facing wire: openai chat-completions or anthropic messages.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Wire {
    OpenAi,
    Anthropic,
}

/// api_error that speaks the client's dialect.
fn wire_error(wire: Wire, status: u16, code: &str, message: String) -> Response {
    match wire {
        Wire::OpenAi => api_error(status, code, message),
        Wire::Anthropic => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
            [(header::CONTENT_TYPE, "application/json")],
            crate::translate::anthropic_error(status, code, &message).to_string(),
        )
            .into_response(),
    }
}

fn api_error(status: u16, code: &str, message: String) -> Response {
    api_error_with(status, code, message, &[])
}

fn api_error_with(
    status: u16,
    code: &str,
    message: String,
    extra: &[(header::HeaderName, String)],
) -> Response {
    let body = serde_json::json!({ "error": { "message": message, "type": code } });
    let mut resp = (
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
        Json(body),
    )
        .into_response();
    for (name, value) in extra {
        if let Ok(v) = header::HeaderValue::from_str(value) {
            resp.headers_mut().insert(name, v);
        }
    }
    resp
}

// ─────────────────────────── SSE usage tap ───────────────────────────

pub struct TapMeta {
    pub pool: String,
    pub profile: String,
    pub requested: String,
    pub served: String,
    pub caller: String,
    pub project: Option<String>,
    pub model: String,
    pub cost_in: f64,
    pub cost_out: f64,
    pub degraded: bool,
}

/// Forwards upstream bytes untouched while scanning SSE lines for a final
/// `usage` object; records one UsageRecord (usage log + metrics) when the
/// stream ends.
pub struct TapStream {
    inner: Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>,
    scan: UsageScan,
    meta: TapMeta,
    usage: Arc<UsageLog>,
    metrics: Arc<Metrics>,
    health: Arc<Health>,
    t0: Instant,
    /// Budget hold for this stream (ondemand only) — settled with the
    /// actual usage in `finish`, released by Drop if never finished.
    reserve: Option<budget::ReserveGuard>,
    /// F12: the concurrency gauge counts the STREAM BODY too — the
    /// handler's own guard dies when it returns headers.
    in_flight: Option<metrics::InFlightGuard>,
    first_byte: Option<Instant>,
    recorded: bool,
}

impl TapStream {
    pub fn new(
        inner: impl Stream<Item = std::io::Result<Bytes>> + Send + 'static,
        meta: TapMeta,
        usage: Arc<UsageLog>,
        metrics: Arc<Metrics>,
        health: Arc<Health>,
        t0: Instant,
        reserve: Option<budget::ReserveGuard>,
    ) -> Self {
        let in_flight = metrics.in_flight(); // F12: body counts as in-flight
        Self {
            inner: Box::pin(inner),
            scan: UsageScan::default(),
            meta,
            usage,
            metrics,
            health,
            t0,
            reserve,
            in_flight: Some(in_flight),
            first_byte: None,
            recorded: false,
        }
    }

    /// Exactly-once finalization (review F4). Statuses:
    /// - "ok": clean EOF — health success
    /// - "aborted": downstream hung up mid-stream — the upstream WAS
    ///   serving, so health success, but the attempt is recorded and
    ///   any billable tokens scanned so far still spend
    /// - "upstream_error": the upstream stream broke — health failure
    ///   (retryable), attempt recorded
    fn finish(&mut self, status: &str) {
        // F2: actual usage replaces the estimate hold exactly once
        if let Some(g) = self.reserve.take() {
            g.settle();
        }
        if self.recorded {
            return;
        }
        self.recorded = true;
        match status {
            "ok" | "aborted" => self.health.record_success_latency(
                &self.meta.model,
                Some(self.t0.elapsed().as_millis() as u64),
            ),
            "upstream_error" => self.health.record_failure(&self.meta.model, true),
            _ => {}
        }
        let (tin, tout) = self.scan.usage();
        let rec = UsageRecord {
            ts_unix: unix_now(),
            pool: self.meta.pool.clone(),
            profile: self.meta.profile.clone(),
            caller: self.meta.caller.clone(),
            project: self.meta.project.clone(),
            grade_requested: self.meta.requested.clone(),
            grade_served: self.meta.served.clone(),
            model: self.meta.model.clone(),
            status: status.into(),
            http_status: Some(200),
            tokens_in: tin,
            tokens_out: tout,
            cost_usd: UsageRecord::cost_for(tin, tout, self.meta.cost_in, self.meta.cost_out),
            latency_ms: self.t0.elapsed().as_millis() as u64,
            ttfb_ms: self
                .first_byte
                .map(|t| t.duration_since(self.t0).as_millis() as u64),
            degraded: self.meta.degraded,
        };
        self.usage.record(rec.clone());
        self.metrics.record(&rec);
    }
}

impl Stream for TapStream {
    type Item = std::io::Result<Bytes>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(cx) {
            std::task::Poll::Ready(Some(Ok(bytes))) => {
                if self.first_byte.is_none() {
                    self.first_byte = Some(Instant::now());
                }
                self.scan.feed(&bytes);
                std::task::Poll::Ready(Some(Ok(bytes)))
            }
            std::task::Poll::Ready(Some(Err(e))) => {
                self.finish("upstream_error");
                std::task::Poll::Ready(Some(Err(e)))
            }
            std::task::Poll::Ready(None) => {
                self.finish("ok");
                std::task::Poll::Ready(None)
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// Client cancellation is finalization too (review F4): when hyper
/// drops the response body, the attempt must still be recorded —
/// exactly once (the `recorded` flag makes the explicit paths no-ops).
impl Drop for TapStream {
    fn drop(&mut self) {
        self.finish("aborted");
        // F12: stream body over — release the in-flight gauge slot
        drop(self.in_flight.take());
    }
}

/// Wraps a stream of upstream openai-SSE bytes and re-emits anthropic
/// SSE frames (E7). Sits *outside* the TapStream so usage accounting,
/// health and metrics still see the openai wire they already parse.
struct AnthropicSse<S> {
    inner: S,
    tr: crate::translate::SseTranslator,
    tail: Option<Bytes>,
}

impl<S> AnthropicSse<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            tr: crate::translate::SseTranslator::new(),
            tail: None,
        }
    }
}

impl<S> Stream for AnthropicSse<S>
where
    S: Stream<Item = std::io::Result<Bytes>> + Unpin,
{
    type Item = std::io::Result<Bytes>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = &mut *self;
        if let Some(t) = this.tail.take() {
            return std::task::Poll::Ready(Some(Ok(t)));
        }
        match Pin::new(&mut this.inner).poll_next(cx) {
            std::task::Poll::Ready(Some(Ok(bytes))) => {
                let frames = this.tr.push_bytes(&bytes);
                if frames.is_empty() {
                    // no complete anthropic event yet: poll again
                    cx.waker().wake_by_ref();
                    return std::task::Poll::Pending;
                }
                std::task::Poll::Ready(Some(Ok(Bytes::from(frames.concat()))))
            }
            std::task::Poll::Ready(Some(Err(e))) => std::task::Poll::Ready(Some(Err(e))),
            std::task::Poll::Ready(None) => {
                let frames = this.tr.finish();
                if frames.is_empty() {
                    std::task::Poll::Ready(None)
                } else {
                    this.tail = None;
                    std::task::Poll::Ready(Some(Ok(Bytes::from(frames.concat()))))
                }
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// E7: wraps a Codex Responses-SSE upstream and re-emits
/// chat-completions-SSE. Sits inside the TapStream so usage
/// accounting, health and metrics see the openai wire they already
/// parse; upstream SSE errors and a missing terminal event surface as
/// stream errors (F4), never as a silent empty success.
struct CodexSse<S> {
    inner: S,
    tr: crate::codex::SseTranslator,
    out: std::collections::VecDeque<Bytes>,
}

impl<S> CodexSse<S>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    fn new(inner: S, tr: crate::codex::SseTranslator) -> Self {
        Self {
            inner,
            tr,
            out: Default::default(),
        }
    }
}

impl<S> Stream for CodexSse<S>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    type Item = std::io::Result<Bytes>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = &mut *self;
        loop {
            if let Some(b) = this.out.pop_front() {
                return std::task::Poll::Ready(Some(Ok(b)));
            }
            match Pin::new(&mut this.inner).poll_next(cx) {
                std::task::Poll::Ready(Some(Ok(bytes))) => {
                    this.tr.push_bytes(&bytes);
                    while let Some(frame) = this.tr.pop_frame() {
                        this.out.push_back(Bytes::from(frame));
                    }
                    continue;
                }
                std::task::Poll::Ready(Some(Err(e))) => {
                    this.out.clear();
                    return std::task::Poll::Ready(Some(Err(std::io::Error::other(
                        e.to_string(),
                    ))));
                }
                std::task::Poll::Ready(None) => {
                    return match this.tr.finish() {
                        Ok(()) => std::task::Poll::Ready(None),
                        Err(e) => {
                            this.out.clear();
                            std::task::Poll::Ready(Some(Err(std::io::Error::other(e))))
                        }
                    }
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}

/// Incremental SSE scanner: keeps the last `usage` seen in `data:` payloads.
#[derive(Default)]
pub struct UsageScan {
    buf: Vec<u8>,
    tokens_in: Option<u64>,
    tokens_out: Option<u64>,
}

impl UsageScan {
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=nl).collect();
            line.pop(); // strip \n
            self.handle_line(&line);
        }
        // Cap: never let a malformed stream grow the buffer unbounded.
        if self.buf.len() > 1_048_576 {
            self.buf.clear();
        }
    }

    fn handle_line(&mut self, line: &[u8]) {
        let line = match line.strip_suffix(b"\r") {
            Some(l) => l,
            None => line,
        };
        let Some(payload) = line.strip_prefix(b"data:") else {
            return;
        };
        let payload = payload.strip_prefix(b" ").unwrap_or(payload);
        if payload == b"[DONE]" {
            return;
        }
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(payload) {
            if let Some(u) = v.get("usage") {
                if let Some(t) = u.get("prompt_tokens").and_then(|x| x.as_u64()) {
                    self.tokens_in = Some(t);
                }
                if let Some(t) = u.get("completion_tokens").and_then(|x| x.as_u64()) {
                    self.tokens_out = Some(t);
                }
            }
        }
    }

    pub fn usage(&self) -> (Option<u64>, Option<u64>) {
        (self.tokens_in, self.tokens_out)
    }
}

/// Apply a model's outgoing role map (`[[models]].role_map`, e.g.
/// `developer` → `system` for upstreams that reject the newer role) to a
/// chat-completions payload in place. Unmapped roles and non-object
/// messages pass through untouched.
fn apply_role_map(payload: &mut serde_json::Value, map: &BTreeMap<String, String>) {
    if map.is_empty() {
        return;
    }
    let Some(msgs) = payload.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    for msg in msgs.iter_mut() {
        let from = match msg.get("role").and_then(|r| r.as_str()) {
            Some(r) => r.to_string(),
            None => continue,
        };
        if let Some(to) = map.get(&from) {
            msg["role"] = serde_json::Value::String(to.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::pin_mut;
    use std::task::Poll;

    #[test]
    fn role_map_rewrites_matching_roles_only() {
        let mut map = BTreeMap::new();
        map.insert("developer".to_string(), "system".to_string());
        let mut payload = serde_json::json!({
            "messages": [
                {"role": "developer", "content": "sys"},
                {"role": "user", "content": "hi"},
                {"content": "no role"}
            ]
        });
        apply_role_map(&mut payload, &map);
        assert_eq!(payload["messages"][0]["role"], "system");
        assert_eq!(payload["messages"][1]["role"], "user");
        assert!(payload["messages"][2].get("role").is_none());
    }

    #[test]
    fn role_map_empty_is_noop() {
        let mut payload = serde_json::json!({"messages": [{"role": "developer"}]});
        let before = payload.clone();
        apply_role_map(&mut payload, &BTreeMap::new());
        assert_eq!(payload, before);
    }

    /// Controllable upstream stream for lifecycle tests: yields nothing
    /// until the test says so (review: barriers, not sleeps).
    struct HeldStream {
        _guard: (),
    }
    impl Stream for HeldStream {
        type Item = std::io::Result<Bytes>;
        fn poll_next(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            std::task::Poll::Pending // never resolves: simulates a live stream
        }
    }

    fn tap_meta() -> TapMeta {
        TapMeta {
            pool: "quota".into(),
            profile: "default".into(),
            requested: "A".into(),
            served: "A".into(),
            caller: "t".into(),
            project: None,
            model: "p/m".into(),
            cost_in: 0.0,
            cost_out: 0.0,
            degraded: false,
        }
    }

    #[test]
    fn dropping_a_pending_stream_records_the_abort() {
        // review F4 contract: client cancellation must record the attempt
        let usage = Arc::new(UsageLog::in_memory());
        let metrics = Arc::new(Metrics::new());
        let health = crate::health::Health::new(Default::default());
        {
            let tap = TapStream::new(
                HeldStream { _guard: () },
                tap_meta(),
                usage.clone(),
                metrics.clone(),
                health.clone(),
                Instant::now(),
                None,
            );
            pin_mut!(tap);
            assert!(matches!(
                tap.as_mut().poll_next(&mut std::task::Context::from_waker(
                    futures::task::noop_waker_ref()
                )),
                Poll::Pending
            ));
            // drop happens here — the client hung up mid-stream
        }
        let recs = usage.records();
        assert_eq!(recs.len(), 1, "abort recorded");
        assert_eq!(recs[0].status, "aborted");
    }

    #[tokio::test]
    async fn upstream_stream_error_is_failure_not_success() {
        // review F4 contract: a broken upstream stream must NOT count as
        // health success
        let usage = Arc::new(UsageLog::in_memory());
        let metrics = Arc::new(Metrics::new());
        let health = crate::health::Health::new(Default::default());
        // a REAL reqwest error (connection refused) — constructing one
        // synthetically isn't public API
        let err = reqwest::get("http://127.0.0.1:1/x")
            .await
            .expect_err("nothing listens on port 1");
        let broken = futures::stream::iter(vec![Err::<Bytes, std::io::Error>(std::io::Error::other(
            err.to_string(),
        ))]);
        {
            let tap = TapStream::new(
                broken,
                tap_meta(),
                usage.clone(),
                metrics.clone(),
                health.clone(),
                Instant::now(),
                None,
            );
            pin_mut!(tap);
            let out = tap.as_mut().poll_next(&mut std::task::Context::from_waker(
                futures::task::noop_waker_ref(),
            ));
            assert!(matches!(&out, Poll::Ready(Some(Err(_)))));
            // scope end drops the TapStream itself — the Err arm already
            // finished, so Drop must not double-record
        }
        let recs = usage.records();
        assert_eq!(recs.len(), 1, "exactly once");
        assert_eq!(recs[0].status, "upstream_error");
        let snap = health.snapshot();
        assert!(
            snap.get("p/m").is_some_and(|s| s.fail_ratio_1h > 0.0),
            "health failure recorded"
        );
    }

    fn feed_chunks(scan: &mut UsageScan, chunks: &[&str]) -> (Option<u64>, Option<u64>) {
        for c in chunks {
            scan.feed(c.as_bytes());
        }
        scan.usage()
    }

    #[test]
    fn scan_extracts_usage_from_final_chunk() {
        let mut s = UsageScan::default();
        let (tin, tout) = feed_chunks(
            &mut s,
            &[
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2}}\n",
                "data: [DONE]\n\n",
            ],
        );
        assert_eq!((tin, tout), (Some(7), Some(2)));
    }

    #[test]
    fn scan_handles_split_boundaries() {
        let mut s = UsageScan::default();
        // one logical line split across three network chunks
        let (tin, tout) = feed_chunks(
            &mut s,
            &[
                "data: {\"usage\":{\"prompt",
                "_tokens\":42,\"completion",
                "_tokens\":9}}\n",
            ],
        );
        assert_eq!((tin, tout), (Some(42), Some(9)));
    }

    #[test]
    fn scan_ignores_non_data_and_done() {
        let mut s = UsageScan::default();
        let (tin, tout) = feed_chunks(
            &mut s,
            &[
                ": keep-alive\n",
                "event: message\n",
                "data: [DONE]\n",
                "data: {\"choices\":[]}\n",
            ],
        );
        assert_eq!((tin, tout), (None, None));
    }

    #[test]
    fn scan_no_usage_in_stream() {
        let mut s = UsageScan::default();
        let (tin, tout) = feed_chunks(
            &mut s,
            &["data: {\"choices\":[{\"delta\":{}}]}\ndata: [DONE]\n"],
        );
        assert_eq!((tin, tout), (None, None));
    }

    #[test]
    fn extract_usage_non_stream() {
        let body = r#"{"id":"1","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":4}}"#;
        assert_eq!(extract_usage(body.as_bytes()), (Some(10), Some(4)));
        assert_eq!(extract_usage(b"{}"), (None, None));
        assert_eq!(extract_usage(b"not json"), (None, None));
    }

    #[test]
    fn retryable_codes() {
        assert!(retryable(429));
        assert!(retryable(500));
        assert!(retryable(503));
        assert!(retryable(402)); // payment required → try next candidate
        assert!(retryable(408));
        assert!(!retryable(400));
        assert!(!retryable(401));
        assert!(!retryable(404));
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hello…");
        // multibyte boundary safety
        let t = "açãoaçãoação";
        let out = truncate(t, 5);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn bearer_extraction() {
        let mut h = HeaderMap::new();
        assert!(bearer_of(&h).is_none());
        h.insert(header::AUTHORIZATION, "Bearer tok-1".parse().unwrap());
        assert_eq!(bearer_of(&h).as_deref(), Some("tok-1"));
        h.insert(header::AUTHORIZATION, "bearer tok-2".parse().unwrap());
        assert_eq!(bearer_of(&h).as_deref(), Some("tok-2"));
        h.insert(header::AUTHORIZATION, "Basic abc".parse().unwrap());
        assert!(bearer_of(&h).is_none());
        h.insert(req_hdr::API_KEY, "tok-3".parse().unwrap());
        assert_eq!(bearer_of(&h).as_deref(), Some("tok-3"));
    }
}
