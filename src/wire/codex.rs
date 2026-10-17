//! Native Codex subscription adapter (E7): lets a model entry speak the
//! ChatGPT Codex Responses wire (`wire = "codex"`) instead of OpenAI
//! chat-completions.
//!
//! Replaces `contrib/codex-bridge`: no Node/Pi dependency, no loopback
//! port, no bridge key. The router itself translates chat-completions
//! requests into Codex Responses SSE requests and back, so routing,
//! quota, budget, health and both client wires (openai + anthropic)
//! work unchanged.
//!
//! Credential policy (same as the bridge): arouter never owns the login.
//! It reads `~/.codex/auth.json` (or `CODEX_AUTH_FILE`) on every request
//! and forwards the current access token; Codex owns login and renewal.
//! Only `auth_mode = "chatgpt"` subscriptions are used — never a
//! separately billed OpenAI API key.

use crate::quota::QuotaWindow;
use axum::http::HeaderMap;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
const DEFAULT_INSTRUCTIONS: &str = "You are a helpful assistant.";

static CHUNK_SEQ: AtomicU64 = AtomicU64::new(1);

/// Adapter failure with the HTTP status the proxy should surface
/// (503 = fix the login, 400 = bad request, 502 = upstream weirdness).
#[derive(Debug)]
pub struct CodexError {
    pub status: u16,
    pub message: String,
}

impl CodexError {
    fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: 400,
            message: msg.into(),
        }
    }
    fn login(msg: &str) -> Self {
        Self {
            status: 503,
            message: format!("{msg}; run codex login"),
        }
    }
}

impl std::fmt::Display for CodexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

fn invalid(msg: impl Into<String>) -> CodexError {
    CodexError::bad_request(msg)
}

// ───────────────────────────── login ─────────────────────────────

/// Path of the Codex CLI login file: `CODEX_AUTH_FILE` or `~/.codex/auth.json`.
pub fn auth_file() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CODEX_AUTH_FILE") {
        if !p.trim().is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    crate::config::home_dir().map(|h| h.join(".codex/auth.json"))
}

pub struct CodexLogin {
    pub token: String,
    pub account_id: String,
}

/// Read the Codex CLI login and validate it is a live ChatGPT
/// subscription session. Called per request — renewal happens in Codex.
pub async fn read_login() -> Result<CodexLogin, CodexError> {
    let path = auth_file().ok_or_else(|| CodexError::login("No home directory for Codex login"))?;
    let raw = tokio::fs::read_to_string(&path)
        .await
        .map_err(|_| CodexError::login("Cannot read Codex login"))?;
    read_login_from(&raw, unix_now_secs()).map_err(|m| CodexError::login(&m))
}

/// Pure core of [`read_login`] (unit-tested).
pub fn read_login_from(raw: &str, now: u64) -> Result<CodexLogin, String> {
    let auth: Value =
        serde_json::from_str(raw).map_err(|_| "Cannot parse Codex login".to_string())?;
    if auth.get("auth_mode").and_then(Value::as_str) != Some("chatgpt") {
        return Err("A ChatGPT subscription login is required".into());
    }
    let token = auth
        .pointer("/tokens/access_token")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| "Codex login has no access token".to_string())?
        .to_string();
    let claims = jwt_claims(&token).ok_or_else(|| "Invalid Codex login token".to_string())?;
    let exp = claims.get("exp").and_then(Value::as_u64).unwrap_or(0);
    if exp <= now {
        return Err("Codex login expired; refresh it in Codex".into());
    }
    let account_id = claims
        .pointer(&format!("/{}", jwt_claim_path_ptr()))
        .and_then(|c| c.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .ok_or_else(|| "Codex login token has no account id".to_string())?
        .to_string();
    Ok(CodexLogin { token, account_id })
}

/// JSON pointer for the nested JWT claim namespace, escaped per RFC 6901.
fn jwt_claim_path_ptr() -> String {
    JWT_CLAIM_PATH.replace('~', "~0").replace('/', "~1")
}

/// Decode the payload segment of a JWT into parsed JSON claims.
pub fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = b64url_decode(payload)?;
    serde_json::from_slice(&bytes).ok()
}

fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for ch in s.bytes() {
        if ch == b'=' {
            break;
        }
        let v = match ch {
            b'A'..=b'Z' => (ch - b'A') as u32,
            b'a'..=b'z' => (ch - b'a' + 26) as u32,
            b'0'..=b'9' => (ch - b'0' + 52) as u32,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ─────────────────────── request translation ───────────────────────

/// Translate an OpenAI chat-completions body into a Codex Responses
/// request body (always `stream: true` — the upstream has no buffered
/// mode; buffering is done on our side for non-streaming clients).
/// `model` is the upstream model name.
pub fn openai_to_codex(body: &Value, model: &str) -> Result<Value, CodexError> {
    let obj = body
        .as_object()
        .ok_or_else(|| invalid("request body must be a JSON object"))?;
    if let Some(n) = obj.get("n").and_then(Value::as_u64) {
        if n != 1 {
            return Err(invalid("only n=1 is supported by the codex wire"));
        }
    }
    let msgs = obj
        .get("messages")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
        .ok_or_else(|| invalid("messages must be a nonempty array"))?;

    let mut instructions: Option<String> = None;
    let mut input: Vec<Value> = vec![];
    for (i, m) in msgs.iter().enumerate() {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
        match role {
            "system" | "developer" => {
                let text = text_of(m.get("content"))?;
                if i == 0 && role == "system" && instructions.is_none() {
                    instructions = Some(text);
                } else if !text.is_empty() {
                    input.push(json!({ "role": "developer", "content": text }));
                }
            }
            "user" => {
                let content = input_parts(m.get("content"))?;
                if !content.is_empty() {
                    input.push(json!({ "role": "user", "content": content }));
                }
            }
            "assistant" => {
                input.extend(assistant_items(m)?);
            }
            "tool" => {
                let call_id = m
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| invalid("tool message has no tool_call_id"))?;
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output_parts(m.get("content"))?,
                }));
            }
            other => return Err(invalid(format!("unsupported message role: {other}"))),
        }
    }

    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    out.insert("store".into(), json!(false));
    out.insert("stream".into(), json!(true));
    out.insert(
        "instructions".into(),
        json!(instructions
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_INSTRUCTIONS.into())),
    );
    out.insert("input".into(), Value::Array(input));
    out.insert("text".into(), json!({ "verbosity": "low" }));
    out.insert("include".into(), json!(["reasoning.encrypted_content"]));
    out.insert("tool_choice".into(), map_tool_choice(body)?);
    out.insert("parallel_tool_calls".into(), json!(true));
    if let Some(t) = obj.get("temperature").filter(|v| !v.is_null()) {
        out.insert("temperature".into(), t.clone());
    }
    let effort = obj
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .filter(|s| ["minimal", "low", "medium", "high"].contains(s))
        .unwrap_or("low");
    out.insert(
        "reasoning".into(),
        json!({ "effort": effort, "summary": "auto" }),
    );
    if let Some(tools) = tools_of(obj) {
        out.insert("tools".into(), tools);
    }
    Ok(Value::Object(out))
}

fn map_tool_choice(body: &Value) -> Result<Value, CodexError> {
    match body.get("tool_choice") {
        None | Some(Value::Null) => Ok(json!("auto")),
        Some(Value::String(s)) if ["auto", "none", "required"].contains(&s.as_str()) => {
            Ok(json!(s))
        }
        Some(v)
            if v.get("type").and_then(Value::as_str) == Some("function")
                && v.pointer("/function/name")
                    .and_then(Value::as_str)
                    .is_some() =>
        {
            Ok(json!({ "type": "function", "name": v.pointer("/function/name").unwrap() }))
        }
        Some(_) => Err(invalid("invalid tool_choice")),
    }
}

fn tools_of(obj: &Map<String, Value>) -> Option<Value> {
    let tools = obj.get("tools").and_then(Value::as_array)?;
    let out: Vec<Value> = tools
        .iter()
        .filter(|t| t.get("type").and_then(Value::as_str) == Some("function"))
        .filter_map(|t| {
            let f = t.get("function")?;
            Some(json!({
                "type": "function",
                "name": f.get("name")?.as_str()?,
                "description": f.get("description").and_then(Value::as_str).unwrap_or(""),
                "parameters": f
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
                "strict": false,
            }))
        })
        .collect();
    (!out.is_empty()).then_some(Value::Array(out))
}

/// Message content restricted to text blocks, joined (system prompts).
fn text_of(content: Option<&Value>) -> Result<String, CodexError> {
    match content {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Array(parts)) => {
            let mut out = vec![];
            for p in parts {
                let t = p.get("text").and_then(Value::as_str).ok_or_else(|| {
                    invalid("message content arrays may only hold text or image_url parts")
                })?;
                if p.get("type").and_then(Value::as_str) == Some("text") {
                    out.push(t.to_string());
                } else if p.get("type").and_then(Value::as_str) == Some("image_url") {
                    return Err(invalid("images are not allowed in this message"));
                } else {
                    return Err(invalid("unsupported content part"));
                }
            }
            Ok(out.join("\n"))
        }
        Some(_) => Err(invalid("message content must be text or an array")),
    }
}

/// Chat content parts → Codex user-input parts (`input_text`/`input_image`).
fn input_parts(content: Option<&Value>) -> Result<Vec<Value>, CodexError> {
    match content {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::String(s)) if !s.is_empty() => {
            Ok(vec![json!({ "type": "input_text", "text": s })])
        }
        Some(Value::String(_)) => Ok(vec![]),
        Some(Value::Array(parts)) => {
            let mut out = vec![];
            for p in parts {
                match p.get("type").and_then(Value::as_str) {
                    Some("text") => out.push(json!({
                        "type": "input_text",
                        "text": p.get("text").and_then(Value::as_str).unwrap_or(""),
                    })),
                    Some("image_url") => {
                        let url = p
                            .pointer("/image_url/url")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if !url.starts_with("data:image/") {
                            return Err(invalid("images must use base64 data URLs"));
                        }
                        out.push(
                            json!({ "type": "input_image", "detail": "auto", "image_url": url }),
                        );
                    }
                    _ => return Err(invalid("unsupported content part")),
                }
            }
            Ok(out)
        }
        Some(_) => Err(invalid("message content must be text or an array")),
    }
}

/// Tool-result content: plain text when text-only (Codex's preferred
/// form), otherwise an array of input parts (text + images).
fn output_parts(content: Option<&Value>) -> Result<Value, CodexError> {
    let parts = input_parts(content)?;
    let has_image = parts
        .iter()
        .any(|p| p.get("type").and_then(Value::as_str) == Some("input_image"));
    if !has_image {
        let text = parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        return Ok(json!(text));
    }
    Ok(Value::Array(parts))
}

/// Assistant message → output items (text `message` + `function_call`s).
fn assistant_items(m: &Value) -> Result<Vec<Value>, CodexError> {
    let mut out = vec![];
    let text = text_of(m.get("content")).unwrap_or_default();
    if !text.is_empty() {
        out.push(json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": text, "annotations": [] }],
            "status": "completed",
        }));
    }
    if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            if call.get("type").and_then(Value::as_str) != Some("function") {
                return Err(invalid("only function tool calls are supported"));
            }
            let (id, name, args) = (
                call.get("id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty()),
                call.pointer("/function/name").and_then(Value::as_str),
                call.pointer("/function/arguments").and_then(Value::as_str),
            );
            let (Some(id), Some(name), Some(args)) = (id, name, args) else {
                return Err(invalid("invalid function tool call"));
            };
            let parsed: Value = serde_json::from_str(args)
                .map_err(|_| invalid("tool arguments must be valid JSON"))?;
            if !parsed.is_object() {
                return Err(invalid("tool arguments must be a JSON object"));
            }
            out.push(json!({
                "type": "function_call",
                "call_id": id,
                "name": name,
                "arguments": args,
            }));
        }
    }
    Ok(out)
}

/// Resolve the upstream endpoint from a configured base URL the same way
/// the Codex transports do: `…[/codex][/responses]`.
pub fn responses_url(base_url: &str) -> String {
    let raw = base_url.trim();
    let base = if raw.is_empty() {
        DEFAULT_BASE_URL
    } else {
        raw
    }
    .trim_end_matches('/');
    if base.ends_with("/codex/responses") {
        base.to_string()
    } else if base.ends_with("/codex") {
        format!("{base}/responses")
    } else {
        format!("{base}/codex/responses")
    }
}

// ─────────────────────── response translation ───────────────────────

/// Streamable piece of a response under construction.
#[derive(Debug, PartialEq)]
pub enum Delta {
    Text(String),
    Reasoning(String),
}

/// Accumulates Codex Responses SSE events into a chat-completions
/// response. Used directly for buffered relays and as the state machine
/// for the streaming translator.
pub struct CodexParser {
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
    /// A terminal event (`response.completed` / `incomplete`) was seen.
    pub terminal: bool,
}

impl CodexParser {
    pub fn new(model: &str) -> Self {
        let seq = CHUNK_SEQ.fetch_add(1, Ordering::Relaxed);
        Self {
            model: model.to_string(),
            id: format!("chatcmpl-{:x}-{seq}", unix_now_secs()),
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

    /// Feed one parsed SSE event. Returns deltas worth streaming out
    /// (text/reasoning); tool arguments are buffered until the call is
    /// complete, matching the upstream's own validation boundary.
    pub fn event(&mut self, ev: &Value) -> Result<Vec<Delta>, String> {
        let ty = ev.get("type").and_then(Value::as_str).unwrap_or("");
        let mut deltas = vec![];
        match ty {
            "response.output_text.delta" | "response.refusal.delta" => {
                let d = ev.get("delta").and_then(Value::as_str).unwrap_or("");
                self.text.push_str(d);
                if !d.is_empty() {
                    deltas.push(Delta::Text(d.to_string()));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let d = ev.get("delta").and_then(Value::as_str).unwrap_or("");
                self.reasoning.push_str(d);
                if !d.is_empty() {
                    deltas.push(Delta::Reasoning(d.to_string()));
                }
            }
            "response.reasoning_summary_part.done" => {
                self.reasoning.push_str("\n\n");
                deltas.push(Delta::Reasoning("\n\n".into()));
            }
            "response.output_item.done" => {
                if let Some(item) = ev.get("item") {
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
                        let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                        let args = item
                            .get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}");
                        self.tool_calls.push(json!({
                            "id": call_id,
                            "type": "function",
                            "function": { "name": name, "arguments": args },
                        }));
                    }
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = ev.get("response").cloned().unwrap_or(Value::Null);
                self.usage = usage_of(&response);
                self.finish_reason = Some(match ty {
                    "response.incomplete" => {
                        let reason = response
                            .pointer("/incomplete_details/reason")
                            .and_then(Value::as_str);
                        if reason == Some("max_output_tokens") {
                            "length"
                        } else {
                            return Err(format!(
                                "response incomplete: {}",
                                reason.unwrap_or("no provider reason")
                            ));
                        }
                    }
                    _ => "stop",
                });
                self.terminal = true;
            }
            "error" | "response.failed" => {
                let msg = ev
                    .pointer("/error/message")
                    .or_else(|| ev.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("Codex subscription request failed");
                return Err(msg.to_string());
            }
            _ => {}
        }
        Ok(deltas)
    }

    fn finish_reason(&self) -> &'static str {
        if !self.tool_calls.is_empty() {
            "tool_calls"
        } else {
            self.finish_reason.unwrap_or("stop")
        }
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
            "choices": [{ "index": 0, "message": Value::Object(message),
                          "finish_reason": self.finish_reason() }],
            "usage": self.usage.clone().unwrap_or_else(null_usage),
        })
    }

    /// Chat-completions SSE frames for the streaming relay: the role
    /// chunk before the first delta, the deltas themselves, and — at
    /// the terminal event — the buffered tool calls, the finish chunk,
    /// the usage chunk and `[DONE]`.
    pub fn chunks_for(&mut self, deltas: Vec<Delta>) -> Vec<String> {
        let mut out = vec![];
        let has_output = !deltas.is_empty() || self.terminal;
        if has_output && !self.role_sent {
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
            for (i, call) in self.tool_calls.iter().enumerate() {
                out.push(self.chunk(
                    json!({ "tool_calls": [{ "index": i,
                        "id": call.get("id").cloned().unwrap_or(Value::Null),
                        "type": "function",
                        "function": call.get("function").cloned().unwrap_or(Value::Null) }] }),
                    None,
                ));
            }
            out.push(self.chunk(json!({}), Some(self.finish_reason())));
            let usage = self.usage.clone().unwrap_or_else(null_usage);
            out.push(format!(
                "data: {}\n\n",
                json!({ "id": self.id, "object": "chat.completion.chunk",
                        "created": self.created, "model": self.model,
                        "choices": [], "usage": usage })
            ));
            out.push("data: [DONE]\n\n".into());
        }
        out
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> String {
        format!(
            "data: {}\n\n",
            json!({ "id": self.id, "object": "chat.completion.chunk",
                    "created": self.created, "model": self.model,
                    "choices": [{ "index": 0, "delta": delta,
                                  "finish_reason": finish }] })
        )
    }
}

fn null_usage() -> Value {
    json!({ "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 })
}

/// Codex usage → chat-completions usage. Codex's `input_tokens` already
/// includes cached tokens, which is what `prompt_tokens` means too.
fn usage_of(response: &Value) -> Option<Value> {
    let u = response.get("usage")?;
    let input = u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
    let output = u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
    let cached = u
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(json!({
        "prompt_tokens": input,
        "completion_tokens": output,
        "total_tokens": input + output,
        "prompt_tokens_details": { "cached_tokens": cached },
    }))
}

// ───────────────────────── SSE byte plumbing ─────────────────────────

/// Incremental Codex-SSE → chat-completions-SSE translator for streaming
/// relays: feed upstream bytes, get back bytes to forward.
pub struct SseTranslator {
    buf: Vec<u8>,
    parser: CodexParser,
    frames: std::collections::VecDeque<String>,
    failed: Option<String>,
}

impl SseTranslator {
    pub fn new(model: &str) -> Self {
        Self {
            buf: vec![],
            parser: CodexParser::new(model),
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
            Err(e) => self.failed = Some(e),
        }
    }

    /// Bytes ready to forward, if any.
    pub fn pop_frame(&mut self) -> Option<String> {
        self.frames.pop_front()
    }

    /// The accumulated buffered response (only valid after a terminal
    /// event).
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
            Err("codex stream ended without a completion".into())
        }
    }
}

fn find_frame_end(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
}

/// Parse a complete Codex SSE byte stream into a buffered
/// chat-completions response (non-streaming clients).
pub fn parse_buffered_sse(bytes: &[u8], model: &str) -> Result<Value, String> {
    let mut tr = SseTranslator::new(model);
    tr.push_bytes(bytes);
    tr.finish()?;
    Ok(tr.completion())
}

// ───────────────────────── quota headers ─────────────────────────

/// Codex subscription windows (`x-codex-{primary,secondary}-used-percent`)
/// normalized into quota windows arouter already understands. Learned
/// only from real inference responses — no quota endpoint polling.
pub fn quota_windows(headers: &HeaderMap, now: u64) -> Vec<QuotaWindow> {
    let mut out = vec![];
    for window in ["primary", "secondary"] {
        let Some(used) = header_f64(headers, &format!("x-codex-{window}-used-percent")) else {
            continue;
        };
        let reset = header_f64(headers, &format!("x-codex-{window}-reset-after-seconds"))
            .map(|s| now + s.ceil().max(0.0) as u64);
        out.push(QuotaWindow {
            label: window.to_string(),
            pct: used.clamp(0.0, 100.0),
            reset_unix: reset,
            pace: None,
        });
    }
    out
}

fn header_f64(headers: &HeaderMap, name: &str) -> Option<f64> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse().ok())
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

pub(crate) struct CodexFail {
    pub(crate) last_error: String,
    /// Boxed: `Response` is large and clippy's `result_large_err`
    /// fires on fat Err variants returned from hot paths.
    pub(crate) fatal: Option<Box<Response>>,
}

/// One attempt against the Codex subscription backend (E7). Mirrors the
/// openai attempt path: headers/body deadlines (F5), a budget hold
/// (F2/F3) and quota windows harvested from real inference responses;
/// retryable failures fail over, and the login is re-read per request
/// (the Codex CLI owns renewal — arouter never caches it).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn codex_attempt(
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
) -> Result<Response, CodexFail> {
    let fail = |msg: String| CodexFail {
        last_error: msg,
        fatal: None,
    };
    let login = read_login().await.map_err(|e| fail(e.message))?;
    let body = openai_to_codex(payload, &m.upstream_name).map_err(|e| {
        // invalid request is the client's fault: no failover, plain 400
        CodexFail {
            last_error: format!("invalid request: {}", e.message),
            fatal: Some(Box::new(wire_error(
                wire,
                e.status,
                "invalid_request",
                e.message,
            ))),
        }
    })?;
    let url = responses_url(&m.base_url);
    let request = st
        .http
        .post(&url)
        .header(header::AUTHORIZATION, format!("Bearer {}", login.token))
        .header("chatgpt-account-id", login.account_id.as_str())
        .header("OpenAI-Beta", "responses=experimental")
        .header(header::ACCEPT, "text/event-stream")
        .header("originator", concat!("arouter/", env!("CARGO_PKG_VERSION")))
        .json(&body);
    let prefix = m.id.split('/').next().unwrap_or("").to_string();
    let harvest = rt.quota_harvest.get(&prefix).copied().unwrap_or(true);
    // review F5: bound the wait for response headers, same as the openai wire
    let resp = match tokio::time::timeout(
        Duration::from_secs(rt.cfg.timeouts.headers_secs),
        request.send(),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err(fail(format!("connect error: {e}"))),
        Err(_) => return Err(fail("headers timeout".into())),
    };
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        // E3b (codex flavor): quota headers on failures are ground truth
        // too; a bare 429 without them still pins a synthetic window.
        let now = unix_now();
        let windows = quota_windows(resp.headers(), now);
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
        let msg = format!("codex upstream {code}: {body_text}");
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
        // 401/403 = login expired or wrong account: behaves like a 503
        // so the next candidate is tried instead of hard-failing
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
    let windows = quota_windows(resp.headers(), now);
    if harvest && !windows.is_empty() {
        st.quota.apply(&prefix, windows, now);
    }
    if !is_stream {
        // upstream always streams: buffer (deadline-bounded, F5),
        // translate, relay a plain chat-completions JSON body. A
        // truncated stream (no terminal event) is an upstream failure —
        // fail over, never an empty 200.
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
                let msg = "codex body stalled past idle_secs".to_string();
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
    let stream = CodexSse::new(resp.bytes_stream(), SseTranslator::new(&m.upstream_name));
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

/// E7: wraps a Codex Responses-SSE upstream and re-emits
/// chat-completions-SSE. Sits inside the TapStream so usage
/// accounting, health and metrics see the openai wire they already
/// parse; upstream SSE errors and a missing terminal event surface as
/// stream errors (F4), never as a silent empty success.
pub(crate) struct CodexSse<S> {
    inner: S,
    tr: SseTranslator,
    out: std::collections::VecDeque<Bytes>,
}
impl<S> CodexSse<S>
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

impl<S> Stream for CodexSse<S>
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

    fn b64url(bytes: &[u8]) -> String {
        const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        let mut acc: u32 = 0;
        let mut bits = 0;
        for b in bytes {
            acc = (acc << 8) | *b as u32;
            bits += 8;
            while bits >= 6 {
                bits -= 6;
                out.push(TBL[((acc >> bits) & 0x3f) as usize] as char);
            }
        }
        if bits > 0 {
            out.push(TBL[((acc << (6 - bits)) & 0x3f) as usize] as char);
        }
        out
    }

    fn fake_login(exp: u64, account: Option<&str>) -> String {
        let header = b64url(br#"{"alg":"RS256"}"#);
        let mut claims = serde_json::json!({ "exp": exp });
        if let Some(a) = account {
            claims[JWT_CLAIM_PATH] = serde_json::json!({ "chatgpt_account_id": a });
        }
        format!(
            "{}.{}.sig",
            header,
            b64url(serde_json::to_string(&claims).unwrap().as_bytes())
        )
    }

    #[test]
    fn login_roundtrip() {
        let now = 1000;
        let token = fake_login(now + 100, Some("acct-9"));
        let raw = serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": { "access_token": token },
        })
        .to_string();
        let login = read_login_from(&raw, now).unwrap();
        assert_eq!(login.account_id, "acct-9");
    }

    #[test]
    fn login_rejections() {
        let now = 1000;
        // expired
        let raw = serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": { "access_token": fake_login(now - 1, Some("a")) },
        })
        .to_string();
        assert!(read_login_from(&raw, now).is_err());
        // api-key mode (separately billed) must be refused
        let raw = serde_json::json!({
            "auth_mode": "apikey",
            "tokens": { "access_token": fake_login(now + 100, Some("a")) },
        })
        .to_string();
        assert!(read_login_from(&raw, now).is_err());
        // missing account id
        let raw = serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": { "access_token": fake_login(now + 100, None) },
        })
        .to_string();
        assert!(read_login_from(&raw, now).is_err());
        // garbage
        assert!(read_login_from("not json", now).is_err());
    }

    fn chat_body() -> Value {
        serde_json::json!({
            "model": "A+",
            "messages": [
                { "role": "system", "content": "be terse" },
                { "role": "user", "content": [
                    { "type": "text", "text": "look:" },
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,QUJD" } },
                ]},
                { "role": "assistant", "content": "checking", "tool_calls": [
                    { "id": "call_1", "type": "function",
                      "function": { "name": "read", "arguments": "{\"path\":\"x\"}" } },
                ]},
                { "role": "tool", "tool_call_id": "call_1", "content": "file body" },
                { "role": "user", "content": "summarize" },
            ],
            "tools": [
                { "type": "function", "function": {
                    "name": "read", "description": "read a file",
                    "parameters": { "type": "object", "properties": { "path": { "type": "string" } } } } },
            ],
        })
    }

    #[test]
    fn request_translation() {
        let out = openai_to_codex(&chat_body(), "gpt-6-astra").unwrap();
        assert_eq!(out["model"], "gpt-6-astra");
        assert_eq!(out["store"], false);
        assert_eq!(out["stream"], true);
        assert_eq!(out["instructions"], "be terse");
        assert_eq!(out["tool_choice"], "auto");
        assert_eq!(out["reasoning"]["effort"], "low");
        let input = out["input"].as_array().unwrap();
        // user w/ image, assistant message, function_call, tool output, user
        assert_eq!(input.len(), 5);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][1]["type"], "input_image");
        assert_eq!(
            input[0]["content"][1]["image_url"],
            "data:image/png;base64,QUJD"
        );
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["content"][0]["type"], "output_text");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[2]["arguments"], "{\"path\":\"x\"}");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call_1");
        assert_eq!(input[3]["output"], "file body");
        assert_eq!(input[4]["role"], "user");
        assert_eq!(out["tools"][0]["name"], "read");
        assert_eq!(out["tools"][0]["strict"], false);
    }

    #[test]
    fn request_rejections() {
        let mut b = chat_body();
        b["n"] = json!(2);
        assert_eq!(openai_to_codex(&b, "m").unwrap_err().status, 400);
        let mut b = chat_body();
        b["tool_choice"] = json!({ "type": "wat" });
        assert!(openai_to_codex(&b, "m").is_err());
        let mut b = chat_body();
        b["messages"][2]["tool_calls"][0]["function"]["arguments"] = json!("not json");
        assert!(openai_to_codex(&b, "m").is_err());
        let bad = json!({ "messages": [] });
        assert!(openai_to_codex(&bad, "m").is_err());
        // remote image URLs are refused
        let bad = json!({ "messages": [
            { "role": "user", "content": [{ "type": "image_url", "image_url": { "url": "https://x/y.png" } }] },
        ]});
        assert!(openai_to_codex(&bad, "m").is_err());
    }

    #[test]
    fn tool_choice_object_maps() {
        let mut b = chat_body();
        b["tool_choice"] = json!({ "type": "function", "function": { "name": "read" } });
        let out = openai_to_codex(&b, "m").unwrap();
        assert_eq!(
            out["tool_choice"],
            json!({ "type": "function", "name": "read" })
        );
    }

    #[test]
    fn default_instructions_when_no_system() {
        let b = json!({ "messages": [{ "role": "user", "content": "hi" }] });
        let out = openai_to_codex(&b, "m").unwrap();
        assert_eq!(out["instructions"], "You are a helpful assistant.");
    }

    #[test]
    fn responses_url_forms() {
        assert_eq!(
            responses_url(""),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            responses_url("https://chatgpt.com/backend-api/"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            responses_url("http://localhost:9/codex"),
            "http://localhost:9/codex/responses"
        );
        assert_eq!(
            responses_url("http://localhost:9/codex/responses"),
            "http://localhost:9/codex/responses"
        );
    }

    // ── response side ──

    fn ev(ty: &str, extra: Value) -> Value {
        let mut v = json!({ "type": ty });
        if let (Some(obj), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            for (k, val) in e {
                obj.insert(k.clone(), val.clone());
            }
        }
        v
    }

    fn terminal(usage: Value) -> Value {
        ev(
            "response.completed",
            json!({ "response": { "status": "completed", "usage": usage } }),
        )
    }

    const USAGE: &str = r#"{
        "input_tokens": 120, "output_tokens": 30,
        "input_tokens_details": { "cached_tokens": 20 }
    }"#;

    #[test]
    fn buffered_completion_with_tools_and_cache() {
        let mut p = CodexParser::new("gpt-6-astra");
        assert_eq!(
            p.event(&ev("response.output_text.delta", json!({ "delta": "hel" })))
                .unwrap(),
            vec![Delta::Text("hel".into())]
        );
        assert_eq!(
            p.event(&ev("response.output_text.delta", json!({ "delta": "lo" })))
                .unwrap(),
            vec![Delta::Text("lo".into())]
        );
        assert_eq!(
            p.event(&ev(
                "response.reasoning_summary_text.delta",
                json!({ "delta": "hm" })
            ))
            .unwrap(),
            vec![Delta::Reasoning("hm".into())]
        );
        p.event(&ev(
            "response.output_item.done",
            json!({ "item": {
            "type": "function_call", "call_id": "c1", "name": "read", "arguments": "{\"a\":1}" } }),
        ))
        .unwrap();
        p.event(&terminal(serde_json::from_str::<Value>(USAGE).unwrap()))
            .unwrap();
        assert!(p.terminal);

        let out = p.completion();
        assert_eq!(out["choices"][0]["message"]["content"], "hello");
        assert_eq!(out["choices"][0]["message"]["reasoning_content"], "hm");
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
        let call = &out["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(call["id"], "c1");
        assert_eq!(call["function"]["name"], "read");
        assert_eq!(call["function"]["arguments"], "{\"a\":1}");
        assert_eq!(out["usage"]["prompt_tokens"], 120);
        assert_eq!(out["usage"]["completion_tokens"], 30);
        assert_eq!(out["usage"]["total_tokens"], 150);
        assert_eq!(out["usage"]["prompt_tokens_details"]["cached_tokens"], 20);
    }

    #[test]
    fn buffered_length_and_text_only() {
        let mut p = CodexParser::new("m");
        p.event(&ev("response.output_text.delta", json!({ "delta": "x" })))
            .unwrap();
        p.event(&ev(
            "response.incomplete",
            json!({ "response": {
            "status": "incomplete",
            "incomplete_details": { "reason": "max_output_tokens" },
            "usage": serde_json::from_str::<Value>(USAGE).unwrap() } }),
        ))
        .unwrap();
        assert_eq!(p.completion()["choices"][0]["finish_reason"], "length");
    }

    #[test]
    fn error_events_fail() {
        let mut p = CodexParser::new("m");
        assert!(p.event(&ev("error", json!({ "message": "boom" }))).is_err());
        let mut p = CodexParser::new("m");
        assert!(p
            .event(&ev(
                "response.failed",
                json!({ "response": { "error": { "code": "x", "message": "bad" } } })
            ))
            .is_err());
        // incomplete without a reason is an error, not a silent stop
        let mut p = CodexParser::new("m");
        assert!(p
            .event(&ev(
                "response.incomplete",
                json!({ "response": { "status": "incomplete" } })
            ))
            .is_err());
    }

    #[test]
    fn sse_translator_streaming_frames() {
        let mut tr = SseTranslator::new("gpt-6-astra");
        // split one event across chunks on purpose
        tr.push_bytes(b"data: {\"type\":\"response.output_text.del");
        assert!(tr.pop_frame().is_none());
        tr.push_bytes(b"ta\",\"delta\":\"hi\"}\n\n");
        let f1 = tr.pop_frame().unwrap();
        assert!(f1.starts_with("data: "), "{f1}");
        assert!(
            f1.contains("\"role\":\"assistant\""),
            "role chunk first: {f1}"
        );
        let f2 = tr.pop_frame().unwrap();
        assert!(f2.contains("\"content\":\"hi\""), "{f2}");
        assert!(tr.pop_frame().is_none());
        tr.push_bytes(
            format!(
                "data: {}\n\n",
                terminal(serde_json::from_str::<Value>(USAGE).unwrap())
            )
            .as_bytes(),
        );
        let mut all = String::new();
        while let Some(f) = tr.pop_frame() {
            all.push_str(&f);
        }
        assert!(all.contains("\"finish_reason\":\"stop\""), "{all}");
        assert!(all.contains("\"prompt_tokens\":120"), "{all}");
        assert!(all.ends_with("data: [DONE]\n\n"), "{all}");
        tr.finish().unwrap();
    }

    #[test]
    fn sse_translator_tool_args_buffered() {
        let mut tr = SseTranslator::new("m");
        // argument deltas are NOT streamed out as partial tool calls
        tr.push_bytes(b"data: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"{\\\"a\\\":\"}\n\n");
        tr.push_bytes(
            b"data: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"1}\"}\n\n",
        );
        assert!(
            tr.pop_frame().is_none(),
            "no frames until the call completes"
        );
        tr.push_bytes(b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c9\",\"name\":\"run\",\"arguments\":\"{\\\"a\\\":1}\"}}\n\n");
        tr.push_bytes(
            format!(
                "data: {}\n\n",
                terminal(serde_json::from_str::<Value>(USAGE).unwrap())
            )
            .as_bytes(),
        );
        let mut all = String::new();
        while let Some(f) = tr.pop_frame() {
            all.push_str(&f);
        }
        assert!(all.contains("\"tool_calls\""), "{all}");
        assert!(all.contains("\"name\":\"run\""), "{all}");
        assert!(all.contains("\"finish_reason\":\"tool_calls\""), "{all}");
        tr.finish().unwrap();
    }

    #[test]
    fn sse_translator_requires_terminal() {
        let mut tr = SseTranslator::new("m");
        tr.push_bytes(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n");
        while tr.pop_frame().is_some() {}
        assert!(tr.finish().is_err());
        let mut tr = SseTranslator::new("m");
        tr.push_bytes(b"data: {\"type\":\"error\",\"message\":\"quota\"}\n\n");
        assert!(tr.finish().is_err());
    }

    #[test]
    fn sse_translator_ignores_done_and_ignorable_events() {
        let mut tr = SseTranslator::new("m");
        tr.push_bytes(b": keepalive\ndata: [DONE]\ndata: {\"type\":\"response.created\"}\n\n");
        assert!(tr.pop_frame().is_none());
    }

    #[test]
    fn buffered_sse_end_to_end() {
        let sse = format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\n",
            json!({ "type": "response.output_text.delta", "delta": "hi" }),
            json!({ "type": "response.output_item.done", "item": {
                "type": "function_call", "call_id": "c1", "name": "run", "arguments": "{}" } }),
            terminal(serde_json::from_str::<Value>(USAGE).unwrap()),
        );
        let out = parse_buffered_sse(sse.as_bytes(), "gpt-6-astra").unwrap();
        assert_eq!(out["model"], "gpt-6-astra");
        assert_eq!(out["choices"][0]["message"]["content"], "hi");
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(out["usage"]["prompt_tokens"], 120);
        // truncated stream (no terminal) is an error
        assert!(parse_buffered_sse(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
            "m"
        )
        .is_err());
    }

    #[test]
    fn quota_header_mapping() {
        let mut h = HeaderMap::new();
        h.insert("x-codex-primary-used-percent", "42.5".parse().unwrap());
        h.insert(
            "x-codex-primary-reset-after-seconds",
            "3600".parse().unwrap(),
        );
        h.insert("x-codex-secondary-used-percent", "10".parse().unwrap());
        let w = quota_windows(&h, 1000);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].label, "primary");
        assert_eq!(w[0].pct, 42.5);
        assert_eq!(w[0].reset_unix, Some(4600));
        assert_eq!(w[1].label, "secondary");
        assert_eq!(w[1].pct, 10.0);
        assert_eq!(w[1].reset_unix, None);
        // no codex headers → nothing learned
        assert!(quota_windows(&HeaderMap::new(), 1000).is_empty());
    }
}
