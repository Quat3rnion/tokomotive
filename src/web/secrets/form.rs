//! Typed store configuration. Secret fields are write-only; blank keeps a value.
use crate::{secrets::Store, settings::SecretKey};
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub(super) struct StoreForm {
    pub name: String,
    pub kind: String,
    pub address: String,
    pub typed: bool,
    pub credentials: String,
    pub options: String,
    pub allow_private: Option<String>,
    pub is_default: Option<String>,
    pub refresh_secs: Option<i64>,
    pub vault_auth: String,
    pub vault_token: String,
    pub vault_role_id: String,
    pub vault_secret_id: String,
    pub vault_namespace: String,
    pub vault_kv_version: String,
    pub vault_browse_root: String,
    pub aws_access_key: String,
    pub aws_secret_key: String,
    pub aws_session_token: String,
    pub aws_clear_session_token: Option<String>,
    pub aws_region: String,
    pub gcp_auth: String,
    pub gcp_service_account: String,
    pub gcp_project: String,
    pub bws_access_token: String,
    pub bws_region: String,
    pub bws_api_url: String,
    pub bws_identity_url: String,
}
type Result<T> = std::result::Result<T, &'static str>;
fn object(raw: &str) -> Result<Map<String, Value>> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|_| "Advanced settings must be JSON objects with string values.")?;
    let map = value
        .as_object()
        .ok_or("Advanced settings must be JSON objects with string values.")?;
    if map.values().any(|v| !v.is_string()) {
        return Err("Advanced settings must be JSON objects with string values.");
    }
    Ok(map.clone())
}
fn secret(map: &mut Map<String, Value>, name: &str, value: &str) {
    if !value.trim().is_empty() {
        map.insert(name.into(), Value::String(value.into()));
    }
}
fn option(map: &mut Map<String, Value>, name: &str, value: &str) {
    if value.trim().is_empty() {
        map.remove(name);
    } else {
        map.insert(name.into(), Value::String(value.trim().into()));
    }
}
fn get<'a>(map: &'a Map<String, Value>, name: &str) -> &'a str {
    map.get(name).and_then(Value::as_str).unwrap_or("")
}
fn required(map: &Map<String, Value>, name: &str, message: &'static str) -> Result<()> {
    if get(map, name).trim().is_empty() {
        Err(message)
    } else {
        Ok(())
    }
}
fn allowed(map: &Map<String, Value>, fields: &[&str]) -> Result<()> {
    if map.keys().any(|k| !fields.contains(&k.as_str())) {
        Err("A setting does not apply to the selected store type.")
    } else {
        Ok(())
    }
}

impl StoreForm {
    pub fn prepare(&self, old: Option<&Store>, key: &SecretKey) -> Result<Store> {
        let old = old.filter(|s| s.kind == self.kind);
        let old_auth = old
            .map(|s| {
                key.decrypt(&s.credentials)
                    .map_err(|_| "Saved credentials are unavailable.")
            })
            .transpose()?
            .unwrap_or_else(|| "{}".into());
        let original_auth = object(&old_auth)?;
        let mut auth = original_auth.clone();
        let mut options = object(old.map_or("{}", |s| s.options.as_str()))?;
        let mut address = self.address.trim().trim_end_matches('/').to_owned();
        if self.typed {
            match self.kind.as_str() {
                "vault" => {
                    secret(&mut auth, "token", &self.vault_token);
                    secret(&mut auth, "role_id", &self.vault_role_id);
                    secret(&mut auth, "secret_id", &self.vault_secret_id);
                    option(&mut options, "namespace", &self.vault_namespace);
                    option(
                        &mut options,
                        "kv_version",
                        if self.vault_kv_version.is_empty() {
                            "2"
                        } else {
                            &self.vault_kv_version
                        },
                    );
                    option(&mut options, "browse_root", &self.vault_browse_root);
                }
                "aws-sm" => {
                    secret(&mut auth, "access_key", &self.aws_access_key);
                    secret(&mut auth, "secret_key", &self.aws_secret_key);
                    secret(&mut auth, "session_token", &self.aws_session_token);
                    if self.aws_clear_session_token.is_some() {
                        auth.remove("session_token");
                    }
                    option(&mut options, "region", &self.aws_region);
                }
                "gcp-sm" => {
                    secret(&mut auth, "service_account", &self.gcp_service_account);
                    option(&mut options, "project", &self.gcp_project);
                }
                "bws" => {
                    secret(&mut auth, "access_token", &self.bws_access_token);
                }
                "bw" => {}
                _ => return Err("Choose a secret store type."),
            }
        }
        for (raw, map) in [
            (&self.credentials, &mut auth),
            (&self.options, &mut options),
        ] {
            if !raw.trim().is_empty() {
                map.extend(object(raw)?);
            }
        }
        match self.kind.as_str() {
            "vault" => {
                allowed(&auth, &["token", "role_id", "secret_id"])?;
                allowed(&options, &["namespace", "kv_version", "browse_root"])?;
                let mode = if self.typed {
                    self.vault_auth.as_str()
                } else if get(&auth, "token").is_empty() {
                    "approle"
                } else {
                    "token"
                };
                match mode {
                    "token" => {
                        required(&auth, "token", "Enter a Vault token or env:NAME.")?;
                        auth.remove("role_id");
                        auth.remove("secret_id");
                    }
                    "approle" => {
                        required(&auth, "role_id", "Enter an AppRole role ID or env:NAME.")?;
                        required(
                            &auth,
                            "secret_id",
                            "Enter an AppRole secret ID or env:NAME.",
                        )?;
                        auth.remove("token");
                    }
                    _ => return Err("Choose Vault token or AppRole authentication."),
                }
                if !matches!(get(&options, "kv_version"), "" | "1" | "2") {
                    return Err("Choose Vault KV version 1 or 2.");
                }
            }
            "aws-sm" => {
                allowed(&auth, &["access_key", "secret_key", "session_token"])?;
                allowed(&options, &["region"])?;
                required(
                    &auth,
                    "access_key",
                    "Enter an AWS access key ID or env:NAME.",
                )?;
                required(
                    &auth,
                    "secret_key",
                    "Enter an AWS secret access key or env:NAME.",
                )?;
                crate::sigv4::host(get(&options, "region"))
                    .map_err(|_| "Enter a valid AWS region, for example eu-west-1.")?;
            }
            "gcp-sm" => {
                allowed(&auth, &["service_account"])?;
                allowed(&options, &["project", "token_url"])?;
                let mode = if self.typed {
                    self.gcp_auth.as_str()
                } else if get(&auth, "service_account").is_empty() {
                    "metadata"
                } else {
                    "service_account"
                };
                match mode {
                    "metadata" => {
                        auth.remove("service_account");
                        if self.allow_private.is_none() {
                            return Err("Allow private addresses to use the GCP metadata server.");
                        }
                    }
                    "service_account" => {
                        required(
                            &auth,
                            "service_account",
                            "Choose a service-account JSON file or enter env:NAME.",
                        )?;
                        let raw = get(&auth, "service_account");
                        if !raw.starts_with("env:") {
                            let sa: Value = serde_json::from_str(raw)
                                .map_err(|_| "Choose a valid service-account JSON file.")?;
                            let email = sa
                                .get("client_email")
                                .and_then(Value::as_str)
                                .filter(|v| !v.trim().is_empty())
                                .ok_or("The service-account JSON needs client_email.")?;
                            if !email.contains('@') {
                                return Err("The service-account JSON needs a valid client_email.");
                            }
                            let pem = sa
                                .get("private_key")
                                .and_then(Value::as_str)
                                .ok_or("The service-account JSON needs an RSA private_key.")?;
                            jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes()).map_err(
                                |_| "The service-account JSON needs a valid RSA private_key.",
                            )?;
                        }
                    }
                    _ => return Err("Choose a service-account JSON key or the metadata server."),
                }
                if !get(&options, "project")
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                {
                    return Err("Use letters, digits, underscores or hyphens for the GCP project.");
                }
            }
            "bws" => {
                allowed(&auth, &["access_token"])?;
                allowed(&options, &["identity_url", "api_url"])?;
                required(
                    &auth,
                    "access_token",
                    "Enter a Bitwarden machine-account access token or env:NAME.",
                )?;
                if self.typed {
                    match self.bws_region.as_str() {
                        "us" | "eu" => {
                            address = format!(
                                "https://api.bitwarden.{}",
                                if self.bws_region == "us" { "com" } else { "eu" }
                            );
                            options.clear();
                        }
                        "self-hosted" => {
                            if self.bws_identity_url.trim().is_empty() {
                                return Err("Enter the self-hosted Bitwarden identity URL.");
                            }
                            if self.bws_api_url.trim().is_empty() {
                                return Err("Enter the self-hosted Bitwarden API URL.");
                            }
                            address = self.bws_api_url.trim().trim_end_matches('/').into();
                            option(&mut options, "api_url", &address);
                            option(&mut options, "identity_url", &self.bws_identity_url);
                        }
                        _ => return Err("Choose Bitwarden US, EU or self-hosted."),
                    }
                }
            }
            "bw" => {
                allowed(&auth, &[])?;
                allowed(&options, &[])?;
            }
            _ => return Err("Choose a secret store type."),
        }
        for value in auth.values().filter_map(Value::as_str) {
            if let Some(env) = value.strip_prefix("env:")
                && (env.is_empty()
                    || env.len() > 128
                    || !env.bytes().enumerate().all(|(i, b)| {
                        b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit())
                    }))
            {
                return Err("Use env:NAME with a valid environment variable name.");
            }
        }
        let credentials = if let Some(old) = old
            && auth == original_auth
        {
            old.credentials.clone()
        } else {
            key.encrypt(&Value::Object(auth).to_string())
                .map_err(|_| "Cannot encrypt store credentials.")?
        };
        Ok(Store {
            name: self.name.clone(),
            kind: self.kind.clone(),
            address,
            credentials,
            options: Value::Object(options).to_string(),
            allow_private: self.allow_private.is_some(),
            is_default: self.is_default.is_some(),
            refresh_secs: self.refresh_secs.unwrap_or(900),
        })
    }
}

pub(super) struct StoreView {
    pub form: StoreForm,
    pub saved: bool,
    pub new: bool,
}
impl StoreView {
    pub fn new() -> Self {
        Self {
            form: StoreForm {
                kind: "vault".into(),
                vault_auth: "token".into(),
                vault_kv_version: "2".into(),
                vault_browse_root: "kv".into(),
                aws_region: "us-east-1".into(),
                gcp_auth: "service_account".into(),
                bws_region: "us".into(),
                ..Default::default()
            },
            saved: false,
            new: true,
        }
    }
    pub fn from_store(store: Store, key: &SecretKey) -> anyhow::Result<Self> {
        let raw = key
            .decrypt(&store.credentials)
            .map_err(|_| anyhow::anyhow!("Saved store credentials are unavailable."))?;
        let auth = object(&raw).map_err(anyhow::Error::msg)?;
        let options = object(&store.options).map_err(anyhow::Error::msg)?;
        let bws_region = match store.address.as_str() {
            "https://api.bitwarden.com" => "us",
            "https://api.bitwarden.eu" => "eu",
            _ => "self-hosted",
        }
        .into();
        let bws_api_url = if get(&options, "api_url").is_empty() {
            format!("{}/api", store.address)
        } else {
            get(&options, "api_url").into()
        };
        let bws_identity_url = if get(&options, "identity_url").is_empty() {
            format!("{}/identity/connect/token", store.address)
        } else {
            get(&options, "identity_url").into()
        };
        Ok(Self {
            saved: !auth.is_empty(),
            new: false,
            form: StoreForm {
                name: store.name,
                kind: store.kind,
                address: store.address,
                allow_private: store.allow_private.then(|| "on".into()),
                is_default: store.is_default.then(|| "on".into()),
                refresh_secs: Some(store.refresh_secs),
                vault_auth: if auth.contains_key("token") {
                    "token"
                } else {
                    "approle"
                }
                .into(),
                vault_namespace: get(&options, "namespace").into(),
                vault_kv_version: if get(&options, "kv_version") == "1" {
                    "1"
                } else {
                    "2"
                }
                .into(),
                vault_browse_root: if get(&options, "browse_root").is_empty() {
                    "kv"
                } else {
                    get(&options, "browse_root")
                }
                .into(),
                aws_region: get(&options, "region").into(),
                gcp_auth: if auth.contains_key("service_account") {
                    "service_account"
                } else {
                    "metadata"
                }
                .into(),
                gcp_project: get(&options, "project").into(),
                bws_region,
                bws_api_url,
                bws_identity_url,
                ..Default::default()
            },
        })
    }
    pub fn refresh_secs(&self) -> i64 {
        self.form.refresh_secs.unwrap_or(900)
    }
}
