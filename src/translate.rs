//! Convert Chat Completions and Messages requests, responses, errors and SSE.
//!
//! Text, images, function tools, stop reasons and cache-aware usage are mapped.
//! `n > 1` and JSON-schema response formats are rejected. Other unsupported
//! request options, non-text/image/tool blocks, cache-control hints, thinking
//! blocks and signatures are dropped. Messages `thinking` is dropped; chat
//! reasoning effort enables thinking with a fixed budget. If no token limit is
//! supplied, the default is raised above that budget. Explicit smaller limits
//! are rejected. Chat tools are buffered until stream end so interleaved calls
//! become serial Messages content blocks. Only choice zero is translated.

use std::collections::BTreeMap;

use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Client sent OpenAI chat; upstream is Anthropic `/v1/messages`.
    ChatToMessages,
    /// Client sent Anthropic messages; upstream is OpenAI `/v1/chat/completions`.
    MessagesToChat,
}

impl Direction {
    pub fn upstream_path(&self) -> &'static str {
        match self {
            Self::ChatToMessages => "/v1/messages",
            Self::MessagesToChat => "/v1/chat/completions",
        }
    }
}

/// A request cannot be represented in the upstream format.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct TranslateError {
    pub message: String,
}

fn invalid(message: &str) -> TranslateError {
    TranslateError {
        message: message.into(),
    }
}

pub fn validate_chat_limits(body: &Value) -> Result<(), TranslateError> {
    if let (Some(a), Some(b)) = (
        body.get("max_tokens").filter(|v| !v.is_null()),
        body.get("max_completion_tokens").filter(|v| !v.is_null()),
    ) && a != b
    {
        return Err(invalid(
            "max_tokens and max_completion_tokens must not have different values",
        ));
    }
    Ok(())
}

/// The effective explicit output limit, in the order used by Chat translation.
pub fn output_token_limit(body: &Value) -> Option<&Value> {
    ["max_completion_tokens", "max_tokens", "max_output_tokens"]
        .iter()
        .find_map(|key| body.get(*key).filter(|v| !v.is_null()))
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn number(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn array(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}

fn copy_fields(from: &Value, to: &mut Value, keys: &[&str]) {
    for key in keys {
        if let Some(v) = from.get(*key) {
            to[*key] = v.clone();
        }
    }
}

fn chat_parts(content: &Value) -> Result<Vec<Value>, TranslateError> {
    if let Some(s) = content.as_str() {
        return Ok(if s.is_empty() {
            vec![]
        } else {
            vec![json!({"type":"text","text":s})]
        });
    }
    let mut out = Vec::new();
    for part in array(content) {
        match text(part, "type") {
            "text" if !text(part, "text").is_empty() => {
                out.push(json!({"type":"text","text":text(part,"text")}))
            }
            "image_url" => {
                let url = part["image_url"]
                    .as_str()
                    .unwrap_or_else(|| text(&part["image_url"], "url"));
                let source = if let Some(data) = url.strip_prefix("data:") {
                    let (media, data) = data
                        .split_once(";base64,")
                        .ok_or_else(|| invalid("image data URL must use base64 encoding"))?;
                    json!({"type":"base64","media_type":media,"data":data})
                } else if url.starts_with("https://") || url.starts_with("http://") {
                    json!({"type":"url","url":url})
                } else {
                    return Err(invalid("image URL must be HTTP(S) or a base64 data URL"));
                };
                out.push(json!({"type":"image","source":source}));
            }
            _ => {}
        }
    }
    Ok(out)
}

fn message_parts(content: &Value) -> Vec<Value> {
    if let Some(s) = content.as_str() {
        return vec![json!({"type":"text","text":s})];
    }
    array(content)
        .iter()
        .filter_map(|part| match text(part, "type") {
            "text" => Some(json!({"type":"text","text":text(part,"text")})),
            "image" => {
                let source = &part["source"];
                let url = match text(source, "type") {
                    "base64" => format!(
                        "data:{};base64,{}",
                        text(source, "media_type"),
                        text(source, "data")
                    ),
                    "url" => text(source, "url").to_owned(),
                    _ => return None,
                };
                Some(json!({"type":"image_url","image_url":{"url":url}}))
            }
            _ => None,
        })
        .collect()
}

// Chat represents text-only content as a string, and multimodal content as parts.
fn chat_content(parts: Vec<Value>) -> Value {
    if parts.iter().all(|p| p["type"] == "text") {
        Value::String(
            parts
                .iter()
                .map(|p| text(p, "text"))
                .collect::<Vec<_>>()
                .join(""),
        )
    } else {
        Value::Array(parts)
    }
}

fn push_message(messages: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if let Some(last) = messages.last_mut().filter(|last| last["role"] == role) {
        last["content"]
            .as_array_mut()
            .expect("content is an array")
            .extend(parts);
    } else {
        messages.push(json!({"role":role,"content":parts}));
    }
}

fn tool_use(call: &Value) -> Result<Value, TranslateError> {
    let function = &call["function"];
    let args = function
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or("{}");
    let input: Value = serde_json::from_str(args)
        .map_err(|_| invalid("tool call arguments must be valid JSON"))?;
    if !input.is_object() {
        return Err(invalid("tool call arguments must be a JSON object"));
    }
    let mut tool = json!({"type":"tool_use","id":call["id"],"name":function["name"],"input":input});
    if let Some(signature) = call.get("thought_signature") {
        tool["thought_signature"] = signature.clone();
    }
    Ok(tool)
}

fn tool_call(block: &Value) -> Value {
    let mut call = json!({"id":block["id"],"type":"function","function":{
        "name":block["name"],"arguments":block.get("input").unwrap_or(&json!({})).to_string()}});
    if let Some(signature) = block.get("thought_signature") {
        call["thought_signature"] = signature.clone();
    }
    call
}

fn choice_to_messages(choice: &Value) -> Value {
    match choice.as_str().unwrap_or("") {
        "auto" => json!({"type":"auto"}),
        "none" => json!({"type":"none"}),
        "required" => json!({"type":"any"}),
        _ => json!({"type":"tool","name":choice["function"]["name"]}),
    }
}

fn choice_to_chat(choice: &Value) -> Value {
    match text(choice, "type") {
        "any" => json!("required"),
        "tool" => json!({"type":"function","function":{"name":choice["name"]}}),
        "none" => json!("none"),
        _ => json!("auto"),
    }
}

pub fn translate_request(dir: Direction, body: &Value) -> Result<Value, TranslateError> {
    if !body.is_object() {
        return Err(invalid("request body must be a JSON object"));
    }
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("messages must be an array"))?;
    let mut out = json!({});
    copy_fields(body, &mut out, &["model", "temperature", "top_p", "stream"]);
    let mut translated = Vec::new();
    match dir {
        Direction::ChatToMessages => {
            validate_chat_limits(body)?;
            if number(body, "n") > 1 {
                return Err(invalid("cross-format translation supports only n = 1"));
            }
            if body["response_format"]["type"] == "json_schema" {
                return Err(invalid(
                    "JSON-schema response format is not supported across API formats",
                ));
            }
            let mut system = Vec::new();
            for message in messages {
                let role = text(message, "role");
                let mut parts = chat_parts(&message["content"])?;
                match role {
                    "system" | "developer" => system.extend(parts),
                    "user" | "assistant" => {
                        for call in array(&message["tool_calls"]) {
                            parts.push(tool_use(call)?);
                        }
                        push_message(&mut translated, role, parts);
                    }
                    "tool" => push_message(
                        &mut translated,
                        "user",
                        vec![json!({
                        "type":"tool_result","tool_use_id":message["tool_call_id"],
                        "content":parts})],
                    ),
                    _ => return Err(invalid("unsupported chat message role")),
                }
            }
            if !system.is_empty() {
                out["system"] = json!(system);
            }
            let limit = output_token_limit(body);
            out["max_tokens"] = limit.cloned().unwrap_or(json!(4096));
            let budget = match text(body, "reasoning_effort") {
                "low" => 1024,
                "medium" => 4096,
                "high" => 16384,
                _ => 0,
            };
            if budget > 0 {
                if number(&out, "max_tokens") <= budget {
                    if limit.is_some() {
                        return Err(invalid("max_tokens must exceed the thinking budget"));
                    }
                    out["max_tokens"] = json!(budget + 1);
                }
                out["thinking"] = json!({"type":"enabled","budget_tokens":budget});
            }
            if let Some(stop) = body.get("stop").filter(|s| !s.is_null()) {
                out["stop_sequences"] = if stop.is_string() {
                    json!([stop])
                } else {
                    stop.clone()
                };
            }
            if let Some(user) = body.get("user") {
                out["metadata"] = json!({"user_id":user});
            }
            if let Some(tools) = body.get("tools") {
                out["tools"] = json!(array(tools).iter().map(|t| {
                    let f = &t["function"];
                    let mut tool = json!({"name":f["name"],"input_schema":f.get("parameters").cloned().unwrap_or(json!({"type":"object"}))});
                    copy_fields(f,&mut tool,&["description"]);
                    tool
                }).collect::<Vec<_>>());
            }
            if let Some(choice) = body.get("tool_choice") {
                out["tool_choice"] = choice_to_messages(choice);
            }
        }
        Direction::MessagesToChat => {
            let mut tool_images = Vec::new();
            if let Some(system) = body.get("system") {
                translated
                    .push(json!({"role":"system","content":chat_content(message_parts(system))}));
            }
            for message in messages {
                let role = text(message, "role");
                if !matches!(role, "user" | "assistant") {
                    return Err(invalid("unsupported Messages role"));
                }
                let content = &message["content"];
                if content.is_string() {
                    flush_tool_images(&mut translated, &mut tool_images);
                    translated.push(json!({"role":role,"content":content}));
                    continue;
                }
                let mut parts = Vec::new();
                let mut calls = Vec::new();
                for block in array(content) {
                    match text(block, "type") {
                        "tool_use" => {
                            flush_tool_images(&mut translated, &mut tool_images);
                            calls.push(tool_call(block));
                        }
                        "tool_result" => {
                            flush_chat_message(&mut translated, role, &mut parts, &mut calls);
                            let (images, texts): (Vec<_>, Vec<_>) =
                                message_parts(&block["content"])
                                    .into_iter()
                                    .partition(|p| p["type"] == "image_url");
                            if !images.is_empty() {
                                tool_images.push(json!({"type":"text","text":format!("Images from tool result {}:", text(block, "tool_use_id"))}));
                                tool_images.extend(images);
                            }
                            translated
                                .push(json!({"role":"tool","tool_call_id":block["tool_use_id"],
                                "content":chat_content(texts)}));
                        }
                        _ => {
                            flush_tool_images(&mut translated, &mut tool_images);
                            parts.extend(message_parts(&json!([block])));
                        }
                    }
                }
                flush_chat_message(&mut translated, role, &mut parts, &mut calls);
            }
            flush_tool_images(&mut translated, &mut tool_images);
            copy_fields(body, &mut out, &["max_tokens"]);
            if let Some(stop) = body.get("stop_sequences") {
                out["stop"] = stop.clone();
            }
            if let Some(user) = body["metadata"].get("user_id") {
                out["user"] = user.clone();
            }
            if body["stream"] == true {
                out["stream_options"] = json!({"include_usage":true});
            }
            if let Some(tools) = body.get("tools") {
                out["tools"] = json!(
                    array(tools)
                        .iter()
                        .map(|t| {
                            let mut f = json!({"name":t["name"],"parameters":t["input_schema"]});
                            copy_fields(t, &mut f, &["description"]);
                            json!({"type":"function","function":f})
                        })
                        .collect::<Vec<_>>()
                );
            }
            if let Some(choice) = body.get("tool_choice") {
                out["tool_choice"] = choice_to_chat(choice);
            }
        }
    }
    out["messages"] = json!(translated);
    Ok(out)
}

fn flush_tool_images(messages: &mut Vec<Value>, images: &mut Vec<Value>) {
    if !images.is_empty() {
        messages.push(json!({"role":"user","content":std::mem::take(images)}));
    }
}

fn flush_chat_message(
    messages: &mut Vec<Value>,
    role: &str,
    parts: &mut Vec<Value>,
    calls: &mut Vec<Value>,
) {
    if parts.is_empty() && calls.is_empty() {
        return;
    }
    let mut message = json!({"role":role,"content":chat_content(std::mem::take(parts))});
    if !calls.is_empty() {
        if message["content"] == "" {
            message["content"] = Value::Null;
        }
        message["tool_calls"] = json!(std::mem::take(calls));
    }
    messages.push(message);
}

fn finish_to_chat(reason: &str) -> &'static str {
    match reason {
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        _ => "stop",
    }
}

fn finish_to_messages(reason: &str) -> &'static str {
    match reason {
        "length" => "max_tokens",
        "tool_calls" | "function_call" => "tool_use",
        _ => "end_turn",
    }
}

fn usage_to_chat(usage: &Value) -> Value {
    let input = number(usage, "input_tokens")
        .saturating_add(number(usage, "cache_read_input_tokens"))
        .saturating_add(number(usage, "cache_creation_input_tokens"));
    let output = number(usage, "output_tokens");
    json!({"prompt_tokens":input,"completion_tokens":output,"total_tokens":input.saturating_add(output),
        "prompt_tokens_details":{"cached_tokens":number(usage,"cache_read_input_tokens")}})
}

fn usage_to_messages(usage: &Value) -> Value {
    let prompt = number(usage, "prompt_tokens");
    let cached = number(&usage["prompt_tokens_details"], "cached_tokens").min(prompt);
    json!({"input_tokens":prompt-cached,"cache_read_input_tokens":cached,"output_tokens":number(usage,"completion_tokens")})
}

pub fn translate_response(dir: Direction, body: &Value) -> Value {
    match dir {
        Direction::ChatToMessages => {
            let mut content = String::new();
            let mut calls = Vec::new();
            for block in array(&body["content"]) {
                match text(block, "type") {
                    "text" => content.push_str(text(block, "text")),
                    "tool_use" => calls.push(tool_call(block)),
                    _ => {}
                }
            }
            let mut message = json!({"role":"assistant","content":content});
            if !calls.is_empty() {
                if content.is_empty() {
                    message["content"] = Value::Null;
                }
                message["tool_calls"] = json!(calls);
            }
            json!({"id":body["id"],"object":"chat.completion","created":0,"model":body["model"],
                "choices":[{"index":0,"message":message,"finish_reason":finish_to_chat(text(body,"stop_reason"))}],
                "usage":usage_to_chat(&body["usage"])})
        }
        Direction::MessagesToChat => {
            let choice = &body["choices"][0];
            let message = &choice["message"];
            let mut blocks = chat_parts(&message["content"]).unwrap_or_default();
            for call in array(&message["tool_calls"]) {
                // Success responses should carry valid JSON arguments. Retain malformed
                // arguments as a string rather than panic or silently discard the call.
                blocks.push(tool_use(call).unwrap_or_else(|_| {
                    json!({"type":"tool_use","id":call["id"],
                    "name":call["function"]["name"],"input":call["function"]["arguments"]})
                }));
            }
            json!({"id":body["id"],"type":"message","role":"assistant","model":body["model"],
                "content":blocks,"stop_reason":finish_to_messages(text(choice,"finish_reason")),
                "stop_sequence":null,"usage":usage_to_messages(&body["usage"])})
        }
    }
}

pub fn translate_error(dir: Direction, status: u16, body: &[u8]) -> Vec<u8> {
    let body: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let message = body["error"]["message"]
        .as_str()
        .or_else(|| body["message"].as_str())
        .unwrap_or("upstream request failed");
    let kind = match text(&body["error"], "type") {
        known @ ("invalid_request_error"
        | "authentication_error"
        | "permission_error"
        | "not_found_error"
        | "request_too_large"
        | "rate_limit_error"
        | "overloaded_error") => known,
        _ => match status {
            400 | 422 => "invalid_request_error",
            401 => "authentication_error",
            403 => "permission_error",
            404 => "not_found_error",
            413 => "request_too_large",
            429 => "rate_limit_error",
            529 => "overloaded_error",
            _ => "api_error",
        },
    };
    let error = json!({"type":kind,"message":message});
    let out = match dir {
        Direction::ChatToMessages => {
            json!({"error":{"type":kind,"message":message,"param":null,"code":null}})
        }
        Direction::MessagesToChat => json!({"type":"error","error":error}),
    };
    out.to_string().into_bytes()
}

#[derive(Debug, Default)]
struct PendingTool {
    id: String,
    name: String,
    thought_signature: Option<String>,
    arguments: Vec<String>,
}

/// Incremental SSE translator. `new` omits optional Chat usage chunks; enable
/// them with `with_usage(true)` when the Chat client requested `include_usage`.
#[derive(Debug)]
pub struct StreamTranslator {
    dir: Direction,
    model: String,
    id: String,
    buf: Vec<u8>,
    include_usage: bool,
    started: bool,
    ended: bool,
    final_chunk: bool,
    usage: Value,
    stop: String,
    tool_indices: BTreeMap<u64, (usize, bool)>,
    tools: BTreeMap<u64, PendingTool>,
    tool_bytes: usize,
    pub failed: bool,
    next_block: usize,
    text_block: Option<usize>,
}

impl StreamTranslator {
    pub fn new(dir: Direction, model: &str) -> Self {
        Self {
            dir,
            model: model.into(),
            id: "translated".into(),
            buf: Vec::new(),
            include_usage: false,
            started: false,
            ended: false,
            final_chunk: false,
            usage: json!({}),
            stop: String::new(),
            tool_indices: BTreeMap::new(),
            tools: BTreeMap::new(),
            tool_bytes: 0,
            failed: false,
            next_block: 0,
            text_block: None,
        }
    }

    pub fn with_usage(mut self, include_usage: bool) -> Self {
        self.include_usage = include_usage;
        self
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.ended {
            return Vec::new();
        }
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((end, sep)) = find_event_end(&self.buf) {
            let raw: Vec<u8> = self.buf.drain(..end + sep).collect();
            self.event(&raw[..end], &mut out);
        }
        if self.buf.len() > 4 * 1024 * 1024 {
            self.stream_error(
                &json!({"error":{"message":"upstream SSE event exceeds 4 MiB"}}),
                &mut out,
            );
            self.buf.clear();
        }
        out
    }

    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        let remaining = std::mem::take(&mut self.buf);
        if !remaining.is_empty() {
            self.event(&remaining, &mut out);
        }
        if self.started && !self.ended {
            self.end(&mut out);
        }
        out
    }

    fn event(&mut self, raw: &[u8], out: &mut Vec<u8>) {
        if self.ended {
            return;
        }
        let Ok(s) = std::str::from_utf8(raw) else {
            return;
        };
        let mut event_type = "";
        let mut lines = Vec::new();
        for line in s.lines() {
            if let Some(data) = line.strip_prefix("data:") {
                lines.push(data.strip_prefix(' ').unwrap_or(data));
            }
            if let Some(name) = line.strip_prefix("event:") {
                event_type = name.trim();
            }
        }
        let data = lines.join("\n");
        if data == "[DONE]" {
            self.end(out);
            return;
        }
        let Ok(mut value) = serde_json::from_str::<Value>(&data) else {
            return;
        };
        if !value.is_object() {
            return;
        }
        if value.get("type").is_none() && !event_type.is_empty() {
            value["type"] = json!(event_type);
        }
        if value.get("error").is_some() {
            self.stream_error(&value, out);
            return;
        }
        match self.dir {
            Direction::ChatToMessages => self.anthropic_event(&value, out),
            Direction::MessagesToChat => self.chat_event(&value, out),
        }
    }

    fn stream_error(&mut self, value: &Value, out: &mut Vec<u8>) {
        self.failed = true;
        let translated = translate_error(self.dir, 502, value.to_string().as_bytes());
        let error = serde_json::from_slice(&translated).expect("serialized error is JSON");
        emit(
            out,
            if self.dir == Direction::MessagesToChat {
                Some("error")
            } else {
                None
            },
            &error,
        );
        self.ended = true;
    }

    fn chat_chunk(&self, delta: Value, finish: Option<&str>) -> Value {
        json!({"id":self.id,"object":"chat.completion.chunk","created":0,"model":self.model,
            "choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    }

    fn start(&mut self, out: &mut Vec<u8>) {
        if self.started {
            return;
        }
        self.started = true;
        match self.dir {
            Direction::ChatToMessages => emit(
                out,
                None,
                &self.chat_chunk(json!({"role":"assistant","content":""}), None),
            ),
            Direction::MessagesToChat => emit(
                out,
                Some("message_start"),
                &json!({"type":"message_start","message":{
                "id":self.id,"type":"message","role":"assistant","model":self.model,"content":[],
                "stop_reason":null,"stop_sequence":null,"usage":usage_to_messages(&self.usage)}}),
            ),
        }
    }

    fn anthropic_event(&mut self, value: &Value, out: &mut Vec<u8>) {
        match text(value, "type") {
            "message_start" => {
                let message = &value["message"];
                if let Some(id) = message["id"].as_str() {
                    self.id = id.into();
                }
                if let Some(model) = message["model"].as_str() {
                    self.model = model.into();
                }
                self.merge_usage(&message["usage"]);
                self.start(out);
            }
            "content_block_start" => {
                self.start(out);
                let block = &value["content_block"];
                match text(block, "type") {
                    "text" if !text(block, "text").is_empty() => emit(
                        out,
                        None,
                        &self.chat_chunk(json!({"content":block["text"]}), None),
                    ),
                    "tool_use" => {
                        let next = self.tool_indices.len();
                        let has_input = block.get("input").is_some_and(|input| {
                            input.is_object() && !input.as_object().unwrap().is_empty()
                        });
                        self.tool_indices
                            .insert(number(value, "index"), (next, has_input));
                        emit(out,None,&self.chat_chunk(json!({"tool_calls":[{"index":next,"id":block["id"],"type":"function",
                            "function":{"name":block["name"],"arguments":""}}]}),None));
                        if has_input {
                            emit(out,None,&self.chat_chunk(json!({"tool_calls":[{"index":next,"function":{"arguments":block["input"].to_string()}}]}),None));
                        }
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                self.start(out);
                let delta = &value["delta"];
                match text(delta, "type") {
                    "text_delta" => emit(
                        out,
                        None,
                        &self.chat_chunk(json!({"content":delta["text"]}), None),
                    ),
                    "input_json_delta" => {
                        if let Some((index, has_input)) =
                            self.tool_indices.get_mut(&number(value, "index"))
                        {
                            *has_input |= !text(delta, "partial_json").is_empty();
                            let index = *index;
                            emit(out,None,&self.chat_chunk(json!({"tool_calls":[{"index":index,"function":{"arguments":delta["partial_json"]}}]}),None));
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => self.complete_tool_input(number(value, "index"), out),
            "message_delta" => {
                self.merge_usage(&value["usage"]);
                if let Some(reason) = value["delta"]["stop_reason"].as_str() {
                    self.stop = finish_to_chat(reason).into();
                    if !self.final_chunk {
                        self.start(out);
                        emit(out, None, &self.chat_chunk(json!({}), Some(&self.stop)));
                        self.final_chunk = true;
                    }
                }
            }
            "message_stop" => self.end(out),
            _ => {}
        }
    }

    fn merge_usage(&mut self, usage: &Value) {
        if let Some(fields) = usage.as_object() {
            for (key, value) in fields {
                self.usage[key] = value.clone();
            }
        }
    }

    fn complete_tool_input(&mut self, block: u64, out: &mut Vec<u8>) {
        if let Some((index, has_input)) = self.tool_indices.get_mut(&block)
            && !*has_input
        {
            *has_input = true;
            let index = *index;
            emit(
                out,
                None,
                &self.chat_chunk(
                    json!({"tool_calls":[{"index":index,"function":{"arguments":"{}"}}]}),
                    None,
                ),
            );
        }
    }

    fn chat_event(&mut self, value: &Value, out: &mut Vec<u8>) {
        if let Some(id) = value["id"].as_str() {
            self.id = id.into();
        }
        if let Some(model) = value["model"].as_str() {
            self.model = model.into();
        }
        self.merge_usage(&value["usage"]);
        self.start(out);
        let choice = &value["choices"][0];
        let delta = &choice["delta"];
        if let Some(content) = delta["content"].as_str().filter(|s| !s.is_empty()) {
            let index = match self.text_block {
                Some(index) => index,
                None => {
                    let index = self.next_block;
                    self.next_block += 1;
                    self.text_block = Some(index);
                    emit(
                        out,
                        Some("content_block_start"),
                        &json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}),
                    );
                    index
                }
            };
            emit(
                out,
                Some("content_block_delta"),
                &json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":content}}),
            );
        }
        for call in array(&delta["tool_calls"]) {
            let index = number(call, "index");
            let bytes = text(call, "id").len()
                + text(&call["function"], "name").len()
                + text(&call["function"], "arguments").len()
                + text(call, "thought_signature").len()
                + std::mem::size_of::<String>();
            if (self.tools.len() >= 128 && !self.tools.contains_key(&index))
                || bytes > (16 * 1024 * 1024usize).saturating_sub(self.tool_bytes)
            {
                self.stream_error(
                    &json!({"error":{"message":"upstream tool buffer limit exceeded"}}),
                    out,
                );
                return;
            }
            self.tool_bytes += bytes;
            let tool = self.tools.entry(number(call, "index")).or_default();
            tool.id.push_str(text(call, "id"));
            tool.name.push_str(text(&call["function"], "name"));
            if let Some(signature) = call["thought_signature"].as_str() {
                tool.thought_signature = Some(signature.into());
            }
            if let Some(arguments) = call["function"]["arguments"].as_str() {
                tool.arguments.push(arguments.into());
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.stop = finish_to_messages(reason).into();
        }
    }

    fn end(&mut self, out: &mut Vec<u8>) {
        if self.ended {
            return;
        }
        self.start(out);
        self.ended = true;
        match self.dir {
            Direction::ChatToMessages => {
                let blocks: Vec<_> = self.tool_indices.keys().copied().collect();
                for block in blocks {
                    self.complete_tool_input(block, out);
                }
                if !self.final_chunk {
                    let stop = if self.stop.is_empty() {
                        "stop"
                    } else {
                        &self.stop
                    };
                    emit(out, None, &self.chat_chunk(json!({}), Some(stop)));
                }
                if self.include_usage {
                    emit(
                        out,
                        None,
                        &json!({"id":self.id,"object":"chat.completion.chunk","created":0,"model":self.model,
                        "choices":[],"usage":usage_to_chat(&self.usage)}),
                    );
                }
                out.extend_from_slice(b"data: [DONE]\n\n");
            }
            Direction::MessagesToChat => {
                if let Some(index) = self.text_block.take() {
                    block_stop(out, index);
                }
                for tool in self.tools.values() {
                    let index = self.next_block;
                    self.next_block += 1;
                    let mut block =
                        json!({"type":"tool_use","id":tool.id,"name":tool.name,"input":{}});
                    if let Some(signature) = &tool.thought_signature {
                        block["thought_signature"] = json!(signature);
                    }
                    emit(
                        out,
                        Some("content_block_start"),
                        &json!({"type":"content_block_start","index":index,
                        "content_block":block}),
                    );
                    for args in &tool.arguments {
                        emit(
                            out,
                            Some("content_block_delta"),
                            &json!({"type":"content_block_delta","index":index,
                            "delta":{"type":"input_json_delta","partial_json":args}}),
                        );
                    }
                    block_stop(out, index);
                }
                let stop = if self.stop.is_empty() {
                    "end_turn"
                } else {
                    &self.stop
                };
                emit(
                    out,
                    Some("message_delta"),
                    &json!({"type":"message_delta","delta":{"stop_reason":stop,"stop_sequence":null},
                    "usage":usage_to_messages(&self.usage)}),
                );
                emit(out, Some("message_stop"), &json!({"type":"message_stop"}));
            }
        }
    }
}

fn emit(out: &mut Vec<u8>, event: Option<&str>, value: &Value) {
    if let Some(name) = event {
        out.extend_from_slice(format!("event: {name}\n").as_bytes());
    }
    out.extend_from_slice(format!("data: {value}\n\n").as_bytes());
}

fn block_stop(out: &mut Vec<u8>, index: usize) {
    emit(
        out,
        Some("content_block_stop"),
        &json!({"type":"content_block_stop","index":index}),
    );
}

fn find_event_end(buf: &[u8]) -> Option<(usize, usize)> {
    for i in 0..buf.len().saturating_sub(1) {
        if buf[i..].starts_with(b"\n\n") {
            return Some((i, 2));
        }
        if buf[i..].starts_with(b"\r\n\r\n") {
            return Some((i, 4));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CM: Direction = Direction::ChatToMessages;
    const MC: Direction = Direction::MessagesToChat;

    #[test]
    fn translate_chat_request_roles_parts_and_options() {
        let v = translate_request(
            CM,
            &json!({"model":"m", "messages":[
            {"role":"system","content":"rules"}, {"role":"developer","content":"more"},
            {"role":"user","content":[{"type":"text","text":"look"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,YQ=="}},
                {"type":"image_url","image_url":{"url":"https://example.org/i.png"}}]},
            {"role":"user","content":"again"}],
            "max_completion_tokens":9000, "temperature":0.2, "top_p":0.8,
            "stop":"END", "stream":true, "user":"u", "reasoning_effort":"medium",
            "presence_penalty":1}),
        )
        .unwrap();
        assert_eq!(CM.upstream_path(), "/v1/messages");
        assert_eq!(
            v["system"],
            json!([{"type":"text","text":"rules"},{"type":"text","text":"more"}])
        );
        assert_eq!(v["messages"].as_array().unwrap().len(), 1);
        assert_eq!(
            v["messages"][0]["content"][1]["source"],
            json!({"type":"base64","media_type":"image/png","data":"YQ=="})
        );
        assert_eq!(
            v["messages"][0]["content"][2]["source"],
            json!({"type":"url","url":"https://example.org/i.png"})
        );
        assert_eq!(v["max_tokens"], 9000);
        assert_eq!(
            v["thinking"],
            json!({"type":"enabled","budget_tokens":4096})
        );
        assert_eq!(v["stop_sequences"], json!(["END"]));
        assert_eq!(v["metadata"]["user_id"], "u");
        assert_eq!(v["temperature"], 0.2);
        assert_eq!(v["top_p"], 0.8);
        assert_eq!(v["stream"], true);
        assert!(v.get("presence_penalty").is_none());
        assert_eq!(
            translate_request(CM, &json!({"messages":[]})).unwrap()["max_tokens"],
            4096
        );
    }

    #[test]
    fn translate_tool_call_request_round_trip() {
        let input = json!({"model":"m","max_tokens":100,"messages":[
            {"role":"assistant","content":"checking", "tool_calls":[
                {"id":"t1","type":"function","function":{"name":"search","arguments":"{\"q\":\"hi\"}"}},
                {"id":"t2","type":"function","function":{"name":"search","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"t1","content":"one"},
            {"role":"tool","tool_call_id":"t2","content":"two"}],
            "tools":[{"type":"function","function":{"name":"search","description":"find", "parameters":{"type":"object"}}}],
            "tool_choice":{"type":"function","function":{"name":"search"}}});
        let messages = translate_request(CM, &input).unwrap();
        assert_eq!(
            messages["messages"][0]["content"][1],
            json!({"type":"tool_use","id":"t1","name":"search","input":{"q":"hi"}})
        );
        assert_eq!(
            messages["messages"][1]["content"].as_array().unwrap().len(),
            2
        );
        assert_eq!(
            messages["tools"][0]["input_schema"],
            json!({"type":"object"})
        );
        assert_eq!(
            messages["tool_choice"],
            json!({"type":"tool","name":"search"})
        );
        let chat = translate_request(MC, &messages).unwrap();
        assert_eq!(chat["messages"], input["messages"]);
        assert_eq!(chat["tools"], input["tools"]);
        assert_eq!(chat["tool_choice"], input["tool_choice"]);
    }

    #[test]
    fn translate_messages_request_system_images_and_options() {
        let v = translate_request(MC, &json!({"model":"m","system":[{"type":"text","text":"rules","cache_control":{"type":"ephemeral"}}],
            "messages":[{"role":"user","content":[{"type":"text","text":"look"},
                {"type":"image","source":{"type":"base64","media_type":"image/jpeg","data":"YWJj"}},
                {"type":"image","source":{"type":"url","url":"https://example.org/x"}}]}],
            "max_tokens":80,"stop_sequences":["STOP"],"stream":true,"metadata":{"user_id":"u"},"thinking":{"type":"enabled"}})).unwrap();
        assert_eq!(MC.upstream_path(), "/v1/chat/completions");
        assert_eq!(v["messages"][0], json!({"role":"system","content":"rules"}));
        assert_eq!(
            v["messages"][1]["content"][1]["image_url"]["url"],
            "data:image/jpeg;base64,YWJj"
        );
        assert_eq!(
            v["messages"][1]["content"][2]["image_url"]["url"],
            "https://example.org/x"
        );
        assert_eq!(v["stream_options"]["include_usage"], true);
        assert_eq!(v["max_tokens"], 80);
        assert_eq!(v["stop"], json!(["STOP"]));
        assert_eq!(v["user"], "u");
        assert!(v.get("thinking").is_none());
        assert_eq!(
            translate_request(MC, &json!({"system":"s","messages":[]})).unwrap()["messages"][0]["content"],
            "s"
        );
    }

    #[test]
    fn translate_request_rejections_and_tool_choices() {
        for body in [
            json!({"messages":[],"n":2}),
            json!({"messages":[],"response_format":{"type":"json_schema"}}),
            json!({"messages":[{"role":"assistant","tool_calls":[{"function":{"arguments":"not json"}}]}]}),
            json!(null),
        ] {
            assert!(
                !translate_request(CM, &body)
                    .unwrap_err()
                    .to_string()
                    .is_empty()
            );
        }
        for (chat, messages) in [
            (json!("auto"), json!({"type":"auto"})),
            (json!("none"), json!({"type":"none"})),
            (json!("required"), json!({"type":"any"})),
        ] {
            let m = translate_request(CM, &json!({"messages":[],"tool_choice":chat})).unwrap();
            assert_eq!(m["tool_choice"], messages);
            assert_eq!(translate_request(MC, &m).unwrap()["tool_choice"], chat);
        }
        for (effort, budget) in [("low", 1024), ("medium", 4096), ("high", 16384)] {
            let v =
                translate_request(CM, &json!({"messages":[],"reasoning_effort":effort})).unwrap();
            assert_eq!(v["thinking"]["budget_tokens"], budget);
            assert!(v["max_tokens"].as_u64().unwrap() > budget);
        }
    }

    #[test]
    fn translate_responses_content_tools_usage_and_finish() {
        let msg = json!({"id":"id1","model":"m","type":"message","role":"assistant","content":[
            {"type":"thinking","thinking":"hidden"},{"type":"text","text":"hi"},
            {"type":"tool_use","id":"t1","name":"f","input":{"x":1}}],"stop_reason":"tool_use",
            "usage":{"input_tokens":10,"cache_read_input_tokens":20,"cache_creation_input_tokens":3,"output_tokens":5}});
        let chat = translate_response(CM, &msg);
        assert_eq!(chat["id"], "id1");
        assert_eq!(chat["model"], "m");
        assert_eq!(chat["choices"][0]["message"]["content"], "hi");
        assert_eq!(
            chat["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{\"x\":1}"
        );
        assert_eq!(chat["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            chat["usage"],
            json!({"prompt_tokens":33,"completion_tokens":5,"total_tokens":38,"prompt_tokens_details":{"cached_tokens":20}})
        );
        let back = translate_response(MC, &chat);
        assert_eq!(
            back["content"],
            json!([{"type":"text","text":"hi"},{"type":"tool_use","id":"t1","name":"f","input":{"x":1}}])
        );
        assert_eq!(
            back["usage"],
            json!({"input_tokens":13,"cache_read_input_tokens":20,"output_tokens":5})
        );
        for (stop, finish) in [
            ("end_turn", "stop"),
            ("stop_sequence", "stop"),
            ("max_tokens", "length"),
            ("tool_use", "tool_calls"),
        ] {
            assert_eq!(
                translate_response(CM, &json!({"stop_reason":stop}))["choices"][0]["finish_reason"],
                finish
            );
        }
        for (finish, stop) in [
            ("stop", "end_turn"),
            ("length", "max_tokens"),
            ("tool_calls", "tool_use"),
        ] {
            assert_eq!(
                translate_response(
                    MC,
                    &json!({"choices":[{"finish_reason":finish,"message":{"content":""}}]})
                )["stop_reason"],
                stop
            );
        }
    }

    #[test]
    fn translate_errors_both_formats_and_non_json() {
        let chat: Value = serde_json::from_slice(&translate_error(
            CM,
            429,
            br#"{"type":"error","error":{"type":"rate_limit_error","message":"slow"}}"#,
        ))
        .unwrap();
        assert_eq!(chat["error"]["message"], "slow");
        assert!(chat.get("type").is_none());
        let msg: Value = serde_json::from_slice(&translate_error(
            MC,
            401,
            br#"{"error":{"message":"bad key","type":"invalid_api_key"}}"#,
        ))
        .unwrap();
        assert_eq!(msg["type"], "error");
        assert_eq!(msg["error"]["type"], "authentication_error");
        let fallback: Value =
            serde_json::from_slice(&translate_error(MC, 502, b"not json")).unwrap();
        assert_eq!(fallback["error"]["type"], "api_error");
    }

    fn wire(events: &[Value], crlf: bool) -> Vec<u8> {
        let sep = if crlf { "\r\n" } else { "\n" };
        events
            .iter()
            .map(|v| format!("data: {v}{sep}{sep}"))
            .collect::<String>()
            .into_bytes()
    }

    fn parsed(bytes: &[u8]) -> Vec<Value> {
        String::from_utf8(bytes.to_vec())
            .unwrap()
            .lines()
            .filter_map(|s| s.strip_prefix("data: "))
            .filter(|s| *s != "[DONE]")
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    }

    fn run_stream(dir: Direction, bytes: &[u8], split: usize, usage: bool) -> Vec<u8> {
        let mut t = StreamTranslator::new(dir, "fallback").with_usage(usage);
        let mut out = Vec::new();
        for part in bytes.chunks(split) {
            out.extend(t.feed(part));
        }
        out.extend(t.finish());
        assert!(t.finish().is_empty());
        out
    }

    #[test]
    fn translate_anthropic_stream_byte_splits_tools_usage_and_thinking() {
        let events = vec![
            json!({"type":"message_start","message":{"id":"id1","model":"m","usage":{"input_tokens":10,"cache_read_input_tokens":20,"cache_creation_input_tokens":3,"output_tokens":0}}}),
            json!({"type":"ping"}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"secret"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"h\u{e9}"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t1","name":"f","input":{}}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"x\":"}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"1}"}}),
            json!({"type":"content_block_stop","index":2}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}),
            json!({"type":"message_stop"}),
        ];
        let bytes = wire(&events, true);
        let out = run_stream(CM, &bytes, 1, true);
        assert_eq!(out, run_stream(CM, &bytes, bytes.len(), true));
        assert!(out.ends_with(b"data: [DONE]\n\n"));
        let chunks = parsed(&out);
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "h\u{e9}");
        assert_eq!(
            chunks[2]["choices"][0]["delta"]["tool_calls"][0],
            json!({"index":0,"id":"t1","type":"function","function":{"name":"f","arguments":""}})
        );
        assert_eq!(
            chunks[3]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{\"x\":"
        );
        assert_eq!(chunks[5]["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(chunks[6]["choices"], json!([]));
        assert_eq!(chunks[6]["usage"]["prompt_tokens"], 33);
        assert_eq!(chunks[6]["usage"]["completion_tokens"], 7);
        assert!(!String::from_utf8(out).unwrap().contains("secret"));
        assert!(
            parsed(&run_stream(CM, &bytes, 3, false))
                .iter()
                .all(|v| v.get("usage").is_none())
        );
    }

    #[test]
    fn translate_openai_stream_byte_splits_interleaved_tools_and_final_usage() {
        let mut bytes = wire(
            &[
                json!({"id":"id1","model":"m","choices":[{"delta":{"role":"assistant"},"finish_reason":null}]}),
                json!({"choices":[{"delta":{"content":"h\u{e9}"}}]}),
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"t1","function":{"name":"f","arguments":"{\"x\":"}},{"index":1,"id":"t2","function":{"name":"g","arguments":"{"}}]}}]}),
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}},{"index":1,"function":{"arguments":"}"}}]},"finish_reason":"tool_calls"}]}),
                json!({"choices":[],"usage":{"prompt_tokens":33,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":20}}}),
            ],
            true,
        );
        bytes.extend_from_slice(b"data: [DONE]\r\n\r\n");
        let out = run_stream(MC, &bytes, 1, false);
        assert_eq!(out, run_stream(MC, &bytes, bytes.len(), false));
        let events = parsed(&out);
        assert_eq!(events[0]["type"], "message_start");
        assert_eq!(events[0]["message"]["id"], "id1");
        assert_eq!(events[0]["message"]["usage"]["input_tokens"], 0);
        assert!(events.iter().any(|v| v["delta"]["text"] == "h\u{e9}"));
        let starts: Vec<_> = events
            .iter()
            .filter(|v| v["type"] == "content_block_start")
            .collect();
        assert_eq!(starts.len(), 3);
        assert_eq!(starts[1]["content_block"]["id"], "t1");
        assert_eq!(starts[2]["content_block"]["name"], "g");
        let final_delta = &events[events.len() - 2];
        assert_eq!(final_delta["delta"]["stop_reason"], "tool_use");
        assert_eq!(
            final_delta["usage"],
            json!({"input_tokens":13,"cache_read_input_tokens":20,"output_tokens":7})
        );
        assert_eq!(events.last().unwrap()["type"], "message_stop");
        let mut open = None;
        for v in &events {
            match v["type"].as_str().unwrap() {
                "content_block_start" => {
                    assert!(open.is_none());
                    open = Some(v["index"].clone());
                }
                "content_block_delta" => assert_eq!(open, Some(v["index"].clone())),
                "content_block_stop" => {
                    assert_eq!(open.take(), Some(v["index"].clone()));
                }
                _ => {}
            }
        }
    }

    #[test]
    fn translate_stream_trailing_event_errors_and_usage_at_start() {
        let bytes = b"data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}";
        let out = run_stream(CM, bytes, 1, false);
        assert_eq!(parsed(&out)[0]["error"]["message"], "busy");
        assert_eq!(parsed(&out)[0]["error"]["type"], "overloaded_error");
        assert!(!out.windows(6).any(|w| w == b"[DONE]"));
        let b = wire(
            &[json!({"id":"a","usage":{"prompt_tokens":10,"completion_tokens":2},"choices":[]})],
            false,
        );
        let out = parsed(&run_stream(MC, &b, 1, false));
        assert_eq!(out[0]["message"]["usage"]["input_tokens"], 10);
        assert_eq!(out[out.len() - 2]["usage"]["output_tokens"], 2);
    }

    #[test]
    fn translate_stream_empty_tool_input_is_valid_json() {
        let bytes = wire(
            &[
                json!({"type":"message_start","message":{"id":"m","model":"model"}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"f","input":{}}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_stop"}),
            ],
            false,
        );
        let chunks = parsed(&run_stream(CM, &bytes, 1, false));
        let arguments: String = chunks
            .iter()
            .filter_map(|c| {
                c["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str()
            })
            .collect();
        assert_eq!(arguments, "{}");
    }
}
