//! Subscription credentials. Secrets are encrypted in SQLite; attempts stay in memory.
use crate::{
    db::{self, Db},
    provider::ProviderAuth,
    settings::SecretKey,
};
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::Mutex;

pub const CLAUDE_CLIENT: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const CLAUDE_CALLBACK: &str = "https://platform.claude.com/oauth/code/callback";
pub const CLAUDE_SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
pub const CLAUDE_BETAS: &str = "oauth-2025-04-20,interleaved-thinking-2025-05-14";
pub const CLAUDE_UA: &str = "claude-cli/2.1.280 (external, cli)";
pub const CLAUDE_IDENTITY: &str = "You are a Claude agent, built on Anthropic's Claude Agent SDK.";
const CODEX_CLIENT: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const SHARING_SCOPE: &str = "chatgpt.tokens.use.direct";

mod code_assist;
mod config;
mod service_account;
pub use config::{ClientOptions, FlowConfig, FlowType, InferenceBase, RefreshRule, TokenEncoding};

/// Stable storage identity, not a protocol enum. The registry supplies the data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Flow(&'static str);
#[allow(non_upper_case_globals)]
impl Flow {
    pub const Chatgpt: Self = Self("chatgpt");
    pub const Claude: Self = Self("claude");
    pub const Codex: Self = Self("codex");
    pub const HuggingFace: Self = Self("hugging_face");
    pub const Entra: Self = Self("entra");
    pub const GoogleVertex: Self = Self("google_vertex");
    pub const OpenRouter: Self = Self("openrouter");
    pub const GithubCopilot: Self = Self("github_copilot");
    pub const QwenCode: Self = Self("qwen_code");
    pub const KimiCode: Self = Self("kimi_code");
    pub const GeminiCli: Self = Self("gemini_cli");
    pub fn as_str(self) -> &'static str {
        self.0
    }
    pub fn from_auth(auth: ProviderAuth) -> Option<Self> {
        match auth {
            ProviderAuth::ApiKey => None,
            ProviderAuth::Chatgpt => Some(Self::Chatgpt),
            ProviderAuth::Codex => Some(Self::Codex),
            ProviderAuth::Claude => Some(Self::Claude),
            ProviderAuth::HuggingFace => Some(Self::HuggingFace),
            ProviderAuth::Entra => Some(Self::Entra),
            ProviderAuth::GoogleVertex => Some(Self::GoogleVertex),
            ProviderAuth::GithubCopilot => Some(Self::GithubCopilot),
            ProviderAuth::QwenCode => Some(Self::QwenCode),
            ProviderAuth::KimiCode => Some(Self::KimiCode),
            ProviderAuth::GeminiCli => Some(Self::GeminiCli),
        }
    }
    pub fn redirect(self) -> &'static str {
        match self {
            Self::Claude => CLAUDE_CALLBACK,
            Self::Chatgpt => "http://localhost:1455/auth/callback",
            Self::Codex => "http://127.0.0.1:1455/auth/callback",
            Self::GeminiCli => "https://codeassist.google.com/authcode",
            _ => "http://localhost/auth/callback",
        }
    }
    pub fn device(self) -> bool {
        matches!(
            self,
            Self::Codex
                | Self::HuggingFace
                | Self::Entra
                | Self::GithubCopilot
                | Self::QwenCode
                | Self::KimiCode
        )
    }
    pub fn paste_supported(self) -> bool {
        !matches!(self, Self::GithubCopilot | Self::QwenCode | Self::KimiCode)
    }
}

/// Injectable only at process construction, never from an HTTP form or Provider URL.
#[derive(Clone)]
pub struct OAuthEndpoints {
    pub issuer: String,
    pub openai_resource: String,
    pub claude_authorize: String,
    pub claude_token: String,
    pub claude_resource: String,
    pub codex_usage: String,
    pub codex_resource: String,
    pub hf_device: String,
    pub hf_authorize: String,
    pub hf_token: String,
    pub hf_resource: String,
    pub entra_issuer: String,
    pub entra_resource: Option<String>,
    pub google_authorize: String,
    pub google_token: String,
    pub openrouter_authorize: String,
    pub openrouter_token: String,
    pub copilot_device: String,
    pub copilot_token: String,
    pub copilot_user: String,
    pub copilot_resource: String,
    pub qwen_device: String,
    pub qwen_token: String,
    pub qwen_resource: String,
    pub kimi_device: String,
    pub kimi_token: String,
    pub kimi_resource: String,
    pub code_assist_resource: String,
    pub vertex_resource: Option<String>,
}
impl Default for OAuthEndpoints {
    fn default() -> Self {
        Self {
            issuer: "https://auth.openai.com".into(),
            openai_resource: "https://api.openai.com/v1".into(),
            claude_authorize: "https://claude.ai/oauth/authorize".into(),
            claude_token: "https://platform.claude.com/v1/oauth/token".into(),
            claude_resource: "https://api.anthropic.com".into(),
            codex_usage: "https://chatgpt.com/backend-api/wham/usage".into(),
            codex_resource: "https://chatgpt.com/backend-api/codex".into(),
            hf_device: "https://huggingface.co/oauth/device".into(),
            hf_authorize: "https://huggingface.co/oauth/authorize".into(),
            hf_token: "https://huggingface.co/oauth/token".into(),
            hf_resource: "https://router.huggingface.co".into(),
            entra_issuer: "https://login.microsoftonline.com".into(),
            entra_resource: None,
            google_authorize: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            google_token: "https://oauth2.googleapis.com/token".into(),
            openrouter_authorize: "https://openrouter.ai/auth".into(),
            openrouter_token: "https://openrouter.ai/api/v1/auth/keys".into(),
            copilot_device: "https://github.com/login/device/code".into(),
            copilot_token: "https://github.com/login/oauth/access_token".into(),
            copilot_user: "https://api.github.com/copilot_internal/user".into(),
            copilot_resource: "https://api.githubcopilot.com".into(),
            qwen_device: "https://chat.qwen.ai/api/v1/oauth2/device/code".into(),
            qwen_token: "https://chat.qwen.ai/api/v1/oauth2/token".into(),
            qwen_resource: "https://dashscope.aliyuncs.com/compatible-mode/v1".into(),
            kimi_device: "https://auth.kimi.com/api/oauth/device_authorization".into(),
            kimi_token: "https://auth.kimi.com/api/oauth/token".into(),
            kimi_resource: "https://api.kimi.com/coding/v1".into(),
            code_assist_resource: "https://cloudcode-pa.googleapis.com".into(),
            vertex_resource: None,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    service_account: Option<service_account::ServiceAccount>,
    host_id: String,
    client_id: Option<String>,
    credential: Option<Credential>,
    last_error: Option<String>,
    #[serde(default)]
    options: ClientOptions,
}
#[derive(Serialize, Deserialize)]
struct Credential {
    access_token: String,
    refresh_token: String,
    scope: String,
    expires_at: i64,
    last_refresh: Option<i64>,
    account: Option<String>,
    plan: Option<String>,
    account_id: Option<String>,
    #[serde(default)]
    inference_base: Option<String>,
}
#[derive(Default, Serialize)]
pub struct Status {
    pub connected: bool,
    pub account: Option<String>,
    pub plan: Option<String>,
    pub expires_at: Option<i64>,
    pub last_refresh: Option<i64>,
    pub last_error: Option<String>,
}
#[derive(Clone, Serialize)]
pub struct Started {
    pub authorize_url: String,
    pub expires_at: i64,
}
/// Only the public user code is serializable. The device credential stays private.
#[derive(Clone, Serialize)]
pub struct DeviceStatus {
    pub attempt: String,
    pub user_code: String,
    pub verification_url: String,
    pub expires_at: i64,
    pub remaining_secs: i64,
    pub interval: i64,
    pub state: &'static str,
}
struct DeviceAttempt {
    status: DeviceStatus,
    device_auth_id: String,
    next_poll: i64,
    seen_at: i64,
    verifier: Option<String>,
}
#[derive(Clone)]
pub struct UsageWindow {
    pub label: String,
    pub used_percent: f64,
    pub resets_at: Option<i64>,
}
#[derive(Clone)]
pub struct PlanUsage {
    pub windows: Vec<UsageWindow>,
}
struct Attempt {
    started: Started,
    verifier: String,
    state: String,
    nonce: String,
    client_id: Option<String>,
    redirect: &'static str,
}
#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
    #[serde(flatten)]
    extra: HashMap<String, Value>,
}
type Identity = (String, Flow);
type FlightLocks = Mutex<HashMap<Identity, Arc<Mutex<()>>>>;
pub struct OAuthManager {
    db: Db,
    key: SecretKey,
    endpoints: OAuthEndpoints,
    http: reqwest::Client,
    locks: FlightLocks,
    pending: Mutex<HashMap<Identity, Attempt>>,
    devices: Mutex<HashMap<Identity, DeviceAttempt>>,
    usage_cache: Mutex<HashMap<String, (i64, Option<PlanUsage>)>>,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    configs: HashMap<Flow, FlowConfig>,
}
fn random(bytes: usize) -> String {
    let mut v = vec![0; bytes];
    rand::rng().fill_bytes(&mut v);
    URL_SAFE_NO_PAD.encode(v)
}
fn uuid() -> String {
    let mut bytes = [0; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let h = hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}
pub(crate) fn session_id() -> String {
    uuid()
}
pub fn kimi_headers(device: &str) -> Vec<(String, String)> {
    [
        ("user-agent", "Tokomotive/0.1.0".to_owned()),
        ("x-msh-platform", "tokomotive".into()),
        ("x-msh-version", env!("CARGO_PKG_VERSION").into()),
        ("x-msh-device-name", "Tokomotive server".into()),
        ("x-msh-device-model", std::env::consts::ARCH.into()),
        ("x-msh-os-version", std::env::consts::OS.into()),
        ("x-msh-device-id", device.into()),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v))
    .collect()
}
fn trusted_resource(value: &str, hosts: &[String], version: Option<&str>) -> Result<String> {
    let value = if version.is_some() && !value.contains("://") {
        format!("https://{value}")
    } else {
        value.into()
    };
    let url = reqwest::Url::parse(&value).context("Invalid inference base URL")?;
    let host = url.host_str().context("Invalid inference base URL")?;
    if url.scheme() != "https"
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        // Exact hosts only. A suffix match would trust every subdomain of a
        // provider domain with an account-wide bearer token.
        || !hosts.iter().any(|h| host.eq_ignore_ascii_case(h))
    {
        bail!("Untrusted inference base URL");
    }
    let mut base = url.as_str().trim_end_matches('/').to_owned();
    if let Some(v) = version
        && !base.ends_with(&format!("/{v}"))
    {
        base.push('/');
        base.push_str(v);
    }
    Ok(base)
}
fn has_scope(scope: &str) -> bool {
    scope.split_whitespace().any(|s| s == SHARING_SCOPE)
}
async fn bounded_json(mut response: reqwest::Response) -> Result<Value> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("Invalid OAuth response"))?
    {
        if bytes.len() + chunk.len() > 262_144 {
            bail!("OAuth response is too large");
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("Invalid OAuth response"))
}
fn id_claims(token: &str) -> Option<Value> {
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(token.split('.').nth(1)?).ok()?).ok()
}
impl Attempt {
    fn callback(&self, flow: Flow, input: &str, now: i64) -> Result<(String, String)> {
        if self.started.expires_at <= now {
            bail!("OAuth attempt expired");
        }
        let input = input.trim();
        if input.len() > 16_384 {
            bail!("Invalid OAuth callback");
        }
        let mut params = HashMap::new();
        if flow == Flow::GeminiCli
            && !input.contains('#')
            && !input.starts_with("https://")
            && !input.starts_with("http://")
        {
            if input.is_empty() || input.len() > 8192 {
                bail!("Missing OAuth code");
            }
            return Ok((
                input.into(),
                self.client_id.clone().context("Missing OAuth client")?,
            ));
        }
        if !input.starts_with("https://") && !input.starts_with("http://") {
            if flow == Flow::OpenRouter && !input.contains('#') {
                if input.is_empty() || input.len() > 8192 {
                    bail!("Missing OAuth code");
                }
                return Ok((input.into(), String::new()));
            }
            let (code, state) = input
                .split_once('#')
                .context("Missing OAuth code or state")?;
            params.insert("code".to_owned(), code.to_owned());
            params.insert("state".to_owned(), state.to_owned());
        } else {
            let url =
                reqwest::Url::parse(input).map_err(|_| anyhow::anyhow!("Invalid callback URL"))?;
            if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
                bail!("Invalid callback URL");
            }
            if !matches!(flow, Flow::Chatgpt | Flow::Codex) {
                let expected = reqwest::Url::parse(self.redirect)?;
                if url.origin() != expected.origin() || url.path() != expected.path() {
                    bail!("Invalid callback URL");
                }
            } else if url.scheme() != "http"
                || !matches!(url.host_str(), Some("localhost" | "127.0.0.1"))
                || url.port()
                    != if matches!(flow, Flow::Chatgpt | Flow::Codex) {
                        Some(1455)
                    } else {
                        None
                    }
                || url.path() != "/auth/callback"
            {
                bail!("Invalid callback URL");
            }
            for (k, v) in url.query_pairs() {
                if params.insert(k.into_owned(), v.into_owned()).is_some() {
                    bail!("Duplicate callback parameter");
                }
            }
        }
        if params.contains_key("error") || params.contains_key("error_description") {
            bail!("OAuth consent rejected");
        }
        if params.get("state") != Some(&self.state) {
            bail!("OAuth state mismatch");
        }
        let code = params
            .get("code")
            .filter(|s| !s.is_empty() && s.len() <= 8192)
            .context("Missing OAuth code")?
            .clone();
        let client = match flow {
            Flow::Claude => CLAUDE_CLIENT.into(),
            Flow::Codex => CODEX_CLIENT.into(),
            Flow::Chatgpt => {
                let received = params.get("client_id").filter(|s| !s.is_empty());
                if self
                    .client_id
                    .as_ref()
                    .zip(received)
                    .is_some_and(|(saved, new)| saved != new)
                {
                    bail!("OAuth client ID mismatch");
                }
                self.client_id
                    .as_ref()
                    .or(received)
                    .context("Missing OAuth client ID")?
                    .clone()
            }
            _ => self.client_id.clone().unwrap_or_default(),
        };
        Ok((code, client))
    }
}

/// Claude Code's billing marker is a system text block, not an HTTP header.
pub fn prepare_claude(body: &mut Value) {
    let first = body["messages"]
        .as_array()
        .and_then(|messages| messages.iter().find(|m| m["role"] == "user"))
        .and_then(|m| {
            m["content"].as_str().map(str::to_owned).or_else(|| {
                m["content"].as_array().map(|blocks| {
                    blocks
                        .iter()
                        .filter(|b| b["type"] == "text")
                        .filter_map(|b| b["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
            })
        });
    let mut blocks = Vec::new();
    if let Some(text) = first {
        let sample: String = [4, 7, 20]
            .iter()
            .map(|i| text.chars().nth(*i).unwrap_or('0'))
            .collect();
        let suffix = hex::encode(Sha256::digest(format!("59cf53e54c78{sample}2.1.280")));
        let cch = hex::encode(Sha256::digest(text));
        blocks.push(json!({"type":"text","text":format!("x-anthropic-billing-header: cc_version=2.1.280.{}; cc_entrypoint=sdk-cli; cch={};",&suffix[..3],&cch[..5])}));
    }
    blocks.push(json!({"type":"text","text":CLAUDE_IDENTITY}));
    match body.get("system") {
        Some(Value::Array(existing)) => blocks.extend(existing.iter().cloned()),
        Some(Value::String(text)) if !text.is_empty() => {
            blocks.push(json!({"type":"text","text":text}))
        }
        _ => {}
    }
    body["system"] = json!(blocks);
}
impl OAuthManager {
    pub fn new(db: Db, key: SecretKey, endpoints: OAuthEndpoints) -> Result<Self> {
        Ok(Self {
            db,
            key,
            endpoints,
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .timeout(Duration::from_secs(30))
                .build()?,
            locks: Mutex::default(),
            pending: Mutex::default(),
            devices: Mutex::default(),
            usage_cache: Mutex::default(),
            clock: Arc::new(db::now_ms),
            configs: HashMap::new(),
        })
    }
    pub fn with_clock(mut self, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }
    /// Trusted process-construction seam, like OAuthEndpoints; not an HTTP input.
    pub fn with_flow_config(mut self, flow: Flow, config: FlowConfig) -> Self {
        self.configs.insert(flow, config);
        self
    }
    fn now(&self) -> i64 {
        (self.clock)()
    }
    pub async fn check_storage(&self) -> Result<()> {
        for secret in sqlx::query_scalar::<_, String>("SELECT secret FROM oauth_credentials")
            .fetch_all(&self.db)
            .await?
        {
            let _: Saved = serde_json::from_str(&self.key.decrypt(&secret)?)?;
        }
        Ok(())
    }
    async fn flight(&self, name: &str, flow: Flow) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self
            .locks
            .lock()
            .await
            .entry((name.into(), flow))
            .or_default()
            .clone();
        lock.lock_owned().await
    }
    /// Keep provider rename/delete atomic with in-flight credential writes.
    pub(crate) async fn provider_write(
        &self,
        name: &str,
    ) -> [tokio::sync::OwnedMutexGuard<()>; 11] {
        [
            self.flight(name, Flow::Chatgpt).await,
            self.flight(name, Flow::Claude).await,
            self.flight(name, Flow::Codex).await,
            self.flight(name, Flow::HuggingFace).await,
            self.flight(name, Flow::Entra).await,
            self.flight(name, Flow::GoogleVertex).await,
            self.flight(name, Flow::OpenRouter).await,
            self.flight(name, Flow::GithubCopilot).await,
            self.flight(name, Flow::QwenCode).await,
            self.flight(name, Flow::KimiCode).await,
            self.flight(name, Flow::GeminiCli).await,
        ]
    }
    async fn load(&self, name: &str, flow: Flow) -> Result<Saved> {
        let text: Option<String> =
            sqlx::query_scalar("SELECT secret FROM oauth_credentials WHERE provider=? AND flow=?")
                .bind(name)
                .bind(flow.as_str())
                .fetch_optional(&self.db)
                .await?;
        text.map(|v| Ok(serde_json::from_str(&self.key.decrypt(&v)?)?))
            .unwrap_or_else(|| Ok(Saved::default()))
    }
    async fn save(&self, name: &str, flow: Flow, saved: &Saved) -> Result<()> {
        sqlx::query("INSERT INTO oauth_credentials(provider,flow,secret) VALUES(?,?,?) ON CONFLICT(provider,flow) DO UPDATE SET secret=excluded.secret")
            .bind(name).bind(flow.as_str()).bind(self.key.encrypt(&serde_json::to_string(saved)?)?).execute(&self.db).await?;
        Ok(())
    }
    async fn validate(&self, name: &str, flow: Flow) -> Result<()> {
        let auth: Option<String> = sqlx::query_scalar("SELECT auth FROM providers WHERE name=?")
            .bind(name)
            .fetch_optional(&self.db)
            .await?;
        let valid = match flow {
            Flow::Claude => auth.as_deref() == Some("claude"),
            Flow::Chatgpt => auth.as_deref() == Some("chatgpt"),
            Flow::Codex => matches!(auth.as_deref(), Some("chatgpt" | "codex")),
            Flow::OpenRouter => {
                let base: Option<String> =
                    sqlx::query_scalar("SELECT base_url FROM providers WHERE name=?")
                        .bind(name)
                        .fetch_optional(&self.db)
                        .await?;
                auth.as_deref() == Some("api_key")
                    && base.as_deref().is_some_and(|s| {
                        matches!(
                            s.trim_end_matches('/'),
                            "https://openrouter.ai/api" | "https://openrouter.ai/api/v1"
                        )
                    })
            }
            _ => auth.as_deref() == Some(flow.as_str()),
        };
        if !valid {
            bail!("OAuth flow does not match this Provider");
        }
        Ok(())
    }
    pub fn resource(&self, auth: ProviderAuth) -> &str {
        match auth {
            ProviderAuth::Claude => &self.endpoints.claude_resource,
            ProviderAuth::Codex => &self.endpoints.codex_resource,
            ProviderAuth::HuggingFace => &self.endpoints.hf_resource,
            ProviderAuth::GithubCopilot => &self.endpoints.copilot_resource,
            ProviderAuth::QwenCode => &self.endpoints.qwen_resource,
            ProviderAuth::KimiCode => &self.endpoints.kimi_resource,
            ProviderAuth::GeminiCli => &self.endpoints.code_assist_resource,
            _ => self.endpoints.openai_resource.trim_end_matches("/v1"),
        }
    }
    pub fn entra_resource<'a>(&'a self, configured: &'a str) -> &'a str {
        self.endpoints
            .entra_resource
            .as_deref()
            .unwrap_or(configured)
    }
    pub async fn inference_resource(
        &self,
        name: &str,
        auth: ProviderAuth,
        configured: &str,
    ) -> Result<String> {
        let Some(flow) = Flow::from_auth(auth) else {
            return Ok(configured.into());
        };
        if auth == ProviderAuth::GoogleVertex {
            return Ok(self
                .endpoints
                .vertex_resource
                .as_deref()
                .unwrap_or(configured)
                .into());
        }
        let saved = self.load(name, flow).await?;
        if let Some(base) = saved.credential.and_then(|c| c.inference_base) {
            return Ok(base);
        }
        Ok(if auth == ProviderAuth::Entra {
            self.entra_resource(configured)
        } else {
            self.resource(auth)
        }
        .into())
    }
    /// Client configuration is encrypted with the tokens and never returned to HTML.
    pub async fn configure(
        &self,
        name: &str,
        flow: Flow,
        mut options: ClientOptions,
    ) -> Result<()> {
        let _guard = self.flight(name, flow).await;
        self.validate(name, flow).await?;
        if !matches!(
            flow,
            Flow::HuggingFace | Flow::Entra | Flow::GoogleVertex | Flow::GithubCopilot
        ) {
            bail!("This sign-in has a fixed client configuration");
        }
        if [
            &options.client_id,
            &options.client_secret,
            &options.tenant,
            &options.scope,
        ]
        .iter()
        .any(|s| s.len() > 8192 || s.contains(['\r', '\n']))
        {
            bail!("Invalid OAuth client configuration");
        }
        let mut saved = self.load(name, flow).await?;
        if options.client_secret.is_empty() {
            options.client_secret = saved.options.client_secret.clone();
        }
        saved.options = options;
        saved.service_account = None;
        self.config(flow, &saved)?;
        saved.credential = None;
        saved.last_error = None;
        self.pending.lock().await.remove(&(name.into(), flow));
        self.devices.lock().await.remove(&(name.into(), flow));
        self.save(name, flow, &saved).await
    }
    pub async fn instructions(&self, name: &str, flow: Flow) -> Result<&'static str> {
        Ok(self
            .config(flow, &self.load(name, flow).await?)?
            .instructions)
    }
    pub async fn client_metadata(&self) -> Result<Option<Value>> {
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM providers WHERE auth='hugging_face' ORDER BY name",
        )
        .fetch_all(&self.db)
        .await?;
        for name in names {
            let saved = self.load(&name, Flow::HuggingFace).await?;
            if saved.options.client_id.starts_with("https://") {
                let url = reqwest::Url::parse(&saved.options.client_id)?;
                return Ok(Some(
                    json!({"client_id":url.as_str(),"client_name":"Tokomotive","redirect_uris":[Flow::HuggingFace.redirect(),"http://127.0.0.1/auth/callback"],"token_endpoint_auth_method":"none","client_uri":url.origin().ascii_serialization()}),
                ));
            }
        }
        Ok(None)
    }
    pub async fn status(&self, name: &str, flow: Flow) -> Result<Status> {
        self.validate(name, flow).await?;
        let s = self.load(name, flow).await?;
        let c = s.credential.as_ref();
        Ok(Status {
            connected: c.is_some() && s.last_error.is_none(),
            account: c.and_then(|c| c.account.clone()),
            plan: c.and_then(|c| c.plan.clone()),
            expires_at: c.map(|c| c.expires_at),
            last_refresh: c.and_then(|c| c.last_refresh),
            last_error: s.last_error,
        })
    }
    pub async fn disconnect(&self, name: &str, flow: Flow) -> Result<()> {
        let _guard = self.flight(name, flow).await;
        self.validate(name, flow).await?;
        self.pending.lock().await.remove(&(name.into(), flow));
        self.devices.lock().await.remove(&(name.into(), flow));
        self.usage_cache.lock().await.remove(name);
        let mut s = self.load(name, flow).await?;
        s.credential = None;
        s.service_account = None;
        s.last_error = None;
        self.save(name, flow, &s).await
    }
    pub async fn start(&self, name: &str, flow: Flow) -> Result<Started> {
        let _guard = self.flight(name, flow).await;
        self.validate(name, flow).await?;
        self.devices.lock().await.remove(&(name.into(), flow));
        let mut s = self.load(name, flow).await?;
        let config = self.config(flow, &s)?;
        if config.flow_type == FlowType::DeviceCode && config.authorize.is_empty() {
            bail!("Use device sign-in for this Provider");
        }
        if s.host_id.is_empty() {
            s.host_id = format!("urn:uuid:{}", uuid());
            self.save(name, flow, &s).await?;
        }
        let verifier = random(if flow == Flow::Chatgpt { 32 } else { 64 });
        let state = if flow == Flow::Claude {
            uuid().replace('-', "")
        } else {
            random(32)
        };
        let nonce = random(32);
        let mut url = reqwest::Url::parse(&config.authorize)?;
        {
            let mut q = url.query_pairs_mut();
            if config.flow_type != FlowType::PkceKey {
                q.append_pair("response_type", "code")
                    .append_pair("redirect_uri", config.redirect)
                    .append_pair("client_id", &config.client_id)
                    .append_pair("scope", &config.scopes);
            }
            q.append_pair("state", &state)
                .append_pair("code_challenge_method", "S256")
                .append_pair(
                    "code_challenge",
                    &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
                );
            for (k, v) in &config.authorize_params {
                q.append_pair(k, v);
            }
            if flow == Flow::Chatgpt {
                q.append_pair("ext_agent_host_id", &s.host_id)
                    .append_pair("nonce", &nonce)
                    .append_pair("resource", &self.endpoints.openai_resource);
                if s.client_id.is_none() {
                    q.append_pair("agent_name_hint", "tokomotive");
                }
            }
        }
        let started = Started {
            authorize_url: url.into(),
            expires_at: self.now() + 600_000,
        };
        let mut pending = self.pending.lock().await;
        pending.retain(|_, a| a.started.expires_at > self.now());
        pending.insert(
            (name.into(), flow),
            Attempt {
                started: started.clone(),
                verifier,
                state,
                nonce,
                client_id: if flow == Flow::Chatgpt {
                    s.client_id
                } else {
                    Some(config.client_id)
                },
                redirect: config.redirect,
            },
        );
        Ok(started)
    }
    /// A rejected access token triggers at most one refresh. Concurrent 401s reuse
    /// the new token instead of consuming its rotating refresh token again.
    pub async fn token(&self, name: &str, flow: Flow, rejected: Option<&str>) -> Result<String> {
        let _guard = self.flight(name, flow).await;
        self.validate(name, flow).await?;
        let mut s = self.load(name, flow).await?;
        if flow == Flow::GoogleVertex && s.service_account.is_some() {
            return self.service_account_token(name, &mut s, rejected).await;
        }
        let config = self.config(flow, &s)?;
        if s.last_error.is_some() {
            bail!("Subscription disconnected; sign in again");
        }
        let c = s
            .credential
            .as_ref()
            .context("Subscription disconnected; sign in again")?;
        if rejected.is_some_and(|old| old != c.access_token)
            || (rejected.is_none() && c.expires_at > self.now() + 300_000)
        {
            return Ok(c.access_token.clone());
        }
        let refresh = c.refresh_token.clone();
        if config.refresh == RefreshRule::None || refresh.is_empty() {
            bail!("Sign in again to renew this credential");
        }
        let client = s.client_id.clone().context("Missing OAuth client")?;
        // Commit before sending: a crash or an ambiguous failure must not replay
        // a refresh token that the issuer may already have consumed.
        s.last_error = Some("Refresh did not complete; sign in again".into());
        self.save(name, flow, &s).await?;
        let t = self.tokens(name,flow,json!({"grant_type":"refresh_token","client_id":client,"refresh_token":refresh,"resource":self.endpoints.openai_resource})).await?;
        let inference_base = if let InferenceBase::TokenResource { field, .. } =
            &config.inference_base
            && !t.extra.contains_key(field)
        {
            s.credential.as_ref().and_then(|c| c.inference_base.clone())
        } else {
            self.derive_inference(&config, &t).await?
        };
        let c = s.credential.as_mut().expect("credential checked");
        let scope = t.scope.unwrap_or_else(|| c.scope.clone());
        if flow == Flow::Chatgpt && !has_scope(&scope) {
            bail!("ChatGPT token sharing scope was not granted");
        }
        let refresh = match t.refresh_token {
            Some(v) => v,
            None if config.refresh == RefreshRule::RefreshToken => refresh,
            None => bail!("Missing rotating refresh token"),
        };
        c.access_token = t.access_token;
        c.refresh_token = refresh;
        c.scope = scope;
        c.expires_at = self.now() + t.expires_in.unwrap_or(3600) * 1000;
        c.last_refresh = Some(self.now());
        c.inference_base = inference_base;
        let token = c.access_token.clone();
        s.last_error = None;
        self.save(name, flow, &s).await?;
        Ok(token)
    }
    pub async fn reject(&self, name: &str, flow: Flow, token: &str) -> Result<()> {
        let _guard = self.flight(name, flow).await;
        let mut s = self.load(name, flow).await?;
        if s.credential
            .as_ref()
            .is_some_and(|c| c.access_token == token)
        {
            s.last_error = Some("Provider rejected refreshed credentials; sign in again".into());
            self.save(name, flow, &s).await?;
        }
        Ok(())
    }
    async fn tokens(&self, name: &str, flow: Flow, mut body: Value) -> Result<Tokens> {
        let config = self.config(flow, &self.load(name, flow).await?)?;
        if flow != Flow::Chatgpt {
            body.as_object_mut()
                .expect("token object")
                .remove("resource");
        }
        if flow != Flow::Claude {
            body.as_object_mut().expect("token object").remove("state");
        }
        let req = self.oauth_request(&config, &config.token, body)?;
        let response = req
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("OAuth token request failed; sign in again"))?;
        if !response.status().is_success() {
            bail!("OAuth token request rejected; sign in again");
        }
        let t: Tokens = serde_json::from_value(bounded_json(response).await?)
            .map_err(|_| anyhow::anyhow!("Invalid OAuth token response"))?;
        Self::check_tokens(flow, &t)?;
        Ok(t)
    }
    fn check_tokens(flow: Flow, t: &Tokens) -> Result<()> {
        if t.access_token.is_empty()
            || t.access_token.len() > 16_384
            || t.refresh_token
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 16_384)
            || t.expires_in.is_some_and(|n| n <= 0 || n > 31_536_000)
            || (flow == Flow::Claude && t.expires_in.is_none())
        {
            bail!("Invalid OAuth token response");
        }
        Ok(())
    }
    fn oauth_request(
        &self,
        config: &FlowConfig,
        url: &str,
        mut body: Value,
    ) -> Result<reqwest::RequestBuilder> {
        let mut req = self.http.post(url);
        for (k, v) in &config.headers {
            req = req.header(k, v);
        }
        if let Some(secret) = &config.client_secret {
            if config.basic_secret {
                req = req.basic_auth(&config.client_id, Some(secret));
            } else {
                body["client_secret"] = json!(secret);
            }
        }
        Ok(match config.token_encoding {
            TokenEncoding::Json => req.json(&body),
            TokenEncoding::Form => {
                req.form(&serde_json::from_value::<HashMap<String, String>>(body)?)
            }
        })
    }
    async fn profile(&self, token: &str) -> Result<Value> {
        let response = self
            .http
            .get(format!(
                "{}/api/oauth/profile",
                self.endpoints.claude_resource
            ))
            .bearer_auth(token)
            .header("anthropic-beta", "oauth-2025-04-20")
            .send()
            .await?;
        if !response.status().is_success() {
            bail!("Profile not available");
        }
        bounded_json(response).await
    }
    async fn verify(&self, token: &str, client: &str, nonce: &str) -> Result<Value> {
        use jsonwebtoken::{
            Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet,
        };
        let fail = || anyhow::anyhow!("Invalid OAuth ID token");
        let header = decode_header(token).map_err(|_| fail())?;
        if header.alg != Algorithm::RS256 {
            return Err(fail());
        }
        let kid = header.kid.ok_or_else(fail)?;
        let response = self
            .http
            .get(format!("{}/.well-known/jwks.json", self.endpoints.issuer))
            .send()
            .await
            .map_err(|_| fail())?;
        if !response.status().is_success() {
            return Err(fail());
        }
        let jwks: JwkSet =
            serde_json::from_value(bounded_json(response).await?).map_err(|_| fail())?;
        let key = DecodingKey::from_jwk(jwks.find(&kid).ok_or_else(fail)?).map_err(|_| fail())?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[&self.endpoints.issuer]);
        validation.set_audience(&[client]);
        validation.leeway = 0;
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        let claims = decode::<Value>(token, &key, &validation)
            .map_err(|_| fail())?
            .claims;
        if claims["nonce"].as_str() != Some(nonce)
            || claims["sub"].as_str().is_none_or(|s| s.is_empty())
        {
            return Err(fail());
        }
        Ok(claims)
    }
    pub async fn finish(&self, name: &str, flow: Flow, callback: &str) -> Result<()> {
        let _guard = self.flight(name, flow).await;
        self.validate(name, flow).await?;
        let a = self
            .pending
            .lock()
            .await
            .remove(&(name.into(), flow))
            .context("OAuth attempt expired or missing")?;
        let (code, client) = a.callback(flow, callback, self.now())?;
        let config = self.config(flow, &self.load(name, flow).await?)?;
        if config.flow_type == FlowType::PkceKey {
            let response = self
                .oauth_request(
                    &config,
                    &config.token,
                    json!({"code":code,"code_verifier":a.verifier,"code_challenge_method":"S256"}),
                )?
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("OAuth key request failed"))?;
            if !response.status().is_success() {
                bail!("OAuth key request rejected");
            }
            let value = bounded_json(response).await?;
            let key = value["key"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 16384)
                .context("Invalid OAuth key response")?;
            let encrypted = self.key.encrypt(key)?;
            sqlx::query("UPDATE providers SET keys_json=json_insert(keys_json,'$[#]',?) WHERE name=? AND auth='api_key'")
                .bind(encrypted).bind(name).execute(&self.db).await?;
            return Ok(());
        }
        let tokens = self.tokens(name,flow,json!({"grant_type":"authorization_code","client_id":client,"code":code,"state":a.state,"redirect_uri":a.redirect,"code_verifier":a.verifier,"resource":self.endpoints.openai_resource})).await?;
        self.store_tokens(name, flow, client, &a.nonce, tokens)
            .await
    }
    async fn store_tokens(
        &self,
        name: &str,
        flow: Flow,
        client: String,
        nonce: &str,
        tokens: Tokens,
    ) -> Result<()> {
        let config = self.config(flow, &self.load(name, flow).await?)?;
        let inference_base = self.derive_inference(&config, &tokens).await?;
        let refresh = match tokens.refresh_token {
            Some(v) => v,
            None if config.refresh == RefreshRule::None => String::new(),
            None => bail!("OAuth response has no refresh token"),
        };
        let scope = tokens.scope.unwrap_or_default();
        let mut account = None;
        let mut plan = None;
        let mut account_id = None;
        if flow == Flow::GeminiCli {
            let (project, tier) = self.code_assist_setup(&tokens.access_token).await?;
            account_id = Some(project);
            plan = Some(tier);
        }
        if flow == Flow::Chatgpt {
            if !has_scope(&scope) {
                bail!("ChatGPT token sharing scope was not granted");
            }
            let claims = self
                .verify(
                    tokens
                        .id_token
                        .as_deref()
                        .context("Missing OAuth ID token")?,
                    &client,
                    nonce,
                )
                .await?;
            account = claims["email"].as_str().map(str::to_owned);
            plan = claims["https://api.openai.com/auth"]["chatgpt_plan_type"]
                .as_str()
                .map(str::to_owned);
        } else if flow == Flow::Codex {
            let claims = id_claims(
                tokens
                    .id_token
                    .as_deref()
                    .context("Missing Codex ID token")?,
            )
            .context("Invalid Codex ID token")?;
            account_id = Some(
                claims["https://api.openai.com/auth"]["chatgpt_account_id"]
                    .as_str()
                    .filter(|v| !v.is_empty())
                    .context("Missing ChatGPT account")?
                    .into(),
            );
            account = claims["email"].as_str().map(str::to_owned);
            plan = claims["https://api.openai.com/auth"]["chatgpt_plan_type"]
                .as_str()
                .map(str::to_owned);
        } else if flow == Flow::Claude
            && let Ok(profile) = self.profile(&tokens.access_token).await
        {
            account = profile["account"]["email"].as_str().map(str::to_owned);
            plan = profile["organization"]["plan_display_name"]
                .as_str()
                .map(str::to_owned);
        }
        let mut s = self.load(name, flow).await?;
        s.client_id = Some(client);
        s.last_error = None;
        s.credential = Some(Credential {
            access_token: tokens.access_token,
            refresh_token: refresh,
            scope,
            expires_at: if config.refresh == RefreshRule::None && tokens.expires_in.is_none() {
                i64::MAX
            } else {
                self.now() + tokens.expires_in.unwrap_or(3600) * 1000
            },
            last_refresh: None,
            account,
            plan,
            account_id,
            inference_base,
        });
        self.save(name, flow, &s).await?;
        self.usage_cache.lock().await.remove(name);
        Ok(())
    }
    async fn derive_inference(
        &self,
        config: &FlowConfig,
        tokens: &Tokens,
    ) -> Result<Option<String>> {
        let (value, origin) = match &config.inference_base {
            InferenceBase::TokenResource {
                field,
                fallback,
                hosts,
                version,
            } => {
                return match tokens.extra.get(field) {
                    None => Ok(Some(fallback.clone())),
                    Some(value) => Ok(Some(trusted_resource(
                        value.as_str().context("Invalid inference base URL")?,
                        hosts,
                        Some(version),
                    )?)),
                };
            }
            InferenceBase::UserResource {
                url,
                pointer,
                fallback,
                hosts,
                headers,
                entitlement,
            } => {
                let mut request = self
                    .http
                    .get(url)
                    .bearer_auth(&tokens.access_token)
                    .header("accept", "application/json");
                for (k, v) in headers {
                    request = request.header(k, v);
                }
                let response = request.send().await.context("Inference discovery failed")?;
                if !response.status().is_success() {
                    bail!("Inference discovery rejected");
                }
                let value = bounded_json(response).await?;
                if value.pointer(entitlement).and_then(Value::as_bool) == Some(false) {
                    bail!("Subscription access is not enabled");
                }
                return match value.pointer(pointer) {
                    None => Ok(Some(fallback.clone())),
                    Some(base) => Ok(Some(trusted_resource(
                        base.as_str().context("Invalid inference base URL")?,
                        hosts,
                        None,
                    )?)),
                };
            }
            InferenceBase::Fixed(base) => return Ok((!base.is_empty()).then(|| base.clone())),
            InferenceBase::AdapterPending => return Ok(None),
            InferenceBase::TokenField { field, origin } => (
                tokens
                    .extra
                    .get(field)
                    .and_then(Value::as_str)
                    .context("Missing inference base URL")?
                    .to_owned(),
                origin,
            ),
            InferenceBase::FollowUp { url, field, origin } => {
                let response = self
                    .http
                    .get(url)
                    .bearer_auth(&tokens.access_token)
                    .send()
                    .await
                    .map_err(|_| anyhow::anyhow!("Inference discovery failed"))?;
                if !response.status().is_success() {
                    bail!("Inference discovery rejected");
                }
                let value = bounded_json(response).await?;
                (
                    value[field]
                        .as_str()
                        .context("Missing inference base URL")?
                        .to_owned(),
                    origin,
                )
            }
        };
        let url = reqwest::Url::parse(&value)
            .map_err(|_| anyhow::anyhow!("Invalid inference base URL"))?;
        if url.scheme() != "https"
            || url.origin().ascii_serialization() != *origin
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!("Invalid inference base URL");
        }
        Ok(Some(value.trim_end_matches('/').into()))
    }
    pub async fn codex_account(&self, name: &str) -> Result<String> {
        self.load(name, Flow::Codex)
            .await?
            .credential
            .and_then(|c| c.account_id)
            .context("Missing Codex account")
    }
    pub async fn device_id(&self, name: &str, flow: Flow) -> Result<String> {
        self.validate(name, flow).await?;
        let id = self.load(name, flow).await?.host_id;
        if id.is_empty() {
            bail!("Sign in to create the device identity");
        }
        Ok(id)
    }

    /// OpenAI's Codex device flow is not RFC 8628. It first yields a PKCE
    /// authorization code, which is exchanged at the normal token endpoint.
    pub async fn start_device(&self, name: &str, flow: Flow) -> Result<DeviceStatus> {
        let _guard = self.flight(name, flow).await;
        self.validate(name, flow).await?;
        let mut saved = self.load(name, flow).await?;
        if saved.host_id.is_empty() {
            saved.host_id = uuid();
            self.save(name, flow, &saved).await?;
        }
        let config = self.config(flow, &saved)?;
        if config.device.is_none() {
            bail!("Device sign-in is not supported for this flow");
        }
        self.pending.lock().await.remove(&(name.into(), flow));
        self.devices.lock().await.remove(&(name.into(), flow));
        let verifier = config.device_pkce.then(|| random(64));
        let mut body = json!({"client_id":config.client_id});
        if flow != Flow::Codex && !config.scopes.is_empty() {
            body["scope"] = json!(config.scopes);
        }
        if let Some(v) = &verifier {
            body["code_challenge_method"] = json!("S256");
            body["code_challenge"] = json!(URL_SAFE_NO_PAD.encode(Sha256::digest(v.as_bytes())));
        }
        let request = if flow == Flow::Codex {
            self.http
                .post(config.device.as_ref().expect("device checked"))
                .json(&body)
        } else {
            self.oauth_request(
                &config,
                config.device.as_ref().expect("device checked"),
                body,
            )?
        };
        let response = request
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Cannot start device sign-in"))?;
        if !response.status().is_success() {
            bail!("Device sign-in is not available; use the redirect URL instead");
        }
        let value = bounded_json(response).await?;
        let field = |key: &str| {
            value[key]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 16384)
                .map(str::to_owned)
                .context("Invalid device sign-in response")
        };
        let device_auth_id = field(if flow == Flow::Codex {
            "device_auth_id"
        } else {
            "device_code"
        })?;
        let user_code = field("user_code").or_else(|_| field("usercode"))?;
        if user_code.len() > 128 {
            bail!("Invalid device sign-in response");
        }
        let interval = value["interval"]
            .as_i64()
            .or_else(|| value["interval"].as_str()?.parse().ok())
            .unwrap_or(5)
            .clamp(1, 900);
        let ttl = value["expires_in"].as_i64().unwrap_or(900).clamp(1, 900);
        let now = self.now();
        let status = DeviceStatus {
            attempt: random(24),
            user_code,
            verification_url: if flow == Flow::Codex {
                format!("{}/codex/device", self.endpoints.issuer)
            } else {
                let uri = field("verification_uri_complete")
                    .or_else(|_| field("verification_uri"))
                    .or_else(|_| field("verification_url"))?;
                let parsed = reqwest::Url::parse(&uri)?;
                if parsed.scheme() != "https"
                    || !parsed.username().is_empty()
                    || parsed.password().is_some()
                {
                    bail!("Invalid device verification URL");
                }
                uri
            },
            expires_at: now + ttl * 1000,
            remaining_secs: ttl,
            interval,
            state: "pending",
        };
        let mut devices = self.devices.lock().await;
        devices.retain(|_, a| a.status.expires_at > now);
        devices.insert(
            (name.into(), flow),
            DeviceAttempt {
                status: status.clone(),
                device_auth_id,
                next_poll: now + interval * 1000,
                seen_at: now,
                verifier,
            },
        );
        Ok(status)
    }
    /// Status requests are also heartbeats. If the panel goes away, polling
    /// stops within 15 seconds (plus at most one bounded issuer request).
    pub async fn device_status(
        &self,
        name: &str,
        flow: Flow,
        attempt: &str,
    ) -> Result<DeviceStatus> {
        self.validate(name, flow).await?;
        let now = self.now();
        let mut devices = self.devices.lock().await;
        devices.retain(|_, a| a.status.expires_at + 60_000 > now);
        let a = devices
            .get_mut(&(name.into(), flow))
            .filter(|a| a.status.attempt == attempt)
            .context("Device attempt expired or missing")?;
        a.seen_at = now;
        a.status.remaining_secs = ((a.status.expires_at - now + 999) / 1000).max(0);
        if a.status.expires_at <= now && a.status.state == "pending" {
            a.status.state = "expired";
            a.device_auth_id.clear();
        }
        Ok(a.status.clone())
    }
    /// One due poll. Returns false for terminal, missing, replaced or stale
    /// attempts. Holding the credential flight prevents a disconnect race.
    pub async fn poll_device(&self, name: &str, flow: Flow, attempt: &str) -> Result<bool> {
        let _guard = self.flight(name, flow).await;
        self.validate(name, flow).await?;
        let identity = (name.into(), flow);
        let now = self.now();
        let (device, user, verifier) = {
            let mut devices = self.devices.lock().await;
            let Some(a) = devices
                .get_mut(&identity)
                .filter(|a| a.status.attempt == attempt)
            else {
                return Ok(false);
            };
            if a.status.state != "pending" {
                return Ok(false);
            }
            if a.seen_at + 15_000 < now {
                devices.remove(&identity);
                return Ok(false);
            }
            if a.status.expires_at <= now {
                a.status.state = "expired";
                a.device_auth_id.clear();
                return Ok(false);
            }
            if a.next_poll > now {
                return Ok(true);
            }
            a.next_poll = now + a.status.interval * 1000;
            (
                a.device_auth_id.clone(),
                a.status.user_code.clone(),
                a.verifier.clone(),
            )
        };
        let result = self
            .poll_device_response(name, flow, &device, &user, verifier.as_deref())
            .await;
        let mut devices = self.devices.lock().await;
        let Some(a) = devices.get_mut(&identity) else {
            return Ok(false);
        };
        match result {
            Ok("slow_down") => {
                a.status.interval = (a.status.interval + 5).min(900);
                a.next_poll = self.now() + a.status.interval * 1000;
            }
            Ok("pending") => {}
            Ok(state) => {
                a.status.state = state;
                a.device_auth_id.clear();
            }
            Err(_) => {
                a.status.state = "failed";
                a.device_auth_id.clear();
            }
        }
        Ok(a.status.state == "pending")
    }
    async fn poll_device_response(
        &self,
        name: &str,
        flow: Flow,
        device: &str,
        user: &str,
        device_verifier: Option<&str>,
    ) -> Result<&'static str> {
        let config = self.config(flow, &self.load(name, flow).await?)?;
        let request = if flow == Flow::Codex {
            self.http
                .post(format!(
                    "{}/api/accounts/deviceauth/token",
                    self.endpoints.issuer
                ))
                .json(&json!({"device_auth_id":device,"user_code":user}))
        } else {
            let mut body = json!({"grant_type":"urn:ietf:params:oauth:grant-type:device_code","client_id":config.client_id,"device_code":device});
            if let Some(v) = device_verifier {
                body["code_verifier"] = json!(v);
            }
            self.oauth_request(&config, &config.token, body)?
        };
        let response = request
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Device poll failed"))?;
        let status = response.status();
        // Never log issuer bodies. 403 and 404 with no OAuth error are pending
        // in Codex's protocol; an explicit denial is terminal.
        let value = bounded_json(response).await.unwrap_or(Value::Null);
        if !status.is_success() || value.get("error").is_some() {
            return Ok(
                match value["error"]
                    .as_str()
                    .or_else(|| value["error"]["code"].as_str())
                {
                    Some("slow_down") => "slow_down",
                    Some("access_denied" | "authorization_declined") => "denied",
                    Some("expired_token") => "expired",
                    Some("authorization_pending") => "pending",
                    _ if status.as_u16() == 429 => "slow_down",
                    _ if flow == Flow::Codex && matches!(status.as_u16(), 403 | 404) => "pending",
                    _ => "failed",
                },
            );
        }
        let tokens = if flow != Flow::Codex {
            let tokens: Tokens = serde_json::from_value(value)
                .map_err(|_| anyhow::anyhow!("Invalid OAuth token response"))?;
            Self::check_tokens(flow, &tokens)?;
            tokens
        } else {
            let code = value["authorization_code"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 16384)
                .context("Invalid device authorization")?;
            let verifier = value["code_verifier"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 16384)
                .context("Invalid device authorization")?;
            if value["code_challenge"].as_str()
                != Some(&URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())))
            {
                bail!("Invalid device PKCE challenge");
            }
            self.tokens(name,Flow::Codex,json!({"grant_type":"authorization_code","client_id":CODEX_CLIENT,"code":code,"redirect_uri":format!("{}/deviceauth/callback",self.endpoints.issuer),"code_verifier":verifier})).await?
        };
        // Expiry and a lost browser heartbeat also cancel an in-flight result.
        let devices = self.devices.lock().await;
        let a = devices
            .get(&(name.into(), flow))
            .context("Device attempt missing")?;
        if a.status.expires_at <= self.now() || a.seen_at + 15_000 < self.now() {
            return Ok("expired");
        }
        drop(devices);
        self.store_tokens(name, flow, config.client_id, "", tokens)
            .await?;
        Ok("connected")
    }
    /// A private usage endpoint must never affect inference or fallback.
    pub async fn usage(&self, name: &str) -> Option<PlanUsage> {
        if let Some((at, usage)) = self.usage_cache.lock().await.get(name)
            && *at + 60_000 > self.now()
        {
            return usage.clone();
        }
        let usage = self.fetch_usage(name).await.ok().flatten();
        self.usage_cache
            .lock()
            .await
            .insert(name.into(), (self.now(), usage.clone()));
        usage
    }
    async fn fetch_usage(&self, name: &str) -> Result<Option<PlanUsage>> {
        let token = self.token(name, Flow::Codex, None).await?;
        let s = self.load(name, Flow::Codex).await?;
        let account = s
            .credential
            .as_ref()
            .and_then(|c| c.account_id.as_ref())
            .context("Missing usage account")?;
        let response = self
            .http
            .get(&self.endpoints.codex_usage)
            .bearer_auth(token)
            .header("chatgpt-account-id", account)
            .header("originator", "codex_cli_rs")
            .header("user-agent", "codex-cli")
            .send()
            .await?;
        if !response.status().is_success() {
            return Ok(None);
        }
        Ok(parse_codex_usage(
            &bounded_json(response).await?,
            self.now(),
        ))
    }
}

pub fn parse_codex_usage(value: &Value, now: i64) -> Option<PlanUsage> {
    let mut windows = Vec::new();
    for name in ["primary_window", "secondary_window"] {
        let w = &value["rate_limit"][name];
        let Some(used) = w["used_percent"].as_f64().filter(|v| v.is_finite()) else {
            continue;
        };
        let label = match w["limit_window_seconds"].as_u64() {
            Some(18000) => "5-hour",
            Some(604800) => "Weekly",
            _ => "Window",
        };
        let reset = w["reset_at"]
            .as_i64()
            .filter(|v| *v > 0)
            .and_then(|s| s.checked_mul(1000))
            .or_else(|| {
                w["reset_after_seconds"]
                    .as_i64()
                    .filter(|v| *v > 0)
                    .and_then(|s| s.checked_mul(1000))
                    .and_then(|ms| now.checked_add(ms))
            })
            .filter(|ms| chrono::DateTime::from_timestamp_millis(*ms).is_some());
        windows.push(UsageWindow {
            label: label.into(),
            used_percent: used.clamp(0.0, 100.0),
            resets_at: reset,
        });
    }
    (!windows.is_empty()).then_some(PlanUsage { windows })
}
