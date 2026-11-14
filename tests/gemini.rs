use serde_json::json;
use tokomotive::{
    provider::{Endpoint, ProviderKind},
    translate_gemini,
};

#[test]
fn gemini_chat_request_and_exclusive_usage() {
    let request = translate_gemini::request(Endpoint::ChatCompletions, &json!({
        "model":"gemini-2.5-flash", "messages":[{"role":"system","content":"Be brief"},
        {"role":"user","content":"Hello"}], "max_completion_tokens":32,
        "tools":[{"type":"function","function":{"name":"weather","parameters":{"type":"object"}}}]
    })).unwrap();
    assert_eq!(
        request["contents"],
        json!([{"role":"user","parts":[{"text":"Hello"}]}])
    );
    assert_eq!(
        request["systemInstruction"],
        json!({"parts":[{"text":"Be brief"}]})
    );
    assert_eq!(request["generationConfig"]["maxOutputTokens"], 32);
    assert_eq!(
        request["tools"][0]["functionDeclarations"][0]["name"],
        "weather"
    );
    assert!(request.get("model").is_none());
    let reply = json!({"usageMetadata":{"promptTokenCount":100,"cachedContentTokenCount":40,
        "candidatesTokenCount":12,"thoughtsTokenCount":8,"totalTokenCount":120}});
    let usage = ProviderKind::Gemini
        .usage_from_json(Endpoint::ChatCompletions, &reply)
        .unwrap();
    assert_eq!((usage.input, usage.cache_read, usage.output), (60, 40, 20));
}

fn reply() -> serde_json::Value {
    json!({"responseId":"g-1","modelVersion":"gemini-2.5-flash",
        "candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"Hello"},
            {"functionCall":{"name":"weather","args":{"city":"Paris"}},"thoughtSignature":"opaque"}]},"finishReason":"STOP"}],
        "usageMetadata":{"promptTokenCount":100,"cachedContentTokenCount":40,"candidatesTokenCount":12,"thoughtsTokenCount":8}})
}

#[test]
fn gemini_replies_and_messages_tool_roundtrip() {
    let chat = translate_gemini::response(Endpoint::ChatCompletions, &reply(), "fallback");
    assert_eq!(chat["choices"][0]["message"]["content"], "Hello");
    assert_eq!(chat["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        chat["choices"][0]["message"]["tool_calls"][0]["thought_signature"],
        "opaque"
    );
    assert_eq!(chat["usage"]["completion_tokens"], 20);
    assert_eq!(chat["usage"]["total_tokens"], 120);
    let messages = translate_gemini::response(Endpoint::Messages, &reply(), "fallback");
    assert_eq!(messages["content"][1]["type"], "tool_use");
    assert_eq!(messages["content"][1]["thought_signature"], "opaque");
    assert_eq!(messages["usage"]["input_tokens"], 60);
    assert_eq!(messages["usage"]["cache_read_input_tokens"], 40);
    let req = translate_gemini::request(Endpoint::Messages, &json!({"model":"gemini","max_tokens":50,
        "system":"Use tools", "messages":[{"role":"assistant","content":[{"type":"tool_use","id":"call1","name":"weather","thought_signature":"opaque","input":{"city":"Paris"}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"call1","content":"sunny"}]}]})).unwrap();
    assert_eq!(
        req["contents"][0]["parts"][0]["functionCall"]["args"],
        json!({"city":"Paris"})
    );
    assert_eq!(req["contents"][0]["parts"][0]["thoughtSignature"], "opaque");
    assert_eq!(
        req["contents"][1]["parts"][0]["functionResponse"]["name"],
        "weather"
    );
    assert_eq!(req["generationConfig"]["maxOutputTokens"], 50);
}

fn events(bytes: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8(bytes.to_vec())
        .unwrap()
        .lines()
        .filter_map(|line| {
            line.strip_prefix("data: ")
                .and_then(|s| serde_json::from_str(s).ok())
        })
        .collect()
}

#[test]
fn gemini_stream_both_clients_at_every_byte_boundary() {
    let wire = format!(
        "data: {}\r\n\r\ndata: {}\n\n",
        json!({"responseId":"g-1","candidates":[{"content":{"parts":[{"text":"Héllo"}]}}]}),
        reply()
    );
    for endpoint in [Endpoint::ChatCompletions, Endpoint::Messages] {
        for split in 0..=wire.len() {
            let mut stream = translate_gemini::GeminiStream::new(endpoint, "gemini", true);
            let mut out = stream.feed(&wire.as_bytes()[..split]);
            out.extend(stream.feed(&wire.as_bytes()[split..]));
            out.extend(stream.finish());
            assert!(!stream.failed, "split {split}");
            let chunks = events(&out);
            if endpoint == Endpoint::ChatCompletions {
                assert_eq!(
                    chunks
                        .iter()
                        .filter_map(|v| v["choices"][0]["delta"]["content"].as_str())
                        .collect::<String>(),
                    "HélloHello"
                );
                assert!(chunks.iter().any(
                    |v| v["choices"][0]["delta"]["tool_calls"][0]["function"]["name"] == "weather"
                ));
                assert_eq!(chunks.last().unwrap()["usage"]["completion_tokens"], 20);
                assert!(out.ends_with(b"data: [DONE]\n\n"));
            } else {
                assert_eq!(chunks.last().unwrap()["type"], "message_stop");
                assert!(chunks.iter().any(|v| v["type"] == "content_block_start"
                    && v["content_block"]["name"] == "weather"));
                assert!(
                    chunks
                        .iter()
                        .any(|v| v["content_block"]["thought_signature"] == "opaque")
                );
                let usage = &chunks
                    .iter()
                    .find(|v| v["type"] == "message_delta")
                    .unwrap()["usage"];
                assert_eq!(usage["input_tokens"], 60);
                assert_eq!(usage["output_tokens"], 20);
            }
        }
    }
}

#[test]
fn gemini_stream_errors_and_truncation_are_not_success() {
    for wire in [
        "data: {broken}\n\n".to_string(),
        format!(
            "data: {}\n\n",
            json!({"candidates":[{"content":{"parts":[{"text":"hi"}]}}]})
        ),
        format!(
            "data: {}\n\n",
            json!({"error":{"message":"quota","code":429}})
        ),
        format!("data: {}", "x".repeat(4 * 1024 * 1024)),
    ] {
        let mut stream = translate_gemini::GeminiStream::new(Endpoint::Messages, "gemini", false);
        let mut out = stream.feed(wire.as_bytes());
        out.extend(stream.finish());
        assert!(stream.failed);
        assert!(events(&out).iter().any(|v| v["type"] == "error"));
        assert!(!events(&out).iter().any(|v| v["type"] == "message_stop"));
    }
}

#[test]
fn gemini_path_generation_and_usage_stream_are_bounded_and_cumulative() {
    assert_eq!(
        translate_gemini::path("models/gemini-2.5-flash", true).unwrap(),
        "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
    );
    for model in [
        "",
        ".",
        "..",
        "../models/x",
        "models/../x",
        "x:generateContent?key=evil",
        "x%2fy",
        "x/y",
        "x\\y",
        "https://evil.test",
    ] {
        assert!(translate_gemini::path(model, false).is_err(), "{model}");
    }
    let mut tracker = ProviderKind::Gemini.new_stream_tracker(Endpoint::Other, false);
    for v in [
        json!({"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":1}}),
        reply(),
    ] {
        let wire = format!("data: {v}\n\n");
        for chunk in wire.as_bytes().chunks(1) {
            tracker.feed(chunk);
        }
    }
    tracker.finish();
    let usage = tracker.usage.unwrap();
    assert_eq!((usage.input, usage.output, usage.cache_read), (60, 20, 40));
    assert!(tracker.usage_final);
    assert_eq!(tracker.model.as_deref(), Some("gemini-2.5-flash"));
}

#[test]
fn gemini_safety_stops_and_image_json_configuration() {
    let body =
        json!({"promptFeedback":{"blockReason":"SAFETY"},"usageMetadata":{"promptTokenCount":5}});
    let v = translate_gemini::response(Endpoint::ChatCompletions, &body, "gemini");
    assert_eq!(v["choices"][0]["finish_reason"], "content_filter");
    let req = translate_gemini::request(Endpoint::ChatCompletions,&json!({"messages":[{"role":"user","content":[
        {"type":"text","text":"What is this?"},{"type":"image_url","image_url":{"url":"data:image/png;base64,YQ=="}}]}],
        "temperature":0.2,"top_p":0.8,"stop":"end","response_format":{"type":"json_schema","json_schema":{"schema":{"type":"object"}}},
        "tool_choice":{"type":"function","function":{"name":"weather"}}
    })).unwrap();
    assert_eq!(
        req["contents"][0]["parts"][1],
        json!({"inlineData":{"mimeType":"image/png","data":"YQ=="}})
    );
    assert_eq!(
        req["generationConfig"]["responseJsonSchema"],
        json!({"type":"object"})
    );
    assert_eq!(req["generationConfig"]["stopSequences"], json!(["end"]));
    assert_eq!(
        req["toolConfig"]["functionCallingConfig"],
        json!({"mode":"ANY","allowedFunctionNames":["weather"]})
    );
    for body in [
        json!({"messages":[],"n":2}),
        json!({"messages":[],"max_tokens":1,"max_completion_tokens":2}),
        json!({"messages":[{"role":"tool","tool_call_id":"missing","content":"x"}]}),
        json!({"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.test/a.png"}}]}]}),
    ] {
        assert!(translate_gemini::request(Endpoint::ChatCompletions, &body).is_err());
    }
}

#[test]
fn gemini_native_does_not_accept_an_unrelated_oauth_credential() {
    let cfg: tokomotive::config::Config = toml::from_str(
        r#"
[[providers]]
name="bad-pair"
kind="gemini"
auth="entra"
base_url="https://resource.openai.azure.com/openai"
"#,
    )
    .unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn gemini_partial_usage_does_not_count_as_a_final_report() {
    let mut tracker = ProviderKind::Gemini.new_stream_tracker(Endpoint::Other, false);
    let wire = format!(
        "data: {}\n\n",
        json!({"candidates":[{"finishReason":"","content":{"parts":[{"text":"partial"}]}}],
        "usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":2}})
    );
    tracker.feed(wire.as_bytes());
    assert!(tracker.usage.is_some());
    assert!(!tracker.usage_final);
    let mut stream = translate_gemini::GeminiStream::new(Endpoint::ChatCompletions, "gemini", true);
    stream.feed(wire.as_bytes());
    stream.finish();
    assert!(stream.failed);
    tracker.feed(
        format!(
            "data: {}\n\n",
            json!({"candidates":[{"finishReason":"STOP"}]})
        )
        .as_bytes(),
    );
    // The provider can send the last usage snapshot after the terminal candidate.
    tracker.feed(
        format!(
            "data: {}\n\n",
            json!({"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":20}})
        )
        .as_bytes(),
    );
    assert!(tracker.usage_final);
    assert_eq!(tracker.usage.unwrap().output, 20);
}
