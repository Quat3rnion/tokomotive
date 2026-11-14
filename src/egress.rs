//! Explicit outbound proxy policy. Secrets never implement Display or Debug.
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Result, bail};

const INVALID: &str =
    "Use http, https, socks5 or socks5h with a host and port, without a path, query or fragment.";

/// Do not retain reqwest errors: their sources can contain endpoint secrets.
pub fn error_class(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_body() {
        "body"
    } else if error.is_decode() {
        "decode"
    } else if error.is_status() {
        "status"
    } else {
        "connect"
    }
}

pub fn request_error(provider: &str, error: reqwest::Error) -> anyhow::Error {
    let class = error_class(&error);
    tracing::warn!(provider, class, "outbound request failed");
    anyhow::anyhow!("{provider}: {class}")
}

#[derive(Clone)]
pub struct ProxyUrl(String);

impl ProxyUrl {
    pub fn parse(input: &str) -> Result<Self> {
        // Inspect the original authority too: URL parsers normalize dot paths,
        // backslashes, whitespace and default ports before validation.
        let Some((scheme, authority)) = input.split_once("://") else {
            bail!(INVALID)
        };
        if !matches!(scheme, "http" | "https" | "socks5" | "socks5h")
            || input.len() > 4096
            || input
                .bytes()
                .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
            || authority.contains(['/', '\\', '?', '#'])
        {
            bail!(INVALID);
        }
        let Some((_, port)) = authority.rsplit_once(':') else {
            bail!(INVALID)
        };
        if port.parse::<u16>().ok().filter(|p| *p > 0).is_none() {
            bail!(INVALID);
        }
        let url = reqwest::Url::parse(input).map_err(|_| anyhow::anyhow!(INVALID))?;
        if url.host_str().is_none_or(str::is_empty)
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!(INVALID);
        }
        Ok(Self(input.to_owned()))
    }

    pub fn label(&self) -> String {
        let url = reqwest::Url::parse(&self.0).expect("validated proxy URL");
        let port = self.0.rsplit_once(':').expect("proxy port").1;
        format!(
            "{}://{}:{port}",
            url.scheme(),
            url.host_str().expect("proxy host")
        )
    }
}

impl fmt::Debug for ProxyUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label())
    }
}

/// Limit distinct proxy endpoints, not Providers that share one endpoint.
pub const MAX_PROXIES: usize = 64;

#[derive(Clone, Copy)]
pub enum Purpose {
    Upstream,
    OAuth,
    Models,
    Webhook,
}

struct Clients {
    upstream: reqwest::Client,
    oauth: reqwest::Client,
}

pub struct Policy {
    global: Option<String>,
    providers: HashMap<String, Option<String>>,
    identities: HashMap<String, String>,
    clients: HashMap<Option<String>, Clients>,
    models: reqwest::Client,
    webhook: reqwest::Client,
}

/// A whole generation is built before a reload replaces the active policy.
#[derive(Clone)]
pub struct Egress(Arc<RwLock<Arc<Policy>>>);

impl fmt::Debug for Egress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Egress")
    }
}

fn builder(proxy: Option<&str>, timeout: u64, purpose: Purpose) -> Result<reqwest::ClientBuilder> {
    let mut builder = reqwest::Client::builder()
        // Do not inherit process proxy variables. "none" must mean direct.
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(timeout));
    if matches!(purpose, Purpose::Upstream) {
        builder = builder.connect_timeout(Duration::from_secs(15));
    }
    if matches!(purpose, Purpose::OAuth) {
        builder = builder.retry(reqwest::retry::never());
    }
    if let Some(proxy) = proxy {
        ProxyUrl::parse(proxy)?;
        builder = builder.proxy(
            reqwest::Proxy::all(proxy).map_err(|_| anyhow::anyhow!("Invalid egress proxy"))?,
        );
    }
    Ok(builder)
}

fn build(proxy: Option<&str>, timeout: u64, purpose: Purpose) -> Result<reqwest::Client> {
    builder(proxy, timeout, purpose)?
        .build()
        .map_err(|_| anyhow::anyhow!("Cannot build egress client"))
}

impl Policy {
    pub fn new(
        global: Option<String>,
        providers: HashMap<String, Option<String>>,
        timeout: u64,
    ) -> Result<Self> {
        let mut distinct: HashSet<_> = providers.values().cloned().collect();
        distinct.insert(global.clone());
        distinct.insert(None);
        if distinct.iter().filter(|p| p.is_some()).count() > MAX_PROXIES {
            bail!("At most 64 distinct egress proxies can be configured.");
        }
        let mut clients = HashMap::new();
        for proxy in distinct {
            let pair = Clients {
                upstream: build(proxy.as_deref(), timeout, Purpose::Upstream)?,
                oauth: build(proxy.as_deref(), 30, Purpose::OAuth)?,
            };
            clients.insert(proxy, pair);
        }
        let models = build(global.as_deref(), 60, Purpose::Models)?;
        let webhook = build(global.as_deref(), 10, Purpose::Webhook)?;
        Ok(Self {
            global,
            providers,
            identities: HashMap::new(),
            clients,
            models,
            webhook,
        })
    }

    pub async fn load(
        db: &crate::db::Db,
        key: &crate::settings::SecretKey,
        timeout: u64,
    ) -> Result<Self> {
        Self::prepare(db, key, timeout, None, None).await
    }

    /// Validate a proposed edit and build its bounded cache before storing it.
    pub async fn prepare(
        db: &crate::db::Db,
        key: &crate::settings::SecretKey,
        timeout: u64,
        global_override: Option<Option<String>>,
        provider_override: Option<(&str, &str, Option<String>)>,
    ) -> Result<Self> {
        let global = match global_override {
            Some(value) => value,
            None => crate::settings::value(db, "egress_proxy")
                .await?
                .map(|v| key.decrypt(&v))
                .transpose()?,
        };
        let rows: Vec<(String, String, Option<String>, String)> =
            sqlx::query_as("SELECT name,proxy_mode,proxy_url,egress_identity FROM providers")
                .fetch_all(db)
                .await?;
        let mut providers = HashMap::new();
        let mut identities = HashMap::new();
        for (name, mut mode, mut secret, identity) in rows {
            if let Some((changed, new_mode, new_url)) = &provider_override
                && *changed == name
            {
                mode = (*new_mode).into();
                secret = new_url.as_ref().map(|v| key.encrypt(v)).transpose()?;
            }
            let proxy = match mode.as_str() {
                "global" => global.clone(),
                "none" => None,
                "custom" => Some(key.decrypt(
                    &secret.ok_or_else(|| anyhow::anyhow!("Missing custom egress proxy"))?,
                )?),
                _ => bail!("Invalid egress proxy mode"),
            };
            identities.insert(name.clone(), identity);
            providers.insert(name, proxy);
        }
        let mut policy = Self::new(global, providers, timeout)?;
        policy.identities = identities;
        Ok(policy)
    }
}

impl Egress {
    pub fn new(policy: Policy) -> Self {
        Self(Arc::new(RwLock::new(Arc::new(policy))))
    }
    pub fn replace(&self, policy: Policy) {
        *self.0.write().expect("egress policy") = Arc::new(policy);
    }
    /// Retain one complete client generation, not a live name lookup.
    pub fn snapshot(&self) -> Self {
        Self(Arc::new(RwLock::new(
            self.0.read().expect("egress policy").clone(),
        )))
    }
    /// Check the row identity before a later send. A reused name is not the
    /// Provider whose credentials the request retained. Database errors close.
    pub async fn validate(&self, db: &crate::db::Db, provider: &str) -> Result<()> {
        let expected = self
            .0
            .read()
            .expect("egress policy")
            .identities
            .get(provider)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Provider egress policy unavailable"))?;
        let current: Option<String> =
            sqlx::query_scalar("SELECT egress_identity FROM providers WHERE name=?")
                .bind(provider)
                .fetch_optional(db)
                .await?;
        if current.as_deref() != Some(expected.as_str()) {
            bail!("Provider egress policy unavailable");
        }
        Ok(())
    }
    pub fn client(&self, provider: Option<&str>, purpose: Purpose) -> Result<reqwest::Client> {
        let policy = self.0.read().expect("egress policy");
        let proxy = match provider {
            Some(name) => policy
                .providers
                .get(name)
                .ok_or_else(|| anyhow::anyhow!("Provider egress policy unavailable"))?,
            None => &policy.global,
        };
        Ok(match purpose {
            Purpose::Upstream => policy.clients[proxy].upstream.clone(),
            Purpose::OAuth => policy.clients[proxy].oauth.clone(),
            Purpose::Models => policy.models.clone(),
            Purpose::Webhook => policy.webhook.clone(),
        })
    }
    /// The global proxy URL for callers that build their own client (the MCP
    /// gateway pins resolved addresses per server). Includes credentials:
    /// never log or render it.
    pub(crate) fn global_proxy(&self) -> Option<String> {
        self.0.read().expect("egress policy").global.clone()
    }
    pub fn label(&self, provider: Option<&str>) -> Option<String> {
        let policy = self.0.read().expect("egress policy");
        let proxy = match provider {
            Some(name) => policy.providers.get(name)?,
            None => &policy.global,
        };
        proxy
            .as_ref()
            .map(|v| ProxyUrl::parse(v).expect("validated proxy").label())
    }
    pub fn connect_error(&self, provider: &str, error: &reqwest::Error) -> bool {
        use std::error::Error;
        // reqwest also marks an upstream TLS verification error as connect.
        // Rustls wraps those in InvalidData; do not call them proxy failures.
        let mut source = error.source();
        let mut invalid_tls = false;
        while let Some(error) = source {
            invalid_tls |= error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::InvalidData);
            source = error.source();
        }
        let proxy_connect =
            self.label(Some(provider)).is_some() && error.is_connect() && !invalid_tls;
        if proxy_connect {
            metrics::counter!("tokomotive_proxy_connect_errors_total", "provider" => provider.to_owned()).increment(1);
        }
        proxy_connect
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn connect_tunnel_sends_https_request_and_refuses_redirect() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = format!("http://user:pass@{}", listener.local_addr().unwrap());
        let cert = rcgen::generate_simple_self_signed(vec!["upstream.invalid".into()]).unwrap();
        let root = reqwest::Certificate::from_der(cert.cert.der()).unwrap();
        let key = tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer::from(
            cert.signing_key.serialize_der(),
        );
        let tls = tokio_rustls::rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.cert.der().clone()], key.into())
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut header = vec![];
            while !header.ends_with(b"\r\n\r\n") {
                header.push(socket.read_u8().await.unwrap());
            }
            assert!(header.starts_with(b"CONNECT upstream.invalid:443 HTTP/1.1\r\n"));
            socket
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
            let mut tls = acceptor.accept(socket).await.unwrap();
            for _ in 0..2 {
                let mut request = vec![];
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(tls.read_u8().await.unwrap());
                }
                let request = String::from_utf8(request).unwrap();
                assert!(request.starts_with("GET /test "));
                assert!(!request.to_lowercase().contains("proxy-authorization"));
                tls.write_all(b"HTTP/1.1 302 Found\r\nContent-Length: 0\r\nLocation: https://redirect.invalid/\r\n\r\n").await.unwrap();
            }
        });
        // A local CA is added only at this test seam. Production always uses
        // the standard trust roots, as checked by the integration test.
        let http = builder(Some(&proxy), 5, Purpose::Upstream)
            .unwrap()
            .add_root_certificate(root)
            .build()
            .unwrap();
        let mut policy = Policy::new(Some(proxy.clone()), HashMap::new(), 5).unwrap();
        policy.clients.get_mut(&Some(proxy)).unwrap().upstream = http;
        let egress = Egress::new(policy);
        for _ in 0..2 {
            let response = egress
                .client(None, Purpose::Upstream)
                .unwrap()
                .get("https://upstream.invalid/test")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 302);
            assert!(response.bytes().await.unwrap().is_empty());
        }
        // Both requests must reuse the same TCP/TLS tunnel.
        task.await.unwrap();
    }
}
