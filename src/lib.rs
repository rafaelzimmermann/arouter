//! arouter — quota-aware LLM agent router.
//!
//! Library crate so unit and e2e tests can exercise the router without a
//! process boundary. The module tree mirrors the request path, bottom-up:
//!
//! ```text
//! base  → config → routing → state → money → wire → serve
//!                                     ops (CLI/support)
//! ```
//!
//! See `docs/architecture.md` for the design.
//!
//! Modules are also re-exported at the crate root (`arouter::quota`,
//! `arouter::registry`, …) so existing downstream paths and the e2e
//! tests keep working; the grouped paths are the canonical ones.

/// Pure building blocks: grades/ladders, glob matching, the embedded
/// auto-grading catalog, and the capability table.
pub mod base {
    pub mod capabilities;
    pub mod catalog;
    pub mod glob;
    pub mod grades;
}

/// Configuration schema, validation, key store, and caller identity.
pub mod config;

/// Static routing: registry resolution and upstream model discovery.
pub mod routing {
    pub mod discover;
    pub mod registry;
}

/// Dynamic routing state: quota windows, quota engine, health breaker,
/// and local-engine residency.
pub mod state {
    pub mod health;
    pub mod localeng;
    pub mod quota;
    pub mod quotaeng;
}

/// Budget and spend accounting: admission holds, usage log, API tokens.
pub mod money {
    pub mod budget;
    pub mod tokens;
    pub mod usage;
}

/// Wire adapters: Anthropic↔OpenAI translation, Codex Responses, and
/// native Gemini generateContent.
pub mod wire {
    pub mod codex;
    pub mod gemini;
    pub mod translate;
}

/// Serving surface: HTTP proxy, web shell/session API, metrics, monitor.
pub mod serve {
    pub mod metrics;
    pub mod monitor;
    pub mod proxy;
    pub mod web;
}

/// Operational commands and state overlay (init/doctor/web.toml/admin.toml).
pub mod ops {
    pub mod admin;
    pub mod doctor;
    pub mod init;
    pub mod overlay;
}

// Compatibility re-exports: the layer map above is canonical, while these
// keep the historical flat paths (`arouter::quota`, `crate::registry`, …)
// stable for tests, CLI code, and any external consumer.
pub use base::{capabilities, catalog, glob, grades};
pub use money::{budget, tokens, usage};
pub use ops::{admin, doctor, init, overlay};
pub use routing::{discover, registry};
pub use serve::{metrics, monitor, proxy, web};
pub use state::{health, localeng, quota, quotaeng};
pub use wire::{codex, gemini, translate};
