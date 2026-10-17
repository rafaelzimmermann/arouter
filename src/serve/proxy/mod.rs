//! HTTP layer: axum server, OpenAI-completions proxy with SSE passthrough,
//! per-caller identity, failover across the attempt list, Prometheus
//! `/metrics`, and `X-Arouter-*` observability headers.
//!
//! Organization (review §3.1):
//! - `routes` — router, thin handlers, observation endpoints, request
//!   validation, and the candidate-set builder;
//! - `attempt` — failover walk, per-attempt helpers, and success relay;
//! - `tap` — SSE usage scanning and exactly-once stream finalization;
//! - `sse` — client/upstream stream adapters (Anthropic re-emit).
//!
//! This module owns the immutable [`Runtime`] snapshot and [`AppState`].

pub(crate) mod attempt;
pub(crate) mod routes;
mod sse;
mod tap;

pub use routes::{router, CallerErr};
pub use tap::{TapMeta, TapStream, UsageScan};

use crate::config::{CallerMap, Config};
use crate::health::Health;
use crate::metrics::Metrics;
use crate::quota;
use crate::registry::Registry;
use crate::usage::UsageLog;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

pub(crate) mod hdr {
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
    /// The PRE-overlay config (manual + discovery-merged). The base is
    /// what web-triggered rebuilds re-derive from — re-applying the
    /// overlay to an already-merged config would lose disabled models
    /// (E10c).
    pub base: Arc<Config>,
    /// The overlay-merged config actually serving.
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
    /// E10c overlay diagnostics — what the overlay disabled and which
    /// entries were rejected (surfaced in `/status.json`).
    pub overlay: Arc<crate::overlay::Diag>,
}

impl Runtime {
    /// Build the full snapshot from a (possibly discovery-merged)
    /// config with NO overlay (tests, startup before the first
    /// reload). Pure — spawns nothing; pollers bind at startup.
    pub fn build(cfg: Config, keys: &crate::config::KeyStore) -> anyhow::Result<Runtime> {
        Self::build_with_overlay(cfg, keys, &crate::overlay::Overlay::default(), Vec::new())
    }

    /// `build` + overlay merge (E10c): applies `overlay` to a CLONE of
    /// `base` (total — overlay additions from earlier generations are
    /// stripped first), keeps `base` untouched for the next rebuild,
    /// and reports what the overlay did in `/status.json`.
    pub fn build_with_overlay(
        base: Config,
        keys: &crate::config::KeyStore,
        overlay: &crate::overlay::Overlay,
        load_errors: Vec<String>,
    ) -> anyhow::Result<Runtime> {
        let mut cfg = base.clone();
        let mut diag = overlay.apply(&mut cfg);
        let apply_errors = std::mem::take(&mut diag.errors);
        diag.errors = load_errors.into_iter().chain(apply_errors).collect();
        let registry =
            Arc::new(Registry::from_config(&cfg, keys).map_err(|e| e.context("registry"))?);
        let callers = Arc::new(CallerMap::from_config(&cfg, keys));
        let provider_map = Arc::new(quota::ProviderMap {
            explicit: cfg.quota.provider_map.clone(),
        });
        let (quota_harvest, quota_sources) = crate::quotaeng::scan_sources(&cfg, &registry);
        Ok(Runtime {
            base: Arc::new(base),
            cfg: Arc::new(cfg),
            registry,
            callers,
            provider_map,
            quota_harvest: Arc::new(quota_harvest),
            quota_sources: Arc::new(quota_sources),
            overlay: Arc::new(diag),
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
    /// E10a web shell: sessions + claim bootstrap.
    pub web: Arc<crate::web::Web>,
    /// E10b API tokens (`ark_…`): hashed store, scopes narrow routing.
    pub tokens: Arc<crate::tokens::TokenStore>,
    /// E10c overlay directory (`state_dir`): `web.toml` lives here and
    /// `rebuild_now` re-reads it.
    pub overlay_dir: std::path::PathBuf,
    /// E10c: key store for `rebuild_now` (api_key resolution on
    /// overlay additions).
    pub keys: Arc<crate::config::KeyStore>,
}

impl AppState {
    /// Current runtime snapshot — bind once per request (review F6).
    pub fn runtime(&self) -> Arc<Runtime> {
        self.runtime.read().unwrap().clone()
    }
}

/// E10c: rebuild the runtime from the CURRENT snapshot's config + the
/// overlay on disk, and swap it in atomically. Used by the overlay
/// write endpoints right after `web.toml` changes — the periodic
/// reload loop keeps doing full (config + discovery) rebuilds.
/// Returns false (and keeps the previous snapshot) if the merged
/// result is invalid.
pub fn rebuild_now(st: &AppState, why: &str) -> bool {
    let base = {
        let rt = st.runtime();
        (*rt.base).clone()
    };
    let (overlay, load_errors) = crate::overlay::Overlay::load(&st.overlay_dir);
    match Runtime::build_with_overlay(base, &st.keys, &overlay, load_errors) {
        Ok(next) => {
            *st.runtime.write().unwrap() = Arc::new(next);
            tracing::info!(
                why,
                models = st.runtime.read().unwrap().registry.models.len(),
                "runtime rebuilt (overlay)"
            );
            true
        }
        Err(e) => {
            tracing::error!(why, error = %e, "overlay rebuild invalid — keeping old runtime");
            false
        }
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
