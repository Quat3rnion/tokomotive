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

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Flow {
    Chatgpt,
    Claude,
    Codex,
}
impl Flow {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chatgpt => "chatgpt",
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
    pub fn from_auth(auth: ProviderAuth) -> Option<Self> {
        match auth {
            ProviderAuth::ApiKey => None,
            ProviderAuth::Chatgpt => Some(Self::Chatgpt),
            ProviderAuth::Codex => Some(Self::Codex),
            ProviderAuth::Claude => Some(Self::Claude),
        }
    }
    pub fn redirect(self) -> &'static str {
        match self {
            Self::Claude => CLAUDE_CALLBACK,
            Self::Chatgpt => "http://localhost:1455/auth/callback",
            Self::Codex => "http://127.0.0.1:1455/auth/callback",
        }
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
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    host_id: String,
    client_id: Option<String>,
    credential: Option<Credential>,
    last_error: Option<String>,
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
}
#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
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
        if flow == Flow::Claude && !input.starts_with("https://") && !input.starts_with("http://") {
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
            if flow == Flow::Claude {
                let expected = reqwest::Url::parse(CLAUDE_CALLBACK)?;
                if url.origin() != expected.origin() || url.path() != expected.path() {
                    bail!("Invalid callback URL");
                }
            } else if url.scheme() != "http"
                || !matches!(url.host_str(), Some("localhost" | "127.0.0.1"))
                || url.port() != Some(1455)
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
        })
    }
    pub fn with_clock(mut self, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
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
    pub(crate) async fn provider_write(&self, name: &str) -> [tokio::sync::OwnedMutexGuard<()>; 3] {
        [
            self.flight(name, Flow::Chatgpt).await,
            self.flight(name, Flow::Claude).await,
            self.flight(name, Flow::Codex).await,
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
            _ => self.endpoints.openai_resource.trim_end_matches("/v1"),
        }
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
        s.last_error = None;
        self.save(name, flow, &s).await
    }
    pub async fn start(&self, name: &str, flow: Flow) -> Result<Started> {
        let _guard = self.flight(name, flow).await;
        self.validate(name, flow).await?;
        self.devices.lock().await.remove(&(name.into(), flow));
        let mut s = self.load(name, flow).await?;
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
        let authorize = match flow {
            Flow::Chatgpt => format!("{}/api/accounts/authorize", self.endpoints.issuer),
            Flow::Codex => format!("{}/oauth/authorize", self.endpoints.issuer),
            Flow::Claude => self.endpoints.claude_authorize.clone(),
        };
        let mut url = reqwest::Url::parse(&authorize)?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("response_type", "code")
                .append_pair("redirect_uri", flow.redirect())
                .append_pair("state", &state)
                .append_pair("code_challenge_method", "S256")
                .append_pair(
                    "code_challenge",
                    &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
                );
            match flow {
                Flow::Chatgpt => {
                    q.append_pair("client_id",s.client_id.as_deref().unwrap_or("dynamic_agent_client"))
                .append_pair("ext_agent_host_id",&s.host_id).append_pair("nonce",&nonce).append_pair("resource",&self.endpoints.openai_resource)
                .append_pair("scope","openid profile email offline_access resource.invoke chatgpt.tokens.use.direct");
                    if s.client_id.is_none() {
                        q.append_pair("agent_name_hint", "tokomotive");
                    }
                }
                Flow::Claude => {
                    q.append_pair("client_id", CLAUDE_CLIENT)
                        .append_pair("code", "true")
                        .append_pair("scope", CLAUDE_SCOPES);
                }
                Flow::Codex => {
                    q.append_pair("client_id",CODEX_CLIENT).append_pair("scope","openid profile email offline_access api.connectors.read api.connectors.invoke")
                .append_pair("id_token_add_organizations","true").append_pair("codex_cli_simplified_flow","true").append_pair("originator","codex_cli_rs");
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
                client_id: s.client_id,
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
        let client = s.client_id.clone().context("Missing OAuth client")?;
        // Commit before sending: a crash or an ambiguous failure must not replay
        // a refresh token that the issuer may already have consumed.
        s.last_error = Some("Refresh did not complete; sign in again".into());
        self.save(name, flow, &s).await?;
        let t = self.tokens(flow,json!({"grant_type":"refresh_token","client_id":client,"refresh_token":refresh,"resource":self.endpoints.openai_resource})).await?;
        let c = s.credential.as_mut().expect("credential checked");
        let scope = t.scope.unwrap_or_else(|| c.scope.clone());
        if flow == Flow::Chatgpt && !has_scope(&scope) {
            bail!("ChatGPT token sharing scope was not granted");
        }
        let refresh = match t.refresh_token {
            Some(v) => v,
            None if flow == Flow::Codex => refresh,
            None => bail!("Missing rotating refresh token"),
        };
        c.access_token = t.access_token;
        c.refresh_token = refresh;
        c.scope = scope;
        c.expires_at = self.now() + t.expires_in.unwrap_or(3600) * 1000;
        c.last_refresh = Some(self.now());
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
    async fn tokens(&self, flow: Flow, mut body: Value) -> Result<Tokens> {
        if flow != Flow::Chatgpt {
            body.as_object_mut()
                .expect("token object")
                .remove("resource");
        }
        if flow != Flow::Claude {
            body.as_object_mut().expect("token object").remove("state");
        }
        let url = match flow {
            Flow::Claude => self.endpoints.claude_token.clone(),
            Flow::Chatgpt => format!("{}/api/accounts/oauth/token", self.endpoints.issuer),
            Flow::Codex => format!("{}/oauth/token", self.endpoints.issuer),
        };
        let req = self.http.post(url);
        let req = if flow == Flow::Claude {
            req.header("accept", "application/json, text/plain, */*")
                .header("user-agent", "axios/1.13.6")
                .json(&body)
        } else {
            let form: HashMap<String, String> = serde_json::from_value(body)?;
            req.form(&form)
        };
        let response = req
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("OAuth token request failed; sign in again"))?;
        if !response.status().is_success() {
            bail!("OAuth token request rejected; sign in again");
        }
        let t: Tokens = serde_json::from_value(bounded_json(response).await?)
            .map_err(|_| anyhow::anyhow!("Invalid OAuth token response"))?;
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
        Ok(t)
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
        let tokens = self.tokens(flow,json!({"grant_type":"authorization_code","client_id":client,"code":code,"state":a.state,"redirect_uri":flow.redirect(),"code_verifier":a.verifier,"resource":self.endpoints.openai_resource})).await?;
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
        let refresh = tokens
            .refresh_token
            .context("OAuth response has no refresh token")?;
        let scope = tokens.scope.unwrap_or_default();
        let mut account = None;
        let mut plan = None;
        let mut account_id = None;
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
        } else if let Ok(profile) = self.profile(&tokens.access_token).await {
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
            expires_at: self.now() + tokens.expires_in.unwrap_or(3600) * 1000,
            last_refresh: None,
            account,
            plan,
            account_id,
        });
        self.save(name, flow, &s).await?;
        self.usage_cache.lock().await.remove(name);
        Ok(())
    }
    pub async fn codex_account(&self, name: &str) -> Result<String> {
        self.load(name, Flow::Codex)
            .await?
            .credential
            .and_then(|c| c.account_id)
            .context("Missing Codex account")
    }

    /// OpenAI's Codex device flow is not RFC 8628. It first yields a PKCE
    /// authorization code, which is exchanged at the normal token endpoint.
    pub async fn start_device(&self, name: &str, flow: Flow) -> Result<DeviceStatus> {
        let _guard = self.flight(name, flow).await;
        self.validate(name, flow).await?;
        if flow != Flow::Codex {
            bail!("Device sign-in is not supported for this flow");
        }
        self.pending.lock().await.remove(&(name.into(), flow));
        self.devices.lock().await.remove(&(name.into(), flow));
        let response = self
            .http
            .post(format!(
                "{}/api/accounts/deviceauth/usercode",
                self.endpoints.issuer
            ))
            .json(&json!({"client_id":CODEX_CLIENT}))
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
        let device_auth_id = field("device_auth_id")?;
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
            verification_url: format!("{}/codex/device", self.endpoints.issuer),
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
        let (device, user) = {
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
            (a.device_auth_id.clone(), a.status.user_code.clone())
        };
        let result = self.poll_device_response(name, &device, &user).await;
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
        device: &str,
        user: &str,
    ) -> Result<&'static str> {
        let response = self
            .http
            .post(format!(
                "{}/api/accounts/deviceauth/token",
                self.endpoints.issuer
            ))
            .json(&json!({"device_auth_id":device,"user_code":user}))
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Device poll failed"))?;
        let status = response.status();
        // Never log issuer bodies. 403 and 404 with no OAuth error are pending
        // in Codex's protocol; an explicit denial is terminal.
        let value = bounded_json(response).await.unwrap_or(Value::Null);
        if !status.is_success() {
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
                    _ if matches!(status.as_u16(), 403 | 404) => "pending",
                    _ => "failed",
                },
            );
        }
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
        let tokens = self.tokens(Flow::Codex,json!({"grant_type":"authorization_code","client_id":CODEX_CLIENT,"code":code,"redirect_uri":format!("{}/deviceauth/callback",self.endpoints.issuer),"code_verifier":verifier})).await?;
        // Expiry and a lost browser heartbeat also cancel an in-flight result.
        let devices = self.devices.lock().await;
        let a = devices
            .get(&(name.into(), Flow::Codex))
            .context("Device attempt missing")?;
        if a.status.expires_at <= self.now() || a.seen_at + 15_000 < self.now() {
            return Ok("expired");
        }
        drop(devices);
        self.store_tokens(name, Flow::Codex, CODEX_CLIENT.into(), "", tokens)
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
