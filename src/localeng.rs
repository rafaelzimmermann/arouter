//! Local engine awareness (E8): which models are resident on local
//! inference engines (ollama, llama.cpp/strata), whether an engine is
//! busy, and warm-cache (sticky) routing for the local pool.
//!
//! Routing stays grade-primary (§4/§5): residency/busyness/stickiness
//! only reorder candidates *within* the same allowed grade — they never
//! change which grades serve a request, never cross pool boundaries,
//! and never exclude a candidate. Unknown engine state = no boost.

use crate::quota::unix_now;
use crate::registry::Model;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::RwLock;

// ─────────────────────────── state ───────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineKind {
    Ollama,
    LlamaCpp,
}

impl EngineKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EngineKind::Ollama => "ollama",
            EngineKind::LlamaCpp => "llamacpp",
        }
    }
}

/// One resident model on an engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedModel {
    /// Bytes resident in (V)RAM as reported by the engine.
    pub size_vram: u64,
    /// When the engine will evict it (keep_alive expiry), if reported.
    pub expires_unix: Option<u64>,
}

/// Poll-updated snapshot for one engine URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineState {
    pub url: String,
    pub kind: EngineKind,
    /// Normalized resident model name → info.
    pub loaded: BTreeMap<String, LoadedModel>,
    /// Any slot currently processing (llama.cpp `/slots`; ollama reports
    /// no such field — it queues internally).
    pub busy: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalState {
    pub engines: Vec<EngineState>,
    pub fetched_unix: u64,
    /// true until the first successful poll, and again while polls fail.
    pub stale: bool,
}

impl Default for LocalState {
    fn default() -> Self {
        Self {
            engines: Vec::new(),
            fetched_unix: 0,
            stale: true,
        }
    }
}

/// Shared, poll-swapped state (same discipline as QuotaWatcher: the lock
/// is never held across `.await`).
#[derive(Debug, Default)]
pub struct LocalEngines {
    inner: RwLock<LocalState>,
}

impl LocalEngines {
    pub fn new(state: LocalState) -> Self {
        Self {
            inner: RwLock::new(state),
        }
    }

    pub fn current(&self) -> LocalState {
        self.inner.read().expect("localeng lock").clone()
    }

    pub fn swap(&self, state: LocalState) {
        *self.inner.write().expect("localeng lock") = state;
    }
}

// ─────────────────────────── parsers (pure) ───────────────────────────

/// ollama `GET /api/ps` →
/// `{"models":[{"name":"gemma3:12b","size_vram":N,"expires_at":"RFC3339"}]}`
pub fn parse_ollama_ps(v: &Value) -> BTreeMap<String, LoadedModel> {
    let mut out = BTreeMap::new();
    let Some(models) = v.get("models").and_then(Value::as_array) else {
        return out;
    };
    for m in models {
        let Some(name) = m.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        out.insert(
            normalize_model_name(name),
            LoadedModel {
                size_vram: m.get("size_vram").and_then(|s| s.as_u64()).unwrap_or(0),
                expires_unix: m
                    .get("expires_at")
                    .and_then(|e| e.as_str())
                    .and_then(crate::quota::parse_rfc3339_unix),
            },
        );
    }
    out
}

/// llama.cpp `GET /slots` → array of slot objects with `model` (a path)
/// and (newer builds) `is_processing`. busy = any slot processing.
pub fn parse_llamacpp_slots(v: &Value) -> (BTreeMap<String, LoadedModel>, bool) {
    let mut out = BTreeMap::new();
    let mut busy = false;
    let Some(slots) = v.as_array() else {
        return (out, busy);
    };
    for s in slots {
        if let Some(path) = s.get("model").and_then(|m| m.as_str()) {
            if !path.is_empty() {
                out.insert(
                    normalize_model_name(path),
                    LoadedModel {
                        size_vram: 0, // /slots does not report sizes
                        expires_unix: None,
                    },
                );
            }
        }
        if s.get("is_processing")
            .and_then(|p| p.as_bool())
            .unwrap_or(false)
        {
            busy = true;
        }
    }
    (out, busy)
}

/// Normalize an engine-side model name for matching against registry
/// `upstream_name`s: drop directories and common weight-file extensions.
fn normalize_model_name(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    let base = base
        .strip_suffix(".gguf")
        .or_else(|| base.strip_suffix(".safetensors"))
        .or_else(|| base.strip_suffix(".bin"))
        .unwrap_or(base);
    base.to_string()
}

/// Does an engine-resident name refer to the model we would send as
/// `upstream_name`? Exact, normalized, or substring (engines may report
/// quantization suffixes or full paths pi/arouter never sees).
pub fn matches_upstream(loaded: &str, upstream: &str) -> bool {
    if loaded == upstream {
        return true;
    }
    let l = normalize_model_name(loaded);
    let u = normalize_model_name(upstream);
    l == u || l.contains(&u) || u.contains(&l)
}

/// The engine whose URL prefixes this model's base_url, if any.
pub fn engine_for<'a>(state: &'a LocalState, base_url: &str) -> Option<&'a EngineState> {
    state
        .engines
        .iter()
        .find(|e| base_url.starts_with(e.url.trim_end_matches('/')))
}

/// Grade-preserving residency boost (E8.1): stable-reorder so that models
/// already resident on a non-busy engine float to the front of their
/// grade group. Everything else keeps the incoming order exactly.
pub fn boost_residency<'a>(ordered: Vec<&'a Model>, state: &LocalState) -> Vec<&'a Model> {
    let mut scored: Vec<(&Model, u8)> = ordered
        .into_iter()
        .map(|m| {
            let boost = engine_for(state, &m.base_url)
                .filter(|e| !e.busy)
                .map(|e| {
                    e.loaded
                        .keys()
                        .any(|l| matches_upstream(l, &m.upstream_name))
                })
                .unwrap_or(false);
            (m, u8::from(!boost))
        })
        .collect();
    scored.sort_by(|a, b| a.0.grade_rank.cmp(&b.0.grade_rank).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(m, _)| m).collect()
}

// ─────────────────────────── sticky (warm-cache) routing ───────────────────────────

/// Warm-cache stickiness for the local pool (E8.2): consecutive requests
/// from the same caller for the same grade stick to the model that just
/// served them, so the engine's prompt/KV cache stays hot. Entries expire
/// after `sticky_secs`; recording is only done for local-pool successes.
#[derive(Debug, Default)]
pub struct StickyMap {
    ttl_secs: u64,
    inner: RwLock<BTreeMap<(String, usize), (String, u64)>>,
}

impl StickyMap {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            ttl_secs,
            inner: RwLock::new(BTreeMap::new()),
        }
    }

    pub fn record(&self, caller: &str, grade_rank: usize, model_id: &str) {
        if self.ttl_secs == 0 {
            return;
        }
        self.inner
            .write()
            .expect("sticky lock")
            .insert((caller.into(), grade_rank), (model_id.into(), unix_now()));
    }

    /// The sticky model for this caller+grade, if recorded and fresh.
    pub fn get(&self, caller: &str, grade_rank: usize) -> Option<String> {
        if self.ttl_secs == 0 {
            return None;
        }
        let (id, ts) = self
            .inner
            .read()
            .expect("sticky lock")
            .get(&(caller.into(), grade_rank))?
            .clone();
        if unix_now().saturating_sub(ts) <= self.ttl_secs {
            Some(id)
        } else {
            None
        }
    }
}

/// Grade-preserving sticky boost: the caller's warm model floats to the
/// front of its grade group; everything else keeps the incoming order.
pub fn boost_sticky<'a>(
    ordered: Vec<&'a Model>,
    sticky: &StickyMap,
    caller: &str,
) -> Vec<&'a Model> {
    // grade ranks present in the list → per-grade sticky targets
    let mut sticky_by_rank: BTreeMap<usize, String> = BTreeMap::new();
    for m in &ordered {
        if let Some(id) = sticky.get(caller, m.grade_rank) {
            sticky_by_rank.entry(m.grade_rank).or_insert(id);
        }
    }
    let mut scored: Vec<(&Model, u8)> = ordered
        .into_iter()
        .map(|m| {
            let is_sticky = sticky_by_rank
                .get(&m.grade_rank)
                .is_some_and(|id| id == &m.id);
            (m, u8::from(!is_sticky))
        })
        .collect();
    scored.sort_by(|a, b| a.0.grade_rank.cmp(&b.0.grade_rank).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(m, _)| m).collect()
}

// ─────────────────────────── poll loop ───────────────────────────

/// Poll every configured engine once and swap the shared state. Probe
/// order for `auto`: ollama `/api/ps`, then llama.cpp `/slots`. Failed
/// polls keep the previous snapshot and mark it stale.
pub async fn poll_once(
    shared: &LocalEngines,
    http: &reqwest::Client,
    urls: &[(String, Option<String>)],
) {
    let mut engines = Vec::new();
    let mut any_ok = false;
    for (url, kind) in urls {
        if let Some(s) = probe_engine(http, url, kind.as_deref()).await {
            any_ok = true;
            engines.push(s);
        }
    }
    shared.swap(LocalState {
        engines,
        fetched_unix: unix_now(),
        // a fully-failed sweep (no engine reachable) keeps stale = true so
        // the monitor can flag it; individual misses keep last-known loaded
        stale: !any_ok,
    });
}

async fn probe_engine(
    http: &reqwest::Client,
    url: &str,
    kind: Option<&str>,
) -> Option<EngineState> {
    let root = url.trim_end_matches('/');
    let want_ollama = |k: &str| k == "ollama";
    let want_llama = |k: &str| k == "llamacpp" || k == "llama.cpp" || k == "strata";
    match kind {
        Some(k) if want_ollama(k) => ollama_state(http, root).await,
        Some(k) if want_llama(k) => llamacpp_state(http, root).await,
        // auto: ollama first, then llama.cpp
        _ => match ollama_state(http, root).await {
            Some(s) => Some(s),
            None => llamacpp_state(http, root).await,
        },
    }
}

async fn ollama_state(http: &reqwest::Client, root: &str) -> Option<EngineState> {
    let v: Value = http
        .get(format!("{root}/api/ps"))
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    Some(EngineState {
        url: root.to_string(),
        kind: EngineKind::Ollama,
        loaded: parse_ollama_ps(&v),
        busy: false,
    })
}

async fn llamacpp_state(http: &reqwest::Client, root: &str) -> Option<EngineState> {
    let v: Value = http
        .get(format!("{root}/slots"))
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    let (loaded, busy) = parse_llamacpp_slots(&v);
    Some(EngineState {
        url: root.to_string(),
        kind: EngineKind::LlamaCpp,
        loaded,
        busy,
    })
}

/// Long-running watcher task (mirrors quota::run_watcher's discipline).
pub async fn run_watcher(
    shared: std::sync::Arc<LocalEngines>,
    http: reqwest::Client,
    urls: Vec<(String, Option<String>)>,
    interval: std::time::Duration,
) {
    loop {
        poll_once(&shared, &http, &urls).await;
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
#[allow(clippy::useless_vec)]
mod tests {
    use super::*;
    use crate::registry::Model;

    fn m(id: &str, grade_rank: usize, base_url: &str, upstream: &str) -> Model {
        Model {
            id: id.into(),
            upstream_name: upstream.into(),
            grade_rank,
            base_url: base_url.into(),
            pools: vec!["local".into()],
            ..Model::default()
        }
    }

    #[test]
    fn ollama_ps_parsing() {
        let v = serde_json::json!({
            "models": [
                {"name": "gemma3:12b", "size_vram": 8103792,
                 "expires_at": "2026-10-11T21:00:00Z"},
                {"name": "/models/qwen3.8-coder-iq1_m.gguf"}
            ]
        });
        let loaded = parse_ollama_ps(&v);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded["gemma3:12b"].size_vram, 8103792);
        assert_eq!(loaded["gemma3:12b"].expires_unix, Some(1791752400));
        assert_eq!(loaded["qwen3.8-coder-iq1_m"].size_vram, 0);
    }

    #[test]
    fn llamacpp_slots_parsing_and_busy() {
        let v = serde_json::json!([
            {"id": 0, "model": "/models/big-coder.gguf", "is_processing": true},
            {"id": 1, "model": "/models/small.gguf", "is_processing": false}
        ]);
        let (loaded, busy) = parse_llamacpp_slots(&v);
        assert!(busy);
        assert!(loaded.contains_key("big-coder"));
        assert!(loaded.contains_key("small"));
    }

    #[test]
    fn name_matching_flavors() {
        assert!(matches_upstream(
            "qwen3.8-coder-iq1_m.gguf",
            "qwen3.8-coder-iq1_m"
        ));
        assert!(matches_upstream(
            "/models/qwen3.8-coder-iq1_m",
            "qwen3.8-coder-iq1_m"
        ));
        assert!(matches_upstream("gemma3:12b", "gemma3:12b-q4")); // substring
        assert!(!matches_upstream("llama3", "qwen3.8"));
    }

    #[test]
    fn residency_boost_is_grade_preserving() {
        let state = LocalState {
            engines: vec![EngineState {
                url: "http://127.0.0.1:11434".into(),
                kind: EngineKind::Ollama,
                loaded: BTreeMap::from([(
                    "llama3".into(),
                    LoadedModel {
                        size_vram: 1,
                        expires_unix: None,
                    },
                )]),
                busy: false,
            }],
            fetched_unix: 1,
            stale: false,
        };
        // grade 0: engine model NOT loaded (cold) + a remote; grade 1: loaded
        let models = vec![
            m("ollama/cold", 0, "http://127.0.0.1:11434/v1", "mistral"),
            m("zai/x", 0, "https://api.z.ai/v4", "glm"),
            m("ollama/llama3", 1, "http://127.0.0.1:11434/v1", "llama3"),
            m("zai/y", 1, "https://api.z.ai/v4", "glm"),
        ];
        let ordered: Vec<&Model> = models.iter().collect();
        let out = boost_residency(ordered, &state);
        let ids: Vec<&str> = out.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            ["ollama/cold", "zai/x", "ollama/llama3", "zai/y"],
            "grade order preserved; loaded llama3 floats above zai/y within grade 1"
        );
    }

    #[test]
    fn busy_engine_gets_no_boost() {
        let state = LocalState {
            engines: vec![EngineState {
                url: "http://127.0.0.1:11434".into(),
                kind: EngineKind::LlamaCpp,
                loaded: BTreeMap::from([(
                    "llama3".into(),
                    LoadedModel {
                        size_vram: 1,
                        expires_unix: None,
                    },
                )]),
                busy: true,
            }],
            fetched_unix: 1,
            stale: false,
        };
        let models = vec![
            m("zai/y", 1, "https://api.z.ai/v4", "glm"),
            m("ollama/llama3", 1, "http://127.0.0.1:11434/v1", "llama3"),
        ];
        let ordered: Vec<&Model> = models.iter().collect();
        let out = boost_residency(ordered, &state);
        // stable: incoming order kept — busy engine's resident model does not float
        assert_eq!(out[0].id, "zai/y");
        assert_eq!(out[1].id, "ollama/llama3");
    }

    #[test]
    fn sticky_record_get_expiry() {
        let s = StickyMap::new(600);
        assert_eq!(s.get("alice", 0), None);
        s.record("alice", 0, "ollama/llama3");
        assert_eq!(s.get("alice", 0).as_deref(), Some("ollama/llama3"));
        assert_eq!(s.get("other", 0), None);
        assert_eq!(s.get("alice", 1), None);
        // ttl=0 disables entirely
        let off = StickyMap::new(0);
        off.record("alice", 0, "x");
        assert_eq!(off.get("alice", 0), None);
    }

    #[test]
    fn sticky_boost_floats_warm_model_within_grade() {
        let s = StickyMap::new(600);
        s.record("alice", 1, "ollama/warm");
        let models = vec![
            m("zai/y", 1, "https://api.z.ai/v4", "glm"),
            m("ollama/warm", 1, "http://127.0.0.1:11434/v1", "llama3"),
            m("ollama/other", 1, "http://127.0.0.1:11434/v1", "mistral"),
        ];
        let ordered: Vec<&Model> = models.iter().collect();
        let out = boost_sticky(ordered, &s, "alice");
        assert_eq!(out[0].id, "ollama/warm");
        // other callers unaffected
        let models2 = vec![
            m("zai/y", 1, "https://api.z.ai/v4", "glm"),
            m("ollama/warm", 1, "http://127.0.0.1:11434/v1", "llama3"),
        ];
        let ordered2: Vec<&Model> = models2.iter().collect();
        let out2 = boost_sticky(ordered2, &s, "someone-else");
        assert_eq!(out2[0].id, "zai/y");
    }
}
