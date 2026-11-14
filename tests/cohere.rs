use serde_json::json;
use tokomotive::{
    provider::{Endpoint, ProviderKind},
    translate_cohere as cohere,
};
#[test]
fn cohere_maps_messages_tools_and_bills_units_not_raw_tokens() {
    let request = json!({"model":"command-a","messages":[{"role":"developer","content":"Be brief"},{"role":"assistant","tool_calls":[{"id":"a","type":"function","function":{"name":"weather","arguments":"{}"}}]},{"role":"tool","tool_call_id":"a","content":"sunny"}],"max_completion_tokens":10,"top_p":0.7,"stop":"END","tool_choice":"required"});
    let mapped = cohere::request(Endpoint::ChatCompletions, &request).unwrap();
    assert_eq!(mapped["messages"][0]["role"], "system");
    assert_eq!(mapped["messages"][2]["tool_call_id"], "a");
    assert_eq!(mapped["max_tokens"], 10);
    assert_eq!(mapped["p"], 0.7);
    assert_eq!(mapped["stop_sequences"], json!(["END"]));
    assert_eq!(mapped["tool_choice"], "REQUIRED");
    let body = json!({"id":"co-1","finish_reason":"TOOL_CALL","message":{"role":"assistant","content":[{"type":"text","text":"Hello"}],"tool_calls":[{"id":"call","type":"function","function":{"name":"weather","arguments":"{}"}}]},"usage":{"billed_units":{"input_tokens":5.0,"output_tokens":26.0},"tokens":{"input_tokens":71,"output_tokens":30}}});
    let out = cohere::response(Endpoint::ChatCompletions, &body, "command-a");
    assert_eq!(out["choices"][0]["message"]["content"], "Hello");
    assert_eq!(out["usage"]["prompt_tokens"], 5);
    assert_eq!(out["usage"]["completion_tokens"], 26);
    let out = cohere::response(Endpoint::Messages, &body, "command-a");
    assert_eq!(out["stop_reason"], "tool_use");
    assert_eq!(out["content"][1]["name"], "weather");
    let usage = ProviderKind::Cohere
        .usage_from_json(Endpoint::Other, &body)
        .unwrap();
    assert_eq!((usage.input, usage.output), (5, 26));
    let fallback = json!({"usage":{"tokens":{"input_tokens":71,"output_tokens":30}}});
    assert_eq!(
        ProviderKind::Cohere
            .usage_from_json(Endpoint::Other, &fallback)
            .unwrap()
            .input,
        71
    );
}

#[test]
fn cohere_stream_text_tools_usage_and_truncation() {
    let events = [
        json!({"type":"message-start","id":"co-1","delta":{"message":{"role":"assistant"}}}),
        json!({"type":"content-delta","index":0,"delta":{"message":{"content":{"text":"Hello"}}}}),
        json!({"type":"tool-call-start","index":0,"delta":{"message":{"tool_calls":{"id":"call","type":"function","function":{"name":"weather","arguments":""}}}}}),
        json!({"type":"tool-call-delta","index":0,"delta":{"message":{"tool_calls":{"function":{"arguments":"{}"}}}}}),
        json!({"type":"tool-call-end","index":0}),
        json!({"type":"message-end","delta":{"finish_reason":"TOOL_CALL","usage":{"billed_units":{"input_tokens":5,"output_tokens":26},"tokens":{"input_tokens":71,"output_tokens":30}}}}),
    ];
    for endpoint in [Endpoint::ChatCompletions, Endpoint::Messages] {
        let mut stream = cohere::CohereStream::new(endpoint, "command-a", true);
        let mut tracker = ProviderKind::Cohere.new_stream_tracker(Endpoint::Other, false);
        let mut out = Vec::new();
        for e in &events {
            for c in format!(
                "event: {}\r\ndata: {e}\r\n\r\n",
                e["type"].as_str().unwrap()
            )
            .as_bytes()
            .chunks(3)
            {
                let bytes = tracker.feed(c);
                out.extend(stream.feed(&bytes));
            }
        }
        out.extend(stream.feed(&tracker.finish()));
        out.extend(stream.finish());
        let out = String::from_utf8(out).unwrap();
        assert!(!stream.failed, "{out}");
        assert!(out.contains("Hello") && out.contains("weather"));
        assert!(out.contains(if endpoint == Endpoint::Messages {
            "message_stop"
        } else {
            "[DONE]"
        }));
        assert_eq!(tracker.usage.unwrap().input, 5);
        assert!(tracker.usage_final);
    }
    let mut s = cohere::CohereStream::new(Endpoint::ChatCompletions, "m", true);
    s.finish();
    assert!(s.failed);
}
