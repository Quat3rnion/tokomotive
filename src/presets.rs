//! Verified API-key presets from docs/provider-roster.md, groups A and B.
use crate::provider::{KeyHeader, ProviderKind};

pub struct Preset {
    pub name: &'static str,
    pub label: &'static str,
    pub kind: ProviderKind,
    pub base_url: &'static str,
    pub key_header: KeyHeader,
    pub include_usage: bool,
    pub docs: &'static str,
    pub key_help: &'static str,
}

impl Preset {
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            ProviderKind::Openai => "openai",
            ProviderKind::Anthropic => "anthropic",
        }
    }
}

macro_rules! preset {
    ($name:literal, $label:literal, $kind:ident, $url:literal, $header:ident, $usage:literal, $docs:literal, $help:literal) => {
        Preset {
            name: $name,
            label: $label,
            kind: ProviderKind::$kind,
            base_url: $url,
            key_header: KeyHeader::$header,
            include_usage: $usage,
            docs: $docs,
            key_help: $help,
        }
    };
}

// TODO: Verify base URLs before adding Together AI, Fireworks, MiniMax OpenAI,
// Groq, Perplexity, SiliconFlow, Hyperbolic, NVIDIA NIM, Volcengine Ark OpenAI,
// AWS Bedrock OpenAI, Cloudflare Workers AI, Moonshot China and Z.ai China OpenAI.
pub static PRESETS: &[Preset] = &[
    preset!(
        "openai",
        "OpenAI (generic)",
        Openai,
        "https://api.openai.com",
        Bearer,
        true,
        "https://platform.openai.com/docs/api-reference",
        "Use an OpenAI API key. A subscription is separate."
    ),
    preset!(
        "anthropic",
        "Anthropic",
        Anthropic,
        "https://api.anthropic.com",
        XApiKey,
        false,
        "https://docs.anthropic.com/en/api/overview",
        "Use an Anthropic Console API key."
    ),
    preset!(
        "deepseek",
        "DeepSeek",
        Openai,
        "https://api.deepseek.com",
        Bearer,
        false,
        "https://api-docs.deepseek.com/api/create-chat-completion/",
        "Use a DeepSeek API key. Final stream chunks report usage without include_usage."
    ),
    preset!(
        "kimi",
        "Moonshot Kimi",
        Openai,
        "https://api.moonshot.ai/v1",
        Bearer,
        true,
        "https://platform.moonshot.ai/docs/api/overview",
        "Use a Moonshot API key for the international endpoint."
    ),
    preset!(
        "zai",
        "Z.ai (GLM)",
        Openai,
        "https://api.z.ai/api/paas/v4",
        Bearer,
        true,
        "https://docs.z.ai/guides/capabilities/cache",
        "Use a Z.ai API key."
    ),
    preset!(
        "zai-coding",
        "Z.ai Coding Plan",
        Openai,
        "https://api.z.ai/api/coding/paas/v4",
        Bearer,
        true,
        "https://docs.z.ai/devpack/quick-start",
        "Use a Z.ai Coding Plan key. The Provider keeps thinking on by default."
    ),
    preset!(
        "openrouter",
        "OpenRouter",
        Openai,
        "https://openrouter.ai/api/v1",
        Bearer,
        false,
        "https://openrouter.ai/docs/cookbook/administration/usage-accounting",
        "Use an OpenRouter API key. Provider routing options pass through unchanged."
    ),
    preset!(
        "xai",
        "xAI",
        Openai,
        "https://api.x.ai/v1",
        Bearer,
        true,
        "https://docs.x.ai/developers/cost-tracking",
        "Use an xAI API key."
    ),
    preset!(
        "mistral",
        "Mistral",
        Openai,
        "https://api.mistral.ai/v1",
        Bearer,
        true,
        "https://docs.mistral.ai/resources/known-limitations",
        "Use a Mistral API key."
    ),
    preset!(
        "deepinfra",
        "DeepInfra",
        Openai,
        "https://api.deepinfra.com/v1/openai",
        Bearer,
        true,
        "https://docs.deepinfra.com/chat/overview",
        "Use a DeepInfra API key. Check usage with your model."
    ),
    preset!(
        "nebius",
        "Nebius Token Factory",
        Openai,
        "https://api.tokenfactory.nebius.com/v1",
        Bearer,
        true,
        "https://docs.studio.nebius.com/quickstart",
        "Use a Nebius Token Factory API key. Check usage with your model."
    ),
    preset!(
        "cerebras",
        "Cerebras",
        Openai,
        "https://api.cerebras.ai/v1",
        Bearer,
        true,
        "https://inference-docs.cerebras.ai/",
        "Use a Cerebras API key. Check usage with your model."
    ),
    preset!(
        "dashscope",
        "Alibaba DashScope",
        Openai,
        "https://dashscope.aliyuncs.com/compatible-mode/v1",
        Bearer,
        true,
        "https://help.aliyun.com/en/model-studio/qwen-api-via-openai-responses",
        "Use a DashScope API key for this region. Session cache needs x-dashscope-session-cache: enable."
    ),
    preset!(
        "dashscope-coding",
        "Alibaba Coding Plan (international)",
        Openai,
        "https://coding-intl.dashscope.aliyuncs.com/v1",
        Bearer,
        true,
        "https://help.aliyun.com/en/model-studio/qwen-api-via-openai-responses",
        "Use an international Alibaba Coding Plan key, not a standard DashScope key."
    ),
    preset!(
        "azure",
        "Azure OpenAI / Foundry v1",
        Openai,
        "https://<resource>.openai.azure.com/openai/v1",
        ApiKey,
        true,
        "https://learn.microsoft.com/azure/foundry/openai/api-version-lifecycle",
        "Replace <resource> with your Azure resource. Use its API key. v1 needs no api-version."
    ),
    preset!(
        "huggingface",
        "Hugging Face router",
        Openai,
        "https://router.huggingface.co/v1",
        Bearer,
        true,
        "https://huggingface.co/docs/inference-providers/index",
        "Use a Hugging Face token with inference permission. This endpoint has no embeddings."
    ),
    preset!(
        "local",
        "Local server",
        Openai,
        "",
        Bearer,
        false,
        "",
        "Enter your server URL. Leave the key empty if the server needs no key. Add Models separately."
    ),
    preset!(
        "kimi-messages",
        "Moonshot Kimi (Messages)",
        Anthropic,
        "https://api.moonshot.ai/anthropic",
        Bearer,
        false,
        "https://platform.moonshot.ai/docs/api/messages",
        "Use a Moonshot API key for the international endpoint."
    ),
    preset!(
        "deepseek-messages",
        "DeepSeek (Messages)",
        Anthropic,
        "https://api.deepseek.com/anthropic",
        XApiKey,
        false,
        "https://api-docs.deepseek.com/guides/anthropic_api",
        "Use a DeepSeek API key. Cache writes are not supported."
    ),
    preset!(
        "zai-messages",
        "Z.ai (Messages)",
        Anthropic,
        "https://api.z.ai/api/anthropic",
        Bearer,
        false,
        "https://docs.z.ai/guides/anthropic",
        "Use a Z.ai API key for the international endpoint."
    ),
    preset!(
        "zai-china-messages",
        "Z.ai China (Messages)",
        Anthropic,
        "https://open.bigmodel.cn/api/anthropic",
        Bearer,
        false,
        "https://docs.z.ai/guides/anthropic",
        "Use a key for the China endpoint."
    ),
    preset!(
        "minimax-messages",
        "MiniMax (Messages)",
        Anthropic,
        "https://api.minimax.io/anthropic",
        Bearer,
        false,
        "https://platform.minimax.io/docs/api-reference/text-anthropic-api",
        "Use a MiniMax API key. Check cache support with your model."
    ),
    preset!(
        "openrouter-messages",
        "OpenRouter (Messages)",
        Anthropic,
        "https://openrouter.ai/api/v1/messages",
        Bearer,
        false,
        "https://openrouter.ai/docs/api/api-reference/anthropic-messages/create-a-message",
        "Use an OpenRouter API key. Messages support depends on the upstream model."
    ),
    preset!(
        "ark-messages",
        "Volcengine Ark (Messages)",
        Anthropic,
        "https://ark.cn-beijing.volces.com/api/compatible/v1/messages",
        Bearer,
        false,
        "https://www.volcengine.com/docs/ark/agent-model-invocation-practice",
        "Use an Ark API key. Only the endpoint and authentication are verified."
    ),
];

pub fn find(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|preset| preset.name == name)
}
