use serde_json::json;
use tokomotive::{
    provider::{Endpoint, ProviderKind},
    translate_bedrock as bedrock,
};
#[test]
fn converse_request_and_response_for_both_clients() {
    let chat = json!({"messages":[{"role":"system","content":"Be brief"},{"role":"user","content":"Hi"},{"role":"assistant","content":null,"tool_calls":[{"id":"a","type":"function","function":{"name":"weather","arguments":"{\"city\":\"Paris\"}"}}]},{"role":"tool","tool_call_id":"a","content":"sunny"}],"max_tokens":40,"temperature":0.2,"tools":[{"type":"function","function":{"name":"weather","parameters":{"type":"object"}}}],"tool_choice":"required"});
    let out = bedrock::request(Endpoint::ChatCompletions, &chat).unwrap();
    assert_eq!(out["system"], json!([{"text":"Be brief"}]));
    assert_eq!(
        out["messages"][2]["content"][0]["toolResult"]["toolUseId"],
        "a"
    );
    assert_eq!(out["inferenceConfig"]["maxTokens"], 40);
    assert_eq!(out["toolConfig"]["toolChoice"], json!({"any":{}}));
    let response = json!({"output":{"message":{"role":"assistant","content":[{"text":"Hi"},{"toolUse":{"toolUseId":"b","name":"weather","input":{"city":"Paris"}}}]}},"stopReason":"tool_use","usage":{"inputTokens":10,"outputTokens":3,"cacheReadInputTokens":20,"cacheWriteInputTokens":5}});
    let out = bedrock::response(Endpoint::ChatCompletions, &response, "model");
    assert_eq!(out["choices"][0]["message"]["tool_calls"][0]["id"], "b");
    assert_eq!(out["usage"]["prompt_tokens"], 35);
    let out = bedrock::response(Endpoint::Messages, &response, "model");
    assert_eq!(out["stop_reason"], "tool_use");
    assert_eq!(out["usage"]["input_tokens"], 10);
    assert_eq!(out["usage"]["cache_creation_input_tokens"], 5);
    let usage = ProviderKind::Bedrock
        .usage_from_json(Endpoint::Other, &response)
        .unwrap();
    assert_eq!(
        (
            usage.input,
            usage.output,
            usage.cache_read,
            usage.cache_write_5m
        ),
        (10, 3, 20, 5)
    );
    assert_eq!(
        bedrock::path("arn:aws:bedrock:us-east-1:123:inference-profile/a", true).unwrap(),
        "/model/arn%3Aaws%3Abedrock%3Aus-east-1%3A123%3Ainference-profile%2Fa/converse-stream"
    );
}

#[test]
fn converse_stream_tools_usage_and_truncation() {
    for endpoint in [Endpoint::ChatCompletions, Endpoint::Messages] {
        let mut stream = bedrock::BedrockStream::new(endpoint, "model", true);
        let events = [
            json!({"event":"messageStart","body":{"role":"assistant"}}),
            json!({"event":"contentBlockDelta","body":{"contentBlockIndex":0,"delta":{"text":"Hello"}}}),
            json!({"event":"contentBlockStart","body":{"contentBlockIndex":1,"start":{"toolUse":{"toolUseId":"call","name":"weather"}}}}),
            json!({"event":"contentBlockDelta","body":{"contentBlockIndex":1,"delta":{"toolUse":{"input":"{\"city\":\"Paris\"}"}}}}),
            json!({"event":"contentBlockStop","body":{"contentBlockIndex":1}}),
            json!({"event":"messageStop","body":{"stopReason":"tool_use"}}),
            json!({"event":"metadata","body":{"usage":{"inputTokens":10,"outputTokens":3,"cacheReadInputTokens":20,"cacheWriteInputTokens":5}}}),
        ];
        let mut out = Vec::new();
        for event in events {
            for c in format!("data: {event}\n\n").as_bytes().chunks(3) {
                out.extend(stream.feed(c));
            }
        }
        out.extend(stream.finish());
        let out = String::from_utf8(out).unwrap();
        assert!(!stream.failed);
        assert!(out.contains("Hello") && out.contains("weather") && out.contains("Paris"));
        if endpoint == Endpoint::Messages {
            assert!(out.contains("message_stop") && out.contains("cache_creation_input_tokens"));
        } else {
            assert!(out.contains("[DONE]") && out.contains("prompt_tokens"));
        }
    }
    let mut stream = bedrock::BedrockStream::new(Endpoint::ChatCompletions, "m", false);
    stream.finish();
    assert!(stream.failed);
}
