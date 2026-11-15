use super::*;
use futures_util::StreamExt;
use std::collections::{BTreeMap, BTreeSet};

fn frame(kind: &str, value: &Value, exception: bool) -> Vec<u8> {
    let mut headers = Vec::new();
    for (name, value) in [
        (
            ":message-type",
            if exception { "exception" } else { "event" },
        ),
        (
            if exception {
                ":exception-type"
            } else {
                ":event-type"
            },
            kind,
        ),
        (":content-type", "application/json"),
    ] {
        headers.push(name.len() as u8);
        headers.extend_from_slice(name.as_bytes());
        headers.push(7);
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    }
    let payload = value.to_string();
    let mut out = Vec::from(((16 + headers.len() + payload.len()) as u32).to_be_bytes());
    out.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    out.extend_from_slice(&crc32fast::hash(&out).to_be_bytes());
    out.extend(headers);
    out.extend_from_slice(payload.as_bytes());
    out.extend_from_slice(&crc32fast::hash(&out).to_be_bytes());
    out
}

#[derive(Default)]
struct Tool {
    id: String,
    name: String,
    args: String,
    started: bool,
}
struct Encoder {
    context: Context,
    line: Vec<u8>,
    data: String,
    first: bool,
    started: bool,
    stopped: bool,
    done: bool,
    tools: BTreeMap<u64, Tool>,
    blocks: BTreeSet<u64>,
}
impl Encoder {
    fn new(context: Context) -> Self {
        Self {
            context,
            line: Vec::new(),
            data: String::new(),
            first: true,
            started: false,
            stopped: false,
            done: false,
            tools: BTreeMap::new(),
            blocks: BTreeSet::new(),
        }
    }
    fn emit(&mut self, value: Value, out: &mut Vec<u8>) {
        if self.context.sse {
            out.extend_from_slice(format!("data: {value}\n\n").as_bytes());
        } else {
            out.extend_from_slice(if self.first { b"[" } else { b"," });
            self.first = false;
            out.extend_from_slice(value.to_string().as_bytes());
        }
    }
    fn fail(&mut self, out: &mut Vec<u8>) {
        if self.done {
            return;
        }
        if self.context.format == Format::Gemini {
            self.emit(json!({"error":{"code":502,"message":"The upstream stream failed.","status":"INTERNAL"}}),out);
        } else {
            out.extend(frame(
                "modelStreamErrorException",
                &json!({"message":"The upstream stream failed.","originalStatusCode":502,"originalMessage":"The upstream stream failed."}),
                true,
            ));
        }
        self.done = true;
    }
    fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for &b in bytes {
            if self.done {
                break;
            }
            if b != b'\n' {
                self.line.push(b);
                if self.line.len() + self.data.len() > 1024 * 1024 {
                    self.fail(&mut out);
                }
                continue;
            }
            let line = std::mem::take(&mut self.line);
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                let data = std::mem::take(&mut self.data);
                if data == "[DONE]" {
                    self.done = true;
                    continue;
                }
                if data.is_empty() {
                    continue;
                }
                match serde_json::from_str(&data) {
                    Ok(v) => self.event(v, &mut out),
                    Err(_) => self.fail(&mut out),
                }
            } else if let Some(data) = line.strip_prefix("data:") {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(data.strip_prefix(' ').unwrap_or(data));
            }
        }
        out
    }
    fn event(&mut self, v: Value, out: &mut Vec<u8>) {
        if v.get("error").is_some() || v["type"] == "error" {
            self.fail(out);
            return;
        }
        if self.context.format == Format::Invoke {
            use base64::Engine;
            out.extend(frame(
                "chunk",
                &json!({"bytes":base64::engine::general_purpose::STANDARD.encode(v.to_string())}),
                false,
            ));
            return;
        }
        if self.context.format == Format::Converse && !self.started {
            self.started = true;
            out.extend(frame("messageStart", &json!({"role":"assistant"}), false));
        }
        for choice in array(&v["choices"]) {
            if choice["index"].as_u64().unwrap_or(0) != 0 {
                continue;
            }
            let delta = &choice["delta"];
            if let Some(text) = delta["content"].as_str().filter(|s| !s.is_empty()) {
                if self.context.format == Format::Gemini {
                    self.emit(json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":text}]}}]}),out);
                } else {
                    self.blocks.insert(0);
                    out.extend(frame(
                        "contentBlockDelta",
                        &json!({"contentBlockIndex":0,"delta":{"text":text}}),
                        false,
                    ));
                }
            }
            for call in array(&delta["tool_calls"]) {
                let index = call["index"].as_u64().unwrap_or(0);
                if index > 128 {
                    self.fail(out);
                    return;
                }
                let tool = self.tools.entry(index).or_default();
                if let Some(id) = call["id"].as_str() {
                    tool.id.push_str(id);
                }
                if let Some(name) = call["function"]["name"].as_str() {
                    tool.name.push_str(name);
                }
                let args = call["function"]["arguments"].as_str().unwrap_or_default();
                tool.args.push_str(args);
                if tool.args.len() > 1024 * 1024 || tool.name.len() > 1024 || tool.id.len() > 1024 {
                    self.fail(out);
                    return;
                }
                if self.context.format == Format::Converse {
                    if !tool.started && !tool.name.is_empty() && !tool.id.is_empty() {
                        tool.started = true;
                        self.blocks.insert(index + 1);
                        out.extend(frame("contentBlockStart",&json!({"contentBlockIndex":index+1,"start":{"toolUse":{"toolUseId":tool.id,"name":tool.name}}}),false));
                    }
                    if tool.started && !args.is_empty() {
                        out.extend(frame("contentBlockDelta",&json!({"contentBlockIndex":index+1,"delta":{"toolUse":{"input":args}}}),false));
                    }
                }
            }
            if let Some(reason) = choice["finish_reason"].as_str() {
                if self.context.format == Format::Gemini {
                    let tools = std::mem::take(&mut self.tools);
                    let mut parts = Vec::new();
                    for tool in tools.into_values() {
                        let args = match serde_json::from_str::<Value>(&tool.args) {
                            Ok(v) => v,
                            Err(_) => {
                                self.fail(out);
                                return;
                            }
                        };
                        parts.push(
                            json!({"functionCall":{"id":tool.id,"name":tool.name,"args":args}}),
                        );
                    }
                    let reason = match reason {
                        "length" => "MAX_TOKENS",
                        "content_filter" => "SAFETY",
                        _ => "STOP",
                    };
                    self.emit(json!({"candidates":[{"index":0,"content":{"role":"model","parts":parts},"finishReason":reason}]}),out);
                } else if !self.stopped {
                    for index in &self.blocks {
                        out.extend(frame(
                            "contentBlockStop",
                            &json!({"contentBlockIndex":index}),
                            false,
                        ));
                    }
                    out.extend(frame(
                        "messageStop",
                        &json!({"stopReason":bedrock::stop(reason)}),
                        false,
                    ));
                }
                self.stopped = true;
            }
        }
        if let Some(usage) = v.get("usage") {
            if self.context.format == Format::Gemini {
                self.emit(json!({"usageMetadata":gemini::usage(usage)}), out);
            } else {
                out.extend(frame(
                    "metadata",
                    &json!({"usage":bedrock::usage(usage),"metrics":{"latencyMs":0}}),
                    false,
                ));
            }
        }
    }
    fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        // Messages ends with message_stop, not the OpenAI [DONE] marker.
        if self.context.format != Format::Invoke && !self.stopped && !self.done {
            self.fail(&mut out);
        }
        if self.context.format == Format::Gemini && !self.context.sse {
            out.extend_from_slice(if self.first { b"[]" } else { b"]" });
        }
        out
    }
}

pub(super) fn response(context: Context, response: Response) -> Response {
    let (mut parts, body) = response.into_parts();
    let mime = if context.format == Format::Gemini {
        if context.sse {
            "text/event-stream"
        } else {
            "application/json"
        }
    } else {
        "application/vnd.amazon.eventstream"
    };
    parts
        .headers
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    parts.headers.remove(header::CONTENT_LENGTH);
    let stream = futures_util::stream::unfold(
        (body.into_data_stream(), Encoder::new(context), false),
        |(mut stream, mut encoder, ended)| async move {
            if ended {
                return None;
            }
            while let Some(chunk) = stream.next().await {
                let bytes = match chunk {
                    Ok(b) => encoder.feed(&b),
                    Err(_) => {
                        let mut out = Vec::new();
                        encoder.fail(&mut out);
                        out.extend(encoder.finish());
                        return Some((
                            Ok::<_, std::io::Error>(Bytes::from(out)),
                            (stream, encoder, true),
                        ));
                    }
                };
                if !bytes.is_empty() {
                    return Some((Ok(Bytes::from(bytes)), (stream, encoder, false)));
                }
            }
            let bytes = encoder.finish();
            Some((Ok(Bytes::from(bytes)), (stream, encoder, true)))
        },
    );
    Response::from_parts(parts, Body::from_stream(stream))
}
