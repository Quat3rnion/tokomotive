use tokomotive::presets::{PRESETS, find};
use tokomotive::provider::upstream_url;

#[test]
fn preset_urls_join_the_protocol_path_once() {
    for (name, path, expected) in [
        (
            "openai",
            "/v1/chat/completions",
            "https://api.openai.com/v1/chat/completions",
        ),
        (
            "kimi",
            "/v1/chat/completions",
            "https://api.moonshot.ai/v1/chat/completions",
        ),
        (
            "zai",
            "/v1/chat/completions",
            "https://api.z.ai/api/paas/v4/chat/completions",
        ),
        (
            "zai-coding",
            "/v1/chat/completions",
            "https://api.z.ai/api/coding/paas/v4/chat/completions",
        ),
        (
            "deepinfra",
            "/v1/chat/completions",
            "https://api.deepinfra.com/v1/openai/chat/completions",
        ),
        (
            "dashscope",
            "/v1/responses",
            "https://dashscope.aliyuncs.com/compatible-mode/v1/responses",
        ),
        (
            "azure",
            "/v1/responses",
            "https://<resource>.openai.azure.com/openai/v1/responses",
        ),
        (
            "kimi-messages",
            "/v1/messages",
            "https://api.moonshot.ai/anthropic/v1/messages",
        ),
        (
            "zai-china-messages",
            "/v1/messages",
            "https://open.bigmodel.cn/api/anthropic/v1/messages",
        ),
        (
            "openrouter-messages",
            "/v1/messages",
            "https://openrouter.ai/api/v1/messages",
        ),
        (
            "ark-messages",
            "/v1/messages",
            "https://ark.cn-beijing.volces.com/api/compatible/v1/messages",
        ),
        (
            "openrouter-messages",
            "/v1/models",
            "https://openrouter.ai/api/v1/models",
        ),
    ] {
        assert_eq!(
            upstream_url(find(name).unwrap().base_url, path),
            expected,
            "{name}"
        );
    }
    assert_eq!(
        upstream_url("http://localhost:11434/v1/", "/v1/chat/completions?test=1"),
        "http://localhost:11434/v1/chat/completions?test=1"
    );
    assert_eq!(
        upstream_url("https://custom.example/prefix", "/v1/messages"),
        "https://custom.example/prefix/v1/messages"
    );
}

#[test]
fn roster_has_all_verified_region_and_protocol_variants() {
    let names: Vec<_> = PRESETS.iter().map(|p| p.name).collect();
    assert_eq!(
        names,
        vec![
            "openai",
            "anthropic",
            "gemini",
            "deepseek",
            "kimi",
            "zai",
            "zai-coding",
            "openrouter",
            "xai",
            "mistral",
            "deepinfra",
            "nebius",
            "cerebras",
            "dashscope",
            "dashscope-coding",
            "azure",
            "huggingface",
            "local",
            "kimi-messages",
            "deepseek-messages",
            "zai-messages",
            "zai-china-messages",
            "minimax-messages",
            "openrouter-messages",
            "ark-messages",
            "bedrock-native",
            "cohere",
            "gemini-cli"
        ]
    );
    assert!(!find("deepseek").unwrap().include_usage);
    assert!(!find("openrouter").unwrap().include_usage);
    assert!(find("openai").unwrap().include_usage);
    assert!(find("local").unwrap().base_url.is_empty());
    for preset in PRESETS {
        assert!(!preset.key_help.is_empty());
        assert!(preset.name == "local" || preset.docs.starts_with("https://"));
    }
}
