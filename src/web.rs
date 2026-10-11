//! E10a — web sessions and the claim bootstrap.
//!
//! The review's order is explicit: *"sessions/claim first, then token
//! scope enforcement, then model/provider/quota management"*. This module
//! is that first step, and deliberately touches nothing on the proxy hot
//! path: the web surface is a separate route group on the same listener.
//!
//! - **Claim**: at startup the daemon mints one single-use token in
//!   memory and prints `web: claim http://<bind>/claim/<token>` under the
//!   banner. Only a loopback peer can consume it, and only once; consuming
//!   it mints an admin session. No default password ships with the binary.
//! - **Sessions**: opaque 32-byte ids, in memory only (a restart drops
//!   them and rotates the claim), carried in an `HttpOnly;
//!   SameSite=Strict` cookie, TTL from `[web].session_ttl_hours`.
//! - **CSRF**: mutating calls need an authenticated session *and* a
//!   same-origin request (`Origin`/`Referer` host must match `Host`, or
//!   `Sec-Fetch-Site: same-origin`). Plain-HTTP LAN deployments still get
//!   a real baseline because the browser, not the transport, is the check.
//! - **Assets**: embedded, served from `/` and `/app.js` with a
//!   `default-src 'self'` CSP — no CDN, no inline script.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::Duration;

use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::config::WebConfig;
use crate::proxy::AppState;
use crate::quota::unix_now;
use serde::Deserialize;

const COOKIE: &str = "arouter_sid";
const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const STYLE_CSS: &str = include_str!("../web/style.css");
const CSP: &str = "default-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

#[derive(Debug, Clone)]
pub struct Session {
    pub role: String,
    pub expires_unix: u64,
}

struct Inner {
    sessions: HashMap<String, Session>,
    /// single-use claim token; `None` once consumed
    claim: Option<String>,
}

/// Session + claim state. In-memory by design: durable web state belongs
/// to the managed overlay (E10c), not to an auth token store.
pub struct Web {
    inner: Mutex<Inner>,
    ttl: Duration,
    enabled: bool,
}

impl Web {
    /// Mint a fresh claim token. Nothing is persisted, so every start
    /// rotates the link — an old link is dead the moment the daemon is.
    pub fn new(cfg: &WebConfig) -> Self {
        Web {
            inner: Mutex::new(Inner {
                sessions: HashMap::new(),
                claim: Some(random_token()),
            }),
            ttl: Duration::from_secs(cfg.session_ttl_hours.max(1) * 3600),
            enabled: cfg.enabled,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn ttl_secs(&self) -> u64 {
        self.ttl.as_secs()
    }

    /// The unconsumed claim token — for the startup banner.
    pub fn claim_token(&self) -> Option<String> {
        self.inner.lock().unwrap().claim.clone()
    }

    /// Consume the claim: exact token, loopback peer, once. Returns the
    /// new session id on success.
    pub fn consume_claim(&self, token: &str, peer: Option<IpAddr>, now: u64) -> Option<String> {
        if !peer.is_some_and(|p| p.is_loopback()) {
            return None;
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.claim.as_deref() != Some(token) {
            return None;
        }
        inner.claim = None;
        Some(mint(&mut inner, "admin", now, self.ttl))
    }

    /// Mint a session directly — test harness and E10b token scope use.
    pub fn mint_session(&self, role: &str, now: u64) -> String {
        let mut inner = self.inner.lock().unwrap();
        mint(&mut inner, role, now, self.ttl)
    }

    /// Validate a session id. Expired entries are dropped on read.
    pub fn session(&self, sid: &str, now: u64) -> Option<Session> {
        let mut inner = self.inner.lock().unwrap();
        match inner.sessions.get(sid) {
            Some(s) if s.expires_unix > now => Some(s.clone()),
            Some(_) => {
                inner.sessions.remove(sid);
                None
            }
            None => None,
        }
    }

    pub fn logout(&self, sid: &str) {
        self.inner.lock().unwrap().sessions.remove(sid);
    }
}

fn mint(inner: &mut Inner, role: &str, now: u64, ttl: Duration) -> String {
    let sid = random_token();
    inner.sessions.insert(
        sid.clone(),
        Session {
            role: role.to_string(),
            expires_unix: now + ttl.as_secs(),
        },
    );
    sid
}

/// Where the claim link points. The token is only consumable from
/// loopback, so a wildcard bind must not advertise `0.0.0.0` — the
/// operator needs a URL they can actually open.
pub fn claim_url(bind: &str, token: &str) -> String {
    let host = match bind.rsplit_once(':') {
        Some((h, _)) if h == "0.0.0.0" || h == "[::]" || h == "::" => "127.0.0.1",
        _ => bind.rsplit_once(':').map(|(h, _)| h).unwrap_or(bind),
    };
    let port = bind.rsplit_once(':').map(|(_, p)| p).unwrap_or("3929");
    format!("http://{host}:{port}/claim/{token}")
}

fn random_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).expect("system randomness unavailable");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Session id from `Cookie:` if present.
fn cookie_sid(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k.trim() == COOKIE).then(|| v.trim().to_string())
    })
}

/// Origin check (review: "Origin/CSRF rejection").
///
/// Browsers always send `Sec-Fetch-Site`; `cross-site`/`same-site` are
/// rejected outright. `same-origin` passes. Otherwise fall back to
/// `Origin`/`Referer` host equality — and when neither header exists
/// (curl, typed navigation, same-origin GET with no referrer policy) the
/// caller decides via `bare_ok`: true for the claim landing page, false
/// for every mutating call.
fn origin_ok(headers: &HeaderMap, host: Option<&str>, bare_ok: bool) -> bool {
    match headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        Some("cross-site" | "same-site") => return false,
        Some("same-origin") => return true,
        _ => {}
    }
    let src = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .or_else(|| headers.get(header::REFERER).and_then(|v| v.to_str().ok()));
    match src {
        Some(s) => {
            let src_host = s
                .split("://")
                .nth(1)
                .unwrap_or(s)
                .split('/')
                .next()
                .unwrap_or("");
            host.is_some_and(|h| h == src_host)
        }
        None => bare_ok,
    }
}

fn not_found() -> Response {
    api_error(
        StatusCode::NOT_FOUND,
        "web_disabled",
        "the web interface is disabled ([web].enabled = false)",
    )
}

fn api_error(status: StatusCode, kind: &str, message: &str) -> Response {
    // header parts first, body form last: only the final element may be
    // the `IntoResponse` body
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({"error": {"message": message, "type": kind}})),
    )
        .into_response()
}

fn unauthenticated() -> Response {
    api_error(
        StatusCode::UNAUTHORIZED,
        "unauthenticated",
        "claim the daemon from loopback to start a session",
    )
}

fn bad_origin() -> Response {
    api_error(
        StatusCode::FORBIDDEN,
        "bad_origin",
        "cross-origin writes are rejected",
    )
}

fn asset(content_type: &'static str, csp: &'static str, body: &'static str) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type),
            (header::CONTENT_SECURITY_POLICY, csp),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        body,
    )
        .into_response()
}

// ─────────────────────────── handlers ───────────────────────────

/// `GET /` — the shell. Static and unauthenticated: it holds no data.
pub async fn index(State(st): State<AppState>) -> Response {
    if !st.web.enabled() {
        return not_found();
    }
    asset("text/html; charset=utf-8", CSP, INDEX_HTML)
}

/// `GET /app.js` — tiny, dependency-free front end.
pub async fn app_js(State(st): State<AppState>) -> Response {
    if !st.web.enabled() {
        return not_found();
    }
    asset(
        "text/javascript; charset=utf-8",
        "default-src 'none'",
        APP_JS,
    )
}

/// `GET /style.css` — the shell's only stylesheet.
pub async fn style_css(State(st): State<AppState>) -> Response {
    if !st.web.enabled() {
        return not_found();
    }
    asset("text/css; charset=utf-8", "default-src 'none'", STYLE_CSS)
}

/// `GET /claim/{token}` — consume the single-use link from loopback and
/// start the admin session.
pub async fn claim(
    State(st): State<AppState>,
    Path(token): Path<String>,
    req: Request,
) -> Response {
    if !st.web.enabled() {
        return not_found();
    }
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    // a typed navigation has no Origin/Referer — that is the legitimate
    // path here, so bare requests are allowed; cross-site fetches are not
    if !origin_ok(req.headers(), host.as_deref(), true) {
        return bad_origin();
    }
    match st.web.consume_claim(&token, peer, unix_now()) {
        Some(sid) => {
            let cookie = format!(
                "{COOKIE}={sid}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
                st.web.ttl_secs()
            );
            (
                StatusCode::SEE_OTHER,
                [
                    (header::SET_COOKIE, cookie),
                    (header::LOCATION, "/".to_string()),
                    (header::CACHE_CONTROL, "no-store".to_string()),
                ],
            )
                .into_response()
        }
        None => api_error(
            StatusCode::FORBIDDEN,
            "claim_rejected",
            "the claim link is single-use, loopback-only, and must match this daemon's current token",
        ),
    }
}

/// `GET /api/session` — who am I? Doubles as the shell's auth probe.
pub async fn api_session(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if !st.web.enabled() {
        return not_found();
    }
    match cookie_sid(&headers).and_then(|sid| st.web.session(&sid, unix_now())) {
        Some(s) => Json(serde_json::json!({
            "authenticated": true,
            "role": s.role,
            "expires_unix": s.expires_unix,
        }))
        .into_response(),
        None => unauthenticated(),
    }
}

/// `GET /api/overview` — the authenticated read the shell renders. Proves
/// the read path is gated too, not just writes.
pub async fn api_overview(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if !st.web.enabled() {
        return not_found();
    }
    if cookie_sid(&headers)
        .and_then(|sid| st.web.session(&sid, unix_now()))
        .is_none()
    {
        return unauthenticated();
    }
    let rt = st.runtime();
    Json(serde_json::json!({
        "models": rt.registry.models.len(),
        "profiles": rt.registry.profiles.keys().cloned().collect::<Vec<_>>(),
        "callers": rt.callers.len(),
        "auth_required": rt.cfg.auth.require,
        "uptime_secs": st.started.elapsed().as_secs(),
        "sink_degraded": st
            .usage
            .snapshot()
            .get("sink_degraded")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }))
    .into_response()
}

/// `POST /api/logout` — drop the session and clear the cookie.
pub async fn api_logout(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if !st.web.enabled() {
        return not_found();
    }
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if !origin_ok(&headers, host.as_deref(), false) {
        return bad_origin();
    }
    let now = unix_now();
    let Some(sid) = cookie_sid(&headers) else {
        return unauthenticated();
    };
    if st.web.session(&sid, now).is_none() {
        return unauthenticated();
    }
    st.web.logout(&sid);
    (
        StatusCode::OK,
        [
            (
                header::SET_COOKIE,
                format!("{COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        Json(serde_json::json!({"ok": true})),
    )
        .into_response()
}

/// Session gate shared by the admin token endpoints. `csrf` — writes
/// (POST) must prove same-origin; reads (GET) only need the session,
/// matching the E10a read endpoints.
fn admin_gate(st: &AppState, headers: &HeaderMap, csrf: bool) -> Result<(), Box<Response>> {
    if !st.web.enabled() {
        return Err(Box::new(not_found()));
    }
    if csrf {
        let host = headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        if !origin_ok(headers, host.as_deref(), false) {
            return Err(Box::new(bad_origin()));
        }
    }
    let now = crate::quota::unix_now();
    let Some(sid) = cookie_sid(headers) else {
        return Err(Box::new(unauthenticated()));
    };
    if st.web.session(&sid, now).is_none() {
        return Err(Box::new(unauthenticated()));
    }
    Ok(())
}

/// `GET /api/tokens` — list token metadata (never the hash).
pub async fn api_tokens(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = admin_gate(&st, &headers, false) {
        return *resp;
    }
    Json(serde_json::json!({ "tokens": st.tokens.list() })).into_response()
}

#[derive(Deserialize)]
struct CreateTokenBody {
    name: String,
    #[serde(default)]
    groups: Option<Vec<String>>,
    #[serde(default)]
    pools: Option<Vec<String>>,
    #[serde(default)]
    grades: Option<Vec<String>>,
    #[serde(default)]
    daily_budget_usd: Option<f64>,
}

/// `POST /api/tokens` — mint a scoped token. The plaintext is returned
/// exactly once; only its sha-256 hash is stored.
pub async fn api_tokens_create(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Err(resp) = admin_gate(&st, &headers, true) {
        return *resp;
    }
    let parsed: CreateTokenBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return bad_request(format!("invalid json: {e}")),
    };
    // validate scopes against the live runtime so typos can't mint a
    // token that silently matches nothing
    let rt = st.runtime();
    if let Some(pools) = &parsed.pools {
        for p in pools {
            if !crate::config::POOLS.contains(&p.as_str()) {
                return bad_request(format!(
                    "unknown pool {p:?} (valid: {:?})",
                    crate::config::POOLS
                ));
            }
        }
    }
    if let Some(grades) = &parsed.grades {
        for g in grades {
            if !rt.registry.ladder.list().contains(g) {
                return bad_request(format!(
                    "unknown grade {g:?} (ladder: {})",
                    rt.registry.ladder.list()
                ));
            }
        }
    }
    if let Some(groups) = &parsed.groups {
        for g in groups {
            // globs are allowed; at least one group must be able to match
            if !rt
                .registry
                .groups
                .iter()
                .any(|grp| crate::glob::glob_match(g, &grp.id))
            {
                let known: Vec<&str> = rt.registry.groups.iter().map(|x| x.id.as_str()).collect();
                return bad_request(format!("no routing group matches {g:?} (known: {known:?})"));
            }
        }
    }
    if let Some(b) = parsed.daily_budget_usd {
        if !(0.0..=1_000_000.0).contains(&b) {
            return bad_request("daily_budget_usd must be in [0, 1000000]".into());
        }
    }
    let scopes = crate::tokens::Scopes {
        groups: parsed.groups,
        pools: parsed.pools,
        grades: parsed.grades,
        daily_budget_usd: parsed.daily_budget_usd,
    };
    let created_by = "admin-session";
    match st.tokens.create(&parsed.name, scopes, created_by) {
        Ok((listed, plaintext)) => (
            StatusCode::CREATED,
            [(header::CACHE_CONTROL, "no-store".to_string())],
            Json(serde_json::json!({
                "token": plaintext,
                "note": "store this now — it is not shown again",
                "created": listed,
            })),
        )
            .into_response(),
        Err(e) => bad_request(e.to_string()),
    }
}

#[derive(Deserialize)]
struct RevokeBody {
    id: String,
}

/// `POST /api/tokens/revoke` — revoke by id; authentication fails from
/// the next request on.
pub async fn api_tokens_revoke(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Err(resp) = admin_gate(&st, &headers, true) {
        return *resp;
    }
    let parsed: RevokeBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return bad_request(format!("invalid json: {e}")),
    };
    if st.tokens.revoke(&parsed.id) {
        Json(serde_json::json!({"ok": true})).into_response()
    } else {
        api_error(
            StatusCode::NOT_FOUND,
            "unknown_token",
            &format!("no token with id {:?}", parsed.id),
        )
    }
}

fn bad_request(msg: String) -> Response {
    api_error(StatusCode::BAD_REQUEST, "invalid_request", &msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::HeaderValue;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn claim_is_single_use_and_loopback_only() {
        let web = Web::new(&WebConfig::default());
        let tok = web.claim_token().expect("fresh claim");
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        let far: IpAddr = "192.168.50.9".parse().unwrap();
        let now = 1_000;

        // remote peers and wrong tokens never consume it
        assert!(web.consume_claim(&tok, Some(far), now).is_none());
        assert!(web.consume_claim("deadbeef", Some(loopback), now).is_none());
        assert!(web.consume_claim(&tok, None, now).is_none());
        assert_eq!(web.claim_token().as_deref(), Some(tok.as_str()));

        // loopback consumes it exactly once…
        let sid = web
            .consume_claim(&tok, Some(loopback), now)
            .expect("claimed");
        assert_eq!(web.session(&sid, now).unwrap().role, "admin");
        // …and the link is dead afterwards, even from loopback
        assert!(web.claim_token().is_none());
        assert!(web.consume_claim(&tok, Some(loopback), now).is_none());
    }

    #[test]
    fn sessions_expire_and_revoke() {
        let web = Web::new(&WebConfig {
            enabled: true,
            session_ttl_hours: 1,
        });
        let sid = web.mint_session("admin", 1_000);
        assert!(web.session(&sid, 1_000 + 3599).is_some());
        assert!(web.session(&sid, 1_000 + 3601).is_none(), "expired");
        // expiry evicted the entry
        assert!(web.session(&sid, 1_000).is_none());

        let sid2 = web.mint_session("admin", 1_000);
        web.logout(&sid2);
        assert!(web.session(&sid2, 1_000).is_none(), "revoked");
    }

    #[test]
    fn cookies_parse_and_ignore_lookalikes() {
        let h = headers(&[("cookie", "theme=dark; arouter_sid=abc123; other=1")]);
        assert_eq!(cookie_sid(&h).as_deref(), Some("abc123"));
        let h = headers(&[("cookie", "not_arouter_sid=nope")]);
        assert_eq!(cookie_sid(&h), None);
        assert_eq!(cookie_sid(&headers(&[])), None);
    }

    #[test]
    fn origin_rules_reject_cross_site_and_check_host() {
        // browser-verified: cross-site and same-site never pass
        assert!(!origin_ok(
            &headers(&[("sec-fetch-site", "cross-site")]),
            Some("127.0.0.1:3929"),
            true
        ));
        assert!(!origin_ok(
            &headers(&[("sec-fetch-site", "same-site")]),
            Some("127.0.0.1:3929"),
            false
        ));
        assert!(origin_ok(
            &headers(&[("sec-fetch-site", "same-origin")]),
            None,
            false
        ));
        // typed navigation: no origin, bare allowed only where asked
        assert!(origin_ok(&headers(&[]), Some("127.0.0.1:3929"), true));
        assert!(!origin_ok(&headers(&[]), Some("127.0.0.1:3929"), false));
        // origin/referer must match the Host we are serving
        assert!(origin_ok(
            &headers(&[("origin", "http://127.0.0.1:3929")]),
            Some("127.0.0.1:3929"),
            false
        ));
        assert!(!origin_ok(
            &headers(&[("origin", "http://evil.example")]),
            Some("127.0.0.1:3929"),
            false
        ));
        assert!(origin_ok(
            &headers(&[("referer", "http://box:3929/settings")]),
            Some("box:3929"),
            false
        ));
        // a mismatch between Origin host and Host is a rejection even if
        // the request looks otherwise fine
        assert!(!origin_ok(
            &headers(&[("origin", "http://box:3929")]),
            Some("other:3929"),
            false
        ));
    }

    #[test]
    fn claim_url_is_openable_not_a_wildcard() {
        assert_eq!(
            claim_url("0.0.0.0:3929", "abc"),
            "http://127.0.0.1:3929/claim/abc"
        );
        assert_eq!(
            claim_url("[::]:3929", "abc"),
            "http://127.0.0.1:3929/claim/abc"
        );
        assert_eq!(
            claim_url("127.0.0.1:3929", "abc"),
            "http://127.0.0.1:3929/claim/abc"
        );
        assert_eq!(
            claim_url("192.168.50.192:4000", "abc"),
            "http://192.168.50.192:4000/claim/abc"
        );
    }

    #[test]
    fn claim_tokens_are_unguessable() {
        let a = Web::new(&WebConfig::default());
        let b = Web::new(&WebConfig::default());
        let (ta, tb) = (a.claim_token().unwrap(), b.claim_token().unwrap());
        assert_ne!(ta, tb);
        assert_eq!(ta.len(), 64, "32 random bytes, hex");
        assert!(ta.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
