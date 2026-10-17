//! Failover walk: one upstream attempt per candidate with budget holds,
//! health recording, deadline-bounded body reads, and success relay.
//! Adapter-specific attempts live in [`crate::codex`] / [`crate::gemini`].

use super::routes::{wire_error, Wire};
use super::sse::AnthropicSse;
use super::tap::{extract_usage, TapMeta, TapStream};
use super::{hdr, AppState, Runtime};
use crate::budget;
use crate::codex::codex_attempt;
use crate::config::{CallerIdentity, WireKind};
use crate::gemini::gemini_attempt;
use crate::quota;
use crate::quotaeng;
use crate::registry::Model;
use crate::usage::{unix_now, UsageRecord};
use axum::body::{Body, Bytes};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::{Stream, TryStreamExt};
use std::collections::BTreeMap;
use std::pin::Pin;
use std::time::{Duration, Instant};

pub(crate) struct AttemptErr {
    pub(crate) last_error: String,
    /// Last budget denial, if any (only reported as 402 when it was the
    /// reason every attempt was skipped).
    pub(crate) denial: Option<budget::Denial>,
    pub(crate) attempts: usize,
    pub(crate) denials: usize,
}

/// Walk an ordered candidate list: budget preflight (ondemand), then one
/// upstream attempt per candidate with health recording and failover.
/// `pool` is the *serving* pool (differs from the request pool during
/// cross-pool fallback; `cross_from` names the origin then).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn try_candidates(
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
    let max_attempts = rt.cfg.timeouts.max_attempts.max(1);
    let mut upstream_attempts = 0usize;
    // Role rewrites are per-model (an upstream may reject a role a
    // sibling accepts), so restore the caller's original `messages`
    // before applying each candidate's map — a failed attempt with a
    // mapped payload must not leak into the next candidate.
    let original_messages = payload.get("messages").cloned();
    for m in candidates {
        if upstream_attempts >= max_attempts {
            break;
        }
        // Quota can change while this precomputed candidate list is being
        // walked (for example after an upstream wallet-credit 402).
        if !quota::model_available(&st.quota.current(), &rt.provider_map, m) {
            continue;
        }
        // F13: per-candidate serving pool — mixed-pool member-form groups
        // need each member's own pool for the budget guards, headers and
        // labels below. Identical to the request pool everywhere else.
        let pool = if m.pools.iter().any(|p| p.as_str() == pool) {
            pool
        } else {
            m.pools.first().map(|s| s.as_str()).unwrap_or(pool)
        };
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
        if m.wire == WireKind::Codex {
            upstream_attempts += 1;
            match codex_attempt(
                st, rt, m, caller, profile, pool, raw_model, req, is_stream, payload, est,
                cross_from, wire, t0, reserve,
            )
            .await
            {
                Ok(r) => return Ok(r),
                Err(e) => {
                    if let Some(resp) = e.fatal {
                        return Ok(*resp);
                    }
                    tracing::warn!(
                        model = m.id,
                        requested = %raw_model,
                        pool,
                        profile,
                        caller = %caller.name,
                        detail = %e.last_error,
                        "Codex attempt failed; trying next candidate"
                    );
                    err.last_error = format!("{}: {}", m.id, e.last_error);
                    err.attempts += 1;
                    continue;
                }
            }
        }
        // Native Gemini wire — translate to Google's generateContent
        // endpoint (x-goog-api-key auth), translate the response back.
        if m.wire == WireKind::Gemini {
            upstream_attempts += 1;
            match gemini_attempt(
                st, rt, m, caller, profile, pool, raw_model, req, is_stream, payload, est,
                cross_from, wire, t0, reserve,
            )
            .await
            {
                Ok(r) => return Ok(r),
                Err(e) => {
                    if let Some(resp) = e.fatal {
                        return Ok(*resp);
                    }
                    tracing::warn!(
                        model = m.id,
                        requested = %raw_model,
                        pool,
                        profile,
                        caller = %caller.name,
                        detail = %e.last_error,
                        "Gemini attempt failed; trying next candidate"
                    );
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
        let quota_provider = rt
            .provider_map
            .explicit
            .get(&prefix)
            .cloned()
            .unwrap_or_else(|| prefix.clone());
        let harvest = rt.quota_harvest.get(&prefix).copied().unwrap_or(true);
        upstream_attempts += 1;
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
                tracing::warn!(
                    model = m.id,
                    requested = %raw_model,
                    pool,
                    profile,
                    caller = %caller.name,
                    "headers deadline exceeded"
                );
                st.health.record_failure(&m.id, true);
                err.last_error = format!("{}: headers timeout", m.id);
                err.attempts += 1;
                continue;
            }
        };
        match resp {
            Ok(resp) if resp.status().is_success() => {
                if prefix == "or" {
                    st.quota
                        .clear_openrouter_wallet(&quota_provider, unix_now());
                }
                let degraded = rt.registry.grade_for_request(raw_model, m).1 != req.ceiling
                    && !req.whole_ladder;
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
                            tracing::warn!(
                                model = m.id,
                                requested = %raw_model,
                                pool,
                                profile,
                                caller = %caller.name,
                                "{msg}"
                            );
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
                            tracing::warn!(
                                model = m.id,
                                requested = %raw_model,
                                pool,
                                profile,
                                caller = %caller.name,
                                "{msg}"
                            );
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
                let body = resp.text().await.unwrap_or_default();
                let wallet_credit_error = code == 402 && is_openrouter_wallet_credit_error(&body);
                let body_text = truncate(&body, 300);
                let msg = format!("upstream {code}: {body_text}");
                if wallet_credit_error {
                    st.quota
                        .block_openrouter_wallet(&quota_provider, unix_now(), 300);
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
                    st.health.record_failure(&m.id, false);
                    tracing::warn!(model = m.id, provider = %prefix, "OpenRouter account credits exhausted; suppressing provider for 300s");
                    err.last_error = format!("{}: {msg}", m.id);
                    err.attempts += 1;
                    continue;
                }
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
                st.health.record_failure(&m.id, retryable(code));
                if failover(code) {
                    tracing::warn!(
                        model = m.id,
                        requested = %raw_model,
                        pool,
                        profile,
                        caller = %caller.name,
                        http_status = code,
                        detail = %msg,
                        permanent = !retryable(code),
                        "failing over after upstream error"
                    );
                    err.last_error = format!("{}: {msg}", m.id);
                    err.attempts += 1;
                    continue;
                }
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
                tracing::warn!(
                    model = m.id,
                    requested = %raw_model,
                    pool,
                    profile,
                    caller = %caller.name,
                    detail = %msg,
                    "connect error"
                );
                st.health.record_failure(&m.id, true);
                err.last_error = format!("{}: {msg}", m.id);
                err.attempts += 1;
            }
        }
    }
    Err(err)
}

/// 402-style budget refusal with a machine-readable breakdown
/// (docs/routing.md §6 guard 1).
pub(crate) fn budget_error(
    d: &budget::Denial,
    profile: &str,
    requested: &str,
    wire: Wire,
) -> Response {
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

pub(crate) fn is_openrouter_wallet_credit_error(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/metadata/limit_source")
                .and_then(|x| x.as_str())
                .map(str::to_owned)
        })
        .is_some_and(|source| source == "openrouter_credits")
}

pub(crate) fn retryable(code: u16) -> bool {
    matches!(code, 402 | 408 | 429) || code >= 500
}

/// Should the router move on to the next candidate? Transient faults get
/// an exponential cooldown on top; a 404 is permanent *for that model*
/// (route or model absent upstream — e.g. an OpenRouter `:batch` variant
/// asked for on chat/completions), so it fails over without cooling down
/// and still counts toward the circuit breaker.
pub(crate) fn failover(code: u16) -> bool {
    retryable(code) || code == 404
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
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
pub(crate) fn record_failure(
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
        grade_served: rt.registry.grade_for_request(requested, m).0.into(),
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
pub(crate) enum Upstream {
    Resp(reqwest::Response),
    Stream(Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>),
    Buffered(String, Bytes),
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn relay_success(
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
    let served = reg.grade_for_request(requested, m).0;
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
            st.tokens.clone(),
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
        // E10b: token daily budgets accrue settled spend (failures cost
        // nothing and are not charged).
        st.tokens.charge_by_caller(&caller.name, rec.cost_usd);
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

/// Apply a model's outgoing role map (`[[models]].role_map`, e.g.
/// `developer` → `system` for upstreams that reject the newer role) to a
/// chat-completions payload in place. Unmapped roles and non-object
/// messages pass through untouched.
pub(crate) fn apply_role_map(payload: &mut serde_json::Value, map: &BTreeMap<String, String>) {
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
        // 404 fails over without a cooldown (permanent, model-specific)
        assert!(failover(404));
        assert!(failover(429));
        assert!(!failover(400));
        assert!(!failover(401));
    }

    #[test]
    fn identifies_only_openrouter_wallet_credit_402s() {
        assert!(is_openrouter_wallet_credit_error(
            r#"{"error":{"message":"insufficient","code":402,"metadata":{"limit_source":"openrouter_credits"}}}"#
        ));
        assert!(!is_openrouter_wallet_credit_error(
            r#"{"error":{"metadata":{"limit_source":"key_limit"}}}"#
        ));
        assert!(!is_openrouter_wallet_credit_error("not json"));
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
}
