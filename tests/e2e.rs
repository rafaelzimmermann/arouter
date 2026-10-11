#![allow(clippy::field_reassign_with_default)]

//! E2E tests: full axum router + mock upstreams on ephemeral ports.
//! Covers streaming passthrough, failover, grade delegation, profiles,
//! caller identity, and the Prometheus /metrics endpoint.

use arouter::config::{CallerConfig, CallerMap, Config, KeyStore, ModelConfig, ProfileConfig};
use arouter::health::{Health, HealthPolicy};
use arouter::metrics::Metrics;
use arouter::proxy;
use arouter::quota::{self, ProviderMap, ProviderState, QuotaState, QuotaWatcher};
use arouter::registry::Registry;
use arouter::usage::UsageLog;
use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderName, Request, StatusCode};
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use http_body_util::BodyExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;
use tower::ServiceExt;

static DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

const SSE_BODY: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2}}\n\n",
    "data: [DONE]\n\n",
);
const JSON_BODY: &str = "{\"id\":\"x\",\"choices\":[{\"message\":{\"content\":\"hi\"}}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}";

// ─────────────────────────── mock upstreams ───────────────────────────

async fn spawn_upstream(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}/v1")
}

async fn good_sse() -> String {
    spawn_upstream(Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                Body::from(SSE_BODY),
            )
        }),
    ))
    .await
}

async fn good_json() -> String {
    spawn_upstream(Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            (
                [(header::CONTENT_TYPE, "application/json")],
                Body::from(JSON_BODY),
            )
        }),
    ))
    .await
}

async fn bad_500() -> String {
    spawn_upstream(Router::new().route(
        "/v1/chat/completions",
        post(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
    ))
    .await
}

// ─────────────────────────── harness ───────────────────────────

fn model(
    id: &str,
    grade: &str,
    pools: &[&str],
    base_url: &str,
    weight: f64,
    costs: (f64, f64),
) -> ModelConfig {
    ModelConfig {
        id: id.into(),
        grade: grade.into(),
        pool: pools.iter().map(|s| s.to_string()).collect(),
        base_url: base_url.into(),
        weight,
        cost_in: costs.0,
        cost_out: costs.1,
        ..ModelConfig::default()
    }
}

#[allow(dead_code)]
struct TestApp {
    app: Router,
    usage: Arc<UsageLog>,
    metrics: Arc<Metrics>,
    health: Arc<Health>,
    quota: Arc<QuotaWatcher>,
    local: Arc<arouter::localeng::LocalEngines>,
    sticky: Arc<arouter::localeng::StickyMap>,
}

fn test_app(cfg: Config) -> TestApp {
    test_app_health(cfg, HealthPolicy::default())
}

/// Tests inject tiny policies so cooldowns/circuits act in milliseconds.
fn test_app_health(cfg: Config, policy: HealthPolicy) -> TestApp {
    test_app_health_quota(cfg, policy, QuotaState::default())
}

fn test_app_health_quota(cfg: Config, policy: HealthPolicy, quota_state: QuotaState) -> TestApp {
    let keys = KeyStore::default();
    let registry = Arc::new(RwLock::new(Arc::new(
        Registry::from_config(&cfg, &keys).unwrap(),
    )));
    let dir = std::env::temp_dir().join(format!(
        "arouter-e2e-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    let usage = Arc::new(UsageLog::open(dir.to_str().unwrap()).unwrap());
    let metrics = Arc::new(Metrics::new());
    let callers = Arc::new(CallerMap::from_config(&cfg, &keys));
    let health = Health::new(policy);
    let quota = QuotaWatcher::new(quota_state);
    let sticky_ttl = cfg.routing.sticky_local_secs;
    let local = Arc::new(arouter::localeng::LocalEngines::default());
    let sticky = Arc::new(arouter::localeng::StickyMap::new(sticky_ttl));
    let state = proxy::AppState {
        cfg: Arc::new(cfg),
        registry,
        usage: usage.clone(),
        callers,
        metrics: metrics.clone(),
        health: health.clone(),
        quota: quota.clone(),
        provider_map: Arc::new(ProviderMap::default()),
        quota_harvest: Arc::new(Default::default()),
        quota_sources: Arc::new(Default::default()),
        local: local.clone(),
        sticky: sticky.clone(),
        http: proxy::client(),
        started: Instant::now(),
    };
    TestApp {
        app: proxy::router(state),
        usage,
        metrics,
        health,
        quota,
        local,
        sticky,
    }
}

fn chat_body(model: &str, stream: bool) -> String {
    format!(
        "{{\"model\":\"{model}\",\"stream\":{stream},\"messages\":[{{\"role\":\"user\",\"content\":\"hi\"}}]}}"
    )
}

async fn post_chat(
    app: &Router,
    path: &str,
    model: &str,
    stream: bool,
    headers: HeaderMap,
) -> Response {
    let mut req = Request::post(path)
        .body(Body::from(chat_body(model, stream)))
        .unwrap();
    for (k, v) in &headers {
        req.headers_mut().insert(k, v.clone());
    }
    app.clone().oneshot(req).await.unwrap()
}

fn bearer(token: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        header::AUTHORIZATION,
        format!("Bearer {token}").parse().unwrap(),
    );
    h
}

fn user_hdr(name: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        HeaderName::from_static("x-arouter-user"),
        name.parse().unwrap(),
    );
    h
}

async fn body_string(resp: Response) -> String {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn hstr(resp: &Response, name: &str) -> String {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

// ─────────────────────────── scenarios ───────────────────────────

#[tokio::test]
async fn streaming_passthrough_records_usage_and_caller() {
    let up = good_sse().await;
    let mut cfg = Config::default();
    cfg.callers = vec![CallerConfig {
        budget_usd_day: None,
        name: "alice".into(),
        project: Some("arouter".into()),
        key: Some("key:k-alice".into()),
    }];
    cfg.models = vec![model("zai/glm-5.3", "A+", &["quota"], &up, 1.0, (1.0, 3.0))];
    let t = test_app(cfg);

    let resp = post_chat(
        &t.app,
        "/v1/chat/completions",
        "A+",
        true,
        bearer("k-alice"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(hstr(&resp, "content-type").starts_with("text/event-stream"));
    // byte-level passthrough
    assert_eq!(body_string(resp).await, SSE_BODY);

    // attribution headers
    let resp = post_chat(
        &t.app,
        "/v1/chat/completions",
        "A+",
        true,
        bearer("k-alice"),
    )
    .await;
    assert_eq!(hstr(&resp, "x-arouter-upstream"), "zai/glm-5.3");
    assert_eq!(hstr(&resp, "x-arouter-pool"), "quota");
    assert_eq!(hstr(&resp, "x-arouter-grade"), "A+");
    assert_eq!(hstr(&resp, "x-arouter-caller"), "alice");
    assert_eq!(hstr(&resp, "x-arouter-project"), "arouter");
    assert_eq!(hstr(&resp, "x-arouter-degraded"), ""); // not degraded
    drop(resp.into_body().collect().await.unwrap());

    let records = t.usage.records();
    assert_eq!(records.len(), 2);
    let r = &records[0];
    assert_eq!(r.caller, "alice");
    assert_eq!(r.project.as_deref(), Some("arouter"));
    assert_eq!(r.status, "ok");
    assert_eq!((r.tokens_in, r.tokens_out), (Some(7), Some(2)));
    assert!((r.cost_usd - 13.0 / 1_000_000.0).abs() < 1e-12);
    assert!(r.ttfb_ms.is_some());
    assert!(!r.degraded);
}

#[tokio::test]
async fn nonstream_relay_and_headers() {
    let up = good_json().await;
    let mut cfg = Config::default();
    cfg.models = vec![model("zai/glm-5.2", "A", &["quota"], &up, 1.0, (0.0, 0.0))];
    let t = test_app(cfg);

    let resp = post_chat(&t.app, "/v1/chat/completions", "A", false, HeaderMap::new()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(hstr(&resp, "x-arouter-grade"), "A");
    // anonymous caller → no caller header
    assert_eq!(hstr(&resp, "x-arouter-caller"), "");
    assert_eq!(body_string(resp).await, JSON_BODY);

    let records = t.usage.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].caller, "anonymous");
    assert_eq!(records[0].tokens_in, Some(10));
    assert_eq!(records[0].tokens_out, Some(5));
}

#[tokio::test]
async fn failover_on_retryable_500() {
    let bad = bad_500().await;
    let good = good_json().await;
    let mut cfg = Config::default();
    cfg.models = vec![
        model("zai/primary", "A", &["quota"], &bad, 2.0, (0.0, 0.0)), // weight 2 → tried first
        model("zai/backup", "A", &["quota"], &good, 1.0, (0.0, 0.0)),
    ];
    let t = test_app(cfg);

    let resp = post_chat(&t.app, "/v1/chat/completions", "A", false, HeaderMap::new()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(hstr(&resp, "x-arouter-upstream"), "zai/backup");
    drop(resp.into_body().collect().await.unwrap());

    // both attempts recorded: failure + success
    let records = t.usage.records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].model, "zai/primary");
    assert_eq!(records[0].status, "upstream_error");
    assert_eq!(records[0].http_status, Some(500));
    assert_eq!(records[1].model, "zai/backup");
    assert_eq!(records[1].status, "ok");
    let m = t.metrics.render();
    assert!(m.contains("status=\"upstream_error\""));
    assert!(m.contains("status=\"ok\""));
}

#[tokio::test]
async fn delegation_degrades_but_never_upward() {
    let bad = bad_500().await;
    let good = good_json().await;
    let mut cfg = Config::default();
    cfg.models = vec![
        model("zai/glm-5.3", "A+", &["quota"], &bad, 1.0, (0.0, 0.0)),
        model("zai/glm-5.2", "A", &["quota"], &good, 1.0, (0.0, 0.0)),
    ];
    let t = test_app(cfg);

    // A+ fails → degrades to A (allowed by default [self, one lower])
    let resp = post_chat(
        &t.app,
        "/v1/chat/completions",
        "A+",
        false,
        HeaderMap::new(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(hstr(&resp, "x-arouter-grade"), "A");
    assert_eq!(hstr(&resp, "x-arouter-degraded"), "A");
    drop(resp.into_body().collect().await.unwrap());
    let records = t.usage.records();
    let ok_rec = records.iter().rev().find(|r| r.status == "ok").unwrap();
    assert!(ok_rec.degraded);
    assert_eq!(ok_rec.grade_requested, "A+");
    assert_eq!(ok_rec.grade_served, "A");

    // B request is NEVER served by A+/A models (no upward)
    let resp = post_chat(&t.app, "/v1/chat/completions", "B", false, HeaderMap::new()).await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = body_string(resp).await;
    assert!(body.contains("arouter_exhausted"), "{body}");

    // unknown grade → 400
    let resp = post_chat(&t.app, "/v1/chat/completions", "Z", false, HeaderMap::new()).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn profile_exclusivity_and_routing() {
    let up = good_json().await;
    let mut cfg = Config::default();
    cfg.profiles.insert("vip".into(), ProfileConfig::default());
    let mut m = model("or/opus", "A+", &["quota"], &up, 1.0, (5.0, 25.0));
    m.profiles = vec!["vip".into()];
    cfg.models = vec![m];
    let t = test_app(cfg);

    // default profile can't see the vip-exclusive model → exhausted
    let resp = post_chat(
        &t.app,
        "/v1/chat/completions",
        "A+",
        false,
        HeaderMap::new(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

    // vip profile can
    let resp = post_chat(
        &t.app,
        "/vip/v1/chat/completions",
        "A+",
        false,
        HeaderMap::new(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(hstr(&resp, "x-arouter-profile"), "vip");

    // unknown profile → 404
    let resp = post_chat(
        &t.app,
        "/nope/v1/chat/completions",
        "A+",
        false,
        HeaderMap::new(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(body_string(resp).await.contains("unknown_profile"));

    // models listing respects visibility
    let resp = t
        .app
        .clone()
        .oneshot(Request::get("/v1/models").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = body_string(resp).await;
    assert!(body.contains("\"data\":[]"), "{body}");
    let resp = t
        .app
        .clone()
        .oneshot(Request::get("/vip/v1/models").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = body_string(resp).await;
    assert!(body.contains("\"A+\""), "{body}");
}

#[tokio::test]
async fn models_endpoint_lists_grades_with_models() {
    let up = good_json().await;
    let mut cfg = Config::default();
    cfg.models = vec![
        model("zai/glm-5.3", "A+", &["quota"], &up, 1.0, (0.0, 0.0)),
        model("zai/glm-5.3-flash", "B", &["quota"], &up, 1.0, (0.0, 0.0)),
    ];
    let t = test_app(cfg);

    let resp = t
        .app
        .clone()
        .oneshot(Request::get("/v1/models").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert!(body.contains("\"A+\""));
    assert!(body.contains("\"B\""));
    assert!(
        !body.contains("\"A\""),
        "grade without models must be absent: {body}"
    );
}

#[tokio::test]
async fn caller_identity_open_and_required_modes() {
    let up = good_json().await;
    let mut cfg = Config::default();
    cfg.callers = vec![CallerConfig {
        budget_usd_day: None,
        name: "alice".into(),
        project: Some("arouter".into()),
        key: Some("key:k-alice".into()),
    }];
    cfg.models = vec![model("zai/glm-5.2", "A", &["quota"], &up, 1.0, (0.0, 0.0))];
    let t = test_app(cfg);

    // open mode: unknown bearer → anonymous (still served)
    let resp = post_chat(
        &t.app,
        "/v1/chat/completions",
        "A",
        false,
        bearer("wrong-key"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(hstr(&resp, "x-arouter-caller"), "");
    drop(resp.into_body().collect().await.unwrap());

    // open mode: X-Arouter-User / -Project headers identify the caller
    let mut h = HeaderMap::new();
    h.insert("x-arouter-user", "alice".parse().unwrap());
    h.insert("x-arouter-project", "website".parse().unwrap());
    let resp = post_chat(&t.app, "/v1/chat/completions", "A", false, h).await;
    assert_eq!(hstr(&resp, "x-arouter-caller"), "alice");
    assert_eq!(hstr(&resp, "x-arouter-project"), "website");
    drop(resp.into_body().collect().await.unwrap());

    // required mode
    let mut cfg = Config::default();
    cfg.auth.require = true;
    cfg.callers = vec![CallerConfig {
        budget_usd_day: None,
        name: "alice".into(),
        project: None,
        key: Some("key:k-alice".into()),
    }];
    cfg.models = vec![model("zai/glm-5.2", "A", &["quota"], &up, 1.0, (0.0, 0.0))];
    let t = test_app(cfg);

    let resp = post_chat(&t.app, "/v1/chat/completions", "A", false, HeaderMap::new()).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(body_string(resp).await.contains("missing api key"));

    let resp = post_chat(&t.app, "/v1/chat/completions", "A", false, bearer("nope")).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = post_chat(
        &t.app,
        "/v1/chat/completions",
        "A",
        false,
        bearer("k-alice"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(hstr(&resp, "x-arouter-caller"), "alice");
}

#[tokio::test]
async fn prometheus_metrics_endpoint() {
    let up = good_json().await;
    let mut cfg = Config::default();
    cfg.callers = vec![CallerConfig {
        budget_usd_day: None,
        name: "alice".into(),
        project: Some("arouter".into()),
        key: Some("key:k-alice".into()),
    }];
    cfg.models = vec![model("zai/glm-5.2", "A", &["quota"], &up, 1.0, (1.0, 3.0))];
    let t = test_app(cfg);

    // one priced request + one exhausted request
    let resp = post_chat(
        &t.app,
        "/v1/chat/completions",
        "A",
        false,
        bearer("k-alice"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    drop(resp.into_body().collect().await.unwrap());
    let resp = post_chat(
        &t.app,
        "/v1/chat/completions",
        "C",
        false,
        bearer("k-alice"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    drop(resp.into_body().collect().await.unwrap());

    let resp = t
        .app
        .clone()
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        hstr(&resp, "content-type"),
        "text/plain; version=0.0.4; charset=utf-8"
    );
    let m = body_string(resp).await;
    assert!(m.contains("# TYPE arouter_requests_total counter"));
    assert!(m.contains("arouter_requests_total{caller=\"alice\",grade_requested=\"A\",grade_served=\"A\",model=\"zai/glm-5.2\",pool=\"quota\",profile=\"default\",project=\"arouter\",status=\"ok\"} 1"));
    assert!(m.contains("status=\"exhausted\""));
    assert!(m.contains("arouter_tokens_total{caller=\"alice\",direction=\"in\""));
    assert!(m.contains("arouter_cost_usd_total{caller=\"alice\""));
    assert!(m.contains("# TYPE arouter_request_duration_seconds histogram"));
    assert!(m.contains("arouter_requests_in_flight 0\n"));
    assert!(m.contains("arouter_uptime_seconds"));
    assert!(m.contains("arouter_models{grade=\"A\",pool=\"quota\",profile=\"default\"} 1"));
    assert!(m.contains("arouter_build_info{version=\""));
}

#[tokio::test]
async fn status_endpoint_has_caller_breakdown() {
    let up = good_json().await;
    let mut cfg = Config::default();
    cfg.callers = vec![CallerConfig {
        budget_usd_day: None,
        name: "alice".into(),
        project: Some("arouter".into()),
        key: Some("key:k-alice".into()),
    }];
    cfg.models = vec![model("zai/glm-5.2", "A", &["quota"], &up, 1.0, (0.0, 0.0))];
    let t = test_app(cfg);

    let resp = post_chat(
        &t.app,
        "/v1/chat/completions",
        "A",
        false,
        bearer("k-alice"),
    )
    .await;
    drop(resp.into_body().collect().await.unwrap());

    let resp = t
        .app
        .clone()
        .oneshot(Request::get("/status.json").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = body_string(resp).await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["requests_total"], 1);
    assert_eq!(v["callers"]["alice/arouter"]["requests"], 1);
    assert_eq!(v["usage_by_model"]["zai/glm-5.2"]["ok"], 1);
    assert_eq!(v["auth_required"], false);
}

#[tokio::test]
async fn healthz() {
    let t = test_app(Config::default());
    let resp = t
        .app
        .clone()
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ─────────────────────── E2: health, cooldowns, discovery ───────────────────────

use arouter::catalog::Catalog;
use arouter::config::AutoGrades;
use arouter::discover;
use arouter::health::HealthState;
use std::time::Duration;

/// 500 upstream that counts requests (axum handlers must be Clone).
async fn counting_500() -> (String, Arc<AtomicUsize>) {
    let n = Arc::new(AtomicUsize::new(0));
    let n2 = n.clone();
    let base = spawn_upstream(Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let n = n2.clone();
            async move {
                n.fetch_add(1, Ordering::SeqCst);
                (StatusCode::INTERNAL_SERVER_ERROR, "boom")
            }
        }),
    ))
    .await;
    (base, n)
}

#[tokio::test]
async fn cooldown_excludes_failing_model() {
    let (base, hits) = counting_500().await;
    let mut cfg = Config::default();
    cfg.models = vec![model("p/m1", "B", &["quota"], &base, 1.0, (0.0, 0.0))];
    let t = test_app_health(
        cfg,
        HealthPolicy {
            cooldown_steps: vec![Duration::from_secs(60)],
            circuit_min_samples: 99,
            penalty_min_samples: 99,
            ..HealthPolicy::default()
        },
    );

    // first request hits the model, gets 500 → exhausted
    let resp = post_chat(&t.app, "/v1/chat/completions", "B", false, HeaderMap::new()).await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(t.health.state("p/m1"), HealthState::Cooldown);

    // second request: the model is cooling → skipped entirely
    let resp = post_chat(&t.app, "/v1/chat/completions", "B", false, HeaderMap::new()).await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(hits.load(Ordering::SeqCst), 1, "cooling model not re-tried");

    // and /status.json reports it
    let resp = t
        .app
        .clone()
        .oneshot(Request::get("/status.json").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(resp).await).unwrap();
    assert_eq!(v["health"]["p/m1"]["state"], "cooldown");
}

#[tokio::test]
async fn circuit_opens_and_excludes_model() {
    let (base, hits) = counting_500().await;
    let mut cfg = Config::default();
    cfg.models = vec![model("p/m1", "B", &["quota"], &base, 1.0, (0.0, 0.0))];
    let t = test_app_health(
        cfg,
        HealthPolicy {
            cooldown_steps: vec![],
            circuit_min_samples: 2,
            circuit_ratio: 0.5,
            circuit_open: Duration::from_secs(60),
            penalty_min_samples: 99,
            ..HealthPolicy::default()
        },
    );

    for _ in 0..2 {
        let resp = post_chat(&t.app, "/v1/chat/completions", "B", false, HeaderMap::new()).await;
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    }
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert_eq!(t.health.state("p/m1"), HealthState::CircuitOpen);

    // circuit-open models are excluded from further attempts
    let resp = post_chat(&t.app, "/v1/chat/completions", "B", false, HeaderMap::new()).await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn discovery_finds_grades_and_serves_new_models() {
    // mock upstream: knows "known" (A) + "extra-model" (unknown to config)
    let base = spawn_upstream(
        Router::new()
            .route(
                "/v1/models",
                axum::routing::get(|| async {
                    axum::Json(serde_json::json!({
                        "data": [
                            {"id": "known", "context_length": 8192},
                            {"id": "extra-model", "context_length": 131072},
                        ]
                    }))
                }),
            )
            .route(
                "/v1/chat/completions",
                post(|payload: String| async move {
                    (
                        [(header::CONTENT_TYPE, "application/json")],
                        format!("{{\"served\":{payload}}}"),
                    )
                }),
            ),
    )
    .await;

    let mut cfg = Config::default();
    cfg.models = vec![model("prov/known", "A", &["quota"], &base, 1.0, (0.0, 0.0))];

    // discovery: only the unknown model comes back
    let keys = KeyStore::default();
    let found = discover::discover(&proxy::client(), &keys, &cfg.models).await;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, "prov/extra-model");
    assert_eq!(found[0].context, Some(131072));
    assert!(found[0].local, "127.0.0.1 upstream is local");

    // local + no catalog match → heuristic B ("local (no price)")
    let ladder = cfg.grades.ladder();
    let catalog = Catalog::embedded(&ladder).unwrap();
    let merged = discover::merge_discovered(&cfg, &found, &AutoGrades::default(), &catalog);
    let t = test_app(merged);

    // request B: only the discovered model qualifies → it serves, and the
    // upstream sees the bare upstream name
    let resp = post_chat(&t.app, "/v1/chat/completions", "B", false, HeaderMap::new()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("x-arouter-upstream").unwrap(),
        "prov/extra-model"
    );
    let body = body_string(resp).await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["served"]["model"], "extra-model");
}

// ─────────────────────────── E3: quota-aware routing ───────────────────────────

fn quota_state_exhausted(provider: &str, reset_in_secs: u64) -> QuotaState {
    let now = quota::unix_now();
    let mut qs = QuotaState {
        fetched_unix: Some(now),
        ..QuotaState::default()
    };
    qs.providers.insert(
        provider.into(),
        ProviderState {
            panel: provider.into(),
            error: None,
            stale: false,
            updated_unix: None,
            windows: vec![quota::QuotaWindow {
                label: "5h window".into(),
                pct: 100.0,
                reset_unix: Some(now + reset_in_secs),
                pace: None,
            }],
        },
    );
    qs
}

fn test_app_quota(cfg: Config, state: QuotaState) -> TestApp {
    test_app_health_quota(cfg, HealthPolicy::default(), state)
}

#[tokio::test]
async fn quota_exhausted_429_carries_reset_aware_retry_after() {
    let up = good_json().await;
    let mut cfg = Config::default();
    cfg.models = vec![model("prov/m", "A", &["quota"], &up, 1.0, (0.0, 0.0))];
    let t = test_app_quota(cfg, quota_state_exhausted("prov", 600));

    let resp = post_chat(&t.app, "/v1/chat/completions", "A", false, HeaderMap::new()).await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    // reset − 120s safety, allowing test-runner slop
    let retry: u64 = hstr(&resp, "retry-after").parse().expect("retry-after set");
    assert!(
        (460..=481).contains(&retry),
        "retry-after should be ~480, got {retry}"
    );
    let body = body_string(resp).await;
    assert!(body.contains("arouter_exhausted"), "body: {body}");
}

#[tokio::test]
async fn exhausted_provider_is_skipped_and_peer_serves() {
    let up_a = good_json().await;
    let up_b = good_json().await;
    let mut cfg = Config::default();
    cfg.models = vec![
        model("a/m", "A", &["quota"], &up_a, 1.0, (0.0, 0.0)),
        model("b/m", "A", &["quota"], &up_b, 1.0, (0.0, 0.0)),
    ];
    // a exhausted, b merely unobserved (unknown ≠ unavailable → 0.8)
    let t = test_app_quota(cfg, quota_state_exhausted("a", 600));

    let resp = post_chat(&t.app, "/v1/chat/completions", "A", false, HeaderMap::new()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(hstr(&resp, "x-arouter-upstream"), "b/m");
}

#[tokio::test]
async fn draining_provider_takes_matching_requests() {
    // z.ai case: weekly 76% resetting in ~2h27m vs a peer with days of
    // headroom. Headroom alone prefers the peer ((100-76)×2.5h ≈ 59 vs
    // (100-50)×5d); drain ("use it or lose it") must override within the
    // grade so the dying quota is spent, not wasted.
    let up_a = good_json().await;
    let up_b = good_json().await;
    let mut cfg = Config::default();
    cfg.models = vec![
        model("a/m", "A", &["quota"], &up_a, 1.0, (0.0, 0.0)),
        model("b/m", "A", &["quota"], &up_b, 1.0, (0.0, 0.0)),
    ];
    let now = quota::unix_now();
    let qs = QuotaState {
        stale_after_secs: 0,
        fetched_unix: Some(now),
        providers: [
            (
                "a",
                ProviderState {
                    panel: "a".into(),
                    error: None,
                    stale: false,
                    updated_unix: None,
                    windows: vec![quota::QuotaWindow {
                        label: "weekly".into(),
                        pct: 76.0,
                        reset_unix: Some(now + 9_240), // 2h34m < DRAIN_SECS
                        pace: None,
                    }],
                },
            ),
            (
                "b",
                ProviderState {
                    panel: "b".into(),
                    error: None,
                    stale: false,
                    updated_unix: None,
                    windows: vec![quota::QuotaWindow {
                        label: "5h window".into(),
                        pct: 50.0,
                        reset_unix: Some(now + 5 * 86400),
                        pace: None,
                    }],
                },
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect(),
    };
    let t = test_app_quota(cfg, qs);

    let resp = post_chat(&t.app, "/v1/chat/completions", "A", false, HeaderMap::new()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(hstr(&resp, "x-arouter-upstream"), "a/m");

    // metrics expose the drain countdown: positive for the drainer, -1 for
    // the peer
    let resp = t
        .app
        .clone()
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let m = body_string(resp).await;
    let la = m
        .lines()
        .find(|l| l.starts_with("arouter_quota_drain_reset_secs{provider=\"a\"}"))
        .unwrap_or_else(|| panic!("drain gauge missing: {m}"));
    let secs: f64 = la.rsplit(' ').next().unwrap().parse().unwrap();
    assert!((0.0..=9_240.0).contains(&secs), "drainer countdown {secs}");
    assert!(
        m.contains("arouter_quota_drain_reset_secs{provider=\"b\"} -1"),
        "peer should report not draining: {m}"
    );
}

#[tokio::test]
async fn scarce_provider_still_serves() {
    let up = good_json().await;
    let mut cfg = Config::default();
    cfg.models = vec![model("prov/m", "A", &["quota"], &up, 1.0, (0.0, 0.0))];
    let mut qs = quota_state_exhausted("prov", 7_200);
    qs.providers.get_mut("prov").unwrap().windows[0].pct = 90.0;
    let t = test_app_quota(cfg, qs);

    let resp = post_chat(&t.app, "/v1/chat/completions", "A", false, HeaderMap::new()).await;
    assert_eq!(resp.status(), StatusCode::OK, "scarce ≠ exhausted");
}

#[tokio::test]
async fn status_json_exposes_quota_section_and_metrics_gauges() {
    let up = good_json().await;
    let mut cfg = Config::default();
    cfg.models = vec![model("prov/m", "A", &["quota"], &up, 1.0, (0.0, 0.0))];
    let t = test_app_quota(cfg, quota_state_exhausted("prov", 600));

    let resp = t
        .app
        .clone()
        .oneshot(Request::get("/status.json").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = body_string(resp).await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let q = &v["quota"];
    assert_eq!(q["stale"], serde_json::json!(false));
    assert_eq!(q["providers"]["prov"]["windows"][0]["label"], "5h window");
    assert!(
        (q["providers"]["prov"]["windows"][0]["pct"]
            .as_f64()
            .unwrap()
            - 100.0)
            .abs()
            < 1e-9
    );
    assert_eq!(
        q["providers"]["prov"]["availability"],
        serde_json::json!(0.0)
    );

    let resp = t
        .app
        .clone()
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = body_string(resp).await;
    assert!(
        body.contains("arouter_quota_window_pct{provider=\"prov\",window=\"5h window\"} 100"),
        "gauge missing: {body}"
    );
    assert!(body.contains("arouter_quota_stale{provider=\"prov\"} 0"));
}

// ─────────────────────────── E4: budgets & cross-pool ───────────────────────────

use arouter::config::{BudgetConfig, PoolConfig};

fn set_ondemand_budget(cfg: &mut Config, budget: BudgetConfig) {
    cfg.pool.insert(
        "ondemand".into(),
        PoolConfig {
            preference: "cheapest".into(),
            budget: Some(budget),
        },
    );
}

#[tokio::test]
async fn ondemand_serves_cheapest_with_spend_headers() {
    let pricey = good_json().await;
    let cheap = good_json().await;
    let mut cfg = Config::default();
    cfg.models = vec![
        model("or/pricey", "A", &["ondemand"], &pricey, 1.0, (9.0, 27.0)),
        model("or/cheap", "A", &["ondemand"], &cheap, 1.0, (0.1, 0.4)),
    ];
    let t = test_app(cfg);

    let resp = post_chat(
        &t.app,
        "/ondemand/v1/chat/completions",
        "A",
        false,
        HeaderMap::new(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(hstr(&resp, "x-arouter-upstream"), "or/cheap");
    // actual cost from usage: 10 in + 5 out at (0.1, 0.4)/Mtok = $0.003
    let cost: f64 = hstr(&resp, "x-arouter-cost-usd").parse().unwrap();
    assert!((cost - 0.000003).abs() < 1e-9, "cost header {cost}");
    // no budget configured → no remaining header
    assert_eq!(hstr(&resp, "x-arouter-budget-remaining-today"), "");
}

#[tokio::test]
async fn daily_budget_denies_with_402_json() {
    let up = good_json().await;
    let mut cfg = Config::default();
    cfg.models = vec![model("or/x", "A", &["ondemand"], &up, 1.0, (9.0, 27.0))];
    // "hi" request est ≈ $0.111 → over the $0.05 daily cap
    set_ondemand_budget(
        &mut cfg,
        BudgetConfig {
            daily_usd: 0.05,
            ..Default::default()
        },
    );
    let t = test_app(cfg);

    let resp = post_chat(
        &t.app,
        "/ondemand/v1/chat/completions",
        "A",
        false,
        HeaderMap::new(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED);
    let body = body_string(resp).await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["type"], "budget_exceeded");
    assert_eq!(v["error"]["budget"]["layer"], "pool");
    assert_eq!(v["error"]["budget"]["window"], "daily");
    assert_eq!(v["error"]["budget"]["cap_usd"], 0.05);
}

#[tokio::test]
async fn per_request_cap_skips_to_cheaper_candidate() {
    let pricey = good_json().await;
    let cheap = good_json().await;
    let mut cfg = Config::default();
    cfg.models = vec![
        model("or/pricey", "A", &["ondemand"], &pricey, 5.0, (9.0, 27.0)),
        model("or/cheap", "A", &["ondemand"], &cheap, 1.0, (0.1, 0.4)),
    ];
    // est: pricey ≈ $0.111, cheap ≈ $0.0008; cap between them
    set_ondemand_budget(
        &mut cfg,
        BudgetConfig {
            daily_usd: 1.0,
            per_request_usd: 0.01,
            ..Default::default()
        },
    );
    let t = test_app(cfg);

    let resp = post_chat(
        &t.app,
        "/ondemand/v1/chat/completions",
        "A",
        false,
        HeaderMap::new(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(hstr(&resp, "x-arouter-upstream"), "or/cheap");
    let rem: f64 = hstr(&resp, "x-arouter-budget-remaining-today")
        .parse()
        .unwrap();
    assert!((rem - 1.0).abs() < 1e-6, "remaining {rem}");
}

#[tokio::test]
async fn per_caller_budget_denies_even_when_pool_allows() {
    let up = good_json().await;
    let mut cfg = Config::default();
    cfg.models = vec![model("or/x", "A", &["ondemand"], &up, 1.0, (9.0, 27.0))];
    set_ondemand_budget(
        &mut cfg,
        BudgetConfig {
            daily_usd: 5.0, // pool is generous
            ..Default::default()
        },
    );
    cfg.callers = vec![CallerConfig {
        budget_usd_day: None,
        name: "alice".into(),
        project: None,
        key: Some("key:alice-key".into()),
    }];
    // alice carries her own tighter cap — but it's the *second* caller entry
    cfg.callers.push(CallerConfig {
        name: "frugal".into(),
        project: None,
        key: Some("key:frugal-key".into()),
        budget_usd_day: Some(0.01),
    });
    let t = test_app(cfg);

    let mut h = HeaderMap::new();
    h.insert("authorization", "Bearer frugal-key".parse().unwrap());
    let resp = post_chat(&t.app, "/ondemand/v1/chat/completions", "A", false, h).await;
    assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED);
    let body = body_string(resp).await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["budget"]["layer"], "caller");

    // alice (no personal cap) sails through on the pool cap
    let mut h = HeaderMap::new();
    h.insert("authorization", "Bearer alice-key".parse().unwrap());
    let resp = post_chat(&t.app, "/ondemand/v1/chat/completions", "A", false, h).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn cross_pool_fallback_is_explicit_and_labeled() {
    let bad = bad_500().await;
    let paid = good_json().await;
    let mut cfg = Config::default();
    cfg.models = vec![
        model("zai/glm", "A", &["quota"], &bad, 1.0, (0.0, 0.0)),
        model("or/x", "A", &["ondemand"], &paid, 1.0, (1.0, 4.0)),
    ];
    cfg.routing.fallback.cross_pool = vec!["ondemand".into()];
    let t = test_app(cfg);

    let resp = post_chat(&t.app, "/v1/chat/completions", "A", false, HeaderMap::new()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(hstr(&resp, "x-arouter-upstream"), "or/x");
    assert_eq!(hstr(&resp, "x-arouter-pool"), "ondemand");
    assert_eq!(hstr(&resp, "x-arouter-cross-pool"), "quota");
    // serving attempt is recorded against the ondemand pool
    let records = t.usage.records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[1].pool, "ondemand");

    // without the opt-in the request fails closed
    let mut cfg2 = Config::default();
    cfg2.models = vec![
        model("zai/glm", "A", &["quota"], &bad, 1.0, (0.0, 0.0)),
        model("or/x", "A", &["ondemand"], &paid, 1.0, (1.0, 4.0)),
    ];
    let t2 = test_app(cfg2);
    let resp = post_chat(
        &t2.app,
        "/v1/chat/completions",
        "A",
        false,
        HeaderMap::new(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(hstr(&resp, "x-arouter-cross-pool"), "");
}

// ─────────────────────────── anthropic wire (E7) ───────────────────────────

fn anthropic_body(model: &str, stream: bool) -> String {
    format!(
        "{{\"model\":\"{model}\",\"max_tokens\":64,\"stream\":{stream},\"system\":\"be terse\",\"messages\":[{{\"role\":\"user\",\"content\":\"hi\"}}]}}"
    )
}

async fn post_messages(
    app: &Router,
    path: &str,
    model: &str,
    stream: bool,
    headers: HeaderMap,
) -> Response {
    let mut req = Request::post(path)
        .body(Body::from(anthropic_body(model, stream)))
        .unwrap();
    for (k, v) in &headers {
        req.headers_mut().insert(k, v.clone());
    }
    app.clone().oneshot(req).await.unwrap()
}

async fn body_text(resp: Response) -> String {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[tokio::test]
async fn anthropic_nonstream_translates_both_ways() {
    let up = good_json().await;
    let mut cfg = Config::default();
    cfg.models = vec![model("stub/a", "A", &["quota"], &up, 1.0, (0.0, 0.0))];
    let t = test_app(cfg);

    let resp = post_messages(&t.app, "/v1/messages", "A", false, HeaderMap::new()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body_text(resp).await).unwrap();
    assert_eq!(v["type"], "message");
    assert_eq!(v["role"], "assistant");
    assert_eq!(v["content"][0]["type"], "text");
    assert_eq!(v["content"][0]["text"], "hi");
    assert_eq!(v["stop_reason"], "end_turn");
    assert_eq!(v["usage"]["input_tokens"], 10);
    assert_eq!(v["usage"]["output_tokens"], 5);

    // usage tap recorded the *upstream* (openai) numbers
    let snap = t.usage.snapshot();
    assert_eq!(snap["usage_by_model"]["stub/a"]["tokens_in"], 10);
    assert_eq!(snap["usage_by_model"]["stub/a"]["tokens_out"], 5);
}

#[tokio::test]
async fn anthropic_stream_translates_sse() {
    let up = good_sse().await;
    let mut cfg = Config::default();
    cfg.models = vec![model("stub/s", "A", &["quota"], &up, 1.0, (0.0, 0.0))];
    let t = test_app(cfg);

    let resp = post_messages(&t.app, "/v1/messages", "A", true, HeaderMap::new()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        hstr(&resp, header::CONTENT_TYPE.as_str()),
        "text/event-stream"
    );
    let text = body_text(resp).await;
    assert!(text.starts_with("event: message_start\n"), "got: {text}");
    assert!(text.contains("\"text\":\"Hel\""));
    assert!(text.contains("event: message_delta"));
    assert!(text.contains("\"stop_reason\":\"end_turn\""));
    assert!(text.contains("\"output_tokens\":2"));
    assert!(text.contains("\"input_tokens\":7"));
    assert!(text
        .trim_end()
        .ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}"));

    // usage tap still parsed the openai-wire final chunk
    let snap = t.usage.snapshot();
    assert_eq!(snap["usage_by_model"]["stub/s"]["tokens_out"], 2);
}

#[tokio::test]
async fn anthropic_auth_via_x_api_key() {
    let mut cfg = Config::default();
    cfg.auth.require = true;
    cfg.callers.push(CallerConfig {
        name: "claude-code".into(),
        key: Some("key:sk-ant-test".into()),
        ..CallerConfig::default()
    });
    let t = test_app(cfg);

    let mut h = HeaderMap::new();
    h.insert("x-api-key", "sk-ant-test".parse().unwrap());
    let resp = post_messages(&t.app, "/v1/messages", "A", false, h).await;
    assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);

    // wrong key → anthropic-shaped 401
    let mut h = HeaderMap::new();
    h.insert("x-api-key", "wrong".parse().unwrap());
    let resp = post_messages(&t.app, "/v1/messages", "A", false, h).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let v: serde_json::Value = serde_json::from_str(&body_text(resp).await).unwrap();
    assert_eq!(v["type"], "error");
    assert_eq!(v["error"]["type"], "authentication_error");
}

#[tokio::test]
async fn anthropic_errors_speak_anthropic() {
    let up = good_json().await;
    let mut cfg = Config::default();
    cfg.models = vec![model("stub/a", "A", &["quota"], &up, 1.0, (0.0, 0.0))];
    let t = test_app(cfg);

    // bad grade → 400 in anthropic shape
    let resp = post_messages(&t.app, "/v1/messages", "Z", false, HeaderMap::new()).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v: serde_json::Value = serde_json::from_str(&body_text(resp).await).unwrap();
    assert_eq!(v["type"], "error");

    // invalid json body → anthropic shape too
    let req = Request::post("/v1/messages")
        .body(Body::from("{not json"))
        .unwrap();
    let resp = t.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v: serde_json::Value = serde_json::from_str(&body_text(resp).await).unwrap();
    assert_eq!(v["error"]["type"], "invalid_request_error");
}

#[tokio::test]
async fn anthropic_failover_and_profile_paths() {
    let bad = bad_500().await;
    let up = good_json().await;
    let mut cfg = Config::default();
    cfg.models = vec![
        model("stub/dead", "A", &["quota"], &bad, 2.0, (0.0, 0.0)),
        model("stub/live", "A", &["quota"], &up, 1.0, (0.0, 0.0)),
    ];
    let t = test_app(cfg);

    let resp = post_messages(&t.app, "/v1/messages", "A", false, HeaderMap::new()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(hstr(&resp, "x-arouter-pool"), "quota");
    assert_eq!(hstr(&resp, "x-arouter-upstream"), "stub/live");
    let v: serde_json::Value = serde_json::from_str(&body_text(resp).await).unwrap();
    assert_eq!(v["content"][0]["text"], "hi");
}

// ─────────────────────────── local engine smarts (E8) ───────────────────────────

async fn engine_upstream(loaded: &'static str) -> String {
    spawn_upstream(
        Router::new()
            .route(
                "/v1/chat/completions",
                post(|| async {
                    (
                        [(header::CONTENT_TYPE, "application/json")],
                        Body::from(JSON_BODY),
                    )
                }),
            )
            .route(
                "/api/ps",
                axum::routing::get(move || async move {
                    let body =
                        format!("{{\"models\":[{{\"name\":\"{loaded}\",\"size_vram\":1000}}]}}");
                    (
                        [(header::CONTENT_TYPE, "application/json")],
                        Body::from(body),
                    )
                }),
            ),
    )
    .await
}

#[tokio::test]
async fn local_residency_boost_prefers_loaded_model() {
    // same grade, same engine host but different ports; engine-for-match
    // uses base_url prefix, so wire each model to its own engine URL.
    let a = engine_upstream("model-a").await;
    let b = engine_upstream("model-b").await;
    let mut cfg = Config::default();
    // weight says try b first; residency flips it once a is loaded
    cfg.models = vec![
        model("eng/a", "B", &["local"], &a, 1.0, (0.0, 0.0)),
        model("eng/b", "B", &["local"], &b, 5.0, (0.0, 0.0)),
    ];
    let t = test_app(cfg);
    // inject residency state: engine matching a's base_url has model-a loaded
    t.local.swap(arouter::localeng::LocalState {
        engines: vec![arouter::localeng::EngineState {
            url: a.trim_end_matches("/v1").to_string(),
            kind: arouter::localeng::EngineKind::Ollama,
            loaded: std::iter::once((
                "model-a".to_string(),
                arouter::localeng::LoadedModel {
                    size_vram: 1,
                    expires_unix: None,
                },
            ))
            .collect(),
            busy: false,
        }],
        fetched_unix: 1,
        stale: false,
    });

    let resp = post_chat(
        &t.app,
        "/local/v1/chat/completions",
        "B",
        false,
        HeaderMap::new(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(hstr(&resp, "x-arouter-upstream"), "eng/a");
}

#[tokio::test]
async fn local_sticky_overrides_weight_within_grade() {
    let a = good_json().await;
    let b = good_json().await;
    let mut cfg = Config::default();
    cfg.models = vec![
        model("eng/a", "B", &["local"], &a, 1.0, (0.0, 0.0)),
        model("eng/b", "B", &["local"], &b, 5.0, (0.0, 0.0)),
    ];
    // same grade for the reorder to be legal; eng/b wins on weight
    let keys = KeyStore::default();
    let reg = Registry::from_config(&cfg, &keys).unwrap();
    let rank_a = reg
        .models
        .iter()
        .find(|m| m.id == "eng/a")
        .unwrap()
        .grade_rank;
    let rank_b = reg
        .models
        .iter()
        .find(|m| m.id == "eng/b")
        .unwrap()
        .grade_rank;
    assert_eq!(rank_a, rank_b, "same grade for the reorder to be legal");

    // caller "dev" has eng/a warm: it floats above the weight winner
    let t = test_app(cfg);
    t.sticky.record("dev", rank_a, "eng/a");

    let r = post_chat(
        &t.app,
        "/local/v1/chat/completions",
        "B",
        false,
        user_hdr("dev"),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(hstr(&r, "x-arouter-upstream"), "eng/a");

    // a different caller still gets the weight-preferred model
    let r2 = post_chat(
        &t.app,
        "/local/v1/chat/completions",
        "B",
        false,
        user_hdr("other"),
    )
    .await;
    assert_eq!(r2.status(), StatusCode::OK);
    assert_eq!(hstr(&r2, "x-arouter-upstream"), "eng/b");
}

#[tokio::test]
async fn local_sticky_disabled_when_ttl_zero() {
    let a = good_json().await;
    let b = good_json().await;
    let mut cfg = Config::default();
    cfg.routing.sticky_local_secs = 0;
    cfg.models = vec![
        model("eng/a", "B", &["local"], &a, 1.0, (0.0, 0.0)),
        model("eng/b", "B", &["local"], &b, 5.0, (0.0, 0.0)),
    ];
    let t = test_app(cfg);
    let h = bearer("dev");
    for _ in 0..2 {
        let r = post_chat(&t.app, "/local/v1/chat/completions", "B", false, h.clone()).await;
        assert_eq!(r.status(), StatusCode::OK);
    }
    // sticky off: no entries recorded, status shows no local engines either
    let snap = t.usage.snapshot();
    assert!(!snap["usage_by_model"].as_object().unwrap().is_empty());
}
