//! Provider adapters: request preparation and usage extraction.
//!
//! Each adapter converts the Provider's usage report into exclusive Token
//! Classes (see `Usage`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::usage::Usage;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// OpenAI-compatible: `/v1/chat/completions`, `/v1/responses`.
    #[default]
    Openai,
    /// Anthropic: `/v1/messages`.
    Anthropic,
}

/// The kind of endpoint a request goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    ChatCompletions,
    Responses,
    Messages,
    /// Any other path. Forwarded, recorded as an Unpriced Entry.
    Other,
}

impl Endpoint {
    pub fn from_path(path: &str) -> Endpoint {
        let p = path.trim_end_matches('/');
        if p.ends_with("/chat/completions") {
            Endpoint::ChatCompletions
        } else if p.ends_with("/responses") {
            Endpoint::Responses
        } else if p.ends_with("/messages") {
            Endpoint::Messages
        } else {
            Endpoint::Other
        }
    }

    /// The Provider kind that speaks this endpoint, if the endpoint decides it.
    pub fn native_kind(&self) -> Option<ProviderKind> {
        match self {
            Endpoint::ChatCompletions | Endpoint::Responses => Some(ProviderKind::Openai),
            Endpoint::Messages => Some(ProviderKind::Anthropic),
            Endpoint::Other => None,
        }
    }

    pub fn is_accounted(&self) -> bool {
        !matches!(self, Endpoint::Other)
    }
}

impl ProviderKind {
    /// Mutate the request body before it goes upstream.
    /// For streamed Chat Completions, ask the Provider to report usage.
    pub fn prepare_body(&self, endpoint: Endpoint, body: &mut Value) {
        if *self == ProviderKind::Openai
            && endpoint == Endpoint::ChatCompletions
            && is_stream(body)
            && let Some(obj) = body.as_object_mut()
        {
            let opts = obj
                .entry("stream_options")
                .or_insert_with(|| Value::Object(Default::default()));
            if let Some(o) = opts.as_object_mut() {
                o.insert("include_usage".into(), Value::Bool(true));
            }
        }
    }

    /// Usage from a non-streamed JSON response body.
    pub fn usage_from_json(&self, v: &Value) -> Option<Usage> {
        match self {
            ProviderKind::Openai => openai_usage(v.get("usage")?),
            ProviderKind::Anthropic => anthropic_usage(v.get("usage")?, None),
        }
    }

    pub fn new_stream_tracker(&self) -> StreamTracker {
        StreamTracker {
            kind: *self,
            ..Default::default()
        }
    }
}

pub fn is_stream(body: &Value) -> bool {
    body.get("stream").and_then(Value::as_bool).unwrap_or(false)
}

pub fn model_of(body: &Value) -> Option<String> {
    body.get("model").and_then(Value::as_str).map(str::to_owned)
}

fn int(v: &Value, k: &str) -> i64 {
    v.get(k).and_then(Value::as_i64).unwrap_or(0)
}

/// OpenAI Chat Completions or Responses usage. Cached tokens are a subset of
/// prompt/input tokens, so they are subtracted from input.
fn openai_usage(u: &Value) -> Option<Usage> {
    if !u.is_object() {
        return None;
    }
    let (prompt, output, details) = if u.get("prompt_tokens").is_some() {
        (
            int(u, "prompt_tokens"),
            int(u, "completion_tokens"),
            u.get("prompt_tokens_details"),
        )
    } else {
        (
            int(u, "input_tokens"),
            int(u, "output_tokens"),
            u.get("input_tokens_details"),
        )
    };
    let cached = details
        .map(|d| int(d, "cached_tokens"))
        .unwrap_or(0)
        .min(prompt);
    Some(Usage {
        input: prompt - cached,
        output,
        cache_read: cached,
        ..Default::default()
    })
}

/// Anthropic usage. Cache tokens are separate from `input_tokens`.
/// When `prev` is given, fields missing in `u` keep their previous value
/// (stream `message_delta` events carry only some fields).
fn anthropic_usage(u: &Value, prev: Option<Usage>) -> Option<Usage> {
    if !u.is_object() {
        return None;
    }
    let p = prev.unwrap_or_default();
    let get = |k: &str, old: i64| u.get(k).and_then(Value::as_i64).unwrap_or(old);
    let mut out = Usage {
        input: get("input_tokens", p.input),
        output: get("output_tokens", p.output),
        cache_read: get("cache_read_input_tokens", p.cache_read),
        cache_write_5m: p.cache_write_5m,
        cache_write_1h: p.cache_write_1h,
    };
    match u.get("cache_creation").filter(|c| c.is_object()) {
        Some(cc) => {
            out.cache_write_5m = int(cc, "ephemeral_5m_input_tokens");
            out.cache_write_1h = int(cc, "ephemeral_1h_input_tokens");
        }
        None => {
            if let Some(total) = u.get("cache_creation_input_tokens").and_then(Value::as_i64) {
                out.cache_write_5m = total;
                out.cache_write_1h = 0;
            }
        }
    }
    Some(out)
}

/// Reads a Server-Sent Events byte stream and tracks usage and model.
#[derive(Debug, Default)]
pub struct StreamTracker {
    kind: ProviderKind,
    buf: Vec<u8>,
    pub usage: Option<Usage>,
    pub model: Option<String>,
    /// True when the Provider sent its final usage report.
    pub usage_final: bool,
    pub saw_content: bool,
}

const MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;

impl StreamTracker {
    pub fn feed(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
        while let Some((end, sep)) = find_event_end(&self.buf) {
            let event: Vec<u8> = self.buf.drain(..end + sep).collect();
            self.handle_event(&event[..end]);
        }
        if self.buf.len() > MAX_EVENT_BYTES {
            // A single event this large is not usage data. Drop it.
            self.buf.clear();
        }
    }

    /// Call at end of stream to process a trailing event with no separator.
    pub fn finish(&mut self) {
        if !self.buf.is_empty() {
            let event = std::mem::take(&mut self.buf);
            self.handle_event(&event);
        }
    }

    fn handle_event(&mut self, raw: &[u8]) {
        let Ok(text) = std::str::from_utf8(raw) else {
            return;
        };
        let mut data = String::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            }
        }
        if data.is_empty() || data == "[DONE]" {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(&data) else {
            return;
        };
        match self.kind {
            ProviderKind::Openai => self.handle_openai(&v),
            ProviderKind::Anthropic => self.handle_anthropic(&v),
        }
    }

    fn handle_openai(&mut self, v: &Value) {
        // Responses API: usage arrives in the `response.completed` event.
        if let Some(t) = v.get("type").and_then(Value::as_str) {
            if let Some(resp) = v.get("response") {
                if let Some(m) = resp.get("model").and_then(Value::as_str) {
                    self.model = Some(m.to_owned());
                }
                if matches!(
                    t,
                    "response.completed" | "response.incomplete" | "response.failed"
                ) && let Some(u) = resp.get("usage").and_then(openai_usage)
                {
                    self.usage = Some(u);
                    self.usage_final = true;
                }
            }
            if t == "response.output_text.delta" {
                self.saw_content = true;
            }
            return;
        }
        // Chat Completions chunk.
        if let Some(m) = v.get("model").and_then(Value::as_str) {
            self.model = Some(m.to_owned());
        }
        if v.get("choices")
            .and_then(Value::as_array)
            .is_some_and(|c| !c.is_empty())
        {
            self.saw_content = true;
        }
        if let Some(u) = v.get("usage").and_then(openai_usage) {
            self.usage = Some(u);
            self.usage_final = true;
        }
    }

    fn handle_anthropic(&mut self, v: &Value) {
        match v.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(msg) = v.get("message") {
                    if let Some(m) = msg.get("model").and_then(Value::as_str) {
                        self.model = Some(m.to_owned());
                    }
                    if let Some(u) = msg.get("usage").and_then(|u| anthropic_usage(u, None)) {
                        self.usage = Some(u);
                    }
                }
            }
            Some("content_block_delta") => self.saw_content = true,
            Some("message_delta") => {
                if let Some(u) = v.get("usage").and_then(|u| anthropic_usage(u, self.usage)) {
                    self.usage = Some(u);
                    self.usage_final = true;
                }
            }
            _ => {}
        }
    }
}

/// Find the end of the first SSE event: returns (event length, separator length).
fn find_event_end(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some((i, 2));
        }
        if buf[i] == b'\r' && i + 3 < buf.len() && &buf[i..i + 4] == b"\r\n\r\n" {
            return Some((i, 4));
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn openai_chat_cached_tokens_are_subtracted_from_input() {
        let v = json!({"usage": {"prompt_tokens": 1000, "completion_tokens": 50,
            "prompt_tokens_details": {"cached_tokens": 800}}});
        let u = ProviderKind::Openai.usage_from_json(&v).unwrap();
        assert_eq!(
            u,
            Usage {
                input: 200,
                output: 50,
                cache_read: 800,
                ..Default::default()
            }
        );
    }

    #[test]
    fn openai_responses_usage() {
        let v = json!({"usage": {"input_tokens": 300, "output_tokens": 20,
            "input_tokens_details": {"cached_tokens": 100}}});
        let u = ProviderKind::Openai.usage_from_json(&v).unwrap();
        assert_eq!(
            u,
            Usage {
                input: 200,
                output: 20,
                cache_read: 100,
                ..Default::default()
            }
        );
    }

    #[test]
    fn anthropic_cache_tokens_are_separate_and_tiered() {
        let v = json!({"usage": {"input_tokens": 10, "output_tokens": 5,
            "cache_read_input_tokens": 1000, "cache_creation_input_tokens": 300,
            "cache_creation": {"ephemeral_5m_input_tokens": 100, "ephemeral_1h_input_tokens": 200}}});
        let u = ProviderKind::Anthropic.usage_from_json(&v).unwrap();
        assert_eq!(
            u,
            Usage {
                input: 10,
                output: 5,
                cache_read: 1000,
                cache_write_5m: 100,
                cache_write_1h: 200
            }
        );
    }

    #[test]
    fn anthropic_cache_creation_without_breakdown_is_5m() {
        let v = json!({"usage": {"input_tokens": 1, "output_tokens": 1, "cache_creation_input_tokens": 42}});
        assert_eq!(
            ProviderKind::Anthropic
                .usage_from_json(&v)
                .unwrap()
                .cache_write_5m,
            42
        );
    }

    #[test]
    fn chat_stream_injects_include_usage() {
        let mut b = json!({"model": "gpt-4o", "stream": true});
        ProviderKind::Openai.prepare_body(Endpoint::ChatCompletions, &mut b);
        assert_eq!(b["stream_options"]["include_usage"], json!(true));
        let mut nb = json!({"model": "gpt-4o"});
        ProviderKind::Openai.prepare_body(Endpoint::ChatCompletions, &mut nb);
        assert!(nb.get("stream_options").is_none());
    }

    #[test]
    fn openai_chat_stream_split_across_chunks() {
        let s = concat!(
            "data: {\"model\":\"gpt-4o-2024-08-06\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"model\":\"gpt-4o-2024-08-06\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3,\"prompt_tokens_details\":{\"cached_tokens\":4}}}\n\n",
            "data: [DONE]\n\n"
        );
        let mut t = ProviderKind::Openai.new_stream_tracker();
        for c in s.as_bytes().chunks(7) {
            t.feed(c);
        }
        t.finish();
        assert!(t.usage_final && t.saw_content);
        assert_eq!(t.model.as_deref(), Some("gpt-4o-2024-08-06"));
        assert_eq!(
            t.usage.unwrap(),
            Usage {
                input: 6,
                output: 3,
                cache_read: 4,
                ..Default::default()
            }
        );
    }

    #[test]
    fn responses_stream_completed_event() {
        let s = "event: response.completed\r\ndata: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-5\",\"usage\":{\"input_tokens\":7,\"output_tokens\":2}}}\r\n\r\n";
        let mut t = ProviderKind::Openai.new_stream_tracker();
        t.feed(s.as_bytes());
        assert!(t.usage_final);
        assert_eq!(t.usage.unwrap().input, 7);
    }

    #[test]
    fn anthropic_stream_merges_start_and_delta() {
        let s = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-4-5\",\"usage\":{\"input_tokens\":20,\"output_tokens\":1,\"cache_read_input_tokens\":500,\"cache_creation_input_tokens\":0}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"x\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":42}}\n\n",
        );
        let mut t = ProviderKind::Anthropic.new_stream_tracker();
        t.feed(s.as_bytes());
        assert!(t.usage_final);
        assert_eq!(
            t.usage.unwrap(),
            Usage {
                input: 20,
                output: 42,
                cache_read: 500,
                ..Default::default()
            }
        );
    }

    #[test]
    fn anthropic_stream_without_delta_is_not_final() {
        let s = "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":20,\"output_tokens\":1}}}\n\n";
        let mut t = ProviderKind::Anthropic.new_stream_tracker();
        t.feed(s.as_bytes());
        assert!(!t.usage_final);
        assert_eq!(t.usage.unwrap().input, 20);
    }
}
