//! The provider types of "Add a Provider": every verified preset, every
//! sign-in type, the native API formats, custom entries and the models.dev
//! providers, with search and ranking for the picker.
use crate::presets::PresetChoice;
use crate::provider::{KeyHeader, ProviderAuth, ProviderKind};
use serde::Serialize;

/// Which fields the guided form shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    ApiKey,
    Subscription,
    Bedrock,
    Vertex,
    VertexServiceAccount,
    Entra,
    HuggingFace,
    Local,
    Custom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Verified,
    /// A sign-in flow (subscription or cloud OAuth). Not a verified preset:
    /// unofficial ones carry their own warning.
    SignIn,
    ModelsDev,
    Custom,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Self::Verified => "Verified",
            Self::SignIn => "Sign-in",
            Self::ModelsDev => "from models.dev",
            Self::Custom => "Custom",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ProviderType {
    pub id: String,
    pub label: String,
    /// "API key", "Subscription sign-in", "Device code", "Self-hosted" ...
    pub tagline: &'static str,
    pub source: Source,
    pub category: Category,
    #[serde(skip)]
    pub auth: ProviderAuth,
    #[serde(skip)]
    pub kind: ProviderKind,
    /// "OpenAI", "Anthropic", "Gemini", "AWS Bedrock", "Cohere" or
    /// "OpenAI or Anthropic".
    pub format: &'static str,
    /// Copilot and Kimi Code serve OpenAI and Anthropic routes.
    #[serde(skip)]
    pub kind_choice: bool,
    #[serde(skip)]
    pub base_url: String,
    pub host: String,
    #[serde(skip)]
    pub key_header: KeyHeader,
    #[serde(skip)]
    pub include_usage: bool,
    #[serde(skip)]
    pub docs: String,
    #[serde(skip)]
    pub key_help: String,
    /// A short explanation for sign-in types.
    #[serde(skip)]
    pub about: &'static str,
    /// The risk note and ADR file name for sign-ins that need one.
    #[serde(skip)]
    pub risk: Option<(&'static str, &'static str)>,
    pub unofficial: bool,
    /// The sign-in starts with a device code.
    #[serde(skip)]
    pub device: bool,
    /// Stored in `providers.preset` (a verified or models.dev preset ID).
    #[serde(skip)]
    pub preset: bool,
    #[serde(skip)]
    pub aliases: &'static [&'static str],
}

impl ProviderType {
    pub fn auth_name(&self) -> &'static str {
        self.auth.as_str()
    }
    pub fn kind_name(&self) -> &'static str {
        kind_name(self.kind)
    }
    pub fn source_label(&self) -> &'static str {
        self.source.label()
    }
    pub fn category_name(&self) -> &'static str {
        match self.category {
            Category::ApiKey => "api_key",
            Category::Subscription => "subscription",
            Category::Bedrock => "bedrock",
            Category::Vertex => "vertex",
            Category::VertexServiceAccount => "vertex_sa",
            Category::Entra => "entra",
            Category::HuggingFace => "hugging_face",
            Category::Local => "local",
            Category::Custom => "custom",
        }
    }
    /// A verified base URL without placeholders: read-only until Edit.
    pub fn locked(&self) -> bool {
        self.source == Source::Verified
            && self.category == Category::ApiKey
            && !self.base_url.is_empty()
            && !self.base_url.contains('<')
    }
    /// The base name for a new Provider of this type.
    pub fn name_base(&self) -> String {
        let base = match self.id.as_str() {
            "custom-openai" | "custom-anthropic" => "custom",
            "github-copilot" => "copilot",
            "bedrock-native" => "bedrock",
            "hf-oauth" => "huggingface",
            "vertex-sa" => "vertex",
            id => id,
        };
        let clean: String = base
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .take(48)
            .collect();
        if clean.trim_matches('-').is_empty() {
            "provider".into()
        } else {
            clean
        }
    }
    /// "OpenAI format · Bearer key · api.deepseek.com".
    pub fn summary(&self) -> String {
        let format = match self.format {
            "OpenAI or Anthropic" => "OpenAI format".to_owned(),
            f => format!("{f} format"),
        };
        let auth = match self.category {
            Category::ApiKey | Category::Local | Category::Custom => {
                header_summary(self.key_header).to_owned()
            }
            Category::Bedrock => "AWS SigV4".into(),
            Category::Vertex => "Google OAuth sign-in".into(),
            Category::VertexServiceAccount => "Service-account key".into(),
            Category::Entra => "Entra ID device code".into(),
            Category::HuggingFace => "Hugging Face device code".into(),
            Category::Subscription if self.device => "Device code sign-in".into(),
            Category::Subscription => "Subscription sign-in".into(),
        };
        let host = if self.host.is_empty() {
            "no base URL yet".to_owned()
        } else {
            self.host.clone()
        };
        format!("{format} · {auth} · {host}")
    }
    fn group(&self) -> u8 {
        match (self.source, self.category) {
            (Source::Verified, Category::ApiKey | Category::Local | Category::Bedrock) => 0,
            (Source::Verified | Source::SignIn, _) => 1,
            (Source::Custom, _) => 2,
            (Source::ModelsDev, _) => 3,
        }
    }
}

pub fn kind_name(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Openai => "openai",
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Gemini => "gemini",
        ProviderKind::Bedrock => "bedrock",
        ProviderKind::Cohere => "cohere",
    }
}

fn format_label(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Openai => "OpenAI",
        ProviderKind::Anthropic => "Anthropic",
        ProviderKind::Gemini => "Gemini",
        ProviderKind::Bedrock => "AWS Bedrock",
        ProviderKind::Cohere => "Cohere",
    }
}

pub fn header_summary(h: KeyHeader) -> &'static str {
    match h {
        KeyHeader::Bearer => "Bearer key",
        KeyHeader::XApiKey => "x-api-key key",
        KeyHeader::ApiKey => "api-key key (Azure)",
        KeyHeader::XGoogApiKey => "x-goog-api-key key",
    }
}

pub fn host_of(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.host_str().map(|h| match u.port() {
                Some(p) => format!("{h}:{p}"),
                None => h.to_owned(),
            })
        })
        .unwrap_or_default()
}

/// The search terms of the verified presets, by ID.
fn aliases(id: &str) -> &'static [&'static str] {
    match id {
        "openai" => &["gpt", "o3", "gpt-4o"],
        "anthropic" => &["claude", "claude api"],
        "gemini" => &["google", "ai studio", "gemini api"],
        "kimi" | "kimi-messages" => &["moonshot", "kimi"],
        "zai" | "zai-coding" | "zai-messages" | "zai-china-messages" => {
            &["glm", "zhipu", "bigmodel", "z.ai"]
        }
        "xai" => &["grok"],
        "mistral" => &["codestral", "le chat"],
        "dashscope" | "dashscope-coding" => &["qwen", "alibaba", "aliyun", "bailian"],
        "azure" => &["microsoft", "foundry", "azure openai"],
        "huggingface" => &["hf", "hugging face"],
        "local" => &[
            "ollama",
            "lm studio",
            "lmstudio",
            "vllm",
            "llama.cpp",
            "self-hosted",
        ],
        "minimax-messages" => &["minimax"],
        "ark-messages" => &["volcengine", "doubao", "bytedance"],
        "bedrock-native" => &["aws", "amazon", "sigv4", "bedrock"],
        "cohere" => &["command"],
        "gemini-cli" => &["google", "code assist", "gemini cli"],
        "deepseek-messages" => &["deepseek"],
        "openrouter-messages" => &["openrouter"],
        _ => &[],
    }
}

const RISK_UNOFFICIAL: &str = "This unofficial sign-in uses private routes. They can change without notice. Use can break the service terms.";

struct SignIn {
    id: &'static str,
    label: &'static str,
    tagline: &'static str,
    category: Category,
    auth: ProviderAuth,
    kind: ProviderKind,
    kind_choice: bool,
    base_url: &'static str,
    docs: &'static str,
    about: &'static str,
    risk: Option<(&'static str, &'static str)>,
    unofficial: bool,
    device: bool,
    aliases: &'static [&'static str],
}

const SIGN_INS: &[SignIn] = &[
    SignIn {
        id: "chatgpt",
        label: "ChatGPT subscription",
        tagline: "Subscription sign-in",
        category: Category::Subscription,
        auth: ProviderAuth::Chatgpt,
        kind: ProviderKind::Openai,
        kind_choice: false,
        base_url: "https://api.openai.com",
        docs: "",
        about: "Uses your ChatGPT plan through token sharing. This is the recommended ChatGPT sign-in. You open the sign-in page, then paste the final address back here.",
        risk: Some((
            "Read the risks of subscription sign-in before you connect an account.",
            "0003-subscription-providers.md",
        )),
        unofficial: false,
        device: false,
        aliases: &["openai", "plus", "pro", "subscription", "gpt"],
    },
    SignIn {
        id: "codex",
        label: "ChatGPT via Codex (legacy)",
        tagline: "Device code",
        category: Category::Subscription,
        auth: ProviderAuth::Codex,
        kind: ProviderKind::Openai,
        kind_choice: false,
        base_url: "https://chatgpt.com/backend-api/codex",
        docs: "",
        about: "Uses the private Codex route of a ChatGPT plan. You sign in with a device code. ChatGPT subscription is recommended instead.",
        risk: Some((
            "Legacy Codex OAuth uses a private ChatGPT route. It can change without notice and can break the service terms.",
            "0004-legacy-codex-oauth.md",
        )),
        unofficial: true,
        device: true,
        aliases: &["openai", "chatgpt", "legacy"],
    },
    SignIn {
        id: "claude",
        label: "Claude Pro/Max",
        tagline: "Subscription sign-in",
        category: Category::Subscription,
        auth: ProviderAuth::Claude,
        kind: ProviderKind::Anthropic,
        kind_choice: false,
        base_url: "https://api.anthropic.com",
        docs: "",
        about: "Uses your Claude Pro or Max plan. You open the sign-in page, then paste the code back here.",
        risk: Some((
            "Claude OAuth impersonates Claude Code and can cause account restriction.",
            "0003-subscription-providers.md",
        )),
        unofficial: true,
        device: false,
        aliases: &["anthropic", "claude code", "subscription", "max"],
    },
    SignIn {
        id: "github-copilot",
        label: "GitHub Copilot",
        tagline: "Device code",
        category: Category::Subscription,
        auth: ProviderAuth::GithubCopilot,
        kind: ProviderKind::Openai,
        kind_choice: true,
        base_url: "https://api.githubcopilot.com",
        docs: "",
        about: "Uses your GitHub Copilot plan. You sign in with a device code on github.com.",
        risk: Some((RISK_UNOFFICIAL, "0006-github-copilot.md")),
        unofficial: true,
        device: true,
        aliases: &["copilot", "github", "gh"],
    },
    SignIn {
        id: "qwen-code",
        label: "Qwen Code",
        tagline: "Device code",
        category: Category::Subscription,
        auth: ProviderAuth::QwenCode,
        kind: ProviderKind::Openai,
        kind_choice: false,
        base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        docs: "",
        about: "Uses a Qwen Code account. You sign in with a device code on chat.qwen.ai.",
        risk: Some((RISK_UNOFFICIAL, "0007-qwen-code.md")),
        unofficial: true,
        device: true,
        aliases: &["qwen", "alibaba"],
    },
    SignIn {
        id: "kimi-code",
        label: "Kimi Code",
        tagline: "Device code",
        category: Category::Subscription,
        auth: ProviderAuth::KimiCode,
        kind: ProviderKind::Openai,
        kind_choice: true,
        base_url: "https://api.kimi.com/coding/v1",
        docs: "",
        about: "Uses a Kimi Code plan. You sign in with a device code.",
        risk: Some((RISK_UNOFFICIAL, "0008-kimi-code.md")),
        unofficial: true,
        device: true,
        aliases: &["kimi", "moonshot"],
    },
    SignIn {
        id: "hf-oauth",
        label: "Hugging Face OAuth",
        tagline: "Device code",
        category: Category::HuggingFace,
        auth: ProviderAuth::HuggingFace,
        kind: ProviderKind::Openai,
        kind_choice: false,
        base_url: "https://router.huggingface.co",
        docs: "https://huggingface.co/docs/hub/oauth#creating-an-oauth-app",
        about: "Signs in to the Hugging Face router with your own OAuth app. Cost follows the Rate Card.",
        risk: None,
        unofficial: false,
        device: true,
        aliases: &["hugging face", "hf", "huggingface"],
    },
    SignIn {
        id: "entra",
        label: "Azure Entra ID",
        tagline: "Device code",
        category: Category::Entra,
        auth: ProviderAuth::Entra,
        kind: ProviderKind::Openai,
        kind_choice: false,
        base_url: "",
        docs: "https://learn.microsoft.com/entra/identity-platform/v2-oauth2-device-code",
        about: "Signs in to Azure OpenAI with a public client in your Entra tenant. Cost follows the Rate Card.",
        risk: None,
        unofficial: false,
        device: true,
        aliases: &["azure", "microsoft", "aad", "azure openai"],
    },
    SignIn {
        id: "vertex",
        label: "Google Vertex AI (user sign-in)",
        tagline: "Google sign-in",
        category: Category::Vertex,
        auth: ProviderAuth::GoogleVertex,
        kind: ProviderKind::Gemini,
        kind_choice: false,
        base_url: "https://aiplatform.googleapis.com",
        docs: "https://cloud.google.com/vertex-ai/generative-ai/docs/start/api-keys",
        about: "Routes Gemini and Anthropic models through Vertex AI. Use your own Google Desktop OAuth client. You open the sign-in page, then paste the final address back here.",
        risk: None,
        unofficial: false,
        device: false,
        aliases: &["google", "gcp", "google cloud", "vertex ai"],
    },
    SignIn {
        id: "vertex-sa",
        label: "Google Vertex AI (service account)",
        tagline: "Service account",
        category: Category::VertexServiceAccount,
        auth: ProviderAuth::GoogleVertex,
        kind: ProviderKind::Gemini,
        kind_choice: false,
        base_url: "https://aiplatform.googleapis.com",
        docs: "https://cloud.google.com/iam/docs/keys-create-delete",
        about: "Routes Gemini and Anthropic models through Vertex AI with a service-account JSON key. No sign-in is necessary.",
        risk: None,
        unofficial: false,
        device: false,
        aliases: &["google", "gcp", "google cloud", "vertex ai", "json key"],
    },
];

/// The empty-query choices, in this order.
pub const POPULAR: &[&str] = &[
    "openai",
    "anthropic",
    "openrouter",
    "gemini",
    "deepseek",
    "kimi",
    "zai",
    "chatgpt",
    "claude",
    "github-copilot",
    "local",
    "custom-openai",
];

/// The maximum number of search results.
pub const LIMIT: usize = 20;

fn from_preset(p: &PresetChoice, verified: bool) -> ProviderType {
    let category = match p.name.as_str() {
        _ if !verified => Category::ApiKey,
        "bedrock-native" => Category::Bedrock,
        "local" => Category::Local,
        "gemini-cli" => Category::Subscription,
        _ => Category::ApiKey,
    };
    let (label, tagline) = match category {
        Category::Local => (p.label.clone(), "Self-hosted"),
        Category::Bedrock => (p.label.clone(), "AWS credentials"),
        Category::Subscription => (p.label.clone(), "Subscription sign-in"),
        _ if verified => (p.label.clone(), "API key"),
        _ => (
            p.label.trim_end_matches(" (from models.dev)").to_owned(),
            "API key",
        ),
    };
    let gemini_cli = p.name == "gemini-cli";
    ProviderType {
        id: p.name.clone(),
        label,
        tagline,
        source: if verified {
            Source::Verified
        } else {
            Source::ModelsDev
        },
        category,
        auth: if gemini_cli {
            ProviderAuth::GeminiCli
        } else {
            ProviderAuth::ApiKey
        },
        kind: p.kind,
        format: format_label(p.kind),
        kind_choice: false,
        host: host_of(&p.base_url),
        base_url: p.base_url.clone(),
        key_header: p.key_header,
        include_usage: p.include_usage,
        docs: p.docs.clone(),
        key_help: p.key_help.clone(),
        about: if gemini_cli {
            "Uses a Gemini Code Assist account through unofficial routes. You open the sign-in page, then paste the code back here. Cost is 0."
        } else {
            ""
        },
        risk: gemini_cli.then_some((RISK_UNOFFICIAL, "0009-gemini-cli.md")),
        unofficial: gemini_cli,
        device: false,
        preset: true,
        aliases: if verified { aliases(&p.name) } else { &[] },
    }
}

fn custom(id: &str, label: &str, kind: ProviderKind) -> ProviderType {
    ProviderType {
        id: id.into(),
        label: label.into(),
        tagline: "Any compatible API",
        source: Source::Custom,
        category: Category::Custom,
        auth: ProviderAuth::ApiKey,
        kind,
        format: format_label(kind),
        kind_choice: true,
        base_url: String::new(),
        host: String::new(),
        key_header: KeyHeader::for_kind(kind),
        include_usage: kind == ProviderKind::Openai,
        docs: String::new(),
        key_help: "Use the API key of your Provider, or leave it empty if the server needs no key."
            .into(),
        about: "",
        risk: None,
        unofficial: false,
        device: false,
        preset: false,
        aliases: if kind == ProviderKind::Openai {
            &["custom", "openai compatible", "self-hosted", "proxy"]
        } else {
            &["custom", "anthropic compatible", "messages"]
        },
    }
}

/// Every provider type: verified presets, sign-ins, custom entries, then
/// the models.dev presets. IDs are unique; earlier entries win.
pub fn catalog(presets: &[PresetChoice]) -> Vec<ProviderType> {
    let mut out: Vec<ProviderType> = Vec::with_capacity(presets.len() + SIGN_INS.len() + 2);
    let verified = |p: &PresetChoice| crate::presets::find(&p.name).is_some();
    out.extend(
        presets
            .iter()
            .filter(|p| verified(p))
            .map(|p| from_preset(p, true)),
    );
    for s in SIGN_INS {
        out.push(ProviderType {
            id: s.id.into(),
            label: s.label.into(),
            tagline: s.tagline,
            source: Source::SignIn,
            category: s.category,
            auth: s.auth,
            kind: s.kind,
            format: if s.kind_choice {
                "OpenAI or Anthropic"
            } else {
                format_label(s.kind)
            },
            kind_choice: s.kind_choice,
            base_url: s.base_url.into(),
            host: host_of(s.base_url),
            key_header: KeyHeader::Bearer,
            include_usage: true,
            docs: s.docs.into(),
            key_help: String::new(),
            about: s.about,
            risk: s.risk,
            unofficial: s.unofficial,
            device: s.device,
            preset: false,
            aliases: s.aliases,
        });
    }
    out.push(custom(
        "custom-openai",
        "Custom OpenAI-compatible",
        ProviderKind::Openai,
    ));
    out.push(custom(
        "custom-anthropic",
        "Custom Anthropic-compatible",
        ProviderKind::Anthropic,
    ));
    for p in presets.iter().filter(|p| !verified(p)) {
        if out.iter().any(|t| t.id == p.name) {
            continue;
        }
        out.push(from_preset(p, false));
    }
    out
}

/// 0 exact, 1 prefix, 2 contains; None for no match.
fn tier(t: &ProviderType, q: &str) -> Option<u8> {
    let id = t.id.to_lowercase();
    let label = t.label.to_lowercase();
    let host = t.host.to_lowercase();
    let mut fields: Vec<&str> = vec![&id, &label];
    fields.extend(t.aliases.iter().copied());
    if fields.contains(&q) || host == q {
        return Some(0);
    }
    let words = |s: &str| {
        s.split(|c: char| !c.is_alphanumeric())
            .any(|w| !w.is_empty() && w.starts_with(q))
    };
    if fields.iter().any(|f| f.starts_with(q)) || words(&label) {
        return Some(1);
    }
    if fields.iter().any(|f| f.contains(q)) || host.contains(q) {
        return Some(2);
    }
    // Every word of the query somewhere in the entry.
    let hay = format!("{id} {label} {host} {}", t.aliases.join(" "));
    let terms: Vec<&str> = q.split_whitespace().collect();
    (terms.len() > 1 && terms.iter().all(|w| hay.contains(w))).then_some(2)
}

/// Sort key: (not exact or prefix, tier, group, popular index, catalog index).
type Rank = (u8, u8, u8, usize, usize);

/// Ranked matches: exact and prefix matches first, then verified presets,
/// sign-ins, custom entries and models.dev entries. An empty query gives the
/// popular choices.
pub fn search<'a>(types: &'a [ProviderType], q: &str) -> Vec<&'a ProviderType> {
    let q = q.trim().to_lowercase();
    if q.is_empty() {
        return POPULAR
            .iter()
            .filter_map(|id| types.iter().find(|t| t.id == *id))
            .collect();
    }
    let popular = |t: &ProviderType| {
        POPULAR
            .iter()
            .position(|id| *id == t.id)
            .unwrap_or(POPULAR.len())
    };
    // Exact and prefix matches come first (exact before prefix), then the
    // source group, the popular order and the catalog order.
    let mut hits: Vec<(Rank, &ProviderType)> = types
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            tier(t, &q).map(|tier| ((u8::from(tier > 1), tier, t.group(), popular(t), i), t))
        })
        .collect();
    hits.sort_by_key(|h| h.0);
    hits.into_iter().take(LIMIT).map(|h| h.1).collect()
}

/// The groups of the no-JavaScript select.
pub fn groups(types: &[ProviderType]) -> Vec<(&'static str, Vec<&ProviderType>)> {
    let pick = |f: &dyn Fn(&ProviderType) -> bool| types.iter().filter(|t| f(t)).collect();
    vec![
        (
            "Verified API-key Providers",
            pick(&|t| t.source == Source::Verified && t.category == Category::ApiKey),
        ),
        (
            "Subscriptions and sign-in",
            pick(&|t| {
                matches!(t.source, Source::Verified | Source::SignIn)
                    && !matches!(
                        t.category,
                        Category::ApiKey | Category::Local | Category::Bedrock
                    )
            }),
        ),
        (
            "Cloud, local and custom",
            pick(&|t| {
                matches!(t.category, Category::Local | Category::Bedrock)
                    || t.source == Source::Custom
            }),
        ),
        (
            "From models.dev (not verified)",
            pick(&|t| t.source == Source::ModelsDev),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types() -> Vec<ProviderType> {
        let presets: Vec<PresetChoice> = crate::presets::PRESETS
            .iter()
            .map(|p| PresetChoice {
                name: p.name.into(),
                label: p.label.into(),
                kind: p.kind,
                base_url: p.base_url.into(),
                key_header: p.key_header,
                include_usage: p.include_usage,
                docs: p.docs.into(),
                key_help: p.key_help.into(),
            })
            .collect();
        catalog(&presets)
    }

    #[test]
    fn ids_are_unique_and_popular_ids_exist() {
        let t = types();
        let mut ids: Vec<_> = t.iter().map(|t| t.id.as_str()).collect();
        ids.sort_unstable();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n);
        assert_eq!(search(&t, "").len(), POPULAR.len());
    }

    #[test]
    fn summary_names_format_key_and_host() {
        let t = types();
        let ds = t.iter().find(|t| t.id == "deepseek").unwrap();
        assert_eq!(
            ds.summary(),
            "OpenAI format · Bearer key · api.deepseek.com"
        );
        let cp = t.iter().find(|t| t.id == "github-copilot").unwrap();
        assert_eq!(
            cp.summary(),
            "OpenAI format · Device code sign-in · api.githubcopilot.com"
        );
    }
}
