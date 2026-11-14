//! Doc-derived compatibility fixtures, not live Provider responses.
//! Sources and field locations are listed in docs/provider-roster.md.
use serde_json::{Value, json};
use tokomotive::provider::{Endpoint, ProviderKind};
use tokomotive::usage::Usage;

fn check_json_and_stream(value: Value, expected: Usage) {
    assert_eq!(
        ProviderKind::Openai.usage_from_json(Endpoint::ChatCompletions, &value),
        Some(expected)
    );
    let mut tracker = ProviderKind::Openai.new_stream_tracker(Endpoint::ChatCompletions, false);
    // Providers can report final usage without stream_options.include_usage.
    let event = format!("data: {value}\n\ndata: [DONE]\n\n");
    for bytes in event.as_bytes().chunks(7) {
        tracker.feed(bytes);
    }
    tracker.finish();
    assert!(tracker.usage_final);
    assert_eq!(tracker.usage, Some(expected));
}

#[test]
fn together_top_level_cached_tokens_json_and_stream() {
    check_json_and_stream(
        json!({"usage":{"prompt_tokens":100,"completion_tokens":20,"cached_tokens":60}}),
        Usage {
            input: 40,
            output: 20,
            cache_read: 60,
            ..Default::default()
        },
    );
}

#[test]
fn deepseek_cache_hit_json_and_final_stream_chunk() {
    check_json_and_stream(
        json!({"usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_cache_hit_tokens":60}}),
        Usage {
            input: 40,
            output: 20,
            cache_read: 60,
            ..Default::default()
        },
    );
}

#[test]
fn kimi_per_choice_usage_json_and_stream() {
    check_json_and_stream(
        json!({"usage":null,"choices":[{"index":0,"usage":{"prompt_tokens":100,"completion_tokens":20}}]}),
        Usage {
            input: 100,
            output: 20,
            ..Default::default()
        },
    );
}

#[test]
fn include_usage_off_does_not_inject_or_override_client_options() {
    let mut body = json!({"stream":true,"provider":{"order":["example"]}});
    let original = body.clone();
    assert!(!ProviderKind::Openai.prepare_body(Endpoint::ChatCompletions, &mut body, false));
    assert_eq!(body, original);
    body["stream_options"] = json!({"include_usage":true});
    let original = body.clone();
    assert!(!ProviderKind::Openai.prepare_body(Endpoint::ChatCompletions, &mut body, false));
    assert_eq!(body, original);
}

#[test]
fn openrouter_and_xai_reported_costs_json_and_stream() {
    use tokomotive::provider::reported_cost_micros;
    for (usage, expected) in [
        (
            json!({"prompt_tokens":100,"completion_tokens":20,"cost":0.0012345}),
            1235,
        ),
        (
            json!({"prompt_tokens":100,"completion_tokens":20,"cost_in_usd_ticks":12345000}),
            1235,
        ),
        (json!({"input_tokens":100,"output_tokens":20,"cost":0}), 0),
    ] {
        let value = json!({"usage":usage});
        assert_eq!(reported_cost_micros(&value), Some(expected));
        let mut tracker = ProviderKind::Openai.new_stream_tracker(Endpoint::ChatCompletions, false);
        tracker.feed(format!("data: {value}\n\n").as_bytes());
        assert_eq!(tracker.reported_cost_micros, Some(expected));
        let response = json!({"type":"response.completed","response":value});
        let mut tracker = ProviderKind::Openai.new_stream_tracker(Endpoint::Responses, false);
        tracker.feed(format!("data: {response}\n\n").as_bytes());
        assert_eq!(tracker.reported_cost_micros, Some(expected));
    }
    for invalid in [json!(null), json!(-1), json!("0.3"), json!(1e30)] {
        assert_eq!(
            reported_cost_micros(&json!({"usage":{"cost":invalid}})),
            None
        );
    }
}

// Standard OpenAI-format contract fixtures for each roster Provider. For rows
// whose usage is not verified in the roster, these test the parser only; they
// do not claim that a live service returns this report.
macro_rules! openai_fixture {
    ($name:ident) => { #[test] fn $name() {
        check_json_and_stream(json!({"usage":{"prompt_tokens":100,"completion_tokens":20,
            "prompt_tokens_details":{"cached_tokens":60},
            "completion_tokens_details":{"reasoning_tokens":15}}}),
            Usage {input:40,output:20,cache_read:60,..Default::default()});
    } };
}
openai_fixture!(openai_json_and_stream);
openai_fixture!(zai_json_and_stream);
openai_fixture!(mistral_json_and_stream);
openai_fixture!(fireworks_json_and_final_stream_chunk);
openai_fixture!(deepinfra_format_contract);
openai_fixture!(nebius_format_contract);
openai_fixture!(cerebras_format_contract);
openai_fixture!(groq_format_contract);
openai_fixture!(perplexity_format_contract);
openai_fixture!(siliconflow_format_contract);
openai_fixture!(hyperbolic_format_contract);
openai_fixture!(nvidia_nim_format_contract);
openai_fixture!(volcengine_openai_format_contract);
openai_fixture!(azure_json_and_stream);
openai_fixture!(bedrock_openai_format_contract);
openai_fixture!(huggingface_json_and_stream);
openai_fixture!(local_server_format_contract);

fn check_responses() {
    let value = json!({"usage":{"input_tokens":100,"output_tokens":20,
        "input_tokens_details":{"cached_tokens":60},"output_tokens_details":{"reasoning_tokens":15}}});
    let expected = Usage {
        input: 40,
        output: 20,
        cache_read: 60,
        ..Default::default()
    };
    assert_eq!(
        ProviderKind::Openai.usage_from_json(Endpoint::Responses, &value),
        Some(expected)
    );
    let event = json!({"type":"response.completed","response":value});
    let mut tracker = ProviderKind::Openai.new_stream_tracker(Endpoint::Responses, false);
    tracker.feed(format!("data: {event}\n\n").as_bytes());
    assert_eq!(tracker.usage, Some(expected));
    assert!(tracker.usage_final);
}
#[test]
fn minimax_responses_json_and_stream() {
    check_responses();
}
#[test]
fn dashscope_responses_json_and_stream() {
    check_responses();
}
#[test]
fn openai_responses_json_and_stream() {
    check_responses();
}
// The roster says Workers AI does not stream Responses. Test only JSON.
#[test]
fn cloudflare_responses_format_contract() {
    let value = json!({"usage":{"input_tokens":100,"output_tokens":20}});
    assert_eq!(
        ProviderKind::Openai.usage_from_json(Endpoint::Responses, &value),
        Some(Usage {
            input: 100,
            output: 20,
            ..Default::default()
        })
    );
}

macro_rules! messages_fixture {
    ($name:ident, $cache:expr, $expected:expr) => { #[test] fn $name() {
        let mut usage=json!({"input_tokens":100,"output_tokens":20});
        usage["cache_read_input_tokens"]=$cache;
        let expected=Usage{input:100,output:20,cache_read:$expected,..Default::default()};
        assert_eq!(ProviderKind::Anthropic.usage_from_json(Endpoint::Messages,&json!({"usage":usage})),Some(expected));
        let start=json!({"type":"message_start","message":{"usage":usage}});
        let delta=json!({"type":"message_delta","usage":{"output_tokens":20}});
        let mut tracker=ProviderKind::Anthropic.new_stream_tracker(Endpoint::Messages,false);
        tracker.feed(format!("data: {start}\n\ndata: {delta}\n\n").as_bytes());
        assert_eq!(tracker.usage,Some(expected));
        assert!(tracker.usage_final);
    } };
}
messages_fixture!(anthropic_messages_json_and_stream, json!(60), 60);
messages_fixture!(kimi_messages_json_and_stream, json!(60), 60);
messages_fixture!(deepseek_messages_json_and_stream, json!(60), 60);
messages_fixture!(zai_messages_json_and_stream, json!(60), 60);
messages_fixture!(minimax_messages_json_and_stream, json!(null), 0);
messages_fixture!(
    openrouter_messages_null_cache_json_and_stream,
    json!(null),
    0
);
messages_fixture!(ark_messages_format_contract, json!(null), 0);

#[test]
fn cache_aliases_and_reasoning_are_not_added_twice() {
    check_json_and_stream(
        json!({"usage":{"prompt_tokens":100,"completion_tokens":20,
        "prompt_tokens_details":{"cached_tokens":60},"cached_tokens":60,"prompt_cache_hit_tokens":60,
        "completion_tokens_details":{"reasoning_tokens":15}}}),
        Usage {
            input: 40,
            output: 20,
            cache_read: 60,
            ..Default::default()
        },
    );
    for (cached, input, read) in [(200, 0, 100), (-1, 100, 0)] {
        check_json_and_stream(
            json!({"usage":{"prompt_tokens":100,"completion_tokens":20,"cached_tokens":cached}}),
            Usage {
                input,
                output: 20,
                cache_read: read,
                ..Default::default()
            },
        );
    }
    let value = json!({"usage":{"prompt_tokens":100,"completion_tokens":20},
        "choices":[{"usage":{"prompt_tokens":100,"completion_tokens":20}}]});
    check_json_and_stream(
        value,
        Usage {
            input: 100,
            output: 20,
            ..Default::default()
        },
    );
}
