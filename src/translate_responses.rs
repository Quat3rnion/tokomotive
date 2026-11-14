//! Chat -> Responses requests and Responses -> Chat replies. Messages chains
//! through the existing Chat adapter. Codex always streams; the Proxy collects
//! its final Response when the client requests JSON.
use crate::translate::{self, Direction, StreamTranslator, TranslateError};
use serde_json::{Value, json};

fn invalid(message: &str) -> TranslateError {
    TranslateError {
        message: message.into(),
    }
}
fn array(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}

pub fn request(body: &Value) -> Result<Value, TranslateError> {
    translate::validate_chat_limits(body)?;
    let messages = body["messages"]
        .as_array()
        .ok_or_else(|| invalid("messages must be an array"))?;
    if body["n"].as_u64().is_some_and(|n| n > 1) {
        return Err(invalid("Responses translation supports only n = 1"));
    }
    let mut out = json!({});
    for key in [
        "model",
        "temperature",
        "top_p",
        "stream",
        "parallel_tool_calls",
    ] {
        if let Some(v) = body.get(key) {
            out[key] = v.clone();
        }
    }
    if let Some(v) = translate::output_token_limit(body) {
        out["max_output_tokens"] = v.clone();
    }
    let mut input = Vec::new();
    let mut instructions = Vec::new();
    for message in messages {
        let role = message["role"]
            .as_str()
            .ok_or_else(|| invalid("messages need a role"))?;
        if matches!(role, "system" | "developer") {
            let text = if let Some(s) = message["content"].as_str() {
                s.to_owned()
            } else {
                array(&message["content"])
                    .iter()
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            instructions.push(text);
            continue;
        }
        if role == "tool" {
            input.push(json!({"type":"function_call_output","call_id":message["tool_call_id"],"output":message["content"]}));
            continue;
        }
        if !matches!(role, "user" | "assistant") {
            return Err(invalid("unsupported message role"));
        }
        let text_type = if role == "assistant" {
            "output_text"
        } else {
            "input_text"
        };
        let mut content = Vec::new();
        if let Some(s) = message["content"].as_str() {
            if !s.is_empty() {
                content.push(json!({"type":text_type,"text":s}));
            }
        } else {
            for part in array(&message["content"]) {
                match part["type"].as_str() {
                    Some("text") => content.push(json!({"type":text_type,"text":part["text"]})),
                    Some("image_url") if role == "user" => {
                        let mut image =
                            json!({"type":"input_image","image_url":part["image_url"]["url"]});
                        if let Some(v) = part["image_url"].get("detail") {
                            image["detail"] = v.clone();
                        }
                        content.push(image);
                    }
                    _ => return Err(invalid("unsupported message content for Responses")),
                }
            }
        }
        if !content.is_empty() {
            input.push(json!({"role":role,"content":content}));
        }
        for call in array(&message["tool_calls"]) {
            input.push(json!({"type":"function_call","call_id":call["id"],"name":call["function"]["name"],"arguments":call["function"]["arguments"]}));
        }
    }
    out["instructions"] = json!(instructions.join("\n"));
    out["input"] = json!(input);
    if body.get("tools").is_some() {
        let mut tools = Vec::new();
        for tool in array(&body["tools"]) {
            if tool["type"] != "function" {
                return Err(invalid(
                    "only function tools can be translated to Responses",
                ));
            }
            let mut function = Value::Object(
                tool["function"]
                    .as_object()
                    .ok_or_else(|| invalid("function tools must contain an object"))?
                    .clone(),
            );
            function["type"] = json!("function");
            tools.push(function);
        }
        out["tools"] = json!(tools);
    }
    if let Some(choice) = body.get("tool_choice") {
        out["tool_choice"] = if choice.is_string() {
            choice.clone()
        } else {
            json!({"type":"function","name":choice["function"]["name"]})
        };
    }
    Ok(out)
}

/// Required by the Codex backend, including native Responses requests.
pub fn prepare_codex(body: &mut Value) -> Result<(), TranslateError> {
    if !body.is_object() {
        return Err(invalid("Responses requests must be JSON objects"));
    }
    body["store"] = json!(false);
    body["stream"] = json!(true);
    if body.get("instructions").is_none() {
        body["instructions"] = json!("");
    }
    let mut include = array(&body["include"]).to_vec();
    if !include.iter().any(|v| v == "reasoning.encrypted_content") {
        include.push(json!("reasoning.encrypted_content"));
    }
    body["include"] = json!(include);
    Ok(())
}

fn usage(v: &Value) -> Value {
    let input = v["input_tokens"].as_u64().unwrap_or(0);
    let output = v["output_tokens"].as_u64().unwrap_or(0);
    json!({"prompt_tokens":input,"completion_tokens":output,"total_tokens":input+output,
        "prompt_tokens_details":{"cached_tokens":v["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0)}})
}

pub fn response(body: &Value) -> Value {
    let mut content = String::new();
    let mut calls = Vec::new();
    for item in array(&body["output"]) {
        if item["type"] == "function_call" {
            calls.push(json!({"id":item["call_id"],"type":"function","function":{"name":item["name"],"arguments":item["arguments"]}}));
        }
        for part in array(&item["content"]) {
            if part["type"] == "output_text" {
                content.push_str(part["text"].as_str().unwrap_or(""));
            }
        }
    }
    let reason = if body["status"] == "incomplete" {
        "length"
    } else if calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };
    let mut message = json!({"role":"assistant","content":content});
    if !calls.is_empty() {
        message["tool_calls"] = json!(calls);
    }
    json!({"id":body["id"],"object":"chat.completion","created":body["created_at"].as_u64().unwrap_or(0),"model":body["model"],
        "choices":[{"index":0,"message":message,"finish_reason":reason}],"usage":usage(&body["usage"])})
}

/// Streaming Responses decoder, optionally chained into Chat -> Messages SSE.
pub struct ResponseStream {
    model: String,
    id: String,
    buf: Vec<u8>,
    tools: std::collections::BTreeMap<u64, usize>,
    tool_bytes: usize,
    include_usage: bool,
    started: bool,
    ended: bool,
    messages: Option<StreamTranslator>,
    pub final_response: Option<Value>,
    pub failed: bool,
}
impl ResponseStream {
    pub fn new(model: &str, messages: bool) -> Self {
        Self {
            model: model.into(),
            id: "translated".into(),
            buf: Vec::new(),
            tools: Default::default(),
            tool_bytes: 0,
            include_usage: false,
            started: false,
            ended: false,
            messages: messages.then(|| StreamTranslator::new(Direction::MessagesToChat, model)),
            final_response: None,
            failed: false,
        }
    }
    pub fn with_usage(mut self, include_usage: bool) -> Self {
        self.include_usage = include_usage;
        self
    }
    fn chunk(&self, delta: Value, reason: Value) -> Value {
        json!({"id":self.id,"object":"chat.completion.chunk","created":0,"model":self.model,
            "choices":[{"index":0,"delta":delta,"finish_reason":reason}]})
    }
    fn emit(&mut self, value: Value, out: &mut Vec<u8>) {
        let bytes = format!("data: {value}\n\n");
        if let Some(messages) = self.messages.as_mut() {
            out.extend(messages.feed(bytes.as_bytes()));
            if messages.failed {
                self.failed = true;
                self.ended = true;
            }
        } else {
            out.extend(bytes.as_bytes());
        }
    }
    fn done(&mut self, out: &mut Vec<u8>) {
        if let Some(messages) = self.messages.as_mut() {
            out.extend(messages.feed(b"data: [DONE]\n\n"));
            out.extend(messages.finish());
        } else {
            out.extend(b"data: [DONE]\n\n");
        }
        self.ended = true;
    }
    fn error(&mut self, out: &mut Vec<u8>) {
        self.failed = true;
        self.emit(json!({"error":{"type":"api_error","message":"Responses upstream failed or ended before completion"}}), out);
        self.done(out);
    }
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.ended {
            return Vec::new();
        }
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((end, sep)) = self
            .buf
            .windows(2)
            .position(|b| b == b"\n\n")
            .map(|i| (i, 2))
            .into_iter()
            .chain(
                self.buf
                    .windows(4)
                    .position(|b| b == b"\r\n\r\n")
                    .map(|i| (i, 4)),
            )
            .min_by_key(|v| v.0)
        {
            if end > 4 * 1024 * 1024 {
                self.buf.clear();
                self.error(&mut out);
                break;
            }
            let raw: Vec<u8> = self.buf.drain(..end + sep).collect();
            self.event(&raw[..end], &mut out);
            if self.ended {
                self.buf.clear();
                break;
            }
        }
        if self.buf.len() > 4 * 1024 * 1024 {
            self.buf.clear();
            self.error(&mut out);
        }
        out
    }
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        let raw = std::mem::take(&mut self.buf);
        if !self.ended && !raw.is_empty() {
            self.event(&raw, &mut out);
        }
        if !self.ended {
            self.error(&mut out);
        }
        out
    }
    fn event(&mut self, raw: &[u8], out: &mut Vec<u8>) {
        let Ok(text) = std::str::from_utf8(raw) else {
            return;
        };
        let data = text
            .lines()
            .filter_map(|l| l.strip_prefix("data:").map(str::trim_start))
            .collect::<Vec<_>>()
            .join("\n");
        let Ok(mut v) = serde_json::from_str::<Value>(&data) else {
            return;
        };
        if !v.is_object() {
            return;
        }
        if v.get("type").is_none()
            && let Some(name) = text.lines().find_map(|l| l.strip_prefix("event:"))
        {
            v["type"] = json!(name.trim());
        }
        if let Some(r) = v.get("response") {
            if let Some(id) = r["id"].as_str() {
                self.id = id.into();
            }
            if let Some(model) = r["model"].as_str() {
                self.model = model.into();
            }
        }
        if !self.started {
            self.started = true;
            self.emit(self.chunk(json!({"role":"assistant"}), Value::Null), out);
        }
        let kind = v["type"].as_str().unwrap_or("");
        if matches!(
            kind,
            "response.output_item.added" | "response.function_call_arguments.delta"
        ) {
            if raw.len() > (16 * 1024 * 1024usize).saturating_sub(self.tool_bytes) {
                self.error(out);
                return;
            }
            self.tool_bytes += raw.len();
        }
        match kind {
            "response.output_text.delta" => {
                self.emit(self.chunk(json!({"content":v["delta"]}), Value::Null), out)
            }
            "response.output_item.added" if v["item"]["type"] == "function_call" => {
                if self.tools.len() >= 128
                    && !self
                        .tools
                        .contains_key(&v["output_index"].as_u64().unwrap_or(0))
                {
                    self.error(out);
                    return;
                }
                let index = self.tools.len();
                self.tools
                    .insert(v["output_index"].as_u64().unwrap_or(0), index);
                self.emit(self.chunk(json!({"tool_calls":[{"index":index,"id":v["item"]["call_id"],"type":"function", "function":{"name":v["item"]["name"],"arguments":v["item"]["arguments"].as_str().unwrap_or("")}}]}),Value::Null),out);
            }
            "response.function_call_arguments.delta" => {
                if let Some(index) = self.tools.get(&v["output_index"].as_u64().unwrap_or(0)) {
                    self.emit(self.chunk(json!({"tool_calls":[{"index":index,"function":{"arguments":v["delta"]}}]}),Value::Null),out);
                }
            }
            "response.completed" | "response.incomplete" => {
                let reason = if v["type"] == "response.incomplete" {
                    "length"
                } else if self.tools.is_empty() {
                    "stop"
                } else {
                    "tool_calls"
                };
                self.emit(self.chunk(json!({}), json!(reason)), out);
                // Messages needs usage even when Chat did not request it.
                if self.include_usage || self.messages.is_some() {
                    self.emit(json!({"id":self.id,"object":"chat.completion.chunk","created":0,"model":self.model,"choices":[],"usage":usage(&v["response"]["usage"])}),out);
                }
                self.final_response = Some(v["response"].clone());
                self.done(out);
            }
            "response.failed" | "error" => self.error(out),
            _ if v.get("error").is_some() => self.error(out),
            _ => {}
        }
    }
}

/// A single Proxy stream interface for the old adapters and Responses.
pub enum ProxyStream {
    Chat(StreamTranslator),
    Responses(ResponseStream),
    Gemini(crate::translate_gemini::GeminiStream),
}
impl ProxyStream {
    pub fn failed(&self) -> bool {
        match self {
            Self::Chat(s) => s.failed,
            Self::Responses(s) => s.failed,
            Self::Gemini(s) => s.failed,
        }
    }
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        match self {
            Self::Chat(s) => s.feed(chunk),
            Self::Responses(s) => s.feed(chunk),
            Self::Gemini(s) => s.feed(chunk),
        }
    }
    pub fn finish(&mut self) -> Vec<u8> {
        match self {
            Self::Chat(s) => s.finish(),
            Self::Responses(s) => s.finish(),
            Self::Gemini(s) => s.finish(),
        }
    }
}
