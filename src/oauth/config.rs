//! The trusted sign-in registry. Endpoint overrides are process-local test seams.
use super::*;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientOptions {
    pub client_id: String,
    pub client_secret: String,
    pub tenant: String,
    pub scope: String,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FlowType {
    DeviceCode,
    PkcePaste,
    PkceKey,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TokenEncoding {
    Form,
    Json,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RefreshRule {
    None,
    RefreshToken,
    Rotating,
}
#[derive(Clone)]
pub enum InferenceBase {
    Fixed(String),
    /// Field names and allowed origins are trusted registry data, not form data.
    TokenField {
        field: String,
        origin: String,
    },
    FollowUp {
        url: String,
        field: String,
        origin: String,
    },
    AdapterPending,
}
#[derive(Clone)]
pub struct FlowConfig {
    pub flow_type: FlowType,
    pub authorize: String,
    pub device: Option<String>,
    pub token: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub scopes: String,
    pub authorize_params: Vec<(String, String)>,
    pub token_encoding: TokenEncoding,
    pub headers: Vec<(String, String)>,
    pub inference_base: InferenceBase,
    pub refresh: RefreshRule,
    pub redirect: &'static str,
    pub instructions: &'static str,
    pub device_pkce: bool,
    pub basic_secret: bool,
}
impl OAuthManager {
    pub(super) fn config(&self, flow: Flow, saved: &Saved) -> Result<FlowConfig> {
        if let Some(config) = self.configs.get(&flow) {
            return Ok(config.clone());
        }
        let e = &self.endpoints;
        let o = &saved.options;
        let mut c = FlowConfig {
            flow_type: FlowType::PkcePaste,
            authorize: String::new(),
            device: None,
            token: String::new(),
            client_id: o.client_id.clone(),
            client_secret: (!o.client_secret.is_empty()).then(|| o.client_secret.clone()),
            scopes: o.scope.clone(),
            authorize_params: vec![],
            token_encoding: TokenEncoding::Form,
            headers: vec![],
            inference_base: InferenceBase::AdapterPending,
            refresh: RefreshRule::RefreshToken,
            redirect: flow.redirect(),
            instructions: "Allow access. Copy the full redirect address from the address bar, even if the local page does not load.",
            device_pkce: false,
            basic_secret: false,
        };
        match flow {
            Flow::Chatgpt => {
                c.authorize = format!("{}/api/accounts/authorize", e.issuer);
                c.token = format!("{}/api/accounts/oauth/token", e.issuer);
                c.client_id = saved
                    .client_id
                    .clone()
                    .unwrap_or("dynamic_agent_client".into());
                c.scopes =
                    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct"
                        .into();
                c.inference_base =
                    InferenceBase::Fixed(e.openai_resource.trim_end_matches("/v1").into());
                c.refresh = RefreshRule::Rotating;
            }
            Flow::Claude => {
                c.authorize = e.claude_authorize.clone();
                c.token = e.claude_token.clone();
                c.client_id = CLAUDE_CLIENT.into();
                c.scopes = CLAUDE_SCOPES.into();
                c.authorize_params.push(("code".into(), "true".into()));
                c.token_encoding = TokenEncoding::Json;
                c.headers = vec![
                    ("accept".into(), "application/json, text/plain, */*".into()),
                    ("user-agent".into(), "axios/1.13.6".into()),
                ];
                c.inference_base = InferenceBase::Fixed(e.claude_resource.clone());
                c.refresh = RefreshRule::Rotating;
                c.instructions =
                    "Select Authorize. Copy the full code, including the part after #.";
            }
            Flow::Codex => {
                c.authorize = format!("{}/oauth/authorize", e.issuer);
                c.token = format!("{}/oauth/token", e.issuer);
                c.device = Some(format!("{}/api/accounts/deviceauth/usercode", e.issuer));
                c.client_id = CODEX_CLIENT.into();
                c.scopes =
                    "openid profile email offline_access api.connectors.read api.connectors.invoke"
                        .into();
                c.authorize_params = vec![
                    ("id_token_add_organizations".into(), "true".into()),
                    ("codex_cli_simplified_flow".into(), "true".into()),
                    ("originator".into(), "codex_cli_rs".into()),
                ];
                c.inference_base = InferenceBase::Fixed(e.codex_resource.clone());
            }
            Flow::HuggingFace => {
                c.flow_type = FlowType::DeviceCode;
                c.authorize = e.hf_authorize.clone();
                c.device = Some(e.hf_device.clone());
                c.token = e.hf_token.clone();
                c.scopes = "inference-api".into();
                c.basic_secret = true;
                c.inference_base = InferenceBase::Fixed(e.hf_resource.clone());
                if c.client_id.starts_with("https://") {
                    let url = reqwest::Url::parse(&c.client_id)?;
                    if url.host_str().is_none()
                        || url.path() != "/.well-known/oauth-cimd"
                        || !url.username().is_empty()
                        || url.password().is_some()
                        || url.query().is_some()
                        || url.fragment().is_some()
                        || c.client_secret.is_some()
                    {
                        bail!("CIMD needs the public HTTPS metadata URL and no client secret");
                    }
                    c.device = None;
                    c.flow_type = FlowType::PkcePaste;
                }
            }
            Flow::Entra => {
                if o.tenant.is_empty()
                    || !o
                        .tenant
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'))
                {
                    bail!("Enter the Entra tenant ID or domain");
                }
                let issuer = format!("{}/{}/oauth2/v2.0", e.entra_issuer, o.tenant);
                c.flow_type = FlowType::DeviceCode;
                c.device = Some(format!("{issuer}/devicecode"));
                c.authorize = format!("{issuer}/authorize");
                c.token = format!("{issuer}/token");
                if c.scopes.is_empty() {
                    c.scopes = "https://cognitiveservices.azure.com/.default offline_access".into();
                }
                c.inference_base = InferenceBase::Fixed(String::new()); // Operator's validated Azure v1 base.
            }
            Flow::GoogleVertex => {
                c.authorize = e.google_authorize.clone();
                c.token = e.google_token.clone();
                c.scopes = "https://www.googleapis.com/auth/cloud-platform".into();
                c.authorize_params = vec![
                    ("access_type".into(), "offline".into()),
                    ("prompt".into(), "consent".into()),
                ];
                if c.client_secret.is_none() {
                    bail!("Enter the Google OAuth client secret");
                }
            }
            Flow::OpenRouter => {
                c.flow_type = FlowType::PkceKey;
                c.authorize = e.openrouter_authorize.clone();
                c.token = e.openrouter_token.clone();
                c.refresh = RefreshRule::None;
                c.token_encoding = TokenEncoding::Json;
                c.instructions = "Create the key on OpenRouter. In headless mode, copy the code shown by OpenRouter and paste it here. You can also paste code#state or a redirect URL from this attempt.";
            }
            _ => bail!("Unknown sign-in configuration"),
        }
        if flow != Flow::OpenRouter && c.client_id.is_empty() {
            bail!("Enter the OAuth client ID first");
        }
        Ok(c)
    }
}
