//! Model capabilities: which requests a model can actually execute.
//!
//! A request requiring a capability (an image part → `vision`, a
//! `reasoning_effort` → `reasoning`) is routed only to models that declare
//! it, so a text-only model never receives an image prompt and a
//! non-reasoning model never receives a reasoning request. Capability
//! knowledge comes from explicit `[[models]].capabilities` (authoritative),
//! the curated `src/base/capabilities.toml` table, and a narrow name heuristic.

use serde::Deserialize;

/// Additive capability flags. `Capabilities::default()` is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities(u8);

impl Capabilities {
    pub const NONE: Capabilities = Capabilities(0);
    /// Accepts image inputs (multimodal).
    pub const VISION: Capabilities = Capabilities(1 << 0);
    /// Accepts `reasoning_effort` (a reasoning model).
    pub const REASONING: Capabilities = Capabilities(1 << 1);

    pub fn contains(self, other: Capabilities) -> bool {
        (self.0 & other.0) == other.0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn union(self, other: Capabilities) -> Capabilities {
        Capabilities(self.0 | other.0)
    }

    /// Human-readable names, for logs and error details.
    pub fn names(self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.contains(Capabilities::VISION) {
            out.push("vision");
        }
        if self.contains(Capabilities::REASONING) {
            out.push("reasoning");
        }
        out
    }

    /// Parse config/table capability tokens (`vision`, `reasoning`).
    /// Unknown tokens are ignored with a warning — a typo must not make a
    /// model invisibly lose every capability.
    pub fn parse(tokens: &[String]) -> Capabilities {
        let mut caps = Capabilities::NONE;
        for t in tokens {
            match t.trim().to_ascii_lowercase().as_str() {
                "vision" => caps = caps.union(Capabilities::VISION),
                "reasoning" => caps = caps.union(Capabilities::REASONING),
                other => tracing::warn!(capability = other, "unknown model capability; ignored"),
            }
        }
        caps
    }
}

/// Capabilities a request needs, detected from the chat-completions
/// payload (already in OpenAI shape — anthropic requests were translated
/// on entry, and wire-specific translation happens downstream).
pub fn required(payload: &serde_json::Value) -> Capabilities {
    let mut caps = Capabilities::NONE;
    if needs_vision(payload) {
        caps = caps.union(Capabilities::VISION);
    }
    if let Some(serde_json::Value::String(effort)) = payload.get("reasoning_effort") {
        if effort != "none" {
            caps = caps.union(Capabilities::REASONING);
        }
    }
    caps
}

/// An image anywhere in `messages` means the serving model must be
/// multimodal. Handles OpenAI `image_url` parts (object or string URL) and
/// Codex `input_image` parts for robustness.
fn needs_vision(payload: &serde_json::Value) -> bool {
    let Some(messages) = payload
        .get("messages")
        .and_then(serde_json::Value::as_array)
    else {
        return false;
    };
    messages.iter().any(|m| {
        let Some(parts) = m.get("content").and_then(serde_json::Value::as_array) else {
            return false;
        };
        parts.iter().any(|p| {
            matches!(
                p.get("type").and_then(serde_json::Value::as_str),
                Some("image_url") | Some("input_image")
            ) || p.get("image_url").is_some()
        })
    })
}

/// Narrow name heuristic for models not covered by explicit config or the
/// table. Only unambiguous markers, so an over-claim never routes an image
/// to a text-only model.
pub fn infer(id: &str) -> Capabilities {
    let name = id.rsplit('/').next().unwrap_or(id).to_ascii_lowercase();
    let mut caps = Capabilities::NONE;
    const VISION_MARKERS: [&str; 8] = [
        "vision",
        "vl",
        "omni",
        "pixtral",
        "llava",
        "minicpm-v",
        "internvl",
        "qwen-vl",
    ];
    if VISION_MARKERS.iter().any(|k| name.contains(k)) {
        caps = caps.union(Capabilities::VISION);
    }
    if name.contains("reasoning") || name.contains("thinking") {
        caps = caps.union(Capabilities::REASONING);
    }
    caps
}

/// A model's capability set. Explicit config is authoritative: a non-empty
/// `[[models]].capabilities` list REPLACES the table and heuristic (manual
/// config always wins — list the full set). Empty/absent falls through to
/// the curated table ∪ name heuristic.
pub fn for_model(explicit: &[String], id: &str, table: &Table) -> Capabilities {
    if !explicit.is_empty() {
        return Capabilities::parse(explicit);
    }
    table.lookup(id).union(infer(id))
}

/// Whether a model can execute a request requiring `required`.
pub fn supports(model: Capabilities, required: Capabilities) -> bool {
    model.contains(required)
}

// ─────────────────────────── curated table ───────────────────────────

const CAPABILITIES_TOML: &str = include_str!("capabilities.toml");

#[derive(Debug, Deserialize)]
struct RawTable {
    #[allow(dead_code)]
    #[serde(default)]
    capabilities_version: u32,
    #[serde(default)]
    entry: Vec<RawEntry>,
}

#[derive(Debug, Deserialize)]
struct RawEntry {
    /// `match` is a keyword; serde-rename keeps the TOML key.
    #[serde(rename = "match", default)]
    matches_: Vec<String>,
    #[serde(default)]
    capabilities: Vec<String>,
}

/// Curated capability table (`src/base/capabilities.toml`), resolved by glob
/// against the model id and its last path segment (mirroring catalog
/// lookup). UNION across entries: a model can match both a vision entry
/// and a reasoning entry.
#[derive(Debug, Clone, Default)]
pub struct Table {
    entries: Vec<(Vec<String>, Capabilities)>,
}

impl Table {
    /// The embedded table, parsed. A malformed embedded table disables
    /// table-derived capabilities rather than invalidating the registry.
    pub fn embedded() -> Table {
        match toml::from_str::<RawTable>(CAPABILITIES_TOML) {
            Ok(raw) => Table {
                entries: raw
                    .entry
                    .into_iter()
                    .map(|e| (e.matches_, Capabilities::parse(&e.capabilities)))
                    .collect(),
            },
            Err(e) => {
                tracing::error!(error = %e, "embedded capability table invalid; table capabilities disabled");
                Table::default()
            }
        }
    }

    pub fn lookup(&self, id: &str) -> Capabilities {
        let name = id.rsplit('/').next().unwrap_or(id);
        let mut caps = Capabilities::NONE;
        for (patterns, ecaps) in &self.entries {
            if patterns
                .iter()
                .any(|p| crate::glob::glob_match(p, id) || crate::glob::glob_match(p, name))
            {
                caps = caps.union(*ecaps);
            }
        }
        caps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(json: &str) -> serde_json::Value {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn vision_detected_in_content_arrays() {
        let p = payload(
            r#"{"messages":[{"role":"user","content":[
                {"type":"text","text":"look"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,QQ=="}}
            ]}]}"#,
        );
        assert!(required(&p).contains(Capabilities::VISION));
    }

    #[test]
    fn vision_detected_with_string_image_url() {
        let p = payload(
            r#"{"messages":[{"role":"user","content":[
                {"type":"image_url","image_url":"https://x/y.png"}
            ]}]}"#,
        );
        assert!(required(&p).contains(Capabilities::VISION));
    }

    #[test]
    fn plain_text_needs_nothing() {
        let p = payload(r#"{"messages":[{"role":"user","content":"hi"}]}"#);
        assert!(required(&p).is_empty());
    }

    #[test]
    fn reasoning_effort_needs_reasoning() {
        let p = payload(r#"{"reasoning_effort":"high","messages":[]}"#);
        assert!(required(&p).contains(Capabilities::REASONING));
        assert!(!required(&p).contains(Capabilities::VISION));
    }

    #[test]
    fn reasoning_effort_none_needs_nothing() {
        let p = payload(r#"{"reasoning_effort":"none","messages":[]}"#);
        assert!(required(&p).is_empty());
    }

    #[test]
    fn parse_is_lenient() {
        assert_eq!(
            Capabilities::parse(&["vision".into(), "REASONING".into(), "bogus".into()]),
            Capabilities::VISION.union(Capabilities::REASONING)
        );
    }

    #[test]
    fn inference_marks_vision_and_reasoning_names() {
        assert!(infer("ollama/qwen2.5vl:7b").contains(Capabilities::VISION));
        assert!(infer("x/pixtral-12b").contains(Capabilities::VISION));
        assert!(infer("x/deepseek-reasoning").contains(Capabilities::REASONING));
        assert!(infer("x/gemini-3-flash-thinking").contains(Capabilities::REASONING));
        assert!(infer("x/plain-text").is_empty());
    }

    #[test]
    fn table_marks_multimodal_and_reasoning_families() {
        let t = Table::embedded();
        // multimodal + reasoning
        assert!(t.lookup("or/openai/gpt-5.2").contains(Capabilities::VISION));
        assert!(t
            .lookup("or/openai/gpt-5.2")
            .contains(Capabilities::REASONING));
        // multimodal, reasoning only for the opus tier
        assert!(t
            .lookup("or/anthropic/claude-opus-4.6")
            .contains(Capabilities::VISION));
        assert!(t
            .lookup("or/anthropic/claude-opus-4.6")
            .contains(Capabilities::REASONING));
        assert!(t
            .lookup("or/anthropic/claude-sonnet-4.5")
            .contains(Capabilities::VISION));
        // gemini is natively multimodal; gemini-3 also reasons
        assert!(t
            .lookup("google/gemini-3.8-flash-low")
            .contains(Capabilities::VISION));
        assert!(t
            .lookup("google/gemini-3.8-flash-low")
            .contains(Capabilities::REASONING));
        assert!(t
            .lookup("google/gemini-2.5-pro")
            .contains(Capabilities::VISION));
        // a quantized text coder is neither
        assert!(t.lookup("strata/qwen3.8-flash-next-coder-iq1_m").is_empty());
    }

    #[test]
    fn for_model_explicit_overrides_table() {
        let t = Table::embedded();
        // empty → table ∪ heuristic
        let caps = for_model(&[], "or/openai/gpt-5.2", &t);
        assert!(caps.contains(Capabilities::VISION));
        assert!(caps.contains(Capabilities::REASONING));
        // a non-empty list is authoritative: it can drop a table claim
        let caps = for_model(&["vision".into()], "or/openai/gpt-5.2", &t);
        assert!(caps.contains(Capabilities::VISION));
        assert!(!caps.contains(Capabilities::REASONING));
        // explicit add on a model the table does not cover
        let caps = for_model(&["vision".into()], "ollama/qwen3:14b", &t);
        assert!(caps.contains(Capabilities::VISION));
    }
}
