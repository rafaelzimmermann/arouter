//! Native Gemini wire (`wire = "gemini"`): a model entry speaks Google's
//! `generateContent` API instead of OpenAI chat-completions.
//!
//! arouter's internal currency is chat-completions, so this adapter
//! translates both ways: a chat-completions request becomes `contents` +
//! `systemInstruction` + `generationConfig`, and Google's response (JSON
//! or SSE) becomes a chat-completions response again. Everything above the
//! adapter — routing, grades, pools, budgets, quota harvest, health,
//! metrics and both client wires (openai + anthropic) — stays unchanged.
//!
//! The mapping follows the mature peers of this pattern (LiteLLM's
//! `llms/gemini/chat/transformation.py`, NewAPI's `relay/channel/gemini`,
//! routectl's Rust `gemini` adapter), including their hard-won tables:
//!
//! - `system`/`developer` messages collect into `systemInstruction` —
//!   Gemini has no per-turn system role, so a system message anywhere in
//!   the history still lands there.
//! - assistant `tool_calls` → `functionCall` parts; `tool` results →
//!   `functionResponse` parts inside a **user** turn (Google's shape).
//! - consecutive same-role turns merge: Google requires alternating
//!   user/model turns and rejects a run of one role.
//! - `finishReason` → OpenAI `finish_reason`: safety/policy tokens become
//!   `content_filter`, `MAX_TOKENS` becomes `length`, unknown tokens stay
//!   `stop` (labelling an unknown termination as policy would be wrong).
//! - `usageMetadata` → OpenAI `usage`: `thoughtsTokenCount` counts into
//!   `completion_tokens` (Google bills thinking as output) and stays
//!   visible as `completion_tokens_details.reasoning_tokens`;
//!   `cachedContentTokenCount` becomes `prompt_tokens_details.cached_tokens`.
//! - an empty candidate plus `promptFeedback.blockReason` is a blocked
//!   **200** with `finish_reason = content_filter`, not an upstream error.
//! - JSON-schema keys Google's `Schema` type does not carry (`$schema`,
//!   `additionalProperties`, `$defs`, `definitions`) are stripped from
//!   `functionDeclarations.parameters`.

use serde_json::{json, Map, Value};

/// Google's JSON API surface. A model entry's `base_url` is this (or a
/// proxy in front of it); the adapter appends `/models/{name}:generateContent`.
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Adapter failure: 400 = the caller's payload has no Gemini form.
#[derive(Debug)]
pub struct GeminiError {
    pub status: u16,
    pub message: String,
}

impl GeminiError {
    fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: 400,
            message: msg.into(),
        }
    }
}

impl std::fmt::Display for GeminiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

fn invalid(msg: impl Into<String>) -> GeminiError {
    GeminiError::bad_request(msg)
}

// ─────────────────────────── request: openai → gemini ───────────────────────────

/// Chat-completions payload → `generateContent` body. `model` is the
/// upstream model name (already resolved by the registry) — Gemini carries
/// it in the URL, so it is only used here to pick the right thinking knob.
pub fn openai_to_gemini(body: &Value, model: &str) -> Result<Value, GeminiError> {
    let msgs = body
        .get("messages")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
        .ok_or_else(|| invalid("messages must be a nonempty array"))?;

    let mut system_parts: Vec<Value> = vec![];
    // (role, parts) — merged so roles strictly alternate (Google's rule).
    let mut turns: Vec<(String, Vec<Value>)> = vec![];

    for msg in msgs {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        match role {
            "system" | "developer" => {
                system_parts.extend(parts_of(msg.get("content")));
            }
            "user" => {
                push_turn(&mut turns, "user", parts_of(msg.get("content")));
            }
            "tool" => match function_response_part(msg) {
                Some(part) => {
                    push_turn(&mut turns, "user", vec![part]);
                }
                None => {
                    return Err(invalid(
                        "tool result needs a name or tool_call_id to become a functionResponse",
                    ))
                }
            },
            "assistant" => {
                let mut parts = parts_of(msg.get("content"));
                if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        if let Some(part) = function_call_part(call) {
                            parts.push(part);
                        }
                    }
                }
                push_turn(&mut turns, "model", parts);
            }
            other => return Err(invalid(format!("role {other:?} has no Gemini form"))),
        }
    }

    if turns.is_empty() {
        return Err(invalid("no user/model turn to send"));
    }

    let mut contents: Vec<Value> = vec![];
    for turn in turns {
        contents.push(json!({ "role": turn.0, "parts": turn.1 }));
    }

    let mut out = Map::new();
    out.insert("contents".into(), Value::Array(contents));
    if !system_parts.is_empty() {
        out.insert("systemInstruction".into(), json!({ "parts": system_parts }));
    }
    if let Some(decls) = tool_declarations(body.get("tools")) {
        out.insert("tools".into(), json!([{ "functionDeclarations": decls }]));
    }
    if let Some(cfg) = tool_config(body.get("tool_choice")) {
        out.insert("toolConfig".into(), cfg);
    }
    if let Some(cfg) = gen_config(body, model) {
        out.insert("generationConfig".into(), cfg);
    }
    // A caller that already speaks some Gemini (safety knobs) keeps them.
    if let Some(v) = body
        .get("safetySettings")
        .or_else(|| body.get("safety_settings"))
    {
        out.insert("safetySettings".into(), v.clone());
    }
    Ok(Value::Object(out))
}

/// Append a turn, merging into the previous one when the role repeats —
/// Google rejects consecutive turns of the same role.
fn push_turn(turns: &mut Vec<(String, Vec<Value>)>, role: &str, parts: Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    let same = turns.last().map(|t| t.0 == role).unwrap_or(false);
    if same {
        let last = turns.len().saturating_sub(1);
        turns[last].1.extend(parts);
    } else {
        turns.push((role.to_string(), parts));
    }
}

/// Message content (string, or an array of OpenAI content blocks) → Gemini
/// `Part`s. Unsupported blocks are dropped with a WARN rather than a hard
/// failure — a stray block must not kill an otherwise valid request.
fn parts_of(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(s)) => vec![json!({ "text": s })],
        Some(Value::Array(items)) => {
            let mut out: Vec<Value> = vec![];
            for el in items {
                out.extend(block_part(el));
            }
            out
        }
        _ => vec![],
    }
}

fn block_part(el: &Value) -> Vec<Value> {
    match el.get("type").and_then(Value::as_str).unwrap_or("") {
        "text" => vec![json!({ "text": el.get("text").and_then(Value::as_str).unwrap_or("") })],
        "image_url" => match image_part(el) {
            Some(p) => vec![p],
            None => vec![],
        },
        "input_audio" => audio_part(el),
        "file" => file_part(el),
        other => {
            tracing::debug!(block = other, "gemini: dropping unsupported content block");
            vec![]
        }
    }
}

/// `data:` URLs become `inlineData`; anything else is passed as a
/// `fileData.fileUri` (Google fetches public URLs itself).
fn image_part(el: &Value) -> Option<Value> {
    let url = el
        .pointer("/image_url/url")
        .and_then(Value::as_str)
        .unwrap_or("");
    if url.is_empty() {
        return None;
    }
    if let Some(spec) = url.strip_prefix("data:") {
        let sep = spec.find(';')?;
        let mime = &spec[..sep];
        let rest = &spec[sep + 1..];
        if !rest.starts_with("base64:") || rest.len() <= 7 {
            return None;
        }
        let data = &rest[7..];
        if mime.is_empty() || data.is_empty() {
            return None;
        }
        return Some(json!({ "inlineData": { "mimeType": mime, "data": data } }));
    }
    Some(json!({ "fileData": { "fileUri": url } }))
}

fn audio_part(el: &Value) -> Vec<Value> {
    let data = el
        .pointer("/input_audio/data")
        .and_then(Value::as_str)
        .unwrap_or("");
    if data.is_empty() {
        return vec![];
    }
    let fmt = el
        .pointer("/input_audio/format")
        .and_then(Value::as_str)
        .unwrap_or("wav");
    let mime = format!("audio/{}", fmt);
    vec![json!({ "inlineData": { "mimeType": mime, "data": data } })]
}

fn file_part(el: &Value) -> Vec<Value> {
    let uri = el
        .pointer("/file/file_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    if uri.is_empty() {
        return vec![];
    }
    vec![json!({ "fileData": { "fileUri": uri } })]
}

/// Assistant `tool_calls[]` → a `functionCall` part. OpenAI's arguments are
/// a JSON *string*; Gemini wants an object — unparseable arguments become
/// `{}` so the model gets a call shape it can repair.
fn function_call_part(call: &Value) -> Option<Value> {
    let name = call
        .pointer("/function/name")
        .and_then(Value::as_str)
        .unwrap_or("");
    if name.is_empty() {
        return None;
    }
    let args = match call.pointer("/function/arguments") {
        None => json!({}),
        Some(Value::Null) => json!({}),
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(v) => v,
            Err(_) => {
                tracing::debug!("gemini: tool arguments are not JSON; sending an empty object");
                json!({})
            }
        },
        Some(v) => v.clone(),
    };
    Some(json!({ "functionCall": { "name": name, "args": args } }))
}

/// A `tool` result message → a `functionResponse` part. Google keys
/// responses by *name*, so `tool_call_id` is the fallback name.
fn function_response_part(msg: &Value) -> Option<Value> {
    let name = msg.pointer("/name").and_then(Value::as_str).unwrap_or(
        msg.pointer("/tool_call_id")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    if name.is_empty() {
        return None;
    }
    let response = match msg.get("content") {
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(v) if v.is_object() => v,
            _ => json!({ "result": s }),
        },
        Some(Value::Array(parts)) => json!({ "result": text_of_parts(parts) }),
        _ => json!({ "result": "" }),
    };
    Some(json!({ "functionResponse": { "name": name, "response": response } }))
}

fn text_of_parts(parts: &[Value]) -> String {
    let mut out: Vec<String> = vec![];
    for p in parts {
        out.push(
            p.get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        );
    }
    out.join("\n")
}

/// OpenAI `tools[]` → Gemini `functionDeclarations[]`.
fn tool_declarations(tools: Option<&Value>) -> Option<Value> {
    let items = tools.and_then(Value::as_array)?;
    let mut decls: Vec<Value> = vec![];
    for t in items {
        let fnv = t.get("function").unwrap_or(t);
        let name = fnv.get("name").and_then(Value::as_str).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let mut decl = Map::new();
        decl.insert("name".into(), json!(name));
        if let Some(d) = fnv.get("description") {
            decl.insert("description".into(), d.clone());
        }
        if let Some(p) = fnv.get("parameters").or_else(|| fnv.get("input_schema")) {
            decl.insert("parameters".into(), sanitize_schema(p));
        }
        decls.push(Value::Object(decl));
    }
    if decls.is_empty() {
        return None;
    }
    Some(Value::Array(decls))
}

/// Does this value carry nothing useful? (Empty `required`/`enum` lists
/// are rejected by Google's `Schema` type.)
fn emptyish(v: &Value) -> bool {
    match v.as_array() {
        Some(a) => a.is_empty(),
        None => v.is_null(),
    }
}

/// JSON-schema keys Google's `Schema` type does not carry.
fn sanitize_schema(s: &Value) -> Value {
    let Some(obj) = s.as_object() else {
        return s.clone();
    };
    let mut out = Map::new();
    for (k, v) in obj.iter() {
        match k.as_str() {
            "$schema" | "additionalProperties" | "$defs" | "definitions" | "default" => {}
            "required" | "enum" if emptyish(v) => {}
            "properties" => {
                if let Some(props) = v.as_object() {
                    let mut props_out = Map::new();
                    for (pname, pschema) in props.iter() {
                        props_out.insert(pname.into(), sanitize_schema(pschema));
                    }
                    out.insert(k.into(), Value::Object(props_out));
                }
            }
            "items" => {
                out.insert(k.into(), sanitize_schema(v));
            }
            _ => {
                out.insert(k.into(), v.clone());
            }
        }
    }
    Value::Object(out)
}

/// OpenAI `tool_choice` → Gemini `toolConfig` (named choices keep an
/// allow-list of exactly that function).
fn tool_config(choice: Option<&Value>) -> Option<Value> {
    let obj = choice?;
    let name = obj
        .pointer("/function/name")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !name.is_empty() {
        return Some(json!({
            "functionCallingConfig": {
                "mode": "ANY",
                "allowedFunctionNames": [name],
            },
        }));
    }
    match obj.get("type").and_then(Value::as_str).unwrap_or("") {
        "none" => Some(json!({ "mode": "NONE" })),
        "auto" => Some(json!({ "mode": "AUTO" })),
        "required" | "any" => Some(json!({ "mode": "ANY" })),
        _ => None,
    }
}

/// Sampling knobs onto `generationConfig`; OpenAI knobs with no Gemini
/// slot (`seed`, `n`, `logit_bias`, `logprobs`) are dropped with a WARN.
fn gen_config(body: &Value, model: &str) -> Option<Value> {
    let mut cfg = Map::new();
    for (from, to) in [
        ("temperature", "temperature"),
        ("top_p", "topP"),
        ("top_k", "topK"),
    ] {
        if let Some(v) = body.get(from).filter(|v| !v.is_null()) {
            cfg.insert(to.into(), v.clone());
        }
    }
    for key in ["max_tokens", "max_completion_tokens"] {
        if let Some(v) = body.get(key).and_then(Value::as_u64) {
            cfg.insert("maxOutputTokens".into(), json!(v));
            break;
        }
    }
    match body.get("stop") {
        Some(Value::String(s)) => {
            cfg.insert("stopSequences".into(), json!([s]));
        }
        Some(Value::Array(items)) => {
            cfg.insert("stopSequences".into(), Value::Array(items.clone()));
        }
        _ => {}
    }
    match body
        .pointer("/response_format/type")
        .and_then(Value::as_str)
        .unwrap_or("")
    {
        "json_object" => {
            cfg.insert("responseMimeType".into(), json!("application/json"));
        }
        "json_schema" => {
            cfg.insert("responseMimeType".into(), json!("application/json"));
            if let Some(s) = body.pointer("/response_format/json_schema/schema") {
                cfg.insert("responseSchema".into(), sanitize_schema(s));
            }
        }
        _ => {}
    }
    if let Some(t) = thinking_config(model, body) {
        cfg.insert("thinkingConfig".into(), t);
    }
    for k in ["seed", "n", "logit_bias", "logprobs", "parallel_tool_calls"] {
        if body.get(k).is_some() {
            tracing::debug!(knob = k, "gemini: no generationConfig slot; knob dropped")
        }
    }
    if cfg.is_empty() {
        return None;
    }
    Some(Value::Object(cfg))
}

/// Gemini 3 replaced the numeric `thinkingBudget` with a qualitative
/// `thinkingLevel`; 2.5 and older generations only take the budget.
fn thinking_config(model: &str, body: &Value) -> Option<Value> {
    let effort = body
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .unwrap_or("");
    let level = match effort {
        "minimal" | "low" => "low",
        "medium" => "medium",
        "high" | "xhigh" => "high",
        _ => return None,
    };
    if model.contains("gemini-3") {
        return Some(json!({ "thinkingLevel": level }));
    }
    let budget = match effort {
        "minimal" => 0,
        "low" => 1024,
        "medium" => 8192,
        _ => -1, // dynamic budget: the model decides, capped by maxOutputTokens
    };
    Some(json!({ "thinkingBudget": budget }))
}

/// Endpoint for one attempt. `name` may already carry the `models/`
/// prefix (Google's own listing does), and a `base_url` copied from the
/// docs (`…/v1beta/models`) is tolerated.
pub fn responses_url(base_url: &str, name: &str, stream: bool) -> String {
    let trimmed = base_url.trim_end_matches('/');
    let base = trimmed.strip_suffix("/models").unwrap_or(trimmed);
    let path = if name.starts_with("models/") {
        name.to_string()
    } else {
        format!("models/{}", name)
    };
    let verb = if stream {
        "streamGenerateContent?alt=sse"
    } else {
        "generateContent"
    };
    format!("{base}/{path}:{verb}")
}

// ─────────────────────────── response: gemini → openai ───────────────────────────

/// Google's `finishReason` → OpenAI `finish_reason`. Tool-protocol
/// failures are not errors: with call parts present the turn is a
/// `tool_calls` turn, without them it is a plain `stop`.
pub fn map_finish_reason(reason: &str, has_calls: bool) -> &'static str {
    if has_calls {
        return "tool_calls";
    }
    match reason {
        "" | "STOP" => "stop",
        "MAX_TOKENS" => "length",
        "SAFETY" | "RECITATION" | "PROHIBITED_CONTENT" | "BLOCKLIST" | "SPII" | "IMAGE_SAFETY"
        | "LANGUAGE" => "content_filter",
        "MALFORMED_FUNCTION_CALL" | "UNEXPECTED_TOOL_CALL" | "TOO_MANY_TOOL_CALLS" => "stop",
        other => {
            tracing::debug!(finish = other, "gemini: unmapped finishReason; using stop");
            "stop"
        }
    }
}

/// `usageMetadata` of a whole response/event → chat-completions `usage`.
/// Thinking tokens are part of the output bill, so they join
/// `completion_tokens` (and stay visible as `reasoning_tokens`).
fn usage_of(resp: &Value) -> Option<Value> {
    let meta = resp.get("usageMetadata")?;
    let prompt = meta
        .get("promptTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let out = meta
        .get("candidatesTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let thought = meta
        .get("thoughtsTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = meta
        .get("cachedContentTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total = meta
        .get("totalTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(prompt + out + thought);
    let mut usage = Map::new();
    usage.insert("prompt_tokens".into(), json!(prompt));
    usage.insert("completion_tokens".into(), json!(out + thought));
    usage.insert("total_tokens".into(), json!(total));
    if cached > 0 {
        usage.insert(
            "prompt_tokens_details".into(),
            json!({ "cached_tokens": cached }),
        );
    }
    if thought > 0 {
        usage.insert(
            "completion_tokens_details".into(),
            json!({ "reasoning_tokens": thought }),
        );
    }
    Some(Value::Object(usage))
}

/// One `Candidate` → (text, reasoning, tool_calls). Gemini streams whole
/// `functionCall` parts, so no argument reassembly is needed here.
fn candidate_parts(cand: &Value) -> (String, String, Vec<Value>) {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls: Vec<Value> = vec![];
    let parts: Vec<Value> = match cand.pointer("/content/parts").and_then(Value::as_array) {
        None => vec![],
        Some(a) => a.clone(),
    };
    for part in parts {
        match part.get("functionCall") {
            Some(call) => {
                let name = call.get("name").and_then(Value::as_str).unwrap_or("");
                if name.is_empty() {
                    continue;
                }
                let args = match call.get("args") {
                    None => "".to_string(),
                    Some(Value::Null) => "".to_string(),
                    Some(v) => v.to_string(),
                };
                let id = format!("call-{}-{}", calls.len(), name);
                calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": args },
                }));
            }
            None => {
                let chunk = part.get("text").and_then(Value::as_str).unwrap_or("");
                match part.get("thought").and_then(Value::as_bool) {
                    Some(true) => {
                        reasoning += chunk;
                    }
                    _ => {
                        text += chunk;
                    }
                }
            }
        }
    }
    (text, reasoning, calls)
}

/// A buffered `GenerateContentResponse` → chat-completions JSON.
pub fn gemini_to_openai(resp: &Value, model: &str) -> Result<Value, String> {
    if let Some(m) = resp.pointer("/error/message").and_then(Value::as_str) {
        return Err(m.to_string());
    }
    let candidates: Vec<Value> = match resp.get("candidates").and_then(Value::as_array) {
        None => vec![],
        Some(a) => a.clone(),
    };
    let block = resp
        .pointer("/promptFeedback/blockReason")
        .and_then(Value::as_str)
        .unwrap_or("");
    if candidates.is_empty() {
        if !block.is_empty() {
            // A prompt-level block is a well-formed refusal, not a failure.
            return Ok(blocked(
                resp,
                model,
                &format!("prompt-level block: {}", block),
            ));
        }
        return Err("gemini returned no candidates".into());
    }
    let (text, reasoning, calls) = candidate_parts(&candidates[0]);
    let finish = map_finish_reason(
        candidates[0]
            .get("finishReason")
            .and_then(Value::as_str)
            .unwrap_or(""),
        !calls.is_empty(),
    );
    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    message.insert(
        "content".into(),
        if text.is_empty() && !calls.is_empty() {
            Value::Null
        } else {
            json!(text)
        },
    );
    if !calls.is_empty() {
        message.insert("tool_calls".into(), Value::Array(calls));
    }
    if !reasoning.is_empty() {
        message.insert("reasoning_content".into(), json!(reasoning));
    }
    Ok(json!({
        "id": resp.get("responseId").and_then(Value::as_str).unwrap_or("gemini"),
        "object": "chat.completion",
        "created": unix_now_secs(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": finish,
        }],
        "usage": usage_of(resp).unwrap_or_else(null_usage),
    }))
}

/// A blocked generation, shaped like OpenAI's content-policy response.
fn blocked(resp: &Value, model: &str, why: &str) -> Value {
    json!({
        "id": resp.get("responseId").and_then(Value::as_str).unwrap_or("gemini"),
        "object": "chat.completion",
        "created": unix_now_secs(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": json!({ "role": "assistant", "content": "" }),
            "finish_reason": "content_filter",
            "content_filter_reason": why,
        }],
        "usage": usage_of(resp).unwrap_or_else(null_usage),
    })
}

fn null_usage() -> Value {
    json!({ "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 })
}

// ───────────────────────── streaming: gemini SSE → openai SSE ─────────────────────────

/// Streamable piece of a response under construction.
#[derive(Debug, PartialEq)]
pub enum Delta {
    Text(String),
    Reasoning(String),
}

/// Accumulates Gemini `streamGenerateContent` events into a chat-completions
/// response. Used directly for buffered relays and as the state machine for
/// the streaming translator.
pub struct GeminiParser {
    model: String,
    id: String,
    created: u64,
    role_sent: bool,
    final_sent: bool,
    text: String,
    reasoning: String,
    tool_calls: Vec<Value>,
    finish_reason: Option<&'static str>,
    usage: Option<Value>,
    /// A terminal event (`finishReason` / block) was seen.
    pub terminal: bool,
}

impl GeminiParser {
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_string(),
            id: format!("gemini-{}", unix_now_secs()),
            created: unix_now_secs(),
            role_sent: false,
            final_sent: false,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: vec![],
            finish_reason: None,
            usage: None,
            terminal: false,
        }
    }

    /// Feed one parsed SSE event. Tool calls arrive whole in Gemini's
    /// stream, so they are buffered and emitted at the terminal chunk —
    /// matching how the codex adapter already behaves.
    pub fn event(&mut self, ev: &Value) -> Result<Vec<Delta>, String> {
        if let Some(m) = ev.pointer("/error/message").and_then(Value::as_str) {
            return Err(m.to_string());
        }
        if let Some(u) = usage_of(ev) {
            // Placement is model/version dependent: keep the latest, so
            // a terminal event's own numbers supersede an interim
            // snapshot (and an interim-only stream still reports usage).
            self.usage = Some(u);
        }
        if ev.pointer("/promptFeedback/blockReason").is_some() {
            self.terminal = true;
            self.finish_reason = Some("content_filter");
            return Ok(vec![]);
        }
        let Some(cands) = ev.get("candidates").and_then(Value::as_array) else {
            return Ok(vec![]);
        };
        if cands.is_empty() {
            return Ok(vec![]);
        }
        let (text, reasoning, calls) = candidate_parts(&cands[0]);
        let mut deltas: Vec<Delta> = vec![];
        if !text.is_empty() {
            self.text += &text;
            deltas.push(Delta::Text(text));
        }
        if !reasoning.is_empty() {
            self.reasoning += &reasoning;
            deltas.push(Delta::Reasoning(reasoning));
        }
        self.tool_calls.extend(calls);
        if let Some(r) = cands[0].get("finishReason").and_then(Value::as_str) {
            self.finish_reason = Some(map_finish_reason(r, !self.tool_calls.is_empty()));
            self.terminal = true;
        }
        Ok(deltas)
    }

    fn finish_reason(&self) -> &'static str {
        if !self.tool_calls.is_empty() {
            return "tool_calls";
        }
        self.finish_reason.unwrap_or("stop")
    }

    /// Buffered chat-completions response object.
    pub fn completion(&self) -> Value {
        let calls = !self.tool_calls.is_empty();
        let mut message = Map::new();
        message.insert("role".into(), json!("assistant"));
        message.insert(
            "content".into(),
            if self.text.is_empty() && calls {
                Value::Null
            } else {
                json!(self.text)
            },
        );
        if calls {
            message.insert("tool_calls".into(), Value::Array(self.tool_calls.clone()));
        }
        if !self.reasoning.is_empty() {
            message.insert("reasoning_content".into(), json!(self.reasoning));
        }
        json!({
            "id": self.id,
            "object": "chat.completion",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "message": Value::Object(message),
                "finish_reason": self.finish_reason(),
            }],
            "usage": self.usage.clone().unwrap_or_else(null_usage),
        })
    }

    /// Chat-completions SSE frames: the role chunk before the first delta,
    /// the deltas, then at the terminal event the buffered tool calls, the
    /// finish chunk, the usage chunk and `[DONE]` — the shape `TapStream`'s
    /// usage scan and the anthropic re-emitter already expect.
    pub fn chunks_for(&mut self, deltas: Vec<Delta>) -> Vec<String> {
        let mut out: Vec<String> = vec![];
        if (!deltas.is_empty() || self.terminal) && !self.role_sent {
            self.role_sent = true;
            out.push(self.chunk(json!({ "role": "assistant", "content": "" }), None));
        }
        for d in deltas {
            let delta = match d {
                Delta::Text(t) => json!({ "content": t }),
                Delta::Reasoning(r) => json!({ "reasoning_content": r }),
            };
            out.push(self.chunk(delta, None));
        }
        if self.terminal && !self.final_sent {
            self.final_sent = true;
            for f in self.tool_call_frames() {
                out.push(f);
            }
            out.push(self.chunk(json!({}), Some(self.finish_reason())));
            let usage = self.usage.clone().unwrap_or_else(null_usage);
            out.push(format!(
                "data: {}\n\n",
                json!({
                    "id": self.id,
                    "object": "chat.completion.chunk",
                    "created": self.created,
                    "model": self.model,
                    "choices": [],
                    "usage": usage,
                })
            ));
            out.push("data: [DONE]\n\n".into());
        }
        out
    }

    /// Tool calls arrive whole in Gemini's stream; OpenAI streams them as
    /// `tool_calls` deltas, so emit one delta per buffered call at the
    /// terminal chunk (the codex adapter's buffered behaviour, made
    /// protocol-correct).
    fn tool_call_frames(&self) -> Vec<String> {
        let mut out: Vec<String> = vec![];
        for (i, call) in self.tool_calls.iter().enumerate() {
            let callee = match call.get("function") {
                Some(f) => f.clone(),
                None => json!({}),
            };
            let id = match call.get("id").and_then(Value::as_str) {
                Some(s) => s.to_string(),
                None => format!("call-{}", i),
            };
            let delta = json!({
                "tool_calls": [{ "index": i, "id": id, "type": "function", "function": callee }],
            });
            out.push(self.chunk(delta, None));
        }
        out
    }

    fn chunk(&self, delta: Value, finish: Option<&'static str>) -> String {
        format!(
            "data: {}\n\n",
            json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "created": self.created,
                "model": self.model,
                "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
            })
        )
    }
}

/// Incremental Gemini-SSE → chat-completions-SSE translator for streaming
/// relays: feed upstream bytes, get back bytes to forward.
pub struct SseTranslator {
    buf: Vec<u8>,
    parser: GeminiParser,
    frames: std::collections::VecDeque<String>,
    failed: Option<String>,
}

impl SseTranslator {
    pub fn new(model: &str) -> Self {
        Self {
            buf: vec![],
            parser: GeminiParser::new(model),
            frames: Default::default(),
            failed: None,
        }
    }

    pub fn push_bytes(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        while let Some(pos) = find_frame_end(&self.buf) {
            let frame: Vec<u8> = self.buf.drain(..pos + 2).collect();
            let body = &frame[..frame.len() - 2];
            for line in body.split(|&b| b == b'\n') {
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                if let Some(payload) = line.strip_prefix(b"data:") {
                    self.handle_data(payload.strip_prefix(b" ").unwrap_or(payload));
                }
            }
        }
        if self.buf.len() > 1_048_576 {
            self.buf.clear();
        }
    }

    fn handle_data(&mut self, payload: &[u8]) {
        if payload == b"[DONE]" || payload.is_empty() {
            return;
        }
        if self.failed.is_some() {
            return;
        }
        let Ok(ev) = serde_json::from_slice::<Value>(payload) else {
            return;
        };
        match self.parser.event(&ev) {
            Ok(deltas) => {
                for f in self.parser.chunks_for(deltas) {
                    self.frames.push_back(f);
                }
            }
            Err(e) => {
                self.failed = Some(e);
            }
        }
    }

    /// Bytes ready to forward, if any.
    pub fn pop_frame(&mut self) -> Option<String> {
        self.frames.pop_front()
    }

    /// The accumulated buffered response (only valid after a terminal event).
    pub fn completion(&self) -> Value {
        self.parser.completion()
    }

    /// Upstream EOF: Err if no terminal event arrived (a silent death is
    /// never reported to the client as success — review F4's spirit).
    pub fn finish(&mut self) -> Result<(), String> {
        if let Some(e) = self.failed.take() {
            return Err(e);
        }
        if self.parser.terminal {
            Ok(())
        } else {
            Err("gemini stream ended without a finishReason".into())
        }
    }
}

fn find_frame_end(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
}

/// Whole-response form for non-streaming clients. `:generateContent`
/// answers with a plain JSON body while `:streamGenerateContent?alt=sse`
/// answers with SSE — a non-streaming caller may hit either (a gateway in
/// front of Google may convert), so both shapes are accepted. A stream
/// that ends without a terminal event is a truncated answer, not an empty
/// success (review F4's spirit).
pub fn parse_buffered_sse(bytes: &[u8], model: &str) -> Result<Value, String> {
    if let Ok(v) = serde_json::from_slice::<Value>(bytes) {
        return gemini_to_openai(&v, model);
    }
    let mut tr = SseTranslator::new(model);
    tr.push_bytes(bytes);
    tr.finish()?;
    Ok(tr.completion())
}

// ─────────────────────────── helpers ───────────────────────────

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

use crate::budget::ReserveGuard;
use crate::config::CallerIdentity;
use crate::quota;
use crate::quotaeng;
use crate::registry::Model;
use crate::serve::proxy::attempt::{record_failure, relay_success, retryable, truncate, Upstream};
use crate::serve::proxy::routes::{api_error_with, wire_error, Wire};
use crate::serve::proxy::{hdr, AppState, Runtime};
use crate::usage::unix_now;
use axum::body::Bytes;
use axum::http::header;
use axum::response::Response;
use futures::Stream;
use std::pin::Pin;
use std::time::{Duration, Instant};

pub(crate) struct GeminiFail {
    pub(crate) last_error: String,
    /// Boxed: `Response` is large and clippy's `result_large_err`
    /// fires on fat Err variants returned from hot paths.
    pub(crate) fatal: Option<Box<Response>>,
}

/// One attempt against Google's `generateContent` API. Mirrors the openai
/// attempt path: headers/body deadlines (F5), a budget hold (F2/F3) and
/// quota windows harvested from real inference responses; retryable
/// failures fail over. Gemini's key rides `x-goog-api-key`, not a bearer
/// token, and its model name lives in the URL rather than the body.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn gemini_attempt(
    st: &AppState,
    rt: &Runtime,
    m: &Model,
    caller: &CallerIdentity,
    profile: &str,
    pool: &str,
    raw_model: &str,
    req: &crate::grades::GradeRequest,
    is_stream: bool,
    payload: &serde_json::Value,
    est: &quota::RequestEstimate,
    cross_from: Option<&str>,
    wire: Wire,
    t0: Instant,
    reserve: Option<ReserveGuard>,
) -> Result<Response, GeminiFail> {
    let fail = |msg: String| GeminiFail {
        last_error: msg,
        fatal: None,
    };
    let body = openai_to_gemini(payload, &m.upstream_name).map_err(|e| {
        // invalid request is the client's fault: no failover, plain 400
        GeminiFail {
            last_error: format!("invalid request: {}", e.message),
            fatal: Some(Box::new(wire_error(
                wire,
                e.status,
                "invalid_request",
                e.message,
            ))),
        }
    })?;
    let url = responses_url(&m.base_url, &m.upstream_name, is_stream);
    let mut rb = st.http.post(&url).json(&body);
    if let Some(key) = &m.api_key {
        // Google's JSON API keys are not bearer tokens
        rb = rb.header("x-goog-api-key", key);
    }
    rb = rb.header(
        header::ACCEPT,
        if is_stream {
            "text/event-stream"
        } else {
            "application/json"
        },
    );
    let prefix = m.id.split('/').next().unwrap_or("").to_string();
    let harvest = rt.quota_harvest.get(&prefix).copied().unwrap_or(true);
    let resp =
        match tokio::time::timeout(Duration::from_secs(rt.cfg.timeouts.headers_secs), rb.send())
            .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(fail(format!("connect error: {e}"))),
            Err(_) => return Err(fail("headers timeout".into())),
        };
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        // E3b: quota headers on failures are ground truth too; a bare 429
        // without them still pins a synthetic window from Retry-After.
        let now = unix_now();
        let windows = quotaeng::harvest(resp.headers());
        if harvest && !windows.is_empty() {
            st.quota.apply(&prefix, windows, now);
        } else if harvest && code == 429 {
            let ra = resp
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(60);
            st.quota
                .apply(&prefix, vec![quotaeng::harvest_429(ra)], now);
        }
        let body_text = truncate(&resp.text().await.unwrap_or_default(), 300);
        let msg = format!("gemini upstream {code}: {body_text}");
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
        // a rejected key is a dead upstream: Google answers 400/403 for
        // "API key not valid", which must fail over like a 503 rather
        // than end the request
        let effective = if code == 401 || code == 403 {
            503
        } else {
            code
        };
        if retryable(effective) {
            st.health.record_failure(&m.id, true);
            return Err(fail(msg));
        }
        st.health.record_failure(&m.id, false);
        return Ok(api_error_with(
            code,
            "upstream_error",
            msg,
            &[(hdr::UPSTREAM, m.id.clone())],
        ));
    }
    let degraded =
        rt.registry.grade_for_request(raw_model, m).1 != req.ceiling && !req.whole_ladder;
    let now = unix_now();
    if harvest {
        let w = quotaeng::harvest(resp.headers());
        if !w.is_empty() {
            st.quota.apply(&prefix, w, now);
        }
    }
    if !is_stream {
        // Google's non-streaming form is a plain JSON body; the streaming
        // form is SSE that a non-streaming caller still needs whole, so
        // both are buffered here. A stream that ends without a terminal
        // event is a truncated answer — fail over, never an empty 200.
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
                st.health.record_failure(&m.id, true);
                return Err(fail(msg));
            }
            Err(_) => {
                let msg = "gemini body stalled past idle_secs".to_string();
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
                    &msg,
                    t0.elapsed().as_millis() as u64,
                );
                st.health.record_failure(&m.id, true);
                return Err(fail(msg));
            }
        };
        let completion = parse_buffered_sse(&bytes, &m.upstream_name).map_err(|e| {
            let msg = e.to_string();
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
            st.health.record_failure(&m.id, true);
            fail(msg)
        })?;
        st.health.record_success(&m.id);
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
            Upstream::Buffered(
                "application/json".into(),
                Bytes::from(completion.to_string()),
            ),
            t0,
            est,
            cross_from,
            wire,
            reserve,
        )
        .await);
    }
    st.health.record_success(&m.id);
    let stream = GeminiSse::new(resp.bytes_stream(), SseTranslator::new(&m.upstream_name));
    Ok(relay_success(
        st,
        rt,
        m,
        caller,
        profile,
        pool,
        raw_model,
        degraded,
        is_stream,
        Upstream::Stream(Box::pin(stream)),
        t0,
        est,
        cross_from,
        wire,
        reserve,
    )
    .await)
}

#[allow(clippy::too_many_arguments)]
/// Native Gemini wire: wraps a Google generateContent-SSE upstream and
/// re-emits chat-completions-SSE. Same relay shape as CodexSse - the
/// Gemini translator does the frame translation, and a stream that ends
/// without a terminal candidate keeps its error (F4), never a silent
/// empty success.
pub(crate) struct GeminiSse<S> {
    inner: S,
    tr: SseTranslator,
    out: std::collections::VecDeque<Bytes>,
}

impl<S> GeminiSse<S>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    pub(crate) fn new(inner: S, tr: SseTranslator) -> Self {
        Self {
            inner,
            tr,
            out: Default::default(),
        }
    }
}

impl<S> Stream for GeminiSse<S>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    type Item = std::io::Result<Bytes>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = &mut *self;
        loop {
            if let Some(b) = this.out.pop_front() {
                return std::task::Poll::Ready(Some(Ok(b)));
            }
            match Pin::new(&mut this.inner).poll_next(cx) {
                std::task::Poll::Ready(Some(Ok(bytes))) => {
                    this.tr.push_bytes(&bytes);
                    while let Some(frame) = this.tr.pop_frame() {
                        this.out.push_back(Bytes::from(frame));
                    }
                    continue;
                }
                std::task::Poll::Ready(Some(Err(e))) => {
                    this.out.clear();
                    return std::task::Poll::Ready(Some(Err(std::io::Error::other(e.to_string()))));
                }
                std::task::Poll::Ready(None) => {
                    return match this.tr.finish() {
                        Ok(()) => std::task::Poll::Ready(None),
                        Err(e) => {
                            this.out.clear();
                            std::task::Poll::Ready(Some(Err(std::io::Error::other(e))))
                        }
                    }
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_translates_roles_and_knobs() {
        let req = json!({
            "model": "ignored",
            "messages": [
                { "role": "system", "content": "be terse" },
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": null,
                  "tool_calls": [{ "id": "c1", "type": "function",
                                   "function": { "name": "lookup",
                                                "arguments": "{\"q\":\"x\"}" } }] },
                { "role": "tool", "tool_call_id": "lookup", "content": "{\"n\":2}" },
                { "role": "user", "content": "thanks" },
            ],
            "temperature": 0.3,
            "max_tokens": 64,
            "stop": ["END"],
        });
        let g = openai_to_gemini(&req, "gemini-2.5-pro").unwrap();
        assert_eq!(g["systemInstruction"]["parts"][0]["text"], "be terse");
        // roles strictly alternate: the tool result merges into the user
        // turn that follows it, and the assistant turn carries the call.
        assert_eq!(g["contents"][0]["role"], "user");
        assert_eq!(g["contents"][1]["role"], "model");
        assert_eq!(g["contents"][2]["role"], "user");
        assert_eq!(
            g["contents"][1]["parts"][0]["functionCall"]["name"],
            "lookup"
        );
        assert_eq!(
            g["contents"][2]["parts"][0]["functionResponse"]["response"]["n"],
            2
        );
        assert_eq!(g["generationConfig"]["maxOutputTokens"], 64);
        assert_eq!(g["generationConfig"]["stopSequences"][0], "END");
    }

    #[test]
    fn thinking_level_only_on_gemini_3() {
        let g3 = openai_to_gemini(
            &json!({
                "messages": [{ "role": "user", "content": "think" }],
                "reasoning_effort": "high",
            }),
            "gemini-3-pro",
        )
        .unwrap();
        assert_eq!(
            g3["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "high"
        );
        let g25 = openai_to_gemini(
            &json!({
                "messages": [{ "role": "user", "content": "think" }],
                "reasoning_effort": "low",
            }),
            "gemini-2.5-pro",
        )
        .unwrap();
        assert_eq!(
            g25["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            1024
        );
    }

    #[test]
    fn unknown_role_is_rejected() {
        let e = openai_to_gemini(
            &json!({ "messages": [{ "role": "wizard", "content": "x" }] }),
            "gemini-2.5-pro",
        )
        .unwrap_err();
        assert_eq!(e.status, 400);
    }

    #[test]
    fn url_shape() {
        let u = responses_url(DEFAULT_BASE_URL, "gemini-2.5-pro", true);
        assert!(u.starts_with("https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:streamGenerateContent"), "{u}");
        let kept = responses_url(
            "https://proxy.example/v1beta/",
            "models/gemini-2.5-pro",
            false,
        );
        assert!(
            kept.ends_with("/models/gemini-2.5-pro:generateContent"),
            "{kept}"
        );
    }

    #[test]
    fn finish_reason_table() {
        assert_eq!(map_finish_reason("STOP", false), "stop");
        assert_eq!(map_finish_reason("MAX_TOKENS", false), "length");
        assert_eq!(map_finish_reason("SAFETY", false), "content_filter");
        assert_eq!(map_finish_reason("SOME_FUTURE_TOKEN", false), "stop");
        assert_eq!(map_finish_reason("MAX_TOKENS", true), "tool_calls");
    }

    #[test]
    fn usage_counts_thoughts_into_output() {
        let u = usage_of(&json!({
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5,
                "thoughtsTokenCount": 7,
                "cachedContentTokenCount": 3,
            },
        }))
        .unwrap_or_else(null_usage);
        assert_eq!(u["completion_tokens"], 12);
        assert_eq!(u["prompt_tokens_details"]["cached_tokens"], 3);
        assert_eq!(u["completion_tokens_details"]["reasoning_tokens"], 7);
    }

    #[test]
    fn buffered_response_maps() {
        let v = gemini_to_openai(
            &json!({
                "candidates": [{
                    "content": { "parts": [{ "text": "hi" }, { "text": " there" }] },
                    "finishReason": "STOP",
                }],
                "usageMetadata": { "promptTokenCount": 3, "candidatesTokenCount": 2 },
            }),
            "gemini-2.5-pro",
        )
        .unwrap_or(null_usage());
        assert_eq!(v["choices"][0]["message"]["content"], "hi there");
        assert_eq!(v["usage"]["prompt_tokens"], 3);
    }

    #[test]
    fn buffered_accepts_both_upstream_shapes() {
        // :generateContent answers with plain JSON; the streaming form
        // is SSE. A non-streaming caller must work against either.
        let whole = format!(
            "{}",
            json!({
                "candidates": [{
                    "content": { "parts": [{ "text": "hi" }] },
                    "finishReason": "STOP",
                }],
                "usageMetadata": { "promptTokenCount": 3 },
            }),
        );
        let out = parse_buffered_sse(whole.as_bytes(), "gemini-2.5-pro").unwrap_or(null_usage());
        assert_eq!(out["choices"][0]["message"]["content"], "hi");
        assert_eq!(out["usage"]["prompt_tokens"], 3);
        let sse = format!(
            "data: {}\n\n",
            json!({
                "candidates": [{
                    "content": { "parts": [{ "text": "hi" }] },
                    "finishReason": "STOP",
                }],
            }),
        );
        let out = parse_buffered_sse(sse.as_bytes(), "gemini-2.5-pro").unwrap_or(null_usage());
        assert_eq!(out["choices"][0]["message"]["content"], "hi");
        // a stream that never reports a finishReason is a truncated
        // answer: fail over, never an empty 200
        assert!(
            parse_buffered_sse(
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}]}\n\n"
                    .as_bytes(),
                "gemini-2.5-pro",
            )
            .is_err(),
            "no terminal event"
        );
    }

    #[test]
    fn tool_calls_roundtrip() {
        let v = gemini_to_openai(
            &json!({
                "candidates": [{
                    "content": { "parts": [{ "functionCall": { "name": "lookup",
                                                               "args": { "q": "x" } } }] },
                    "finishReason": "STOP",
                }],
            }),
            "gemini-2.5-pro",
        )
        .unwrap_or(null_usage());
        assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            v["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            "lookup"
        );
        assert_eq!(
            v["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{\"q\":\"x\"}"
        );
    }

    #[test]
    fn blocked_prompt_feedback() {
        let v = gemini_to_openai(
            &json!({
                "candidates": [],
                "promptFeedback": { "blockReason": "SAFETY" },
            }),
            "gemini-2.5-pro",
        )
        .unwrap_or(null_usage());
        assert_eq!(v["choices"][0]["finish_reason"], "content_filter");
    }

    #[test]
    fn sse_translates() {
        let mut tr = SseTranslator::new("gemini-2.5-pro");
        tr.push_bytes(
            b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"He\"}]}}]}\n\n",
        );
        tr.push_bytes(b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"llo\"}]},");
        tr.push_bytes(b"\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":7,");
        tr.push_bytes(b"\"candidatesTokenCount\":2}}\n\n");
        let mut all = String::new();
        while let Some(f) = tr.pop_frame() {
            all += &f;
        }
        assert!(all.contains("\"content\":\"He\""), "{all}");
        assert!(all.contains("\"content\":\"llo\""), "{all}");
        assert!(all.contains("\"prompt_tokens\":7"), "{all}");
        assert!(all.ends_with("data: [DONE]\n\n"), "{all}");
        tr.finish().unwrap();
    }

    #[test]
    fn sse_without_terminal_fails() {
        let mut tr = SseTranslator::new("m");
        tr.push_bytes(b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"x\"}]}}]}\n\n");
        assert!(tr.finish().is_err(), "no finishReason is not a completion");
    }

    #[test]
    fn schema_keys_google_rejects_are_stripped() {
        let req = json!({
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "pick",
                    "parameters": {
                        "type": "object",
                        "$schema": "http://json-schema.org/draft-07/schema#",
                        "additionalProperties": false,
                        "properties": { "n": { "type": "integer" } },
                        "required": [],
                    },
                },
            }],
        });
        let g = openai_to_gemini(&req, "gemini-2.5-pro").unwrap();
        assert_eq!(
            g["tools"][0]["functionDeclarations"][0]["parameters"]["type"],
            "object"
        );
        assert!(
            g.pointer("/tools/0/functionDeclarations/0/parameters/$schema")
                .is_none(),
            "google rejects $schema"
        );
        assert!(
            g.pointer("/tools/0/functionDeclarations/0/parameters/additionalProperties")
                .is_none(),
            "google rejects additionalProperties"
        );
        assert!(
            g.pointer("/tools/0/functionDeclarations/0/parameters/required")
                .is_none(),
            "empty required arrays are dropped"
        );
    }
}
