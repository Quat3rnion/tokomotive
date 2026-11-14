//! Cohere v2 Chat: https://docs.cohere.com/v2/reference/chat
//! Streaming: https://docs.cohere.com/v2/reference/chat-stream
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
                "Cohere supports Chat Completions and Messages only",
            ));
        }
    };
    translate::validate_chat_limits(body)?;
    if body["n"].as_u64().is_some_and(|n| n != 1) {
        return Err(invalid("Cohere supports only n = 1"));
    }
    let mut messages = body["messages"]
        .as_array()
        .ok_or_else(|| invalid("messages must be an array"))?
        .clone();
    for m in &mut messages {
        if m["role"] == "developer" {
            m["role"] = json!("system");
        }
        if !matches!(
            m["role"].as_str(),
            Some("system" | "user" | "assistant" | "tool")
        ) {
            return Err(invalid("unsupported Cohere role"));
        }
        if m["content"].is_null() {
            m.as_object_mut()
                .expect("message role object")
                .remove("content");
        }
        if let Some(c) = m["content"].as_array() {
            for p in c {
                if p["type"] != "text" && !(m["role"] == "user" && p["type"] == "image_url") {
                    return Err(invalid("unsupported Cohere content"));
                }
            }
        }
    }
    let mut out = json!({"model":body["model"],"messages":messages,"stream":body["stream"].as_bool().unwrap_or(false)});
    for k in [
        "temperature",
        "seed",
        "frequency_penalty",
        "presence_penalty",
        "tools",
    ] {
        if let Some(v) = body.get(k) {
            out[k] = v.clone();
        }
    }
    if let Some(limit) = translate::output_token_limit(body) {
        out["max_tokens"] = limit.clone();
    }
    for (from, to) in [("top_p", "p"), ("top_k", "k"), ("stop", "stop_sequences")] {
        if let Some(v) = body.get(from) {
            out[to] = if from == "stop" && v.is_string() {
                json!([v])
            } else {
                v.clone()
            };
        }
    }
    if let Some(choice) = body.get("tool_choice") {
        match choice.as_str() {
            Some("auto") => {}
            Some("required") => out["tool_choice"] = json!("REQUIRED"),
            Some("none") => out["tool_choice"] = json!("NONE"),
            _ => return Err(invalid("Cohere cannot select a named tool")),
        }
    }
    if let Some(format) = body.get("response_format") {
        out["response_format"] = match format["type"].as_str() {
            Some("text" | "json_object") => format.clone(),
            Some("json_schema") => {
                json!({"type":"json_object","json_schema":format["json_schema"]["schema"]})
            }
            _ => return Err(invalid("unsupported Cohere response format")),
        };
    }
    Ok(out)
}
fn count(v: &Value) -> Option<i64> {
    v.as_i64().filter(|n| *n >= 0).or_else(|| {
        v.as_f64()
            .filter(|n| n.is_finite() && *n >= 0.0 && *n < i64::MAX as f64 && n.fract() == 0.0)
            .map(|n| n as i64)
    })
}
/// Billed units drive cost, not raw model tokens. Fall back to tokens only
/// when a complete billed input/output report is absent; never add reports.
pub fn usage(v: &Value) -> Option<crate::usage::Usage> {
    ["billed_units", "tokens"].into_iter().find_map(|key| {
        Some(crate::usage::Usage {
            input: count(&v[key]["input_tokens"])?,
            output: count(&v[key]["output_tokens"])?,
            ..Default::default()
        })
    })
}
pub fn chat_usage(v: &Value) -> Value {
    let u = usage(v).unwrap_or_default();
    json!({"prompt_tokens":u.input,"completion_tokens":u.output,"total_tokens":u.input.saturating_add(u.output)})
}
pub fn stop(reason: &str) -> Option<&'static str> {
    match reason {
        "COMPLETE" | "STOP_SEQUENCE" => Some("stop"),
        "MAX_TOKENS" => Some("length"),
        "TOOL_CALL" => Some("tool_calls"),
        _ => None,
    }
}
pub fn response(endpoint: Endpoint, body: &Value, model: &str) -> Value {
    let text = array(&body["message"]["content"])
        .iter()
        .filter(|p| p["type"] == "text")
        .filter_map(|p| p["text"].as_str())
        .collect::<Vec<_>>()
        .join("");
    let mut message = json!({"role":"assistant","content":text});
    if let Some(calls) = body["message"].get("tool_calls") {
        message["tool_calls"] = calls.clone();
    }
    let chat = json!({"id":body["id"].as_str().unwrap_or("cohere"),"object":"chat.completion","created":0,"model":model,"choices":[{"index":0,"message":message,"finish_reason":stop(body["finish_reason"].as_str().unwrap_or(""))}],"usage":chat_usage(&body["usage"])});
    if endpoint == Endpoint::Messages {
        translate::translate_response(Direction::MessagesToChat, &chat)
    } else {
        chat
    }
}

pub struct CohereStream {
    buf: Vec<u8>,
    scanned: usize,
    model: String,
    id: String,
    started: bool,
    ended: bool,
    include_usage: bool,
    tools: std::collections::BTreeMap<u64, usize>,
    messages: Option<translate::StreamTranslator>,
    pub failed: bool,
}
impl CohereStream {
    pub fn new(endpoint: Endpoint, model: &str, include_usage: bool) -> Self {
        Self {
            buf: Vec::new(),
            scanned: 0,
            model: model.into(),
            id: "cohere".into(),
            started: false,
            ended: false,
            include_usage,
            tools: Default::default(),
            messages: (endpoint == Endpoint::Messages)
                .then(|| translate::StreamTranslator::new(Direction::MessagesToChat, model)),
            failed: false,
        }
    }
    fn chunk(&self, delta: Value, stop: Option<&str>) -> Value {
        json!({"id":self.id,"object":"chat.completion.chunk","created":0,"model":self.model,"choices":[{"index":0,"delta":delta,"finish_reason":stop}]})
    }
    fn forward(&mut self, b: &[u8], out: &mut Vec<u8>) {
        if let Some(m) = self.messages.as_mut() {
            out.extend(m.feed(b));
            self.failed |= m.failed;
        } else {
            out.extend(b);
        }
    }
    fn emit(&mut self, v: Value, out: &mut Vec<u8>) {
        self.forward(format!("data: {v}\n\n").as_bytes(), out);
    }
    fn error(&mut self, out: &mut Vec<u8>) {
        if !self.ended {
            self.failed = true;
            self.ended = true;
            self.buf.clear();
            self.emit(json!({"error":{"type":"api_error","message":"invalid or incomplete Cohere stream"}}),out);
        }
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for c in bytes.chunks(8192) {
            if self.ended {
                break;
            }
            self.buf.extend(c);
            loop {
                let lf = self.buf[self.scanned..]
                    .windows(2)
                    .position(|b| b == b"\n\n")
                    .map(|i| (i + self.scanned, 2));
                let crlf = self.buf[self.scanned..]
                    .windows(4)
                    .position(|b| b == b"\r\n\r\n")
                    .map(|i| (i + self.scanned, 4));
                let Some((end, sep)) = (match (lf, crlf) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                }) else {
                    self.scanned = self.buf.len().saturating_sub(3);
                    break;
                };
                if end > 4 * 1024 * 1024 {
                    self.error(&mut out);
                    break;
                }
                let raw: Vec<_> = self.buf.drain(..end + sep).collect();
                self.scanned = 0;
                self.event(&raw[..end], &mut out);
                if self.ended {
                    break;
                }
            }
            if self.buf.len() > 4 * 1024 * 1024 {
                self.error(&mut out);
            }
        }
        out
    }
    fn event(&mut self, raw: &[u8], out: &mut Vec<u8>) {
        let Ok(text) = std::str::from_utf8(raw) else {
            self.error(out);
            return;
        };
        let data = text
            .lines()
            .filter_map(|l| l.strip_prefix("data:").map(str::trim_start))
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(&data) else {
            self.error(out);
            return;
        };
        let d = &v["delta"];
        let message = &d["message"];
        match v["type"].as_str().unwrap_or("") {
            "message-start" => {
                if self.started {
                    self.error(out);
                    return;
                }
                self.started = true;
                if let Some(id) = v["id"].as_str() {
                    self.id = id.into();
                }
                self.emit(
                    self.chunk(json!({"role":"assistant","content":""}), None),
                    out,
                );
            }
            "content-start" | "content-delta" => {
                if let Some(t) = message["content"]["text"].as_str() {
                    self.emit(self.chunk(json!({"content":t}), None), out);
                }
            }
            "tool-call-start" => {
                let Some(i) = v["index"].as_u64() else {
                    self.error(out);
                    return;
                };
                let t = &message["tool_calls"];
                if self.tools.len() >= 128
                    || self.tools.contains_key(&i)
                    || !t["id"].is_string()
                    || !t["function"]["name"].is_string()
                {
                    self.error(out);
                    return;
                }
                let n = self.tools.len();
                self.tools.insert(i, n);
                let mut t = t.clone();
                t["index"] = json!(n);
                if t["function"]["arguments"].is_null() {
                    t["function"]["arguments"] = json!("");
                }
                self.emit(self.chunk(json!({"tool_calls":[t]}), None), out);
            }
            "tool-call-delta" => {
                let Some(i) = v["index"]
                    .as_u64()
                    .and_then(|i| self.tools.get(&i))
                    .copied()
                else {
                    self.error(out);
                    return;
                };
                if let Some(args) = message["tool_calls"]["function"]["arguments"].as_str() {
                    self.emit(
                        self.chunk(
                            json!({"tool_calls":[{"index":i,"function":{"arguments":args}}]}),
                            None,
                        ),
                        out,
                    );
                }
            }
            "message-end" => {
                let Some(stop) = d["finish_reason"].as_str().and_then(stop) else {
                    self.error(out);
                    return;
                };
                if !self.started || d["error"].is_string() {
                    self.error(out);
                    return;
                }
                self.emit(self.chunk(json!({}), Some(stop)), out);
                if (self.include_usage || self.messages.is_some()) && usage(&d["usage"]).is_some() {
                    self.emit(json!({"id":self.id,"object":"chat.completion.chunk","created":0,"model":self.model,"choices":[],"usage":chat_usage(&d["usage"])}),out);
                }
                self.forward(b"data: [DONE]\n\n", out);
                self.ended = true;
            }
            "content-end" | "tool-call-end" | "tool-plan-delta" | "citation-start"
            | "citation-end" | "debug" => {}
            _ => self.error(out),
        }
    }
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.ended {
            self.error(&mut out);
        }
        out
    }
}
