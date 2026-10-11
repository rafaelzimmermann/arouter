//! arouter — quota-aware LLM agent router.
//!
//! Library crate so unit and e2e tests can exercise the router without a
//! process boundary. See PLAN.md and docs/ for the design.

pub mod budget;
pub mod catalog;
pub mod codex;
pub mod config;
pub mod discover;
pub mod doctor;
pub mod glob;
pub mod grades;
pub mod health;
pub mod init;
pub mod localeng;
pub mod metrics;
pub mod monitor;
pub mod proxy;
pub mod quota;
pub mod quotaeng;
pub mod registry;
pub mod tokens;
pub mod translate;
pub mod usage;
pub mod web;
