//! Google service-account JWT bearer grants. Token destinations are fixed.
use super::*;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};

#[derive(Serialize, Deserialize)]
pub(super) struct ServiceAccount {
    #[serde(rename = "type")]
    kind: String,
    client_email: String,
    private_key_id: String,
    private_key: String,
    token_uri: String,
}
impl ServiceAccount {
    fn assertion(&self, now: i64) -> Result<String> {
        let key = EncodingKey::from_rsa_pem(self.private_key.as_bytes())
            .map_err(|_| anyhow::anyhow!("Invalid service-account RSA key"))?;
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(self.private_key_id.clone());
        encode(
            &header,
            &json!({
                "iss": self.client_email,
                "scope": "https://www.googleapis.com/auth/cloud-platform",
                "aud": "https://oauth2.googleapis.com/token",
                "iat": now,
                "exp": now + 3600,
            }),
            &key,
        )
        .map_err(|_| anyhow::anyhow!("Cannot sign service-account assertion"))
    }
}
impl OAuthManager {
    pub async fn configure_service_account(&self, name: &str, input: &str) -> Result<()> {
        if input.len() > 32_768 {
            bail!("Service-account JSON is too large");
        }
        let account: ServiceAccount = serde_json::from_str(input)
            .map_err(|_| anyhow::anyhow!("Invalid service-account JSON"))?;
        if account.kind != "service_account"
            || account.token_uri != "https://oauth2.googleapis.com/token"
            || !account.client_email.ends_with(".gserviceaccount.com")
            || !account.client_email.contains('@')
            || account.client_email.len() > 256
            || account.client_email.contains(['\r', '\n', ' '])
            || account.private_key_id.is_empty()
            || account.private_key_id.len() > 256
        {
            bail!("Invalid service-account identity or token endpoint");
        }
        account.assertion(self.now() / 1000)?;
        let _guard = self.flight(name, Flow::GoogleVertex).await;
        self.validate(name, Flow::GoogleVertex).await?;
        let mut saved = self.load(name, Flow::GoogleVertex).await?;
        let recovered = saved.last_error.is_some() || saved.refresh.last_error.is_some();
        saved.options = ClientOptions::default();
        saved.client_id = None;
        saved.credential = None;
        saved.last_error = None;
        saved.refresh = Default::default();
        saved.service_account = Some(account);
        self.pending
            .lock()
            .await
            .remove(&(name.into(), Flow::GoogleVertex));
        self.devices
            .lock()
            .await
            .remove(&(name.into(), Flow::GoogleVertex));
        // Mint once so that the dashboard can report a connected credential.
        self.service_account_token(name, &mut saved, None).await?;
        if recovered {
            self.refresh_event(name, Flow::GoogleVertex, true).await;
        }
        Ok(())
    }
    pub(super) async fn service_account_token(
        &self,
        name: &str,
        saved: &mut Saved,
        rejected: Option<&str>,
    ) -> Result<String> {
        if saved.last_error.is_some() {
            bail!("Credential rejected; save a service-account key again");
        }
        if let Some(c) = &saved.credential
            && c.expires_at > self.now()
            && (rejected.is_some_and(|old| old != c.access_token)
                || (rejected.is_none() && !self.refresh_due(Flow::GoogleVertex, saved)))
        {
            self.expiry_metric(name, Flow::GoogleVertex, c.expires_at);
            return Ok(c.access_token.clone());
        }
        if saved
            .refresh
            .next_retry_at
            .is_some_and(|at| at > self.now())
        {
            return self.usable_or_retrying(saved, rejected);
        }
        let account = saved
            .service_account
            .as_ref()
            .context("Missing service-account key")?;
        let assertion = account.assertion(self.now() / 1000)?;
        let email = account.client_email.clone();
        let config = FlowConfig {
            flow_type: FlowType::PkcePaste,
            authorize: String::new(),
            device: None,
            token: self.endpoints.google_token.clone(),
            client_id: String::new(),
            client_secret: None,
            scopes: String::new(),
            authorize_params: vec![],
            token_encoding: TokenEncoding::Form,
            headers: vec![],
            inference_base: InferenceBase::AdapterPending,
            refresh: RefreshRule::RefreshToken,
            redirect: "",
            instructions: "",
            device_pkce: false,
            basic_secret: false,
        };
        let Some(tokens) = self.refresh_tokens(name, Flow::GoogleVertex, &config, saved,
            json!({"grant_type":"urn:ietf:params:oauth:grant-type:jwt-bearer", "assertion":assertion})).await? else {
            return self.usable_or_retrying(saved, rejected);
        };
        let token = tokens.access_token.clone();
        saved.credential = Some(Credential {
            access_token: tokens.access_token,
            refresh_token: String::new(),
            scope: "https://www.googleapis.com/auth/cloud-platform".into(),
            expires_at: self.now() + tokens.expires_in.unwrap_or(3600) * 1000,
            last_refresh: Some(self.now()),
            account: Some(email),
            plan: None,
            account_id: None,
            inference_base: None,
        });
        self.refresh_succeeded(name, Flow::GoogleVertex, saved)
            .await?;
        self.recover_health(name, Flow::GoogleVertex).await?;
        Ok(token)
    }
}
