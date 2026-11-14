//! Gemini CLI @4ed06ed: setup.ts:179-307 and server.ts:537-550.
use super::*;

impl OAuthManager {
    pub async fn code_assist_project(&self, name: &str) -> Result<String> {
        self.validate(name, Flow::GeminiCli).await?;
        self.load(name, Flow::GeminiCli)
            .await?
            .credential
            .and_then(|c| c.account_id)
            .context("Code Assist project is missing; sign in again")
    }

    pub(super) async fn code_assist_setup(
        &self,
        provider: &str,
        token: &str,
    ) -> Result<(String, String)> {
        let base = format!(
            "{}/v1internal",
            self.endpoints.code_assist_resource.trim_end_matches('/')
        );
        let metadata = json!({"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"});
        let response = self
            .http(provider)
            .post(format!("{base}:loadCodeAssist"))
            .bearer_auth(token)
            .json(&json!({"metadata":metadata}))
            .send()
            .await
            .map_err(|e| self.request_error(provider, e, "Code Assist discovery failed"))?;
        if !response.status().is_success() {
            bail!("Code Assist discovery rejected");
        }
        let load = bounded_json(response).await?;
        let tier;
        let project;
        if load["currentTier"].is_object() {
            tier = load["paidTier"]["id"]
                .as_str()
                .or_else(|| load["currentTier"]["id"].as_str())
                .unwrap_or("standard-tier")
                .to_owned();
            project = load["cloudaicompanionProject"]
                .as_str()
                .context("Code Assist did not return a project")?
                .to_owned();
        } else {
            tier = load["allowedTiers"]
                .as_array()
                .and_then(|tiers| tiers.iter().find(|t| t["isDefault"] == true))
                .and_then(|t| t["id"].as_str())
                .unwrap_or("legacy-tier")
                .to_owned();
            let response = self
                .http(provider)
                .post(format!("{base}:onboardUser"))
                .bearer_auth(token)
                .json(&json!({"tierId":tier,"metadata":metadata}))
                .send()
                .await
                .map_err(|e| self.request_error(provider, e, "Code Assist onboarding failed"))?;
            if !response.status().is_success() {
                bail!("Code Assist onboarding rejected");
            }
            let mut operation = bounded_json(response).await?;
            let name = operation["name"].as_str().unwrap_or_default().to_owned();
            // Operation names are paths, never origins, queries or relative segments.
            if operation["done"] != true
                && (!name.starts_with("operations/")
                    || name.len() > 512
                    || name.split('/').any(|p| {
                        p.is_empty()
                            || p == "."
                            || p == ".."
                            || !p
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
                    }))
            {
                bail!("Invalid Code Assist operation name");
            }
            for _ in 0..12 {
                if operation["done"] == true {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
                let response = self
                    .http(provider)
                    .get(format!("{base}/{name}"))
                    .bearer_auth(token)
                    .send()
                    .await
                    .map_err(|e| self.request_error(provider, e, "Code Assist operation failed"))?;
                if !response.status().is_success() {
                    bail!("Code Assist operation rejected");
                }
                operation = bounded_json(response).await?;
            }
            if operation["done"] != true || operation.get("error").is_some() {
                bail!("Code Assist onboarding did not complete; try again");
            }
            project = operation["response"]["cloudaicompanionProject"]["id"]
                .as_str()
                .context("Code Assist did not return a project")?
                .to_owned();
        }
        if project.is_empty()
            || project.len() > 256
            || !project
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':'))
            || tier.len() > 128
        {
            bail!("Invalid Code Assist project or tier");
        }
        Ok((project, tier))
    }
}
