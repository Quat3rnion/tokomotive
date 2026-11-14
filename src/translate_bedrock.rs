//! Converse mapping: https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_Converse.html
use crate::{
    provider::Endpoint,
    translate::{self, Direction, TranslateError},
};
use serde_json::{Value, json};
fn invalid(message: &str) -> TranslateError {
    TranslateError {
        message: message.into(),
    }
}
fn array(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}
pub fn path(model: &str, stream: bool) -> Result<String, TranslateError> {
    if model.is_empty()
        || model.len() > 2048
        || model.chars().any(char::is_control)
        || matches!(model, "." | "..")
    {
        return Err(invalid("invalid Bedrock model ID"));
    }
    Ok(format!(
        "/model/{}/{}",
        crate::sigv4::encode(model, false),
        if stream {
            "converse-stream"
        } else {
            "converse"
        }
    ))
}
pub fn request(endpoint: Endpoint, body: &Value) -> Result<Value, TranslateError> {
    let chat;
    let body = match endpoint {
        Endpoint::ChatCompletions => body,
        Endpoint::Messages => {
            chat = translate::translate_request(Direction::MessagesToChat, body)?;
            &chat
        }
        _ => {
            return Err(invalid(
                "Bedrock supports Chat Completions and Messages only",
            ));
        }
    };
    translate::validate_chat_limits(body)?;
    if body["n"].as_u64().is_some_and(|n| n != 1) {
        return Err(invalid("Bedrock supports only n = 1"));
    }
    let mut messages: Vec<Value> = Vec::new();
    let mut system = Vec::new();
    for m in body["messages"]
        .as_array()
        .ok_or_else(|| invalid("messages must be an array"))?
    {
        let role = m["role"].as_str().unwrap_or("");
        let mut content = Vec::new();
        if role == "tool" {
            let id = m["tool_call_id"]
                .as_str()
                .ok_or_else(|| invalid("tool result needs an id"))?;
            content.push(json!({"toolResult":{"toolUseId":id,"content":[{"text":m["content"].as_str().unwrap_or("")}]}}));
        } else {
            if !matches!(role, "user" | "assistant" | "system" | "developer") {
                return Err(invalid("unsupported Bedrock role"));
            }
            if let Some(text) = m["content"].as_str().filter(|s| !s.is_empty()) {
                content.push(json!({"text":text}));
            }
            for part in array(&m["content"]) {
                match part["type"].as_str() {
                    Some("text") => content.push(json!({"text":part["text"]})),
                    Some("image_url") if role == "user" => {
                        let url = part["image_url"]["url"].as_str().unwrap_or("");
                        let (mime, data) = url
                            .strip_prefix("data:")
                            .and_then(|s| s.split_once(";base64,"))
                            .ok_or_else(|| invalid("Bedrock images require base64 data URLs"))?;
                        let format = mime
                            .strip_prefix("image/")
                            .filter(|s| matches!(*s, "png" | "jpeg" | "gif" | "webp"))
                            .ok_or_else(|| invalid("unsupported Bedrock image format"))?;
                        use base64::Engine;
                        base64::engine::general_purpose::STANDARD
                            .decode(data)
                            .map_err(|_| invalid("invalid image base64"))?;
                        content.push(json!({"image":{"format":format,"source":{"bytes":data}}}));
                    }
                    _ => return Err(invalid("unsupported Bedrock content")),
                }
            }
            for call in array(&m["tool_calls"]) {
                let input: Value =
                    serde_json::from_str(call["function"]["arguments"].as_str().unwrap_or("{}"))
                        .map_err(|_| invalid("invalid tool arguments"))?;
                if !input.is_object()
                    || !call["id"].is_string()
                    || !call["function"]["name"].is_string()
                {
                    return Err(invalid("invalid tool call"));
                }
                content.push(json!({"toolUse":{"toolUseId":call["id"],"name":call["function"]["name"],"input":input}}));
            }
        }
        if matches!(role, "system" | "developer") {
            system.extend(content);
            continue;
        }
        let role = if role == "assistant" {
            "assistant"
        } else {
            "user"
        };
        if let Some(last) = messages.last_mut().filter(|m| m["role"] == role) {
            last["content"]
                .as_array_mut()
                .expect("array")
                .extend(content);
        } else {
            messages.push(json!({"role":role,"content":content}));
        }
    }
    let mut out = json!({"messages":messages});
    if !system.is_empty() {
        out["system"] = json!(system);
    }
    let mut config = json!({});
    if let Some(limit) = translate::output_token_limit(body) {
        config["maxTokens"] = limit.clone();
    }
    for (from, to) in [
        ("temperature", "temperature"),
        ("top_p", "topP"),
        ("stop", "stopSequences"),
    ] {
        if let Some(v) = body.get(from) {
            config[to] = if from == "stop" && v.is_string() {
                json!([v])
            } else {
                v.clone()
            };
        }
    }
    if !config.as_object().expect("object").is_empty() {
        out["inferenceConfig"] = config;
    }
    if let Some(tools) = body.get("tools") {
        let tools=tools.as_array().ok_or_else(||invalid("tools must be an array"))?.iter().map(|t| {
            if t["type"]!="function" || !t["function"]["name"].is_string() {return Err(invalid("only named function tools are supported"));}
            let f=&t["function"]; let mut spec=json!({"name":f["name"],"inputSchema":{"json":f.get("parameters").cloned().unwrap_or(json!({"type":"object","properties":{}}))}});
            if let Some(d)=f.get("description") {spec["description"]=d.clone();}
            Ok(json!({"toolSpec":spec}))
        }).collect::<Result<Vec<_>,TranslateError>>()?;
        out["toolConfig"] = json!({"tools":tools});
    }
    if let Some(choice) = body.get("tool_choice") {
        let choice = match choice.as_str() {
            Some("auto") => json!({"auto":{}}),
            Some("required") => json!({"any":{}}),
            Some("none") => {
                out.as_object_mut().expect("object").remove("toolConfig");
                return Ok(out);
            }
            _ if choice["function"]["name"].is_string() => {
                json!({"tool":{"name":choice["function"]["name"]}})
            }
            _ => return Err(invalid("unsupported Bedrock tool choice")),
        };
        if !out["toolConfig"].is_object() {
            return Err(invalid("tool choice needs tools"));
        }
        out["toolConfig"]["toolChoice"] = choice;
    }
    Ok(out)
}
pub fn stop(reason: &str) -> &'static str {
    match reason {
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        "content_filtered" | "guardrail_intervened" => "content_filter",
        _ => "stop",
    }
}
pub fn usage(v: &Value) -> Option<crate::usage::Usage> {
    let n = |k: &str| v[k].as_i64().unwrap_or(0).max(0);
    let input = v.get("inputTokens")?.as_i64()?.max(0);
    let write = n("cacheWriteInputTokens");
    let one_hour = array(&v["cacheDetails"])
        .iter()
        .filter(|c| c["ttl"] == "1h")
        .map(|c| c["inputTokens"].as_i64().unwrap_or(0).max(0))
        .fold(0i64, i64::saturating_add)
        .min(write);
    Some(crate::usage::Usage {
        input,
        output: n("outputTokens"),
        cache_read: n("cacheReadInputTokens"),
        cache_write_5m: write - one_hour,
        cache_write_1h: one_hour,
        ..Default::default()
    })
}
pub fn chat_usage(v: &Value) -> Value {
    let u = usage(v).unwrap_or_default();
    let write = u.cache_write_5m.saturating_add(u.cache_write_1h);
    let prompt = u.input.saturating_add(u.cache_read).saturating_add(write);
    json!({"prompt_tokens":prompt,"completion_tokens":u.output,"total_tokens":prompt.saturating_add(u.output),"prompt_tokens_details":{"cached_tokens":u.cache_read,"cache_creation_tokens":write}})
}
pub fn response(endpoint: Endpoint, body: &Value, model: &str) -> Value {
    let mut text = String::new();
    let mut calls = Vec::new();
    for c in array(&body["output"]["message"]["content"]) {
        if let Some(t) = c["text"].as_str() {
            text.push_str(t);
        }
        if let Some(t) = c.get("toolUse") {
            calls.push(json!({"id":t["toolUseId"],"type":"function","function":{"name":t["name"],"arguments":t["input"].to_string()}}));
        }
    }
    let mut message = json!({"role":"assistant","content":text});
    if !calls.is_empty() {
        message["tool_calls"] = json!(calls);
    }
    let chat = json!({"id":"bedrock","object":"chat.completion","created":0,"model":model,"choices":[{"index":0,"message":message,"finish_reason":stop(body["stopReason"].as_str().unwrap_or(""))}],"usage":chat_usage(&body["usage"])});
    if endpoint == Endpoint::Messages {
        translate::translate_response(Direction::MessagesToChat, &chat)
    } else {
        chat
    }
}

/// Reads the checked event-stream frames re-encoded by StreamTracker as SSE.
pub struct BedrockStream {
    buf: Vec<u8>,
    model: String,
    stop: Option<String>,
    usage: Option<Value>,
    tools: std::collections::BTreeMap<u64, usize>,
    started: bool,
    ended: bool,
    include_usage: bool,
    messages: Option<translate::StreamTranslator>,
    pub failed: bool,
}
impl BedrockStream {
    pub fn new(endpoint: Endpoint, model: &str, include_usage: bool) -> Self {
        Self {
            buf: Vec::new(),
            model: model.into(),
            stop: None,
            usage: None,
            tools: Default::default(),
            started: false,
            ended: false,
            include_usage,
            messages: (endpoint == Endpoint::Messages)
                .then(|| translate::StreamTranslator::new(Direction::MessagesToChat, model)),
            failed: false,
        }
    }
    fn chunk(&self, delta: Value, stop: Option<&str>) -> Value {
        json!({"id":"bedrock","object":"chat.completion.chunk","created":0,"model":self.model,"choices":[{"index":0,"delta":delta,"finish_reason":stop}]})
    }
    fn forward(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        if let Some(m) = self.messages.as_mut() {
            out.extend(m.feed(bytes));
            self.failed |= m.failed;
        } else {
            out.extend_from_slice(bytes);
        }
    }
    fn emit(&mut self, value: Value, out: &mut Vec<u8>) {
        self.forward(format!("data: {value}\n\n").as_bytes(), out);
    }
    fn error(&mut self, out: &mut Vec<u8>) {
        if !self.ended {
            self.failed = true;
            self.ended = true;
            self.buf.clear();
            self.emit(json!({"error":{"type":"api_error","message":"invalid or incomplete Bedrock stream"}}),out);
        }
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for bytes in bytes.chunks(8192) {
            if self.ended {
                break;
            }
            self.buf.extend_from_slice(bytes);
            while let Some(end) = self.buf.windows(2).position(|b| b == b"\n\n") {
                if end > crate::eventstream::MAX_FRAME * 2 {
                    self.error(&mut out);
                    break;
                }
                let raw: Vec<_> = self.buf.drain(..end + 2).collect();
                let value = std::str::from_utf8(&raw)
                    .ok()
                    .and_then(|s| s.strip_prefix("data: "))
                    .and_then(|s| serde_json::from_str::<Value>(s.trim()).ok());
                match value {
                    Some(v) => self.event(&v, &mut out),
                    None => self.error(&mut out),
                }
                if self.ended {
                    break;
                }
            }
            if self.buf.len() > crate::eventstream::MAX_FRAME * 2 {
                self.error(&mut out);
            }
        }
        out
    }
    fn event(&mut self, v: &Value, out: &mut Vec<u8>) {
        if v.get("error").is_some() {
            self.error(out);
            return;
        }
        let b = &v["body"];
        match v["event"].as_str().unwrap_or("") {
            "messageStart" => {
                if self.started {
                    self.error(out);
                    return;
                }
                self.started = true;
                self.emit(
                    self.chunk(json!({"role":"assistant","content":""}), None),
                    out,
                );
            }
            "contentBlockStart" => {
                if let Some(t) = b["start"].get("toolUse") {
                    let Some(index) = b["contentBlockIndex"].as_u64() else {
                        self.error(out);
                        return;
                    };
                    if self.tools.len() >= 128 || self.tools.contains_key(&index) {
                        self.error(out);
                        return;
                    }
                    let n = self.tools.len();
                    self.tools.insert(index, n);
                    self.emit(self.chunk(json!({"tool_calls":[{"index":n,"id":t["toolUseId"],"type":"function","function":{"name":t["name"],"arguments":""}}]}),None),out);
                }
            }
            "contentBlockDelta" => {
                if let Some(t) = b["delta"]["text"].as_str() {
                    self.emit(self.chunk(json!({"content":t}), None), out);
                }
                if let Some(t) = b["delta"]["toolUse"]["input"].as_str() {
                    let Some(n) = b["contentBlockIndex"]
                        .as_u64()
                        .and_then(|i| self.tools.get(&i))
                        .copied()
                    else {
                        self.error(out);
                        return;
                    };
                    self.emit(
                        self.chunk(
                            json!({"tool_calls":[{"index":n,"function":{"arguments":t}}]}),
                            None,
                        ),
                        out,
                    );
                }
            }
            "contentBlockStop" => {}
            "messageStop" => {
                self.stop = b["stopReason"].as_str().map(|s| stop(s).into());
            }
            "metadata" => {
                if b["usage"].is_object() {
                    self.usage = Some(chat_usage(&b["usage"]));
                }
            }
            _ => self.error(out),
        }
    }
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.ended {
            return out;
        }
        if !self.buf.is_empty() || !self.started || self.stop.is_none() || self.usage.is_none() {
            self.error(&mut out);
            return out;
        }
        let stop = self.stop.clone().expect("checked stop");
        self.emit(self.chunk(json!({}), Some(&stop)), &mut out);
        if (self.include_usage || self.messages.is_some())
            && let Some(u) = self.usage.take()
        {
            self.emit(json!({"id":"bedrock","object":"chat.completion.chunk","created":0,"model":self.model,"choices":[],"usage":u}),&mut out);
        }
        self.forward(b"data: [DONE]\n\n", &mut out);
        self.ended = true;
        out
    }
}
