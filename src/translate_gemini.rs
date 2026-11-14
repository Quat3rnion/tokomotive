//! Native Gemini translation. Wire field names and usage semantics:
//! https://ai.google.dev/api/generate-content (GenerateContentRequest/Response).
//! Messages reuses the existing Messages -> Chat -> Messages translation seam.
use crate::provider::Endpoint;
use crate::translate::{self, Direction, TranslateError};
use serde_json::{Value, json};

fn invalid(message: &str) -> TranslateError {
    TranslateError {
        message: message.into(),
    }
}
fn array(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}

/// The model is a single resource name, never a caller-controlled URL path.
/// https://ai.google.dev/api/generate-content#method:-models.generatecontent
pub fn path(model: &str, stream: bool) -> Result<String, TranslateError> {
    let model = model.strip_prefix("models/").unwrap_or(model);
    if model.is_empty()
        || model.len() > 200
        || matches!(model, "." | "..")
        || !model
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
    {
        return Err(invalid("Gemini model must be a single model ID"));
    }
    Ok(format!(
        "/v1beta/models/{model}:{}",
        if stream {
            "streamGenerateContent?alt=sse"
        } else {
            "generateContent"
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
                "Gemini supports Chat Completions and Messages only",
            ));
        }
    };
    translate::validate_chat_limits(body)?;
    if body["n"].as_u64().is_some_and(|n| n != 1) {
        return Err(invalid("Gemini translation supports only n = 1"));
    }
    let messages = body["messages"]
        .as_array()
        .ok_or_else(|| invalid("messages must be an array"))?;
    let mut contents = Vec::new();
    let mut system = Vec::new();
    let mut names = std::collections::HashMap::new();
    for message in messages {
        let role = message["role"].as_str().unwrap_or("");
        if role == "tool" {
            let id = message["tool_call_id"].as_str().unwrap_or("");
            let name = names
                .get(id)
                .ok_or_else(|| invalid("tool result has no matching tool call"))?;
            let output = message["content"].as_str().unwrap_or("");
            let result =
                serde_json::from_str::<Value>(output).unwrap_or_else(|_| json!({"result":output}));
            contents.push(json!({"role":"user","parts":[{"functionResponse":{"name":name,"response":if result.is_object() {result} else {json!({"result":result})}}}]}));
            continue;
        }
        if !matches!(role, "user" | "assistant" | "system" | "developer") {
            return Err(invalid("unsupported message role"));
        }
        let mut parts = Vec::new();
        if let Some(text) = message["content"].as_str() {
            parts.push(json!({"text":text}));
        } else {
            for part in array(&message["content"]) {
                match part["type"].as_str() {
                    Some("text") => parts.push(json!({"text":part["text"]})),
                    Some("image_url") if role == "user" => {
                        let url = part["image_url"]["url"]
                            .as_str()
                            .ok_or_else(|| invalid("image URL must be a string"))?;
                        if let Some(data) = url.strip_prefix("data:") {
                            let (mime, data) = data
                                .split_once(";base64,")
                                .ok_or_else(|| invalid("images must use base64 data URLs"))?;
                            parts.push(json!({"inlineData":{"mimeType":mime,"data":data}}));
                        } else {
                            return Err(invalid(
                                "Gemini translation requires base64 image data URLs",
                            ));
                        }
                    }
                    _ => return Err(invalid("unsupported Gemini message content")),
                }
            }
        }
        for call in array(&message["tool_calls"]) {
            let id = call["id"]
                .as_str()
                .ok_or_else(|| invalid("tool call needs an id"))?;
            let name = call["function"]["name"]
                .as_str()
                .ok_or_else(|| invalid("tool call needs a name"))?;
            names.insert(id.to_owned(), name.to_owned());
            let args = serde_json::from_str::<Value>(
                call["function"]["arguments"].as_str().unwrap_or("{}"),
            )
            .map_err(|_| invalid("tool arguments must contain valid JSON"))?;
            if !args.is_object() {
                return Err(invalid("tool arguments must be an object"));
            }
            let mut p = json!({"functionCall":{"name":name,"args":args}});
            if let Some(signature) = call.get("thought_signature") {
                p["thoughtSignature"] = signature.clone();
            }
            parts.push(p);
        }
        if matches!(role, "system" | "developer") {
            system.extend(parts);
        } else if !parts.is_empty() {
            contents
                .push(json!({"role":if role == "assistant" {"model"} else {"user"},"parts":parts}));
        }
    }
    let mut out = json!({"contents":contents});
    if !system.is_empty() {
        out["systemInstruction"] = json!({"parts":system});
    }
    let mut config = json!({});
    for (source, target) in [
        ("temperature", "temperature"),
        ("top_p", "topP"),
        ("top_k", "topK"),
    ] {
        if let Some(value) = body.get(source) {
            config[target] = value.clone();
        }
    }
    if let Some(limit) = translate::output_token_limit(body) {
        config["maxOutputTokens"] = limit.clone();
    }
    if let Some(stop) = body.get("stop") {
        config["stopSequences"] = if stop.is_string() {
            json!([stop])
        } else {
            stop.clone()
        };
    }
    if let Some(format) = body.get("response_format") {
        match format["type"].as_str() {
            Some("json_object") => config["responseMimeType"] = json!("application/json"),
            Some("json_schema") => {
                config["responseMimeType"] = json!("application/json");
                config["responseJsonSchema"] = format["json_schema"]["schema"].clone();
            }
            Some("text") => (),
            _ => return Err(invalid("unsupported response format")),
        }
    }
    if config.as_object().is_some_and(|o| !o.is_empty()) {
        out["generationConfig"] = config;
    }
    if let Some(tools) = body.get("tools") {
        let mut declarations = Vec::new();
        for tool in tools
            .as_array()
            .ok_or_else(|| invalid("tools must be an array"))?
        {
            if tool["type"] != "function" {
                return Err(invalid("only function tools are supported"));
            }
            let function = &tool["function"];
            if !function["name"].is_string() {
                return Err(invalid("tool needs a name"));
            }
            let mut declaration = json!({"name":function["name"]});
            for key in ["description", "parameters"] {
                if let Some(v) = function.get(key) {
                    declaration[key] = v.clone();
                }
            }
            declarations.push(declaration);
        }
        out["tools"] = json!([{"functionDeclarations":declarations}]);
    }
    if let Some(choice) = body.get("tool_choice") {
        let (mode, name) = match choice.as_str() {
            Some("auto") => ("AUTO", None),
            Some("none") => ("NONE", None),
            Some("required") => ("ANY", None),
            _ if choice["function"]["name"].is_string() => {
                ("ANY", choice["function"]["name"].as_str())
            }
            _ => return Err(invalid("unsupported tool choice")),
        };
        out["toolConfig"] = json!({"functionCallingConfig":{"mode":mode}});
        if let Some(name) = name {
            out["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"] = json!([name]);
        }
    }
    Ok(out)
}

fn usage(body: &Value) -> Value {
    let v = &body["usageMetadata"];
    let prompt = v["promptTokenCount"].as_u64().unwrap_or(0);
    let cached = v["cachedContentTokenCount"]
        .as_u64()
        .unwrap_or(0)
        .min(prompt);
    let thoughts = v["thoughtsTokenCount"].as_u64().unwrap_or(0);
    let output = v["candidatesTokenCount"]
        .as_u64()
        .unwrap_or(0)
        .saturating_add(thoughts);
    json!({"prompt_tokens":prompt,"completion_tokens":output,"total_tokens":prompt.saturating_add(output),
        "prompt_tokens_details":{"cached_tokens":cached},"completion_tokens_details":{"reasoning_tokens":thoughts}})
}

fn finish_reason(reason: &str, has_tools: bool) -> &'static str {
    match reason {
        "MAX_TOKENS" => "length",
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "IMAGE_SAFETY" => {
            "content_filter"
        }
        _ if has_tools => "tool_calls",
        _ => "stop",
    }
}

pub fn response(endpoint: Endpoint, body: &Value, model: &str) -> Value {
    let candidate = &body["candidates"][0];
    let mut text = String::new();
    let mut calls = Vec::new();
    for part in array(&candidate["content"]["parts"]) {
        // Thought text is not ordinary assistant output. Its tokens are still
        // included in usageMetadata.thoughtsTokenCount.
        if part["thought"] == true {
            continue;
        }
        if let Some(s) = part["text"].as_str() {
            text.push_str(s);
        }
        if let Some(call) = part.get("functionCall") {
            let mut call = json!({"id":format!("gemini_call_{}", calls.len()),"type":"function",
                "function":{"name":call["name"],"arguments":call.get("args").unwrap_or(&json!({})).to_string()}});
            if let Some(signature) = part.get("thoughtSignature") {
                call["thought_signature"] = signature.clone();
            }
            calls.push(call);
        }
    }
    let stop = if body["promptFeedback"]["blockReason"].is_string() {
        "content_filter"
    } else {
        finish_reason(
            candidate["finishReason"].as_str().unwrap_or(""),
            !calls.is_empty(),
        )
    };
    let mut message = json!({"role":"assistant","content":text});
    if !calls.is_empty() {
        message["tool_calls"] = json!(calls);
    }
    let chat = json!({"id":body["responseId"].as_str().unwrap_or("gemini"),"object":"chat.completion","created":0,
        "model":body["modelVersion"].as_str().unwrap_or(model),"choices":[{"index":0,"message":message,"finish_reason":stop}],"usage":usage(body)});
    if endpoint == Endpoint::Messages {
        translate::translate_response(Direction::MessagesToChat, &chat)
    } else {
        chat
    }
}

/// Incremental native SSE -> Chat SSE, optionally chained into Messages SSE.
/// Bound each upstream event before JSON parsing; never report a truncated
/// generation as a successful client stream.
pub struct GeminiStream {
    buf: Vec<u8>,
    scanned: usize,
    model: String,
    id: String,
    tools: usize,
    started: bool,
    ended: bool,
    stop: Option<String>,
    usage: Option<Value>,
    include_usage: bool,
    messages: Option<translate::StreamTranslator>,
    pub failed: bool,
}

impl GeminiStream {
    pub fn new(endpoint: Endpoint, model: &str, include_usage: bool) -> Self {
        Self {
            buf: Vec::new(),
            scanned: 0,
            model: model.into(),
            id: "gemini".into(),
            tools: 0,
            started: false,
            ended: false,
            stop: None,
            usage: None,
            include_usage,
            failed: false,
            messages: (endpoint == Endpoint::Messages)
                .then(|| translate::StreamTranslator::new(Direction::MessagesToChat, model)),
        }
    }

    fn chunk(&self, delta: Value, stop: Option<&str>) -> Value {
        json!({"id":self.id,"object":"chat.completion.chunk","created":0,"model":self.model,
            "choices":[{"index":0,"delta":delta,"finish_reason":stop}]})
    }

    fn emit(&mut self, value: Value, out: &mut Vec<u8>) {
        self.forward(format!("data: {value}\n\n").as_bytes(), out);
    }

    fn forward(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        if let Some(messages) = self.messages.as_mut() {
            out.extend(messages.feed(bytes));
            self.failed |= messages.failed;
        } else {
            out.extend_from_slice(bytes);
        }
    }

    fn error(&mut self, message: &str, out: &mut Vec<u8>) {
        if self.ended {
            return;
        }
        self.failed = true;
        self.ended = true;
        self.buf.clear();
        self.emit(json!({"error":{"type":"api_error","message":message}}), out);
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        if self.ended {
            return out;
        }
        // Read in bounded slices even when the transport gives us a large chunk.
        for chunk in bytes.chunks(8192) {
            if self.ended {
                break;
            }
            self.buf.extend_from_slice(chunk);
            loop {
                let end = self.buf[self.scanned..]
                    .windows(2)
                    .position(|v| v == b"\n\n")
                    .map(|i| (i + self.scanned, 2));
                let crlf = self.buf[self.scanned..]
                    .windows(4)
                    .position(|v| v == b"\r\n\r\n")
                    .map(|i| (i + self.scanned, 4));
                let end = match (end, crlf) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                let Some((end, sep)) = end else {
                    self.scanned = self.buf.len().saturating_sub(3);
                    break;
                };
                if end > 4 * 1024 * 1024 {
                    self.error("upstream SSE event exceeds 4 MiB", &mut out);
                    break;
                }
                let event: Vec<_> = self.buf.drain(..end + sep).collect();
                self.scanned = 0;
                self.event(&event[..end], &mut out);
                if self.ended {
                    break;
                }
            }
            if self.buf.len() > 4 * 1024 * 1024 {
                self.error("upstream SSE event exceeds 4 MiB", &mut out);
            }
        }
        out
    }

    fn event(&mut self, raw: &[u8], out: &mut Vec<u8>) {
        let Ok(raw) = std::str::from_utf8(raw) else {
            self.error("invalid upstream SSE encoding", out);
            return;
        };
        let data = raw
            .lines()
            .filter_map(|s| {
                s.strip_prefix("data:")
                    .map(|s| s.strip_prefix(' ').unwrap_or(s))
            })
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            return;
        }
        let Ok(body) = serde_json::from_str::<Value>(&data) else {
            self.error("invalid upstream SSE JSON", out);
            return;
        };
        if let Some(error) = body.get("error") {
            self.error(
                error["message"]
                    .as_str()
                    .unwrap_or("upstream stream failed"),
                out,
            );
            return;
        }
        if let Some(id) = body["responseId"].as_str() {
            self.id = id.into();
        }
        if let Some(model) = body["modelVersion"].as_str() {
            self.model = model.into();
        }
        if body["usageMetadata"].is_object() {
            self.usage = Some(usage(&body));
        }
        if !self.started {
            self.started = true;
            self.emit(
                self.chunk(json!({"role":"assistant","content":""}), None),
                out,
            );
        }
        let candidate = &body["candidates"][0];
        for part in array(&candidate["content"]["parts"]) {
            if part["thought"] == true {
                continue;
            }
            if let Some(text) = part["text"].as_str() {
                self.emit(self.chunk(json!({"content":text}), None), out);
            }
            if let Some(call) = part.get("functionCall") {
                if self.tools >= 128 {
                    self.error("upstream tool limit exceeded", out);
                    return;
                }
                let mut tool = json!({"index":self.tools,"id":format!("gemini_call_{}",self.tools),"type":"function",
                    "function":{"name":call["name"],"arguments":call.get("args").unwrap_or(&json!({})).to_string()}});
                if let Some(signature) = part.get("thoughtSignature") {
                    tool["thought_signature"] = signature.clone();
                }
                self.tools += 1;
                self.emit(self.chunk(json!({"tool_calls":[tool]}), None), out);
            }
        }
        if let Some(reason) = candidate["finishReason"]
            .as_str()
            .filter(|s| !s.is_empty() && *s != "FINISH_REASON_UNSPECIFIED")
        {
            self.stop = Some(finish_reason(reason, self.tools > 0).into());
        }
        if body["promptFeedback"]["blockReason"].is_string() {
            self.stop = Some("content_filter".into());
        }
    }

    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.ended {
            return out;
        }
        let tail = std::mem::take(&mut self.buf);
        if !tail.is_empty() {
            self.event(&tail, &mut out);
        }
        if self.ended {
            return out;
        }
        let Some(stop) = self.stop.clone() else {
            self.error("upstream Gemini stream ended before finishReason", &mut out);
            return out;
        };
        self.emit(self.chunk(json!({}), Some(&stop)), &mut out);
        if (self.include_usage || self.messages.is_some())
            && let Some(usage) = self.usage.take()
        {
            self.emit(json!({"id":self.id,"object":"chat.completion.chunk","created":0,"model":self.model,"choices":[],"usage":usage}),&mut out);
        }
        self.forward(b"data: [DONE]\n\n", &mut out);
        self.ended = true;
        out
    }
}
