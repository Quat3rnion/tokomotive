//! Provider adapters: request preparation and usage extraction.
//!
//! Each adapter converts the Provider's usage report into exclusive Token
//! Classes (see `Usage`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::usage::Usage;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAuth {
    #[default]
    ApiKey,
    Chatgpt,
    Codex,
    Claude,
    HuggingFace,
    Entra,
    GoogleVertex,
    GithubCopilot,
    QwenCode,
    KimiCode,
    GeminiCli,
}

impl ProviderAuth {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::Chatgpt => "chatgpt",
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::HuggingFace => "hugging_face",
            Self::Entra => "entra",
            Self::GoogleVertex => "google_vertex",
            Self::GithubCopilot => "github_copilot",
            Self::QwenCode => "qwen_code",
            Self::KimiCode => "kimi_code",
            Self::GeminiCli => "gemini_cli",
        }
    }
    pub fn is_subscription(self) -> bool {
        matches!(
            self,
            Self::Chatgpt
                | Self::Codex
                | Self::Claude
                | Self::GithubCopilot
                | Self::QwenCode
                | Self::KimiCode
                | Self::GeminiCli
        )
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// OpenAI-compatible: `/v1/chat/completions`, `/v1/responses`.
    #[default]
    Openai,
    /// Anthropic: `/v1/messages`.
    Anthropic,
    /// Google native `generateContent` API.
    Gemini,
    Bedrock,
    Cohere,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KeyHeader {
    Bearer,
    XApiKey,
    ApiKey,
    /// Native Gemini authentication: https://ai.google.dev/api#authentication
    XGoogApiKey,
}

impl KeyHeader {
    pub fn for_kind(kind: ProviderKind) -> Self {
        match kind {
            ProviderKind::Openai => Self::Bearer,
            ProviderKind::Anthropic => Self::XApiKey,
            ProviderKind::Gemini => Self::XGoogApiKey,
            ProviderKind::Bedrock => Self::Bearer,
            ProviderKind::Cohere => Self::Bearer,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bearer => "bearer",
            Self::XApiKey => "x-api-key",
            Self::ApiKey => "api-key",
            Self::XGoogApiKey => "x-goog-api-key",
        }
    }

    pub fn apply(self, request: reqwest::RequestBuilder, key: &str) -> reqwest::RequestBuilder {
        match self {
            Self::Bearer => request.bearer_auth(key),
            Self::XApiKey => request.header("x-api-key", key),
            Self::ApiKey => request.header("api-key", key),
            Self::XGoogApiKey => request.header("x-goog-api-key", key),
        }
    }
}

pub fn default_include_usage() -> bool {
    true
}

/// Accept origin bases, versioned API bases and full Messages endpoints.
/// Keep the client query string and custom proxy prefixes unchanged.
pub fn upstream_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let base = base.strip_suffix("/messages").unwrap_or(base);
    if base.ends_with("/v2")
        && let Some(suffix) = path.strip_prefix("/v2/")
    {
        return format!("{base}/{suffix}");
    }
    if base.ends_with("/v1beta")
        && let Some(suffix) = path.strip_prefix("/v1beta/")
    {
        return format!("{base}/{suffix}");
    }
    let versioned = base.ends_with("/v1") || base.ends_with("/v4") || base.ends_with("/v1/openai");
    let suffix = if versioned {
        path.strip_prefix("/v1/").map(|p| format!("/{p}"))
    } else {
        None
    };
    format!("{base}{}", suffix.as_deref().unwrap_or(path))
}

/// The kind of endpoint a request goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    ChatCompletions,
    Responses,
    Messages,
    CountTokens,
    Rerank,
    AudioTranscription,
    AudioTranslation,
    AudioSpeech,
    Embeddings,
    /// `/v1/images/generations` and `/v1/images/edits`.
    Images,
    /// Any other path. Forwarded, recorded as an Unpriced Entry.
    Other,
}

impl Endpoint {
    pub fn from_path(path: &str) -> Endpoint {
        let p = path.trim_end_matches('/');
        if p == "/v1/messages/count_tokens" {
            Endpoint::CountTokens
        } else if p == "/v1/rerank" {
            Endpoint::Rerank
        } else if p == "/v1/audio/transcriptions" {
            Endpoint::AudioTranscription
        } else if p == "/v1/audio/translations" {
            Endpoint::AudioTranslation
        } else if p == "/v1/audio/speech" {
            Endpoint::AudioSpeech
        } else if p.ends_with("/chat/completions") {
            Endpoint::ChatCompletions
        } else if p.ends_with("/responses") {
            Endpoint::Responses
        } else if p.ends_with("/messages") {
            Endpoint::Messages
        } else if p.ends_with("/embeddings") {
            Endpoint::Embeddings
        } else if p.ends_with("/images/generations") || p.ends_with("/images/edits") {
            Endpoint::Images
        } else {
            Endpoint::Other
        }
    }

    /// The Provider kind that speaks this endpoint, if the endpoint decides it.
    pub fn native_kind(&self) -> Option<ProviderKind> {
        match self {
            Endpoint::ChatCompletions
            | Endpoint::Responses
            | Endpoint::Embeddings
            | Endpoint::AudioTranscription
            | Endpoint::AudioTranslation
            | Endpoint::AudioSpeech
            | Endpoint::Images => Some(ProviderKind::Openai),
            Endpoint::Messages | Endpoint::CountTokens => Some(ProviderKind::Anthropic),
            Endpoint::Rerank => Some(ProviderKind::Cohere),
            Endpoint::Other => None,
        }
    }

    pub fn is_accounted(&self) -> bool {
        !matches!(self, Endpoint::Other)
    }

    pub fn name(&self) -> &'static str {
        match self {
            Endpoint::ChatCompletions => "chat_completions",
            Endpoint::Responses => "responses",
            Endpoint::Messages => "messages",
            Endpoint::CountTokens => "count_tokens",
            Endpoint::Rerank => "rerank",
            Endpoint::AudioTranscription => "audio_transcription",
            Endpoint::AudioTranslation => "audio_translation",
            Endpoint::AudioSpeech => "audio_speech",
            Endpoint::Embeddings => "embeddings",
            Endpoint::Images => "images",
            Endpoint::Other => "other",
        }
    }
}

impl ProviderKind {
    pub fn models_path(self) -> &'static str {
        if self == Self::Gemini {
            "/v1beta/models"
        } else {
            "/v1/models"
        }
    }

    /// Mutate the request body before it goes upstream.
    /// For streamed Chat Completions, ask the Provider to report usage.
    /// Returns true when the Proxy turned on `include_usage` itself; the
    /// usage-only chunk must then be removed from the client stream.
    pub fn prepare_body(&self, endpoint: Endpoint, body: &mut Value, include_usage: bool) -> bool {
        if include_usage
            && *self == ProviderKind::Openai
            && endpoint == Endpoint::ChatCompletions
            && is_stream(body)
            && let Some(obj) = body.as_object_mut()
        {
            let opts = obj
                .entry("stream_options")
                .or_insert_with(|| Value::Object(Default::default()));
            if let Some(o) = opts.as_object_mut() {
                let had = o.get("include_usage").and_then(Value::as_bool) == Some(true);
                o.insert("include_usage".into(), Value::Bool(true));
                return !had;
            }
        }
        false
    }

    /// Usage from a non-streamed JSON response body.
    pub fn usage_from_json(&self, endpoint: Endpoint, v: &Value) -> Option<Usage> {
        if endpoint == Endpoint::Rerank {
            return rerank_usage(v);
        }
        match (self, endpoint) {
            (ProviderKind::Openai, Endpoint::Images) => image_usage(v.get("usage")?),
            (ProviderKind::Openai, _) => openai_usage(usage_object(v)?),
            (ProviderKind::Anthropic, _) => anthropic_usage(v.get("usage")?, None),
            (ProviderKind::Gemini, _) => gemini_usage(v.get("usageMetadata")?),
            (ProviderKind::Bedrock, _) => crate::translate_bedrock::usage(v.get("usage")?),
            (ProviderKind::Cohere, _) => crate::translate_cohere::usage(v.get("usage")?),
        }
    }

    /// `strip_usage_chunk`: drop the Chat Completions usage-only chunk from
    /// the forwarded stream (see `prepare_body`).
    pub fn new_stream_tracker(&self, endpoint: Endpoint, strip_usage_chunk: bool) -> StreamTracker {
        StreamTracker {
            kind: *self,
            endpoint: Some(endpoint),
            strip_usage_chunk,
            ..Default::default()
        }
    }
}

pub fn is_stream(body: &Value) -> bool {
    body.get("stream").and_then(Value::as_bool).unwrap_or(false)
}

fn rerank_usage(v: &Value) -> Option<Usage> {
    if let Some(n) = v
        .pointer("/meta/billed_units/search_units")
        .or_else(|| v.pointer("/usage/billed_units/search_units"))
        .and_then(Value::as_i64)
    {
        return Some(Usage {
            service_units: n,
            ..Default::default()
        });
    }
    // Voyage reports total_tokens; Jina reports total_tokens or prompt_tokens.
    let usage = v.get("usage")?;
    let n = usage
        .get("total_tokens")
        .or_else(|| usage.get("prompt_tokens"))?
        .as_i64()?;
    Some(Usage {
        input: n,
        ..Default::default()
    })
}

/// Google counts cached input inside promptTokenCount, but thoughts separately
/// from candidatesTokenCount: https://ai.google.dev/api/generate-content#UsageMetadata
pub fn gemini_usage(v: &Value) -> Option<Usage> {
    let prompt = v.get("promptTokenCount")?.as_i64()?.max(0);
    let cached = v["cachedContentTokenCount"]
        .as_i64()
        .unwrap_or(0)
        .clamp(0, prompt);
    Some(Usage {
        input: prompt.saturating_sub(cached),
        cache_read: cached,
        output: v["candidatesTokenCount"]
            .as_i64()
            .unwrap_or(0)
            .max(0)
            .saturating_add(v["thoughtsTokenCount"].as_i64().unwrap_or(0).max(0)),
        ..Default::default()
    })
}

pub fn model_of(body: &Value) -> Option<String> {
    body.get("model")
        .or_else(|| body.get("modelVersion"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn int(v: &Value, k: &str) -> i64 {
    v.get(k).and_then(Value::as_i64).unwrap_or(0)
}

/// Prefer the request-level report over a compatibility report in a choice.
/// Reports are cumulative snapshots, not deltas to add together.
fn usage_object(v: &Value) -> Option<&Value> {
    v.get("usage").filter(|u| u.is_object()).or_else(|| {
        v.get("choices")?
            .as_array()?
            .iter()
            .find_map(|c| c.get("usage").filter(|u| u.is_object()))
    })
}

/// Provider-reported cost for comparison only, in integer micro-dollars.
/// OpenRouter reports USD; xAI defines 10^10 ticks as one USD.
pub fn reported_cost_micros(v: &Value) -> Option<i64> {
    let u = usage_object(v)?;
    if let Some(ticks) = u.get("cost_in_usd_ticks").and_then(Value::as_u64) {
        return i64::try_from((u128::from(ticks) + 5_000) / 10_000).ok();
    }
    let micros = u.get("cost")?.as_f64()? * 1_000_000.0;
    (micros.is_finite() && micros >= 0.0 && micros < i64::MAX as f64).then(|| micros.round() as i64)
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
        .and_then(|d| d.get("cached_tokens").and_then(Value::as_i64))
        .or_else(|| u.get("cached_tokens").and_then(Value::as_i64))
        .or_else(|| u.get("prompt_cache_hit_tokens").and_then(Value::as_i64))
        .unwrap_or(0)
        .clamp(0, prompt.max(0));
    Some(Usage {
        input: prompt - cached,
        output,
        cache_read: cached,
        ..Default::default()
    })
}

/// OpenAI image generation usage. Output tokens are image tokens. Input is
/// split into text and image tokens; cached tokens are text tokens.
fn image_usage(u: &Value) -> Option<Usage> {
    if !u.is_object() {
        return None;
    }
    let input = int(u, "input_tokens");
    let (text, image, cached) = match u.get("input_tokens_details").filter(|d| d.is_object()) {
        Some(d) => (
            int(d, "text_tokens"),
            int(d, "image_tokens"),
            int(d, "cached_tokens"),
        ),
        None => (input, 0, 0),
    };
    let cached = cached.min(text);
    Some(Usage {
        input: text - cached,
        cache_read: cached,
        image_input: image,
        image_output: int(u, "output_tokens"),
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
        ..Default::default()
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
    endpoint: Option<Endpoint>,
    strip_usage_chunk: bool,
    buf: Vec<u8>,
    pub usage: Option<Usage>,
    pub reported_cost_micros: Option<i64>,
    pub model: Option<String>,
    /// True when the Provider sent its final usage report.
    pub usage_final: bool,
    gemini_finished: bool,
    eventstream: crate::eventstream::Decoder,
    pub saw_content: bool,
}

const MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;

impl StreamTracker {
    /// Process a chunk. Returns the bytes to forward to the client: whole
    /// events only, without dropped events.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.kind == ProviderKind::Bedrock {
            return match self.eventstream.feed(chunk) {
                Ok(frames) => {
                    let mut out = Vec::new();
                    for frame in frames {
                        let body = serde_json::from_slice::<Value>(&frame.payload);
                        let event = frame
                            .headers
                            .get(":event-type")
                            .map(String::as_str)
                            .unwrap_or("");
                        let value = if frame
                            .headers
                            .get(":message-type")
                            .is_none_or(|t| t != "event")
                        {
                            serde_json::json!({"error":{"message":"upstream Bedrock exception or invalid event"}})
                        } else if let Ok(body) = body {
                            if event == "metadata" {
                                self.usage =
                                    body.get("usage").and_then(crate::translate_bedrock::usage);
                                self.usage_final = self.gemini_finished && self.usage.is_some();
                            }
                            if event == "messageStop" {
                                self.gemini_finished = true;
                            }
                            if event == "contentBlockDelta" {
                                self.saw_content = true;
                            }
                            serde_json::json!({"event":event,"body":body})
                        } else {
                            serde_json::json!({"error":{"message":"invalid Bedrock event JSON"}})
                        };
                        out.extend_from_slice(format!("data: {value}\n\n").as_bytes());
                    }
                    out
                }
                Err(_) => {
                    b"data: {\"error\":{\"message\":\"invalid Bedrock event-stream\"}}\n\n".to_vec()
                }
            };
        }
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::with_capacity(chunk.len());
        while let Some((end, sep)) = find_event_end(&self.buf) {
            let event: Vec<u8> = self.buf.drain(..end + sep).collect();
            if self.handle_event(&event[..end]) {
                out.extend_from_slice(&event);
            }
        }
        if self.buf.len() > MAX_EVENT_BYTES {
            // A single event this large is not usage data. Forward it as is.
            out.append(&mut self.buf);
        }
        out
    }

    /// Call at end of stream. Processes a trailing event with no separator
    /// and returns the bytes to forward.
    pub fn finish(&mut self) -> Vec<u8> {
        if self.kind == ProviderKind::Bedrock {
            return if self.eventstream.finish().is_err() {
                b"data: {\"error\":{\"message\":\"truncated Bedrock event-stream\"}}\n\n".to_vec()
            } else {
                Vec::new()
            };
        }
        let event = std::mem::take(&mut self.buf);
        if !event.is_empty() && self.handle_event(&event) {
            event
        } else {
            Vec::new()
        }
    }

    /// Returns false when the event must not go to the client.
    fn handle_event(&mut self, raw: &[u8]) -> bool {
        let Ok(text) = std::str::from_utf8(raw) else {
            return true;
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
            return true;
        }
        let Ok(v) = serde_json::from_str::<Value>(&data) else {
            return true;
        };
        if let Some(cost) = reported_cost_micros(&v)
            .or_else(|| v.get("response").and_then(reported_cost_micros))
            .or_else(|| v.get("message").and_then(reported_cost_micros))
        {
            self.reported_cost_micros = Some(cost);
        }
        match self.kind {
            ProviderKind::Openai => self.handle_openai(&v),
            ProviderKind::Anthropic => {
                self.handle_anthropic(&v);
                true
            }
            ProviderKind::Gemini => {
                let v = v.get("response").unwrap_or(&v);
                if let Some(model) = v["modelVersion"].as_str() {
                    self.model = Some(model.into());
                }
                if v["candidates"].as_array().is_some_and(|c| !c.is_empty()) {
                    self.saw_content = true;
                }
                self.gemini_finished |= v["candidates"].as_array().is_some_and(|candidates| {
                    candidates.iter().any(|c| {
                        c["finishReason"]
                            .as_str()
                            .is_some_and(|s| !s.is_empty() && s != "FINISH_REASON_UNSPECIFIED")
                    })
                }) || v["promptFeedback"]["blockReason"].is_string();
                if let Some(usage) = v.get("usageMetadata").and_then(gemini_usage) {
                    self.usage = Some(usage);
                    self.usage_final = self.gemini_finished;
                }
                true
            }
            ProviderKind::Bedrock => true,
            ProviderKind::Cohere => {
                self.saw_content |= matches!(
                    v["type"].as_str(),
                    Some("content-delta" | "tool-call-start" | "tool-call-delta")
                );
                if v["type"] == "message-end" {
                    self.usage = v["delta"]
                        .get("usage")
                        .and_then(crate::translate_cohere::usage);
                    self.usage_final = self.usage.is_some()
                        && !matches!(
                            v["delta"]["finish_reason"].as_str(),
                            Some("ERROR" | "TIMEOUT")
                        );
                }
                true
            }
        }
    }

    fn handle_openai(&mut self, v: &Value) -> bool {
        // Image generation stream: usage in the `*.completed` event.
        if self.endpoint == Some(Endpoint::Images) {
            if let Some(m) = v.get("model").and_then(Value::as_str) {
                self.model = Some(m.to_owned());
            }
            if v.get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| t.ends_with(".partial_image"))
            {
                self.saw_content = true;
            }
            if let Some(u) = v.get("usage").and_then(image_usage) {
                self.usage = Some(u);
                self.usage_final = true;
                self.saw_content = true;
            }
            return true;
        }
        // Responses API: usage arrives in the `response.completed` event.
        if let Some(t) = v.get("type").and_then(Value::as_str) {
            if let Some(resp) = v.get("response") {
                if let Some(m) = resp.get("model").and_then(Value::as_str) {
                    self.model = Some(m.to_owned());
                }
                if matches!(
                    t,
                    "response.completed" | "response.incomplete" | "response.failed"
                ) && let Some(u) = usage_object(resp).and_then(openai_usage)
                {
                    self.usage = Some(u);
                    self.usage_final = true;
                }
            }
            if t == "response.output_text.delta" {
                self.saw_content = true;
            }
            return true;
        }
        // Chat Completions chunk.
        if let Some(m) = v.get("model").and_then(Value::as_str) {
            self.model = Some(m.to_owned());
        }
        let has_choices = v
            .get("choices")
            .and_then(Value::as_array)
            .is_some_and(|c| !c.is_empty());
        if has_choices {
            self.saw_content = true;
        }
        if let Some(u) = usage_object(v).and_then(openai_usage) {
            self.usage = Some(u);
            self.usage_final = true;
            if self.strip_usage_chunk && !has_choices {
                return false;
            }
        }
        true
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
        let u = ProviderKind::Openai
            .usage_from_json(Endpoint::ChatCompletions, &v)
            .unwrap();
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
        let u = ProviderKind::Openai
            .usage_from_json(Endpoint::ChatCompletions, &v)
            .unwrap();
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
        let u = ProviderKind::Anthropic
            .usage_from_json(Endpoint::Messages, &v)
            .unwrap();
        assert_eq!(
            u,
            Usage {
                input: 10,
                output: 5,
                cache_read: 1000,
                cache_write_5m: 100,
                cache_write_1h: 200,
                ..Default::default()
            }
        );
    }

    #[test]
    fn anthropic_cache_creation_without_breakdown_is_5m() {
        let v = json!({"usage": {"input_tokens": 1, "output_tokens": 1, "cache_creation_input_tokens": 42}});
        assert_eq!(
            ProviderKind::Anthropic
                .usage_from_json(Endpoint::Messages, &v)
                .unwrap()
                .cache_write_5m,
            42
        );
    }

    #[test]
    fn chat_stream_injects_include_usage() {
        let mut b = json!({"model": "gpt-4o", "stream": true});
        ProviderKind::Openai.prepare_body(Endpoint::ChatCompletions, &mut b, true);
        assert_eq!(b["stream_options"]["include_usage"], json!(true));
        let mut nb = json!({"model": "gpt-4o"});
        ProviderKind::Openai.prepare_body(Endpoint::ChatCompletions, &mut nb, true);
        assert!(nb.get("stream_options").is_none());
    }

    #[test]
    fn openai_chat_stream_split_across_chunks() {
        let s = concat!(
            "data: {\"model\":\"gpt-4o-2024-08-06\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"model\":\"gpt-4o-2024-08-06\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3,\"prompt_tokens_details\":{\"cached_tokens\":4}}}\n\n",
            "data: [DONE]\n\n"
        );
        let mut t = ProviderKind::Openai.new_stream_tracker(Endpoint::ChatCompletions, false);
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
        let mut t = ProviderKind::Openai.new_stream_tracker(Endpoint::ChatCompletions, false);
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
        let mut t = ProviderKind::Anthropic.new_stream_tracker(Endpoint::Messages, false);
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
        let mut t = ProviderKind::Anthropic.new_stream_tracker(Endpoint::Messages, false);
        t.feed(s.as_bytes());
        assert!(!t.usage_final);
        assert_eq!(t.usage.unwrap().input, 20);
    }

    #[test]
    fn injected_usage_chunk_is_stripped_but_counted() {
        let s = concat!(
            "data: {\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"model\":\"m\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3}}\n\n",
            "data: [DONE]\n\n"
        );
        let mut t = ProviderKind::Openai.new_stream_tracker(Endpoint::ChatCompletions, true);
        let mut out = Vec::new();
        for c in s.as_bytes().chunks(5) {
            out.extend(t.feed(c));
        }
        out.extend(t.finish());
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("hi") && out.contains("[DONE]"));
        assert!(!out.contains("usage"));
        assert_eq!(t.usage.unwrap().output, 3);

        let mut keep = ProviderKind::Openai.new_stream_tracker(Endpoint::ChatCompletions, false);
        assert_eq!(keep.feed(s.as_bytes()), s.as_bytes());
    }

    #[test]
    fn prepare_body_reports_whether_it_injected() {
        let mut a = json!({"stream": true});
        assert!(ProviderKind::Openai.prepare_body(Endpoint::ChatCompletions, &mut a, true));
        let mut b = json!({"stream": true, "stream_options": {"include_usage": true}});
        assert!(!ProviderKind::Openai.prepare_body(Endpoint::ChatCompletions, &mut b, true));
    }

    #[test]
    fn embeddings_and_images_usage() {
        let e = json!({"usage": {"prompt_tokens": 12, "total_tokens": 12}});
        assert_eq!(
            ProviderKind::Openai
                .usage_from_json(Endpoint::Embeddings, &e)
                .unwrap(),
            Usage {
                input: 12,
                ..Default::default()
            }
        );
        let i = json!({"usage": {"input_tokens": 120, "output_tokens": 4160,
            "input_tokens_details": {"text_tokens": 50, "image_tokens": 70, "cached_tokens": 10}}});
        assert_eq!(
            ProviderKind::Openai
                .usage_from_json(Endpoint::Images, &i)
                .unwrap(),
            Usage {
                input: 40,
                cache_read: 10,
                image_input: 70,
                image_output: 4160,
                ..Default::default()
            }
        );
        assert_eq!(Endpoint::from_path("/v1/images/edits"), Endpoint::Images);
        assert_eq!(Endpoint::from_path("/v1/embeddings"), Endpoint::Embeddings);
    }
}
