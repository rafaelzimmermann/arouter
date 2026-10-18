//! E10a — web sessions and the first-access admin bootstrap.
//!
//! The review's order is explicit: *"sessions first, then token
//! scope enforcement, then model/provider/quota management"*. This module
//! is that first step, and deliberately touches nothing on the proxy hot
//! path: the web surface is a separate route group on the same listener.
//!
//! - **Bootstrap**: while no admin exists, the first visitor may create
//!   the admin username/password. Credentials are stored as an Argon2id
//!   hash in `<state_dir>/admin.toml` (0600); no default password ships
//!   with the binary. Once configured, setup is permanently closed.
//! - **Login**: username/password over same-origin POSTs. Wrong
//!   credentials are indistinguishable, and failed attempts are throttled
//!   per peer in memory.
//! - **Sessions**: opaque 32-byte ids, in memory only (a restart drops
//!   them), carried in an `HttpOnly; SameSite=Strict` cookie, TTL from
//!   `[web].session_ttl_hours`.
//! - **CSRF**: mutating calls need an authenticated session *and* a
//!   same-origin request (`Origin`/`Referer` host must match `Host`, or
//!   `Sec-Fetch-Site: same-origin`). Plain-HTTP LAN deployments still get
//!   a real baseline because the browser, not the transport, is the check.
//! - **Assets**: embedded, served from `/` and `/app.js` with a
//!   `default-src 'self'` CSP — no CDN, no inline script.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::admin::{self, AdminRecord, AdminStore};
use crate::config::WebConfig;
use crate::proxy::AppState;
use crate::quota::unix_now;
use serde::Deserialize;

const COOKIE: &str = "arouter_sid";
const INDEX_HTML: &str = include_str!("../../web/index.html");
const APP_JS: &str = include_str!("../../web/app.js");
const STYLE_CSS: &str = include_str!("../../web/style.css");
const CSP: &str = "default-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

const LOGIN_MAX_FAILURES: u32 = 5;
const LOGIN_WINDOW_SECS: u64 = 5 * 60;
const LOGIN_MAX_TRACKED: usize = 4096;
const MAX_LOGIN_BODY: usize = 8 * 1024;

#[derive(Debug, Clone)]
pub struct Session {
    pub role: String,
    pub username: String,
    pub expires_unix: u64,
}

#[derive(Debug, Clone, Copy)]
struct FailureWindow {
    first_unix: u64,
    count: u32,
    blocked_until: u64,
}

struct Inner {
    sessions: HashMap<String, Session>,
    /// `None` = no admin yet. A load error leaves `admin = None` and
    /// `auth_error = Some(..)`, which keeps setup closed.
    admin: Option<AdminRecord>,
    auth_error: Option<String>,
    /// failed-login windows, bounded and pruned on every attempt
    failures: HashMap<Option<IpAddr>, FailureWindow>,
}

/// Session + durable admin-credential state.
pub struct Web {
    inner: Mutex<Inner>,
    ttl: Duration,
    enabled: bool,
    store: AdminStore,
}

/// Failure modes shared by setup and login. Handlers map these to HTTP
/// statuses; the variants deliberately do not reveal whether a username
/// exists.
#[derive(Debug, PartialEq, Eq)]
pub enum CredentialError {
    Invalid(String),
    SetupRequired,
    AlreadyConfigured,
    BadCredentials,
    Throttled { retry_after_secs: u64 },
    Unavailable(String),
    Storage(String),
    Internal(String),
}

impl Web {
    /// Load `<state_dir>/admin.toml` once at startup. A corrupt existing
    /// file is fail-closed: no setup, no login until repaired.
    pub fn new(cfg: &WebConfig, state_dir: &Path) -> Self {
        let store = AdminStore::new(state_dir);
        let (admin, auth_error) = match store.load() {
            Ok(admin) => (admin, None),
            Err(e) => {
                tracing::error!(
                    path = %store.path().display(),
                    error = %e,
                    "admin credentials unusable; setup and login are closed"
                );
                (None, Some(e))
            }
        };
        Web {
            inner: Mutex::new(Inner {
                sessions: HashMap::new(),
                admin,
                auth_error,
                failures: HashMap::new(),
            }),
            ttl: Duration::from_secs(cfg.session_ttl_hours.max(1) * 3600),
            enabled: cfg.enabled,
            store,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn ttl_secs(&self) -> u64 {
        self.ttl.as_secs()
    }

    /// No admin and no unreadable credential file: show the setup form.
    pub fn setup_required(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.admin.is_none() && inner.auth_error.is_none()
    }

    /// An existing credential file could not be loaded.
    pub fn setup_locked(&self) -> bool {
        self.inner.lock().unwrap().auth_error.is_some()
    }

    pub fn admin_username(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap()
            .admin
            .as_ref()
            .map(|a| a.username.clone())
    }

    /// Create the one admin account and mint its first session. The
    /// read-check happens before hashing (cheap rejection), then again
    /// under the lock so two simultaneous first visitors cannot both win.
    pub async fn create_admin(
        &self,
        username: &str,
        password: &str,
        now: u64,
    ) -> Result<(String, String), CredentialError> {
        let username = admin::validate_username(username).map_err(CredentialError::Invalid)?;
        admin::validate_password(password).map_err(CredentialError::Invalid)?;

        {
            let inner = self.inner.lock().unwrap();
            if let Some(e) = &inner.auth_error {
                return Err(CredentialError::Unavailable(e.clone()));
            }
            if inner.admin.is_some() {
                return Err(CredentialError::AlreadyConfigured);
            }
        }

        let password = password.to_string();
        let hash = tokio::task::spawn_blocking(move || admin::hash_password(&password))
            .await
            .map_err(|e| CredentialError::Internal(format!("hash task failed ({e})")))?
            .map_err(CredentialError::Storage)?;
        let rec = AdminRecord {
            version: admin::VERSION,
            username: username.clone(),
            password_hash: hash,
            created_unix: now,
            updated_unix: now,
        };

        let mut inner = self.inner.lock().unwrap();
        if let Some(e) = &inner.auth_error {
            return Err(CredentialError::Unavailable(e.clone()));
        }
        if inner.admin.is_some() {
            return Err(CredentialError::AlreadyConfigured);
        }
        self.store.save(&rec).map_err(CredentialError::Storage)?;
        inner.admin = Some(rec);
        inner.failures.clear();
        let sid = mint(&mut inner, "admin", &username, now, self.ttl);
        Ok((sid, username))
    }

    /// Verify username/password and mint a session. A wrong username runs
    /// the same Argon2 verification as a wrong password.
    pub async fn login(
        &self,
        username: &str,
        password: &str,
        peer: Option<IpAddr>,
        now: u64,
    ) -> Result<(String, String), CredentialError> {
        {
            let mut inner = self.inner.lock().unwrap();
            prune_failures(&mut inner, now);
            if let Some(w) = inner.failures.get(&peer) {
                if w.blocked_until > now {
                    return Err(CredentialError::Throttled {
                        retry_after_secs: w.blocked_until - now,
                    });
                }
            }
            if let Some(e) = &inner.auth_error {
                return Err(CredentialError::Unavailable(e.clone()));
            }
        }

        let Some(rec) = self.inner.lock().unwrap().admin.clone() else {
            return Err(CredentialError::SetupRequired);
        };
        let username_in = username.trim().to_string();
        let password_in = password.to_string();
        let hash = rec.password_hash.clone();
        let password_ok =
            tokio::task::spawn_blocking(move || admin::verify_password(&hash, &password_in))
                .await
                .map_err(|e| CredentialError::Internal(format!("verify task failed ({e})")))?;

        if !password_ok || username_in != rec.username {
            let mut inner = self.inner.lock().unwrap();
            record_failure(&mut inner, peer, now);
            return Err(CredentialError::BadCredentials);
        }

        let mut inner = self.inner.lock().unwrap();
        inner.failures.remove(&peer);
        let sid = mint(&mut inner, "admin", &rec.username, now, self.ttl);
        Ok((sid, rec.username))
    }

    /// Mint a session directly — test harness and E10b token scope use.
    pub fn mint_session(&self, role: &str, now: u64) -> String {
        let mut inner = self.inner.lock().unwrap();
        mint(&mut inner, role, role, now, self.ttl)
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

fn mint(inner: &mut Inner, role: &str, username: &str, now: u64, ttl: Duration) -> String {
    let sid = random_token();
    inner.sessions.insert(
        sid.clone(),
        Session {
            role: role.to_string(),
            username: username.to_string(),
            expires_unix: now + ttl.as_secs(),
        },
    );
    sid
}

fn prune_failures(inner: &mut Inner, now: u64) {
    let retention = LOGIN_WINDOW_SECS * 2;
    inner
        .failures
        .retain(|_, w| w.blocked_until > now || now.saturating_sub(w.first_unix) <= retention);
}

fn record_failure(inner: &mut Inner, peer: Option<IpAddr>, now: u64) {
    if !inner.failures.contains_key(&peer) && inner.failures.len() >= LOGIN_MAX_TRACKED {
        inner.failures.retain(|_, w| w.blocked_until > now);
        if inner.failures.len() >= LOGIN_MAX_TRACKED {
            inner.failures.clear();
        }
    }
    let w = inner.failures.entry(peer).or_insert(FailureWindow {
        first_unix: now,
        count: 0,
        blocked_until: 0,
    });
    if now.saturating_sub(w.first_unix) > LOGIN_WINDOW_SECS {
        *w = FailureWindow {
            first_unix: now,
            count: 0,
            blocked_until: 0,
        };
    }
    w.count = w.count.saturating_add(1);
    if w.count >= LOGIN_MAX_FAILURES {
        w.blocked_until = now + LOGIN_WINDOW_SECS;
    }
}

/// Where the web UI lives. A wildcard bind must not advertise `0.0.0.0`
/// — the operator needs a URL they can actually open.
pub fn web_url(bind: &str) -> String {
    if bind == "::" {
        return "http://127.0.0.1:3929/".to_string();
    }
    let (host, port) = match bind.rsplit_once(':') {
        Some((h, p)) => (h, p),
        None => (bind, "3929"),
    };
    let host = match host {
        "0.0.0.0" | "[::]" | "::" => "127.0.0.1",
        other => other,
    };
    format!("http://{host}:{port}/")
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
/// caller decides via `bare_ok` (true only for GET pages, false for
/// every mutating call).
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
        "sign in or create the admin account to start a session",
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

/// `GET /api/setup` — public bootstrap state. `allowed` is true while
/// no admin exists: the first visitor to POST credentials wins.
pub async fn api_setup(State(st): State<AppState>) -> Response {
    if !st.web.enabled() {
        return not_found();
    }
    let required = st.web.setup_required();
    let locked = st.web.setup_locked();
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({
            "required": required,
            "allowed": required,
            "locked": locked,
        })),
    )
        .into_response()
}

#[derive(Deserialize)]
struct CredentialsBody {
    username: String,
    password: String,
}

/// `POST /api/setup` — create the first admin and immediately sign in.
pub async fn api_setup_create(State(st): State<AppState>, req: Request) -> Response {
    if !st.web.enabled() {
        return not_found();
    }
    let headers = req.headers().clone();
    if !same_origin(&headers) {
        return bad_origin();
    }
    let body = match axum::body::to_bytes(req.into_body(), MAX_LOGIN_BODY).await {
        Ok(b) => b,
        Err(_) => return bad_request("request body too large".into()),
    };
    let parsed: CredentialsBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return bad_request(format!("invalid json: {e}")),
    };
    match st
        .web
        .create_admin(&parsed.username, &parsed.password, unix_now())
        .await
    {
        Ok((sid, username)) => session_success(StatusCode::CREATED, &st, &sid, &username),
        Err(e) => credential_error(e),
    }
}

/// `POST /api/login` — username/password sign-in. The peer address is
/// used only for the in-memory throttle.
pub async fn api_login(State(st): State<AppState>, req: Request) -> Response {
    if !st.web.enabled() {
        return not_found();
    }
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());
    let headers = req.headers().clone();
    if !same_origin(&headers) {
        return bad_origin();
    }
    let body = match axum::body::to_bytes(req.into_body(), MAX_LOGIN_BODY).await {
        Ok(b) => b,
        Err(_) => return bad_request("request body too large".into()),
    };
    let parsed: CredentialsBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return bad_request(format!("invalid json: {e}")),
    };
    match st
        .web
        .login(&parsed.username, &parsed.password, peer, unix_now())
        .await
    {
        Ok((sid, username)) => session_success(StatusCode::OK, &st, &sid, &username),
        Err(e) => credential_error(e),
    }
}

fn same_origin(headers: &HeaderMap) -> bool {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    origin_ok(headers, host.as_deref(), false)
}

fn session_success(status: StatusCode, st: &AppState, sid: &str, username: &str) -> Response {
    let cookie = format!(
        "{COOKIE}={sid}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
        st.web.ttl_secs()
    );
    (
        status,
        [
            (header::SET_COOKIE, cookie),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        Json(serde_json::json!({
            "ok": true,
            "role": "admin",
            "username": username,
        })),
    )
        .into_response()
}

fn credential_error(e: CredentialError) -> Response {
    match e {
        CredentialError::Invalid(msg) => bad_request(msg),
        CredentialError::SetupRequired => api_error(
            StatusCode::CONFLICT,
            "setup_required",
            "create the admin account before signing in",
        ),
        CredentialError::AlreadyConfigured => api_error(
            StatusCode::CONFLICT,
            "setup_complete",
            "an admin account already exists; sign in instead",
        ),
        CredentialError::BadCredentials => api_error(
            StatusCode::UNAUTHORIZED,
            "bad_credentials",
            "invalid username or password",
        ),
        CredentialError::Throttled { retry_after_secs } => {
            let mut resp = api_error(
                StatusCode::TOO_MANY_REQUESTS,
                "login_throttled",
                "too many failed sign-in attempts; try again later",
            );
            if let Ok(v) = retry_after_secs.to_string().parse() {
                resp.headers_mut().insert(header::RETRY_AFTER, v);
            }
            resp
        }
        CredentialError::Unavailable(msg) => api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "admin_unavailable",
            &format!("admin credentials are unavailable: {msg}"),
        ),
        CredentialError::Storage(msg) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "admin_write_failed",
            &format!("could not persist admin credentials: {msg}"),
        ),
        CredentialError::Internal(msg) => {
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "admin_internal", &msg)
        }
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
            "username": s.username,
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

// ─────────────────── E10c: overlay management ───────────────────

/// `GET /api/models` — the routed model set + overlay diagnostics +
/// the persisted quota overrides (for the management panel).
pub async fn api_models(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = admin_gate(&st, &headers, false) {
        return *resp;
    }
    let rt = st.runtime();
    let reg = &rt.registry;
    let mut models: Vec<serde_json::Value> = reg
        .models
        .iter()
        .map(|m| {
            serde_json::json!({
                "id": m.id,
                "grade": reg.ladder.token(m.grade_rank),
                "pools": m.pools,
                "weight": m.weight,
                "provenance": m.provenance,
                "wire": m.wire,
                "disabled": false,
            })
        })
        .collect();
    for id in &rt.overlay.disabled {
        models.push(serde_json::json!({
            "id": id,
            "grade": "",
            "pools": [],
            "weight": 0.0,
            "provenance": "hidden",
            "wire": "",
            "disabled": true,
        }));
    }
    let (overlay, load_errors) = crate::overlay::Overlay::load(&st.overlay_dir);
    let now = crate::quota::unix_now();
    let overrides: Vec<serde_json::Value> = overlay
        .quota
        .iter()
        .map(|o| {
            serde_json::json!({
                "provider": o.provider,
                "label": o.label,
                "pct": o.pct,
                "reset_unix": o.reset_unix,
                "set_at": o.set_at,
                "set_by": o.set_by,
                "expires_at": o.expires_at,
                "active": o.expires_at > now,
            })
        })
        .collect();
    let q = st.quota.current();
    let panels: Vec<String> = q.providers.keys().cloned().collect();
    Json(serde_json::json!({
        "models": models,
        "overlay": {
            "disabled": rt.overlay.disabled,
            "errors": rt.overlay.errors,
            "load_errors": load_errors,
        },
        "quota_panels": panels,
        "quota_overrides": overrides,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct ModelStateBody {
    id: String,
    #[serde(default)]
    disabled: Option<bool>,
    #[serde(default)]
    weight: Option<f64>,
}

/// Load the overlay, mutate one entry, prune no-ops, dry-run, save,
/// rebuild. Shared tail of the models write endpoints.
fn overlay_write(st: &AppState, overlay: crate::overlay::Overlay) -> Response {
    // dry-run BEFORE saving: a rejected write leaves the previous
    // snapshot serving (docs/architecture.md)
    let rt = st.runtime();
    let diag = overlay.dry_run(&rt.cfg);
    if !diag.errors.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "overlay_rejected",
            &diag.errors.join("; "),
        );
    }
    if let Err(e) = overlay.save(&st.overlay_dir) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "overlay_write_failed",
            &format!(
                "{}: {e}",
                crate::overlay::path_for(&st.overlay_dir).display()
            ),
        );
    }
    drop(rt);
    if !crate::proxy::rebuild_now(st, "overlay write") {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "overlay_rejected",
            "merged config invalid — previous snapshot still serving",
        );
    }
    let rt = st.runtime();
    Json(serde_json::json!({
        "ok": true,
        "disabled": rt.overlay.disabled,
        "errors": rt.overlay.errors,
    }))
    .into_response()
}

/// `POST /api/models/state` — flip `disabled` / set `weight` for an
/// existing (manual, discovered or overlay) model.
pub async fn api_models_state(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Err(resp) = admin_gate(&st, &headers, true) {
        return *resp;
    }
    let parsed: ModelStateBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return bad_request(format!("invalid json: {e}")),
    };
    if parsed.disabled.is_none() && parsed.weight.is_none() {
        return bad_request("nothing to change: set disabled and/or weight".into());
    }
    if let Some(w) = parsed.weight {
        if !(w.is_finite() && w > 0.0) {
            return bad_request("weight must be > 0".into());
        }
    }
    let rt = st.runtime();
    let known = rt.registry.models.iter().any(|m| m.id == parsed.id)
        || rt.overlay.disabled.contains(&parsed.id);
    if !known {
        return api_error(
            StatusCode::NOT_FOUND,
            "unknown_model",
            &format!(
                "no model {:?} — additions go through /api/models/add",
                parsed.id
            ),
        );
    }
    drop(rt);
    let (mut overlay, _) = crate::overlay::Overlay::load(&st.overlay_dir);
    let entry = match overlay.models.iter_mut().find(|e| e.id == parsed.id) {
        Some(e) => e,
        None => {
            overlay.models.push(crate::overlay::OverlayModel {
                id: parsed.id.clone(),
                ..Default::default()
            });
            overlay.models.last_mut().expect("just pushed")
        }
    };
    if let Some(d) = parsed.disabled {
        entry.disabled = d;
    }
    if let Some(w) = parsed.weight {
        entry.weight = Some(w);
    }
    // prune no-op entries so web.toml stays readable
    if !entry.disabled && entry.weight.is_none() && !entry.structure_fields_set() {
        overlay.models.retain(|e| e.id != parsed.id);
    }
    overlay_write(&st, overlay)
}

#[derive(Deserialize)]
struct ModelAddBody {
    id: String,
    base_url: String,
    grade: String,
    #[serde(default)]
    pool: Option<Vec<String>>,
    #[serde(default)]
    context: Option<u64>,
    #[serde(default)]
    auth: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    cost_in: Option<f64>,
    #[serde(default)]
    cost_out: Option<f64>,
    #[serde(default)]
    weight: Option<f64>,
}

/// `POST /api/models/add` — an overlay ADDITION (id unknown to manual
/// config and discovery); must carry base_url + grade.
pub async fn api_models_add(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Err(resp) = admin_gate(&st, &headers, true) {
        return *resp;
    }
    let parsed: ModelAddBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return bad_request(format!("invalid json: {e}")),
    };
    let rt = st.runtime();
    if rt.registry.models.iter().any(|m| m.id == parsed.id)
        || rt.overlay.disabled.contains(&parsed.id)
    {
        return bad_request(format!(
            "model {:?} already exists — use /api/models/state for disabled/weight",
            parsed.id
        ));
    }
    if let Some(w) = parsed.weight {
        if !(w.is_finite() && w > 0.0) {
            return bad_request("weight must be > 0".into());
        }
    }
    drop(rt);
    let (mut overlay, _) = crate::overlay::Overlay::load(&st.overlay_dir);
    if overlay.models.iter().any(|e| e.id == parsed.id) {
        return bad_request(format!("overlay already has an entry for {:?}", parsed.id));
    }
    overlay.models.push(crate::overlay::OverlayModel {
        id: parsed.id,
        base_url: Some(parsed.base_url),
        grade: Some(parsed.grade),
        pool: parsed.pool,
        context: parsed.context,
        auth: parsed.auth,
        name: parsed.name,
        cost_in: parsed.cost_in,
        cost_out: parsed.cost_out,
        weight: parsed.weight,
        disabled: false,
    });
    overlay_write(&st, overlay)
}

/// `POST /api/models/remove` — drop an overlay ADDITION. Manual and
/// discovered models can be disabled but never removed from here.
pub async fn api_models_remove(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Err(resp) = admin_gate(&st, &headers, true) {
        return *resp;
    }
    #[derive(Deserialize)]
    struct RemoveBody {
        id: String,
    }
    let parsed: RemoveBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return bad_request(format!("invalid json: {e}")),
    };
    let rt = st.runtime();
    let is_addition = rt
        .cfg
        .models
        .iter()
        .any(|m| m.id == parsed.id && m.provenance == "overlay");
    let hidden_addition = !is_addition && {
        // a born-disabled addition never entered the registry; it is
        // removable iff it is not a manual/discovered model id
        !rt.cfg.models.iter().any(|m| m.id == parsed.id) && rt.overlay.disabled.contains(&parsed.id)
    };
    if !is_addition && !hidden_addition {
        return bad_request(format!(
            "{:?} is manual/discovered — disable it instead; only overlay additions can be removed",
            parsed.id
        ));
    }
    drop(rt);
    let (mut overlay, _) = crate::overlay::Overlay::load(&st.overlay_dir);
    let before = overlay.models.len();
    overlay.models.retain(|e| e.id != parsed.id);
    if overlay.models.len() == before {
        return api_error(
            StatusCode::NOT_FOUND,
            "unknown_model",
            &format!("no overlay entry for {:?}", parsed.id),
        );
    }
    overlay_write(&st, overlay)
}

#[derive(Deserialize)]
struct QuotaOverrideBody {
    provider: String,
    label: String,
    /// remaining fraction 0..=1
    pct: f64,
    #[serde(default)]
    reset_in_secs: Option<u64>,
    #[serde(default)]
    ttl_secs: Option<u64>,
}

/// `POST /api/quota/override` — synthetic `manual` quota sample with
/// `set_at`/`set_by`/`expires_at`; replaces the same provider+label
/// override and applies immediately. A fresher native fetch supersedes
/// it; expiry degrades it to stale.
pub async fn api_quota_override(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Err(resp) = admin_gate(&st, &headers, true) {
        return *resp;
    }
    let parsed: QuotaOverrideBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return bad_request(format!("invalid json: {e}")),
    };
    if parsed.provider.trim().is_empty() || parsed.label.trim().is_empty() {
        return bad_request("provider and label must not be empty".into());
    }
    if !(parsed.pct.is_finite() && (0.0..=1.0).contains(&parsed.pct)) {
        return bad_request("pct must be within 0..=1".into());
    }
    let now = crate::quota::unix_now();
    let expires_at = now + crate::overlay::clamp_ttl(parsed.ttl_secs);
    let (mut overlay, _) = crate::overlay::Overlay::load(&st.overlay_dir);
    overlay
        .quota
        .retain(|o| !(o.provider == parsed.provider && o.label == parsed.label));
    overlay.quota.push(crate::overlay::QuotaOverride {
        provider: parsed.provider.trim().to_string(),
        label: parsed.label.trim().to_string(),
        pct: parsed.pct,
        reset_unix: parsed.reset_in_secs.map(|s| now + s),
        set_at: now,
        set_by: "admin-session".into(),
        expires_at,
    });
    // quota overrides are validated trivially (above); reuse the
    // atomic save but skip the model dry-run tail
    if let Err(e) = overlay.save(&st.overlay_dir) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "overlay_write_failed",
            &format!(
                "{}: {e}",
                crate::overlay::path_for(&st.overlay_dir).display()
            ),
        );
    }
    let o = overlay.quota.last().expect("just pushed");
    st.quota.apply_manual(
        &o.provider,
        vec![crate::quota::QuotaWindow {
            label: o.label.clone(),
            pct: o.pct,
            reset_unix: o.reset_unix,
            pace: None,
        }],
        o.set_at,
        o.expires_at,
    );
    Json(serde_json::json!({ "ok": true, "expires_at": o.expires_at })).into_response()
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

    fn tempdir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "arouter-web-{}-{}-{:x}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn setup_creates_the_admin_once_and_login_works() {
        let dir = tempdir();
        let web = Web::new(&WebConfig::default(), &dir);
        assert!(web.setup_required());
        assert!(!web.setup_locked());

        let (sid, user) = web
            .create_admin(" alice ", "correct horse battery", 1_000)
            .await
            .unwrap();
        assert_eq!(user, "alice");
        let s = web.session(&sid, 1_000).expect("first session");
        assert_eq!(s.role, "admin");
        assert_eq!(s.username, "alice");
        assert!(!web.setup_required());
        assert_eq!(web.admin_username().as_deref(), Some("alice"));

        // the credential was persisted, and setup is closed forever
        let loaded = AdminStore::new(&dir).load().unwrap().expect("admin file");
        assert_eq!(loaded.username, "alice");
        assert_eq!(
            web.create_admin("bob", "another password", 1_001)
                .await
                .unwrap_err(),
            CredentialError::AlreadyConfigured
        );

        // wrong username and wrong password are the same failure
        let peer: IpAddr = "192.168.50.9".parse().unwrap();
        assert_eq!(
            web.login("alice", "not the password", Some(peer), 1_002)
                .await
                .unwrap_err(),
            CredentialError::BadCredentials
        );
        assert_eq!(
            web.login("bob", "correct horse battery", Some(peer), 1_002)
                .await
                .unwrap_err(),
            CredentialError::BadCredentials
        );

        let (sid2, user) = web
            .login("alice", "correct horse battery", Some(peer), 1_003)
            .await
            .unwrap();
        assert_eq!(user, "alice");
        assert_eq!(web.session(&sid2, 1_003).unwrap().username, "alice");
    }

    #[tokio::test]
    async fn concurrent_setup_has_exactly_one_winner() {
        let dir = tempdir();
        let web = Web::new(&WebConfig::default(), &dir);
        let (a, b) = tokio::join!(
            web.create_admin("alice", "correct horse battery", 1_000),
            web.create_admin("bob", "correct horse battery", 1_000)
        );
        let winners = [&a, &b].iter().filter(|r| r.is_ok()).count();
        assert_eq!(winners, 1, "{a:?} / {b:?}");
        let loser = if a.is_ok() { &b } else { &a };
        assert_eq!(
            *loser.as_ref().unwrap_err(),
            CredentialError::AlreadyConfigured
        );
        assert_eq!(AdminStore::new(&dir).load().unwrap().unwrap().version, 1);
    }

    #[tokio::test]
    async fn login_failures_are_throttled_and_clear_on_success() {
        let dir = tempdir();
        let web = Web::new(&WebConfig::default(), &dir);
        web.create_admin("alice", "correct horse battery", 1_000)
            .await
            .unwrap();
        let peer: IpAddr = "192.168.50.9".parse().unwrap();

        for _ in 0..LOGIN_MAX_FAILURES {
            assert_eq!(
                web.login("alice", "wrong", Some(peer), 1_000)
                    .await
                    .unwrap_err(),
                CredentialError::BadCredentials
            );
        }
        match web
            .login("alice", "correct horse battery", Some(peer), 1_001)
            .await
            .unwrap_err()
        {
            CredentialError::Throttled { retry_after_secs } => {
                assert!(retry_after_secs > 0 && retry_after_secs <= LOGIN_WINDOW_SECS);
            }
            other => panic!("expected throttle, got {other:?}"),
        }

        // another peer is unaffected
        let other: IpAddr = "192.168.50.10".parse().unwrap();
        assert!(web
            .login("alice", "correct horse battery", Some(other), 1_001)
            .await
            .is_ok());

        // the throttle expires, and a success clears the old window
        assert!(web
            .login(
                "alice",
                "correct horse battery",
                Some(peer),
                1_000 + LOGIN_WINDOW_SECS + 1
            )
            .await
            .is_ok());
        assert_eq!(
            web.login("alice", "wrong", Some(peer), 1_000 + LOGIN_WINDOW_SECS + 2)
                .await
                .unwrap_err(),
            CredentialError::BadCredentials
        );
    }

    #[tokio::test]
    async fn corrupt_admin_file_fails_closed() {
        let dir = tempdir();
        std::fs::write(crate::admin::path_for(&dir), "not [ valid toml").unwrap();
        let web = Web::new(&WebConfig::default(), &dir);
        assert!(!web.setup_required());
        assert!(web.setup_locked());
        assert!(matches!(
            web.create_admin("alice", "correct horse battery", 1_000)
                .await
                .unwrap_err(),
            CredentialError::Unavailable(_)
        ));
        assert!(matches!(
            web.login("alice", "correct horse battery", None, 1_000)
                .await
                .unwrap_err(),
            CredentialError::Unavailable(_)
        ));
    }

    #[test]
    fn sessions_expire_and_revoke() {
        let dir = tempdir();
        let web = Web::new(
            &WebConfig {
                enabled: true,
                session_ttl_hours: 1,
            },
            &dir,
        );
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
    fn web_url_is_openable_not_a_wildcard() {
        assert_eq!(web_url("0.0.0.0:3929"), "http://127.0.0.1:3929/");
        assert_eq!(web_url("[::]:3929"), "http://127.0.0.1:3929/");
        assert_eq!(web_url("::"), "http://127.0.0.1:3929/");
        assert_eq!(web_url("127.0.0.1:3929"), "http://127.0.0.1:3929/");
        assert_eq!(
            web_url("192.168.50.192:4000"),
            "http://192.168.50.192:4000/"
        );
    }
}
