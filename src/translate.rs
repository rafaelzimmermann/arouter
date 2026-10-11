//! Anthropic Messages wire ↔ OpenAI chat-completions wire (E7).
//!
//! Lets pi's `anthropic-messages` API — and any Anthropic SDK client —
//! talk to arouter while every upstream stays OpenAI-shaped. Requests
//! arrive as `POST {pool}/v1/messages`, get translated to an OpenAI chat
//! completion, routed through the normal candidate machinery, and the
//! response (JSON or SSE) is translated back on the way out.
//!
//! Everything here is pure; `proxy.rs` owns the I/O.

use serde_json::{json, Map, Value};

// ─────────────────────── request: anthropic → openai ───────────────────────

/// Translate an Anthropic Messages request body into an OpenAI
/// chat-completions body. `model` passes through untouched (it carries
/// the grade token). Returns Err(reason) on structurally invalid input.
pub fn anthropic_to_openai(v: &Value) -> Result<Value, String> {
    let obj = v
        .as_object()
        .ok_or_else(|| "request body must be a JSON object".to_string())?;
    let mut out = Map::new();

    out.insert(
        "model".into(),
        obj.get("model").cloned().unwrap_or(Value::Null),
    );

    if let Some(max) = obj.get("max_tokens").and_then(|m| m.as_u64()) {
        out.insert("max_tokens".into(), json!(max));
    }
    for k in ["temperature", "top_p", "stream"] {
        if let Some(x) = obj.get(k) {
            if !x.is_null() {
                out.insert(k.into(), x.clone());
            }
        }
    }
    if let Some(stops) = obj.get("stop_sequences").and_then(Value::as_array) {
        let stops: Vec<&str> = stops.iter().filter_map(|s| s.as_str()).collect();
        if !stops.is_empty() {
            out.insert("stop".into(), json!(stops));
        }
    }

    let mut messages: Vec<Value> = Vec::new();
    if let Some(system) = system_text(obj.get("system")) {
        messages.push(json!({"role": "system", "content": system}));
    }
    let req_msgs = obj
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing messages[]".to_string())?;
    if req_msgs.is_empty() {
        return Err("messages[] must not be empty".into());
    }
    for m in req_msgs {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let role = match role {
            "assistant" => "assistant",
            _ => "user", // anthropic only has user/assistant; system is top-level
        };
        for msg in message_variants(role, m.get("content"))? {
            push_openai(&mut messages, msg);
        }
    }
    if messages.is_empty() {
        return Err("no translatable content in messages[]".into());
    }
    out.insert("messages".into(), Value::Array(messages));

    if let Some(tools) = obj.get("tools").and_then(Value::as_array) {
        let fns: Vec<Value> = tools
            .iter()
            .filter_map(|t| {
                let name = t.get("name")?.as_str()?.to_string();
                Some(json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": t.get("description").cloned().unwrap_or(Value::Null),
                        "parameters": t.get("input_schema").cloned()
                            .unwrap_or_else(|| json!({"type": "object"})),
                    }
                }))
            })
            .collect();
        if !fns.is_empty() {
            out.insert("tools".into(), json!(fns));
            if let Some(choice) = obj.get("tool_choice") {
                if let Some(mapped) = map_tool_choice(choice) {
                    out.insert("tool_choice".into(), mapped);
                }
            }
        }
    }

    Ok(Value::Object(out))
}

/// system: either a plain string or an array of text blocks.
fn system_text(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(blocks)) => {
            let parts: Vec<String> = blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .map(str::to_string)
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n\n"))
            }
        }
        _ => None,
    }
}

/// One anthropic message can expand to several openai messages.
///
/// Tool history uses the shapes Chat Completions actually validates
/// (review F8): `tool_use` blocks become the assistant message's
/// top-level `tool_calls` array, and every `tool_result` becomes its
/// own `{"role":"tool"}` message — never content-part items, which
/// upstreams reject on the next turn of an agent's tool loop.
fn message_variants(role: &str, content: Option<&Value>) -> Result<Vec<Value>, String> {
    match content {
        None | Some(Value::Null) => Ok(vec![json!({"role": role, "content": ""})]),
        Some(Value::String(s)) => Ok(vec![json!({"role": role, "content": s})]),
        Some(Value::Array(blocks)) => {
            let mut text: Vec<String> = Vec::new();
            let mut parts: Vec<Value> = Vec::new(); // openai content-array parts
            let mut tool_calls: Vec<Value> = Vec::new();
            let mut tool_results: Vec<Value> = Vec::new();
            let mut has_image = false;
            for b in blocks {
                let ty = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match ty {
                    "text" => {
                        let t = b.get("text").and_then(|t| t.as_str()).unwrap_or("");
                        text.push(t.to_string());
                        parts.push(json!({"type": "text", "text": t}));
                    }
                    "image" => {
                        has_image = true;
                        if let Some(url) = image_url(b) {
                            parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                        }
                    }
                    "tool_use" => {
                        // message-level tool_calls entry; arguments is the
                        // JSON-STRING form openai expects
                        tool_calls.push(json!({
                            "id": b.get("id").cloned().unwrap_or(Value::Null),
                            "type": "function",
                            "function": {
                                "name": b.get("name").cloned().unwrap_or(Value::Null),
                                "arguments": b.get("input").cloned()
                                    .unwrap_or_else(|| json!({})).to_string(),
                            }
                        }));
                    }
                    "tool_result" => {
                        // its own sibling message with role "tool"
                        tool_results.push(json!({
                            "role": "tool",
                            "tool_call_id": b.get("tool_use_id").cloned()
                                .unwrap_or(Value::Null),
                            "content": tool_result_content(b.get("content")),
                        }));
                    }
                    "thinking"
                    | "redacted_thinking"
                    | "server_tool_use"
                    | "web_search_tool_result" => {} // upstream-internal; drop
                    _ => {}
                }
            }
            if !tool_calls.is_empty() {
                // assistant turn with tool calls: content is the text (or
                // null when the model called without preamble)
                let content = if text.is_empty() {
                    Value::Null
                } else {
                    Value::String(text.join("\n"))
                };
                return Ok(vec![json!({
                    "role": "assistant",
                    "content": content,
                    "tool_calls": tool_calls,
                })]);
            }
            let mut out: Vec<Value> = Vec::new();
            // tool results answer the previous assistant tool_calls turn;
            // they must precede any same-turn user text
            out.extend(tool_results);
            let content = if has_image {
                Value::Array(parts)
            } else if text.is_empty() {
                // tool_results only, or nothing translatable
                return Ok(out);
            } else {
                Value::String(text.join("\n"))
            };
            out.push(json!({"role": role, "content": content}));
            Ok(out)
        }
        Some(other) => Err(format!(
            "message content must be string or blocks[], got {other}"
        )),
    }
}

/// anthropic {type:"image", source:{type:"base64", media_type, data}}
/// → openai image_url data: URL. URL sources pass through untouched.
fn image_url(b: &Value) -> Option<String> {
    let src = b.get("source")?;
    match src.get("type").and_then(|t| t.as_str()) {
        Some("base64") => {
            let mt = src.get("media_type")?.as_str()?;
            let data = src.get("data")?.as_str()?;
            Some(format!("data:{mt};base64,{data}"))
        }
        Some("url") => src.get("url").and_then(|u| u.as_str()).map(str::to_string),
        _ => None,
    }
}

/// tool_result content: string or blocks (text/image) → openai tool part
/// content (string when plain text, parts array otherwise).
fn tool_result_content(content: Option<&Value>) -> Value {
    match content {
        Some(Value::String(s)) => json!(s),
        Some(Value::Array(blocks)) => {
            let parts: Vec<Value> = blocks
                .iter()
                .filter_map(|b| match b.get("type").and_then(|t| t.as_str()) {
                    Some("text") => Some(json!({"type": "text", "text":
                        b.get("text").cloned().unwrap_or(Value::Null)})),
                    Some("image") => {
                        image_url(b).map(|u| json!({"type": "image_url", "image_url": {"url": u}}))
                    }
                    _ => None,
                })
                .collect();
            if parts.is_empty() {
                json!("")
            } else {
                Value::Array(parts)
            }
        }
        _ => json!(""),
    }
}

fn map_tool_choice(choice: &Value) -> Option<Value> {
    match choice.get("type").and_then(|t| t.as_str()) {
        Some("auto") => Some(json!("auto")),
        Some("any") => Some(json!("required")),
        Some("none") => Some(json!("none")),
        Some("tool") => choice
            .get("name")
            .and_then(|n| n.as_str())
            .map(|name| json!({"type": "function", "function": {"name": name}})),
        _ => None,
    }
}

/// OpenAI-compatible upstreams generally expect user/assistant turns to
/// alternate; anthropic allows consecutive same-role messages. Merge
/// consecutive plain same-role turns (text concat, arrays concat).
/// Tool-bearing messages (tool_calls / role:"tool") are NEVER merged —
/// each tool result must stay addressable by tool_call_id.
fn push_openai(messages: &mut Vec<Value>, msg: Value) {
    let plain = msg.get("tool_calls").is_none() && msg.get("tool_call_id").is_none();
    if plain {
        let role = msg.get("role").cloned().unwrap_or(Value::Null);
        let content = msg.get("content").cloned().unwrap_or(json!(""));
        if let Some(last) = messages.last_mut() {
            if last.get("role").and_then(|r| r.as_str()) == role.as_str() {
                let merged =
                    merge_content(last.get("content").cloned().unwrap_or(json!("")), content);
                last["content"] = merged;
                return;
            }
        }
    }
    messages.push(msg);
}

fn merge_content(a: Value, b: Value) -> Value {
    let a_is_empty_str = a.as_str().is_some_and(str::is_empty);
    let b_is_empty_str = b.as_str().is_some_and(str::is_empty);
    if a_is_empty_str {
        return b;
    }
    if b_is_empty_str {
        return a;
    }
    match (a, b) {
        (Value::String(x), Value::String(y)) => Value::String(format!("{x}\n{y}")),
        (Value::Array(mut x), Value::Array(y)) => {
            x.extend(y);
            Value::Array(x)
        }
        (Value::String(x), Value::Array(mut y)) => {
            y.insert(0, json!({"type": "text", "text": x}));
            Value::Array(y)
        }
        (Value::Array(mut x), Value::String(y)) => {
            x.push(json!({"type": "text", "text": y}));
            Value::Array(x)
        }
        (a, b) => json!(format!("{a}\n{b}")),
    }
}

// ─────────────────────── response: openai → anthropic (JSON) ───────────────────────

/// Translate a non-streaming OpenAI chat completion into an Anthropic
/// Messages response.
pub fn openai_to_anthropic(v: &Value) -> Value {
    let choice = v
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .cloned()
        .unwrap_or(Value::Null);
    let msg = choice.get("message").cloned().unwrap_or(Value::Null);

    let mut content: Vec<Value> = Vec::new();
    if let Some(thinking) = reasoning_text(&msg) {
        content.push(json!({
            "type": "thinking",
            "thinking": thinking,
            "signature": "arouter",
        }));
    }
    match msg.get("content") {
        Some(Value::String(s)) if !s.is_empty() => {
            content.push(json!({"type": "text", "text": s}));
        }
        Some(Value::Array(parts)) => {
            for p in parts {
                if p.get("type").and_then(|t| t.as_str()) == Some("text") {
                    content.push(json!({"type": "text",
                        "text": p.get("text").cloned().unwrap_or(Value::Null)}));
                }
            }
        }
        _ => {}
    }
    if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
        for c in calls {
            let f = c.get("function").cloned().unwrap_or(Value::Null);
            let input = f
                .get("arguments")
                .and_then(|a| a.as_str())
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or_else(|| json!({}));
            content.push(json!({
                "type": "tool_use",
                "id": c.get("id").cloned().unwrap_or_else(|| json!("toolu_0")),
                "name": f.get("name").cloned().unwrap_or(Value::Null),
                "input": input,
            }));
        }
    }

    let stop_reason = match choice
        .get("finish_reason")
        .and_then(|f| f.as_str())
        .unwrap_or("stop")
    {
        "length" => "max_tokens",
        "tool_calls" | "function_call" => "tool_use",
        "content_filter" => "refusal",
        _ => "end_turn",
    };
    let u = v.get("usage").cloned().unwrap_or(json!({}));
    json!({
        "id": v.get("id").cloned().unwrap_or_else(|| json!("msg_arouter")),
        "type": "message",
        "role": "assistant",
        "model": v.get("model").cloned().unwrap_or(Value::Null),
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
            "output_tokens": u.get("completion_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
        }
    })
}

/// OpenAI-compatible thinking upstreams (z.ai glm, OpenRouter, …) park
/// visible chain-of-thought in `reasoning_content` (sometimes `reasoning`).
fn reasoning_text(msg: &Value) -> Option<String> {
    for key in ["reasoning_content", "reasoning"] {
        if let Some(s) = msg.get(key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

// ─────────────────────── response: openai SSE → anthropic SSE ───────────────────────

/// Stateful line-oriented translator: feed it raw upstream SSE bytes,
/// get complete Anthropic SSE frames back.
pub struct SseTranslator {
    buf: String,
    started: bool,
    /// anthropic content_block index of the currently open block
    block: Option<Block>,
    /// openai tool_call index → anthropic block index (multi-tool streams)
    tool_blocks: Vec<(usize, usize)>,
    next_index: usize,
    stop_reason: Option<String>,
    usage: Option<Value>,
    model: String,
    id: String,
    done: bool,
}

enum Block {
    Thinking,
    Text,
    Tool,
}

impl Default for SseTranslator {
    fn default() -> Self {
        Self::new()
    }
}

impl SseTranslator {
    pub fn new() -> Self {
        Self {
            buf: String::new(),
            started: false,
            block: None,
            tool_blocks: Vec::new(),
            next_index: 0,
            stop_reason: None,
            usage: None,
            model: String::new(),
            id: String::new(),
            done: false,
        }
    }

    /// Feed upstream bytes; returns any complete Anthropic SSE frames
    /// (each already `event:`/`data:` formatted and \n\n-terminated).
    pub fn push_bytes(&mut self, bytes: &[u8]) -> Vec<String> {
        let chunk = String::from_utf8_lossy(bytes).replace("\r\n", "\n");
        self.buf.push_str(&chunk);
        let mut frames = Vec::new();
        while let Some(pos) = self.buf.find("\n\n") {
            let event: String = self.buf.drain(..pos + 2).collect();
            let body = &event[..event.len() - 2];
            for line in body.lines() {
                if let Some(frames_from_line) = self.feed_line(line) {
                    frames.extend(frames_from_line);
                }
            }
        }
        frames
    }

    /// Flush any buffered partial event and close out the message.
    pub fn finish(&mut self) -> Vec<String> {
        let mut frames = Vec::new();
        if !self.buf.is_empty() {
            let rest = std::mem::take(&mut self.buf);
            for line in rest.lines() {
                if let Some(f) = self.feed_line(line) {
                    frames.extend(f);
                }
            }
        }
        frames.extend(self.close());
        frames
    }

    fn feed_line(&mut self, line: &str) -> Option<Vec<String>> {
        let data = line.strip_prefix("data:")?.trim().to_string();
        if self.done {
            return None;
        }
        if data == "[DONE]" {
            return Some(self.close());
        }
        let Ok(chunk) = serde_json::from_str::<Value>(&data) else {
            return None;
        };
        let mut frames = Vec::new();
        if !self.started {
            self.started = true;
            self.id = chunk
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("msg_arouter")
                .to_string();
            self.model = chunk
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            frames.push(frame(
                "message_start",
                json!({
                    "type": "message_start",
                    "message": {
                        "id": self.id, "type": "message", "role": "assistant",
                        "model": self.model, "content": [],
                        "stop_reason": Value::Null, "stop_sequence": Value::Null,
                        "usage": {"input_tokens": 0, "output_tokens": 0},
                    }
                }),
            ));
        }
        if let Some(u) = chunk.get("usage") {
            if u.get("prompt_tokens").is_some() || u.get("completion_tokens").is_some() {
                self.usage = Some(u.clone());
            }
        }
        if let Some(fr) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .and_then(|c| c.get("finish_reason"))
            .and_then(|f| f.as_str())
        {
            self.stop_reason = Some(
                match fr {
                    "length" => "max_tokens",
                    "tool_calls" | "function_call" => "tool_use",
                    "content_filter" => "refusal",
                    _ => "end_turn",
                }
                .to_string(),
            );
        }
        if let Some(delta) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .and_then(|c| c.get("delta"))
        {
            if let Some(thinking) = reasoning_text(delta) {
                if !thinking.is_empty() {
                    if !matches!(self.block, Some(Block::Thinking)) {
                        frames.extend(self.close_block());
                        let idx = self.next_index;
                        self.next_index += 1;
                        self.block = Some(Block::Thinking);
                        frames.push(frame(
                            "content_block_start",
                            json!({
                                "type": "content_block_start", "index": idx,
                                "content_block": {"type": "thinking", "thinking": ""}
                            }),
                        ));
                    }
                    let idx = self.block_index();
                    frames.push(frame(
                        "content_block_delta",
                        json!({
                            "type": "content_block_delta", "index": idx,
                            "delta": {"type": "thinking_delta", "thinking": thinking}
                        }),
                    ));
                }
            }
            if let Some(text) = delta.get("content").and_then(|c| c.as_str()) {
                if !text.is_empty() {
                    if !matches!(self.block, Some(Block::Text)) {
                        frames.extend(self.close_block());
                        let idx = self.next_index;
                        self.next_index += 1;
                        self.block = Some(Block::Text);
                        frames.push(frame(
                            "content_block_start",
                            json!({
                                "type": "content_block_start", "index": idx,
                                "content_block": {"type": "text", "text": ""}
                            }),
                        ));
                    }
                    let idx = self.block_index();
                    frames.push(frame(
                        "content_block_delta",
                        json!({
                            "type": "content_block_delta", "index": idx,
                            "delta": {"type": "text_delta", "text": text}
                        }),
                    ));
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for tc in calls {
                    frames.extend(self.feed_tool_call(tc));
                }
            }
        }
        Some(frames)
    }

    fn feed_tool_call(&mut self, tc: &Value) -> Vec<String> {
        let mut frames = Vec::new();
        let oi = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
        let f = tc.get("function").cloned().unwrap_or(Value::Null);
        let name = f.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let args = f.get("arguments").and_then(|a| a.as_str()).unwrap_or("");
        let known = self.tool_blocks.iter().find(|(o, _)| *o == oi).copied();
        let ai = match known {
            Some((_, ai)) => ai,
            None => {
                // a new tool call: either it carries an id, or a fresh name
                let is_new = tc.get("id").is_some() || !name.is_empty();
                if !is_new {
                    return frames; // fragment for a call we never saw start
                }
                frames.extend(self.close_block());
                let ai = self.next_index;
                self.next_index += 1;
                self.tool_blocks.push((oi, ai));
                self.block = Some(Block::Tool);
                frames.push(frame(
                    "content_block_start",
                    json!({
                        "type": "content_block_start", "index": ai,
                        "content_block": {
                            "type": "tool_use",
                            "id": tc.get("id").cloned().unwrap_or_else(|| json!("toolu_0")),
                            "name": name,
                            "input": {},
                        }
                    }),
                ));
                ai
            }
        };
        if !args.is_empty() {
            frames.push(frame(
                "content_block_delta",
                json!({
                    "type": "content_block_delta", "index": ai,
                    "delta": {"type": "input_json_delta", "partial_json": args}
                }),
            ));
        }
        frames
    }

    fn block_index(&self) -> usize {
        match self.block {
            Some(_) => self.next_index.saturating_sub(1),
            None => 0,
        }
    }

    fn close_block(&mut self) -> Vec<String> {
        match self.block.take() {
            Some(_) => vec![frame(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": self.next_index.saturating_sub(1)}),
            )],
            None => vec![],
        }
    }

    fn close(&mut self) -> Vec<String> {
        if self.done {
            return vec![];
        }
        self.done = true;
        let mut frames = self.close_block();
        let mut usage = json!({"output_tokens": 0});
        if let Some(u) = &self.usage {
            usage["output_tokens"] = u.get("completion_tokens").cloned().unwrap_or(json!(0));
            usage["input_tokens"] = u.get("prompt_tokens").cloned().unwrap_or(json!(0));
        }
        frames.push(frame(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": self.stop_reason.clone().unwrap_or_else(|| "end_turn".into()),
                    "stop_sequence": Value::Null,
                },
                "usage": usage,
            }),
        ));
        frames.push(frame("message_stop", json!({"type": "message_stop"})));
        frames
    }
}

fn frame(event: &str, data: Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

// ─────────────────────── errors ───────────────────────

/// Anthropic-shaped error body for a given HTTP status.
pub fn anthropic_error(status: u16, code: &str, message: &str) -> Value {
    let typ = match status {
        401 | 403 => "authentication_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        500..=599 => "api_error",
        _ => "invalid_request_error",
    };
    json!({
        "type": "error",
        "error": {"type": typ, "message": format!("{code}: {message}")},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── request translation ──

    #[test]
    fn basic_request_translation() {
        let req = json!({
            "model": "A+",
            "max_tokens": 1024,
            "system": "be terse",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello"},
                {"role": "user", "content": "bye"}
            ],
            "temperature": 0.2,
            "stream": false
        });
        let out = anthropic_to_openai(&req).unwrap();
        assert_eq!(out["model"], "A+");
        assert_eq!(out["max_tokens"], 1024);
        assert_eq!(out["messages"][0]["role"], "system");
        assert_eq!(out["messages"][0]["content"], "be terse");
        assert_eq!(out["messages"].as_array().unwrap().len(), 4);
        assert_eq!(out["temperature"], 0.2);
    }

    #[test]
    fn system_blocks_and_stop_sequences() {
        let req = json!({
            "model": "A",
            "max_tokens": 10,
            "system": [{"type": "text", "text": "one"}, {"type": "text", "text": "two"}],
            "messages": [{"role": "user", "content": "x"}],
            "stop_sequences": ["END"]
        });
        let out = anthropic_to_openai(&req).unwrap();
        assert_eq!(out["messages"][0]["content"], "one\n\ntwo");
        assert_eq!(out["stop"], json!(["END"]));
    }

    #[test]
    fn tools_and_tool_choice_map() {
        let req = json!({
            "model": "A", "max_tokens": 10,
            "messages": [{"role": "user", "content": "x"}],
            "tools": [{"name": "get_weather", "description": "w",
                        "input_schema": {"type": "object", "properties": {}}}],
            "tool_choice": {"type": "tool", "name": "get_weather"}
        });
        let out = anthropic_to_openai(&req).unwrap();
        assert_eq!(out["tools"][0]["function"]["name"], "get_weather");
        assert_eq!(out["tool_choice"]["function"]["name"], "get_weather");
    }

    #[test]
    fn tool_use_and_tool_result_roundtrip_shapes() {
        // review F8: the openai wire shape upstreams actually validate —
        // assistant-level tool_calls + sibling role:"tool" messages
        let req = json!({
            "model": "A", "max_tokens": 10,
            "messages": [
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "checking"},
                    {"type": "tool_use", "id": "t1", "name": "get_weather",
                     "input": {"city": "sf"}},
                    {"type": "tool_use", "id": "t2", "name": "get_time",
                     "input": {"tz": "pst"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t2", "content": "9:41"},
                    {"type": "tool_result", "tool_use_id": "t1", "content": "sunny 22"},
                    {"type": "text", "text": "thanks"}
                ]}
            ]
        });
        let out = anthropic_to_openai(&req).unwrap();
        let msgs = out["messages"].as_array().unwrap();
        // assistant carries tool_calls at the message level
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"], "checking");
        let calls = msgs[1]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["id"], "t1");
        assert_eq!(calls[0]["type"], "function");
        assert_eq!(calls[0]["function"]["name"], "get_weather");
        assert_eq!(calls[0]["function"]["arguments"], "{\"city\":\"sf\"}");
        // each tool_result is its own sibling message with role "tool"
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "t2");
        assert_eq!(msgs[2]["content"], "9:41");
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "t1");
        assert_eq!(msgs[3]["content"], "sunny 22");
        // same-turn user text follows the tool results
        assert_eq!(msgs[4]["role"], "user");
        assert_eq!(msgs[4]["content"], "thanks");
        // no content-part tool shapes anywhere
        for m in msgs {
            if let Some(arr) = m["content"].as_array() {
                for p in arr {
                    assert_ne!(p["type"], "tool_call");
                    assert_ne!(p["type"], "tool");
                }
            }
        }
    }

    #[test]
    fn tool_only_assistant_turn_has_null_content() {
        let req = json!({
            "model": "A", "max_tokens": 10,
            "messages": [
                {"role": "user", "content": "go"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t9", "name": "act", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t9", "content": ""}
                ]}
            ]
        });
        let out = anthropic_to_openai(&req).unwrap();
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[1]["content"], Value::Null);
        assert_eq!(msgs[1]["tool_calls"][0]["function"]["arguments"], "{}");
        assert_eq!(msgs[2]["role"], "tool");
    }

    #[test]
    fn image_blocks_become_data_urls() {
        let req = json!({
            "model": "A", "max_tokens": 10,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "what is this?"},
                {"type": "image", "source": {"type": "base64",
                    "media_type": "image/png", "data": "QUJD"}}
            ]}]
        });
        let out = anthropic_to_openai(&req).unwrap();
        let parts = out["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,QUJD");
    }

    #[test]
    fn consecutive_same_role_messages_merge() {
        let req = json!({
            "model": "A", "max_tokens": 10,
            "messages": [
                {"role": "user", "content": "one"},
                {"role": "user", "content": "two"}
            ]
        });
        let out = anthropic_to_openai(&req).unwrap();
        assert_eq!(out["messages"].as_array().unwrap().len(), 1);
        assert_eq!(out["messages"][0]["content"], "one\ntwo");
    }

    #[test]
    fn missing_messages_is_rejected() {
        let e = anthropic_to_openai(&json!({"model": "A"})).unwrap_err();
        assert!(e.contains("messages"));
    }

    // ── response translation ──

    #[test]
    fn json_response_translation() {
        let openai = json!({
            "id": "chatcmpl-1", "model": "stub/any",
            "choices": [{"message": {"role": "assistant", "content": "hi there"},
                          "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 3}
        });
        let out = openai_to_anthropic(&openai);
        assert_eq!(out["type"], "message");
        assert_eq!(out["role"], "assistant");
        assert_eq!(out["content"][0]["type"], "text");
        assert_eq!(out["content"][0]["text"], "hi there");
        assert_eq!(out["stop_reason"], "end_turn");
        assert_eq!(out["usage"]["input_tokens"], 10);
        assert_eq!(out["usage"]["output_tokens"], 3);
    }

    #[test]
    fn json_response_tool_calls() {
        let openai = json!({
            "id": "x", "model": "m",
            "choices": [{"message": {"role": "assistant", "content": Value::Null,
                "tool_calls": [{"id": "call_1", "type": "function",
                    "function": {"name": "f", "arguments": "{\"a\":1}"}}]},
                "finish_reason": "tool_calls"}]
        });
        let out = openai_to_anthropic(&openai);
        let block = &out["content"][0];
        assert_eq!(block["type"], "tool_use");
        assert_eq!(block["name"], "f");
        assert_eq!(block["input"]["a"], 1);
        assert_eq!(out["stop_reason"], "tool_use");
    }

    // ── SSE translation ──

    fn drive(chunks: &[&str]) -> Vec<String> {
        let mut t = SseTranslator::new();
        let mut out = t.push_bytes(chunks.concat().as_bytes());
        out.extend(t.finish());
        out
    }

    #[test]
    fn sse_text_stream_sequence() {
        let frames = drive(&[
            "data: {\"id\":\"c1\",\"model\":\"stub/any\",\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        ]);
        let joined = frames.concat();
        assert!(
            joined.starts_with("event: message_start\n"),
            "got: {joined}"
        );
        assert!(joined.contains("event: content_block_start\n"));
        assert_eq!(joined.matches("event: content_block_delta\n").count(), 2);
        assert!(joined.contains("\"text\":\"Hel\""));
        assert!(joined.contains("\"text\":\"lo\""));
        assert!(joined.contains("event: content_block_stop\n"));
        assert!(joined.contains("event: message_delta\n"));
        assert!(joined.contains("\"stop_reason\":\"end_turn\""));
        assert!(joined.contains("\"output_tokens\":2"));
        assert!(joined.contains("\"input_tokens\":5"));
        assert!(joined
            .trim_end()
            .ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}"));
    }

    #[test]
    fn sse_chunks_split_mid_event() {
        // one openai event split across two TCP chunks
        let mut t = SseTranslator::new();
        let a = t.push_bytes(b"data: {\"choices\":[{\"delta\":{\"conte");
        assert!(a.is_empty(), "no complete event yet");
        let b = t.push_bytes(b"nt\":\"x\"}}]}\n\ndata: [DONE]\n\n");
        let all = [a, b, t.finish()].concat();
        let joined = all.concat();
        assert!(joined.contains("\"text\":\"x\""));
        assert!(joined.contains("event: message_stop"));
    }

    #[test]
    fn sse_tool_call_stream() {
        let frames = drive(&[
            "data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"get_w\",\"arguments\":\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"sf\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        ]);
        let joined = frames.concat();
        assert!(joined.contains("\"type\":\"tool_use\""));
        assert!(joined.contains("\"name\":\"get_w\""));
        // the empty leading arguments fragment is skipped; the two real
        // fragments arrive as input_json_delta events
        assert_eq!(joined.matches("\"type\":\"input_json_delta\"").count(), 2);
        // fragments concatenate into valid JSON
        let args: String = joined
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|d| serde_json::from_str::<Value>(d).ok())
            .filter(|v| v["delta"]["type"] == "input_json_delta")
            .map(|v| v["delta"]["partial_json"].as_str().unwrap().to_string())
            .collect();
        let parsed: Value = serde_json::from_str(&args).unwrap();
        assert_eq!(parsed["city"], "sf");
        assert!(joined.contains("\"stop_reason\":\"tool_use\""));
    }

    #[test]
    fn sse_missing_done_still_closes() {
        // upstream that just EOFs without [DONE]
        let frames = drive(&["data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n"]);
        let joined = frames.concat();
        assert!(joined.contains("event: message_delta"));
        assert!(joined.contains("event: message_stop"));
    }

    #[test]
    fn json_response_maps_reasoning_content_to_thinking_block() {
        let openai = json!({
            "id": "x", "model": "glm-5.3",
            "choices": [{"message": {"role": "assistant", "content": "ok",
                "reasoning_content": "the user wants ok"},
                "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 9}
        });
        let out = openai_to_anthropic(&openai);
        assert_eq!(out["content"][0]["type"], "thinking");
        assert_eq!(out["content"][0]["thinking"], "the user wants ok");
        assert_eq!(out["content"][1]["type"], "text");
        assert_eq!(out["content"][1]["text"], "ok");
    }

    #[test]
    fn sse_maps_reasoning_deltas_to_thinking_block() {
        let frames = drive(&[
            "data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}

",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking hard\"}}]}

",
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}

",
            "data: [DONE]

",
        ]);
        let joined = frames.concat();
        assert!(joined.contains("\"type\":\"thinking_delta\""));
        assert!(joined.contains("\"thinking\":\"thinking hard\""));
        // thinking block closes before the text block opens
        let t_stop = joined.find("event: content_block_stop").unwrap();
        let text_start = joined.find("\"text_delta\"").unwrap();
        assert!(t_stop < text_start, "thinking closed before text starts");
        assert!(joined.contains("\"text\":\"ok\""));
    }

    #[test]
    fn anthropic_error_shapes() {
        assert_eq!(
            anthropic_error(401, "unauthorized", "no key")["error"]["type"],
            "authentication_error"
        );
        assert_eq!(
            anthropic_error(429, "arouter_exhausted", "x")["error"]["type"],
            "rate_limit_error"
        );
        assert_eq!(
            anthropic_error(400, "bad", "x")["error"]["type"],
            "invalid_request_error"
        );
    }
}
