//! Axum routes, thin handlers, observation endpoints, caller resolution,
//! request validation, and the candidate-set builder.

use super::attempt::{budget_error, try_candidates, AttemptErr};
use super::{req_hdr, AppState, Runtime, METRICS_CONTENT_TYPE};
use crate::config::{CallerIdentity, CallerMap};
use crate::metrics;
use crate::quota;
use crate::registry::{Group, Model};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use std::time::Instant;

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
        // E10a web shell — static assets, claim bootstrap, session API
        .route("/", get(crate::web::index))
        .route("/app.js", get(crate::web::app_js))
        .route("/style.css", get(crate::web::style_css))
        .route("/claim/{token}", get(crate::web::claim))
        .route("/api/session", get(crate::web::api_session))
        .route("/api/overview", get(crate::web::api_overview))
        .route("/api/logout", post(crate::web::api_logout))
        .route(
            "/api/tokens",
            get(crate::web::api_tokens).post(crate::web::api_tokens_create),
        )
        .route("/api/tokens/revoke", post(crate::web::api_tokens_revoke))
        // E10c overlay management (session-gated; writes same-origin)
        .route("/api/models", get(crate::web::api_models))
        .route("/api/models/state", post(crate::web::api_models_state))
        .route("/api/models/add", post(crate::web::api_models_add))
        .route("/api/models/remove", post(crate::web::api_models_remove))
        .route("/api/quota/override", post(crate::web::api_quota_override))
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
    match resolve_caller(&rt.callers, &st.tokens, headers) {
        Ok(_) => None,
        Err(CallerErr::Unauthorized(msg)) => Some(api_error(401, "unauthorized", msg)),
        Err(CallerErr::BudgetExhausted { retry_after_secs }) => Some(api_error_with(
            429,
            "arouter_token_budget",
            "token daily budget exhausted".into(),
            &[(header::RETRY_AFTER, retry_after_secs.to_string())],
        )),
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
            "overlay".into(),
            serde_json::json!({
                "file": crate::overlay::path_for(&st.overlay_dir).display().to_string(),
                "disabled": rt.overlay.disabled,
                "errors": rt.overlay.errors,
            }),
        );
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
pub(crate) fn bearer_of(headers: &HeaderMap) -> Option<String> {
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

/// Why a request's caller resolution failed.
#[derive(Debug)]
pub enum CallerErr {
    /// 401 — missing/unknown credentials.
    Unauthorized(String),
    /// 429 — the token's daily budget is spent; retry at UTC midnight.
    BudgetExhausted { retry_after_secs: u64 },
}

/// Resolve the caller identity for a request (see docs/architecture.md
/// §Callers): `ark_…` tokens (E10b, dynamic store) take precedence over
/// static `[[callers]]` keys; open mode falls back to
/// `X-Arouter-User`/`X-Arouter-Project` headers, else anonymous.
/// Token requests also yield the scopes that narrow candidate
/// selection (intersected BEFORE the grade sort, like pools).
fn resolve_caller(
    callers: &CallerMap,
    tokens: &crate::tokens::TokenStore,
    headers: &HeaderMap,
) -> Result<(CallerIdentity, Option<crate::tokens::Scopes>), CallerErr> {
    let bearer = bearer_of(headers);
    if let Some(tok) = bearer
        .as_deref()
        .filter(|t| t.starts_with(crate::tokens::PREFIX))
    {
        return match tokens.authenticate(tok) {
            Ok(auth) => Ok((
                CallerIdentity {
                    name: format!("{}{}", crate::tokens::CALLER_PREFIX, auth.name),
                    project: None,
                },
                Some(auth.scopes),
            )),
            Err(crate::tokens::AuthError::BudgetExhausted) => Err(CallerErr::BudgetExhausted {
                retry_after_secs: crate::tokens::secs_to_utc_midnight(crate::tokens::unix_now()),
            }),
            Err(crate::tokens::AuthError::Unknown) => Err(CallerErr::Unauthorized(
                "unknown or revoked api token".into(),
            )),
        };
    }
    callers
        .resolve(
            bearer.as_deref(),
            hdr_str(headers, &req_hdr::USER),
            hdr_str(headers, &req_hdr::PROJECT),
        )
        .map(|c| (c, None))
        .map_err(CallerErr::Unauthorized)
}

/// Everything a candidate-set build needs that is fixed for the request.
struct CandidateQuery<'a> {
    profile: &'a str,
    /// The pool the request originally landed in; group member lists are
    /// attempted there, fallback pools use the fallback list.
    origin_pool: &'a str,
    group: Option<(&'a Group, crate::grades::GradeRequest)>,
    req: &'a crate::grades::GradeRequest,
    direct_model: Option<&'a Model>,
    scopes: Option<&'a crate::tokens::Scopes>,
    required: crate::capabilities::Capabilities,
    est: &'a quota::RequestEstimate,
}

/// Ordered + pre-quota candidate lists for one pool. `pre` keeps every
/// health-ok candidate (even quota-exhausted) so a 429 can quote a
/// reset-aware Retry-After; `ordered` is the failover walk.
struct CandidateSet<'a> {
    ordered: Vec<&'a Model>,
    pre: Vec<&'a Model>,
}

impl<'a> CandidateSet<'a> {
    /// Apply the full filter chain — scopes → health → context →
    /// capabilities → quota — then policy ordering and local residency
    /// boosts. Every exclusion reason is logged when the result is empty.
    fn build(
        st: &AppState,
        rt: &'a Runtime,
        caller: &CallerIdentity,
        q: &CandidateQuery<'a>,
        pool: &str,
        qstate: &quota::QuotaState,
    ) -> Self {
        let reg = &rt.registry;
        let mut considered = match &q.group {
            Some((g, greq)) if pool == q.origin_pool => reg.group_attempt_list(q.profile, g, greq),
            Some((g, greq)) => reg.group_fallback_list(q.profile, g, greq, pool),
            None => reg.attempt_list(q.profile, pool, q.req),
        };
        if let Some(target) = q.direct_model {
            considered.retain(|m| m.id == target.id);
        }
        // Capability filtering applies to the automatic routing path only.
        // An explicit exact-id pin is "use THIS model": forward and let the
        // upstream produce its natural error if the request doesn't fit.
        let enforce_caps = q.direct_model.is_none();
        let supports_req =
            |m: &Model| !enforce_caps || crate::capabilities::supports(m.capabilities, q.required);
        // E10b: token scopes intersect the candidate set BEFORE any
        // ordering (grade sort included) — an out-of-scope model is
        // dropped, never deprioritized, mirroring the pool boundary.
        let scope_ok = |m: &Model| -> bool {
            let Some(sc) = q.scopes else {
                return true;
            };
            if let Some(pools) = &sc.pools {
                if !m
                    .pools
                    .iter()
                    .any(|p| pools.iter().any(|s| s == p.as_str()))
                {
                    return false;
                }
            }
            if let Some(grades) = &sc.grades {
                let g = reg.ladder.token(m.grade_rank);
                if !grades.iter().any(|a| a == g) {
                    return false;
                }
            }
            true
        };
        let effective_ranks: std::collections::BTreeMap<&str, usize> = reg
            .models
            .iter()
            .enumerate()
            .filter_map(|(i, m)| match &q.group {
                Some((g, _)) => reg.group_rank(g, i).map(|rank| (m.id.as_str(), rank)),
                None => Some((m.id.as_str(), m.grade_rank)),
            })
            .collect();
        let rank = |m: &Model| {
            effective_ranks
                .get(m.id.as_str())
                .copied()
                .unwrap_or(usize::MAX)
        };
        let scope_excluded: Vec<String> = considered
            .iter()
            .filter(|m| !scope_ok(m))
            .map(|m| m.id.clone())
            .collect();
        let scoped: Vec<&Model> = considered.into_iter().filter(|m| scope_ok(m)).collect();
        let considered_count = scope_excluded.len() + scoped.len();
        let health_excluded: Vec<String> = scoped
            .iter()
            .filter(|m| st.health.is_excluded(&m.id))
            .map(|m| m.id.clone())
            .collect();
        let healthy: Vec<&Model> = scoped
            .into_iter()
            .filter(|m| !st.health.is_excluded(&m.id))
            .collect();
        let context_excluded: Vec<String> = healthy
            .iter()
            .filter(|m| !quota::context_fits(m, q.est))
            .map(|m| m.id.clone())
            .collect();
        let capability_excluded: Vec<String> = healthy
            .iter()
            .filter(|m| !supports_req(m))
            .map(|m| m.id.clone())
            .collect();
        let pre: Vec<&Model> = healthy
            .into_iter()
            .filter(|m| supports_req(m))
            .filter(|m| quota::context_fits(m, q.est))
            .collect();
        // F11: preference resolved ONCE per request — profile overlay
        // beats pool default; every recognized value maps to an ordering.
        let mode = match reg.preference_for(q.profile, pool) {
            crate::config::Preference::Cheapest if pool == "ondemand" => quota::OrderMode::Cheapest,
            crate::config::Preference::Fastest => quota::OrderMode::Fastest,
            _ => quota::OrderMode::QuotaPool,
        };
        let ordered_by_policy = quota::order_candidates_by_rank(
            qstate,
            &st.health,
            &rt.provider_map,
            mode,
            q.est,
            pre.clone(),
            rank,
        );
        let quota_excluded: Vec<String> = ordered_by_policy
            .iter()
            .filter(|m| pool != "local" && !quota::model_available(qstate, &rt.provider_map, m))
            .map(|m| m.id.clone())
            .collect();
        let ordered: Vec<&Model> = ordered_by_policy
            .into_iter()
            // Local models do not consume provider subscription quota. A
            // local engine may share its provider prefix with a quota model;
            // an exhausted subscription window must not block local fallback.
            .filter(|m| pool == "local" || quota::model_available(qstate, &rt.provider_map, m))
            .collect();
        // E8: residency boost (loaded local models float within their
        // grade), then warm-cache stickiness for the local pool.
        let mut ordered =
            crate::localeng::boost_residency_by_rank(ordered, &st.local.current(), rank);
        if pool == "local" {
            ordered =
                crate::localeng::boost_sticky_by_rank(ordered, &st.sticky, &caller.name, rank);
        }
        if ordered.is_empty() {
            let pool_models: Vec<String> = reg
                .models
                .iter()
                .filter(|m| m.pools.iter().any(|p| p.as_str() == pool))
                .map(|m| format!("{}@{}", m.id, reg.ladder.token(m.grade_rank)))
                .collect();
            tracing::info!(
                candidate_pool = pool,
                caller = %caller.name,
                considered = considered_count,
                pool_models = ?pool_models,
                scope_excluded = ?scope_excluded,
                health_excluded = ?health_excluded,
                context_excluded = ?context_excluded,
                quota_excluded = ?quota_excluded,
                capability_excluded = ?capability_excluded,
                required_capabilities = ?q.required.names(),
                "no eligible candidates in pool"
            );
        }
        CandidateSet { ordered, pre }
    }
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
    let (caller, tok_scopes) = match resolve_caller(&rt.callers, &st.tokens, headers) {
        Ok(c) => c,
        Err(CallerErr::Unauthorized(msg)) => return wire_error(wire, 401, "unauthorized", msg),
        Err(CallerErr::BudgetExhausted { retry_after_secs }) => {
            let mut r = wire_error(
                wire,
                429,
                "arouter_token_budget",
                "token daily budget exhausted (resets at UTC midnight)".into(),
            );
            if let Ok(v) = header::HeaderValue::from_str(&retry_after_secs.to_string()) {
                r.headers_mut().insert(header::RETRY_AFTER, v);
            }
            return r;
        }
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
    // F13 routing groups (`[[groups]]`): `model: "arouter-subscription-A+"`
    // pins a grade inside the group; the bare id `"arouter-subscription"`
    // delegates to the whole ladder. Group ids may contain `-`, so match
    // by exact id first, then `{id}-{grade}`. No group match → the legacy
    // surface: a plain grade token on the path pool.
    let group_sel: Option<(&crate::registry::Group, crate::grades::GradeRequest)> = {
        let mut found = None;
        for g in &reg.groups {
            if raw_model == g.id {
                found = Some((g, g.ladder.whole_ladder_request()));
                break;
            }
            if let Some(gp) = raw_model.strip_prefix(&format!("{}-", g.id)) {
                match g.ladder.parse_request(gp) {
                    Some(r) => found = Some((g, r)),
                    None => {
                        return wire_error(
                            wire,
                            400,
                            "unknown_grade",
                            format!(
                                "model {raw_model:?}: {gp:?} is not a grade of group {} ({})",
                                g.id,
                                g.ladder.list()
                            ),
                        )
                    }
                }
                break;
            }
        }
        found
    };
    // Exact upstream ids are also accepted as a model selector. This is
    // useful for callers that explicitly want a provider such as
    // `codex/gpt-6-astra`; it still goes through the same pool, profile,
    // quota, health, context, and budget gates as grade/group routing.
    let direct_model = if group_sel.is_none() {
        reg.models.iter().find(|m| m.id == raw_model)
    } else {
        None
    };
    let pool: &str = match &group_sel {
        Some((g, _)) => g.pool.unwrap_or(crate::config::Pool::Quota).as_str(),
        None => pool,
    };
    let req = match &group_sel {
        Some((_, greq)) => *greq,
        None if direct_model.is_some() => crate::grades::GradeRequest {
            ceiling: direct_model.unwrap().grade_rank,
            floor: Some(direct_model.unwrap().grade_rank),
            whole_ladder: false,
        },
        None => match reg.ladder.parse_request(&raw_model) {
            Some(r) => r,
            None => {
                return wire_error(
                    wire,
                    400,
                    "unknown_grade",
                    format!(
                        "model {raw_model:?}: not a routing group ({}) or grade ({})",
                        reg.groups
                            .iter()
                            .map(|g| g.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        reg.ladder.list()
                    ),
                )
            }
        },
    };
    let is_stream = payload
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // E10b: a token's group scope applies to the CLIENT-FACING surface —
    // the requested id (group or group-grade). Scoped tokens must address
    // a group; out-of-scope requests never reach candidate selection, so
    // out-of-scope upstreams are never contacted.
    if let Some(groups) = tok_scopes.as_ref().and_then(|s| s.groups.as_ref()) {
        if !groups
            .iter()
            .any(|p| crate::glob::glob_match(p, &raw_model))
        {
            return wire_error(
                wire,
                403,
                "arouter_scope",
                format!(
                    "token {} is not scoped to model {raw_model:?} (allows: {:?})",
                    caller.name, groups
                ),
            );
        }
    }

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
    // (docs/routing.md §1/§3). `pre` keeps every health-ok candidate (even
    // quota-exhausted) so the 429 can quote a reset-aware Retry-After.
    let est = quota::estimate_request(&payload);
    // Capability-aware routing: an image prompt routes only to multimodal
    // models, a reasoning_effort only to reasoning models (capabilities
    // resolved in the registry from config → table → name heuristic).
    let required = crate::capabilities::required(&payload);
    let qstate = st.quota.current();
    let query = CandidateQuery {
        profile,
        origin_pool: pool,
        group: group_sel,
        req: &req,
        direct_model,
        scopes: tok_scopes.as_ref(),
        required,
        est: &est,
    };
    let CandidateSet {
        ordered: candidates,
        pre,
    } = CandidateSet::build(&st, &rt, &caller, &query, pool, &qstate);
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

    // Cross-pool fallback: groups carry their own configured fallback
    // list (F13); the legacy grade surface stays explicit opt-in only
    // (docs/routing.md §2) — the guard against surprise charges. Targets
    // still pass their own budget preflight inside try_candidates.
    let cross: Vec<crate::config::Pool> = match &group_sel {
        Some((g, _)) => g.fallback.clone(),
        None => reg
            .profiles
            .get(profile)
            .map(|p| p.fallback.cross_pool.clone())
            .unwrap_or_else(|| reg.base_fallback.cross_pool.clone())
            .unwrap_or_default()
            .iter()
            .filter_map(|p| crate::config::Pool::parse(p))
            .collect(),
    };
    let fallback_targets = cross.clone();
    for target in cross {
        if target.as_str() == pool {
            continue;
        }
        let CandidateSet {
            ordered: tcands,
            pre: tpre,
        } = CandidateSet::build(&st, &rt, &caller, &query, target.as_str(), &qstate);
        if tcands.is_empty() {
            tracing::info!(
                from = pool,
                to = %target,
                profile,
                caller = %caller.name,
                requested = %raw_model,
                considered = tpre.len(),
                "cross-pool fallback has no eligible candidates"
            );
            continue;
        }
        tracing::info!(
            from = pool,
            to = %target,
            profile,
            caller = %caller.name,
            requested = %raw_model,
            candidates = ?tcands.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            "cross-pool fallback"
        );
        match try_candidates(
            &st,
            &rt,
            &caller,
            profile,
            target.as_str(),
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

    // Budget denial outranks generic exhaustion (E4): the caller asked
    // for a chargeable pool and the guard said no.
    if let Some(e) = &final_err {
        if let Some(d) = &e.denial {
            if e.denials == e.attempts {
                return budget_error(d, profile, &raw_model, wire);
            }
        }
    }
    st.metrics.record_exhausted(
        pool,
        profile,
        &caller.name,
        caller.project.as_deref(),
        &raw_model,
    );
    let retry = quota::nearest_retry_unix(&qstate, &rt.provider_map, &pre)
        .map(|t| t.saturating_sub(quota::unix_now()).max(1))
        .filter(|s| *s > 0);
    let mut extra: Vec<(header::HeaderName, String)> = vec![];
    if let Some(secs) = retry {
        extra.push((header::RETRY_AFTER, secs.to_string()));
    }
    let detail = final_err.map(|e| e.last_error).unwrap_or_else(|| {
        if required.is_empty() {
            "no candidates".into()
        } else {
            format!(
                "no candidates support required capabilities: {}",
                required.names().join(", ")
            )
        }
    });
    tracing::warn!(
        requested = %raw_model,
        pool,
        profile,
        caller = %caller.name,
        fallback_targets = ?fallback_targets,
        candidates_before_quota = pre.len(),
        required_capabilities = ?required.names(),
        detail = %detail,
        "request exhausted"
    );
    if wire == Wire::Anthropic {
        let mut resp = wire_error(
            wire,
            429,
            "arouter_exhausted",
            format!(
                "{} {raw_model} in pool {pool} (profile {profile}) exhausted: {detail}",
                if group_sel.is_some() {
                    "group"
                } else {
                    "grade"
                }
            ),
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
        format!(
            "{} {raw_model} in pool {pool} (profile {profile}) exhausted: {detail}",
            if group_sel.is_some() {
                "group"
            } else {
                "grade"
            }
        ),
        &extra,
    )
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
    // F13: `/v1/models` lists one entry per routing group × grade with
    // models visible to the profile (`arouter-subscription-A+`, …). The
    // bare group ids and legacy grade tokens stay accepted on requests
    // but are no longer advertised.
    let data: Vec<serde_json::Value> = reg
        .groups
        .iter()
        .flat_map(|g| {
            let gid = g.id.clone();
            reg.group_grades(profile, g)
                .into_iter()
                .map(move |(tok, _)| {
                    serde_json::json!({
                        "id": format!("{gid}-{tok}"),
                        "object": "model",
                        "owned_by": "arouter",
                    })
                })
        })
        .collect();
    Json(serde_json::json!({ "object": "list", "data": data })).into_response()
}

/// Client-facing wire: openai chat-completions or anthropic messages.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wire {
    OpenAi,
    Anthropic,
}

/// api_error that speaks the client's dialect.
pub(crate) fn wire_error(wire: Wire, status: u16, code: &str, message: String) -> Response {
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

pub(crate) fn api_error_with(
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

#[cfg(test)]
mod tests {
    use super::*;

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
