use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::json;

fn text<'a>(v: &'a Value, field: &str) -> Result<&'a str> {
    v.get(field)
        .and_then(Value::as_str)
        .ok_or(Failure("secret field unavailable"))
}
fn endpoint(store: &Store, path: &str) -> Result<reqwest::Url> {
    let mut url =
        reqwest::Url::parse(&store.address).map_err(|_| Failure("invalid store address"))?;
    if !matches!(url.scheme(), "https" | "http")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Failure("invalid store address"));
    }
    let base = url.path().trim_end_matches('/').to_owned();
    url.set_path(&format!("{base}/{path}"));
    Ok(url)
}
pub async fn validate_address(store: &Store, egress: &Egress) -> Result<()> {
    client(store, egress, None, &endpoint(store, "")?)
        .await
        .map(|_| ())
}
async fn client(
    store: &Store,
    egress: &Egress,
    provider: Option<&str>,
    url: &reqwest::Url,
) -> Result<reqwest::Client> {
    let builder = direct_builder(url, store.allow_private).await?;
    let builder = if let Some(proxy) = egress
        .proxy_for(provider)
        .map_err(|_| Failure("egress unavailable"))?
    {
        builder.proxy(reqwest::Proxy::all(proxy).map_err(|_| Failure("egress unavailable"))?)
    } else {
        builder
    };
    builder
        .build()
        .map_err(|_| Failure("secret client unavailable"))
}

/// Direct, DNS-pinned HTTPS transport, shared with dashboard OIDC.
pub(crate) async fn direct_client(
    url: &reqwest::Url,
    allow_private: bool,
) -> Result<reqwest::Client> {
    direct_builder(url, allow_private)
        .await?
        .build()
        .map_err(|_| Failure("client unavailable"))
}
async fn direct_builder(url: &reqwest::Url, allow_private: bool) -> Result<reqwest::ClientBuilder> {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.as_str().len() > 8192
    {
        return Err(Failure("invalid store address"));
    }
    let host = url
        .host_str()
        .ok_or(Failure("invalid store address"))?
        .trim_matches(['[', ']']);
    let port = url
        .port_or_known_default()
        .ok_or(Failure("invalid store address"))?;
    let addresses: Vec<_> = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::lookup_host((host, port)),
    )
    .await
    .map_err(|_| Failure("store DNS timeout"))?
    .map_err(|_| Failure("store DNS unavailable"))?
    .take(33)
    .collect();
    fn private(ip: std::net::IpAddr) -> bool {
        match ip {
            std::net::IpAddr::V4(v) => v.is_private() || v.is_loopback(),
            std::net::IpAddr::V6(v) => v
                .to_ipv4_mapped()
                .map(|v| private(v.into()))
                .unwrap_or(v.is_loopback() || v.is_unique_local()),
        }
    }
    fn public(ip: std::net::IpAddr) -> bool {
        match ip {
            std::net::IpAddr::V4(v) => {
                let [a, b, _, _] = v.octets();
                !v.is_private()
                    && !v.is_loopback()
                    && !v.is_link_local()
                    && !v.is_multicast()
                    && !v.is_unspecified()
                    && !v.is_broadcast()
                    && !v.is_documentation()
                    && a != 0
                    && a < 224
                    && !(a == 100 && (64..128).contains(&b))
                    && !(a == 198 && matches!(b, 18 | 19))
                    && !(a == 192 && b == 0)
            }
            std::net::IpAddr::V6(v) => {
                v.to_ipv4_mapped()
                    .map(|v| public(v.into()))
                    .unwrap_or_else(|| {
                        let s = v.segments();
                        s[0] & 0xe000 == 0x2000
                            && s[0] != 0x2002
                            && !(s[0] == 0x2001 && (s[1] < 0x200 || s[1] == 0xdb8))
                    })
            }
        }
    }
    if addresses.is_empty()
        || addresses.len() > 32
        || addresses.iter().any(|a| {
            a.ip().is_unspecified()
                || a.ip().is_multicast()
                || (!public(a.ip()) && !allow_private)
                || (url.scheme() == "http" && !(allow_private && private(a.ip())))
        })
    {
        return Err(Failure(
            "store address denied: use HTTPS; explicitly allow private addresses",
        ));
    }
    Ok(reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .resolve_to_addrs(host, &addresses))
}
async fn send(request: reqwest::RequestBuilder) -> Result<Value> {
    let response = request
        .send()
        .await
        .map_err(|_| Failure("secret request unavailable"))?;
    if !response.status().is_success() {
        return Err(Failure("secret unavailable"));
    }
    if response.content_length().is_some_and(|n| n > 1024 * 1024) {
        return Err(Failure("secret response too large"));
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| Failure("secret response unavailable"))?;
        if bytes.len() + chunk.len() > 1024 * 1024 {
            return Err(Failure("secret response too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| Failure("invalid secret response"))
}
fn header(value: &str) -> Result<reqwest::header::HeaderValue> {
    let mut value = reqwest::header::HeaderValue::from_str(value)
        .map_err(|_| Failure("invalid secret authentication"))?;
    value.set_sensitive(true);
    Ok(value)
}
fn select(value: String, field: Option<&str>) -> Result<SecretValue> {
    let value = if let Some(field) = field {
        let v: Value = serde_json::from_str(&value).map_err(|_| Failure("secret is not JSON"))?;
        text(&v, field)?.to_owned()
    } else {
        value
    };
    if value.trim().is_empty() || value.len() > 65536 {
        return Err(Failure("secret value unavailable"));
    }
    Ok(SecretValue(value))
}

pub async fn resolve(
    store: &Store,
    key: &SecretKey,
    egress: &Egress,
    provider: Option<&str>,
    reference: &str,
) -> Result<SecretValue> {
    let r = Reference::parse(reference)?;
    if r.kind != store.kind || r.store.is_some_and(|s| s != store.name) {
        return Err(Failure("wrong secret store"));
    }
    let auth = store.auth(key)?;
    match store.kind.as_str() {
        "vault" => {
            let data = vault(store, &auth, egress, provider, &r, false).await?;
            let field = r.field.ok_or(Failure("choose a Vault field"))?;
            select(text(&data, field)?.to_owned(), None)
        }
        "bw" => {
            let url = endpoint(
                store,
                &format!("object/item/{}", crate::sigv4::encode(r.path, false)),
            )?;
            let c = client(store, egress, provider, &url).await?;
            let status = send(c.get(endpoint(store, "status")?)).await?;
            if status
                .pointer("/data/template/status")
                .or_else(|| status.pointer("/data/status"))
                .and_then(Value::as_str)
                == Some("locked")
            {
                return Err(Failure("vault locked"));
            }
            let item = send(c.get(url))
                .await
                .map_err(|_| Failure("vault locked or item unavailable"))?;
            let item = item
                .pointer("/data/template")
                .or_else(|| item.get("data"))
                .unwrap_or(&item);
            let field = r.field.ok_or(Failure("choose an item field"))?;
            let value = match field {
                "password" | "username" => item
                    .get("login")
                    .and_then(|v| v.get(field))
                    .and_then(Value::as_str),
                "notes" => item.get("notes").and_then(Value::as_str),
                "uri" => item.pointer("/login/uris/0/uri").and_then(Value::as_str),
                "totp" => {
                    let v = send(c.get(endpoint(
                        store,
                        &format!("object/totp/{}", crate::sigv4::encode(r.path, false)),
                    )?))
                    .await?;
                    return select(
                        v.pointer("/data/data")
                            .or_else(|| v.get("data"))
                            .and_then(Value::as_str)
                            .ok_or(Failure("TOTP unavailable"))?
                            .into(),
                        None,
                    );
                }
                other => item
                    .get("fields")
                    .and_then(Value::as_array)
                    .and_then(|f| f.iter().find(|v| v["name"] == other))
                    .and_then(|v| v["value"].as_str()),
            }
            .ok_or(Failure("item field unavailable"))?;
            select(value.into(), None)
        }
        "aws-sm" => aws_resolve(store, &auth, egress, provider, &r).await,
        "gcp-sm" => gcp_resolve(store, &auth, egress, provider, &r).await,
        "bws" => bws_resolve(store, &auth, egress, provider, &r).await,
        _ => Err(Failure("unknown secret store")),
    }
}

async fn vault(
    store: &Store,
    auth: &Value,
    egress: &Egress,
    provider: Option<&str>,
    r: &Reference<'_>,
    list: bool,
) -> Result<Value> {
    let (mount, path) = r.path.split_once('/').unwrap_or((r.path, ""));
    let v1 = store.option("kv_version").as_deref() == Some("1");
    let path = if v1 {
        format!("v1/{mount}/{path}")
    } else {
        format!(
            "v1/{mount}/{}/{path}",
            if list { "metadata" } else { "data" }
        )
    };
    let url = endpoint(store, &path)?;
    let c = client(store, egress, provider, &url).await?;
    let namespace = store.option("namespace");
    let token = if let Some(token) = auth.get("token").and_then(Value::as_str) {
        token.to_owned()
    } else {
        let login = endpoint(store, "v1/auth/approle/login")?;
        let mut req = c
            .post(login)
            .json(&json!({"role_id":text(auth,"role_id")?,"secret_id":text(auth,"secret_id")?}));
        if let Some(ns) = &namespace {
            req = req.header("x-vault-namespace", header(ns)?);
        }
        let value = send(req).await?;
        value
            .pointer("/auth/client_token")
            .and_then(Value::as_str)
            .ok_or(Failure("Vault login unavailable"))?
            .to_owned()
    };
    let mut req = if list {
        c.request(reqwest::Method::from_bytes(b"LIST").expect("LIST"), url)
    } else {
        c.get(url)
    };
    req = req.header("x-vault-token", header(&token)?);
    if let Some(ns) = &namespace {
        req = req.header("x-vault-namespace", header(ns)?);
    }
    let value = send(req).await?;
    value
        .pointer(if v1 || list { "/data" } else { "/data/data" })
        .cloned()
        .ok_or(Failure("Vault secret unavailable"))
}

async fn aws(
    store: &Store,
    auth: &Value,
    egress: &Egress,
    provider: Option<&str>,
    region: &str,
    action: &str,
    body: Value,
) -> Result<Value> {
    crate::sigv4::host(region).map_err(|_| Failure("invalid AWS region"))?;
    let url = endpoint(store, "")?;
    let c = client(store, egress, provider, &url).await?;
    let body = serde_json::to_vec(&body).map_err(|_| Failure("invalid AWS request"))?;
    let host = format!(
        "{}{}",
        url.host_str().ok_or(Failure("invalid AWS host"))?,
        url.port().map(|p| format!(":{p}")).unwrap_or_default()
    );
    let mut headers = std::collections::BTreeMap::from([
        ("host".into(), host),
        ("content-type".into(), "application/x-amz-json-1.1".into()),
        (
            "x-amz-date".into(),
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string(),
        ),
        ("x-amz-target".into(), format!("secretsmanager.{action}")),
    ]);
    let creds = crate::sigv4::Credentials::new(
        text(auth, "access_key")?,
        text(auth, "secret_key")?,
        auth.get("session_token")
            .and_then(Value::as_str)
            .map(str::to_owned),
    );
    if let Some(token) = &creds.session_token {
        headers.insert("x-amz-security-token".into(), token.clone());
    }
    let signature = crate::sigv4::sign(
        &creds,
        region,
        "secretsmanager",
        "POST",
        url.path(),
        "",
        &headers,
        &body,
    )
    .map_err(|_| Failure("AWS signing unavailable"))?;
    let mut req = c.post(url).body(body);
    for (name, value) in headers {
        req = req.header(name, header(&value)?);
    }
    send(req.header("authorization", header(&signature.authorization)?)).await
}
async fn aws_resolve(
    store: &Store,
    auth: &Value,
    egress: &Egress,
    provider: Option<&str>,
    r: &Reference<'_>,
) -> Result<SecretValue> {
    let region = if r.path.starts_with("arn:") {
        r.path.split(':').nth(3).map(str::to_owned)
    } else {
        store.option("region")
    }
    .ok_or(Failure("AWS region required"))?;
    let value = aws(
        store,
        auth,
        egress,
        provider,
        &region,
        "GetSecretValue",
        json!({"SecretId":r.path}),
    )
    .await?;
    let plain = if let Some(s) = value.get("SecretString").and_then(Value::as_str) {
        s.to_owned()
    } else {
        String::from_utf8(
            STANDARD
                .decode(text(&value, "SecretBinary")?)
                .map_err(|_| Failure("invalid secret encoding"))?,
        )
        .map_err(|_| Failure("secret is not text"))?
    };
    select(plain, r.field)
}

async fn gcp_token(
    store: &Store,
    auth: &Value,
    egress: &Egress,
    provider: Option<&str>,
) -> Result<String> {
    if auth.get("service_account").is_some() {
        let sa: Value = serde_json::from_str(text(auth, "service_account")?)
            .map_err(|_| Failure("invalid service account"))?;
        let raw = store
            .option("token_url")
            .unwrap_or_else(|| "https://oauth2.googleapis.com/token".into());
        let mut token_store = store.clone();
        token_store.address = raw;
        let url = reqwest::Url::parse(token_store.address.trim_end_matches('/'))
            .map_err(|_| Failure("invalid GCP token address"))?;
        let c = client(&token_store, egress, provider, &url).await?;
        let now = chrono::Utc::now().timestamp();
        let claims = json!({"iss":text(&sa,"client_email")?,"scope":"https://www.googleapis.com/auth/cloud-platform","aud":url.as_str().trim_end_matches('/'),"iat":now,"exp":now+3600});
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(text(&sa, "private_key")?.as_bytes())
            .map_err(|_| Failure("invalid service account key"))?;
        let jwt = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
            &claims,
            &key,
        )
        .map_err(|_| Failure("service account signing unavailable"))?;
        let v = send(c.post(url).form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", jwt.as_str()),
        ]))
        .await?;
        Ok(text(&v, "access_token")?.into())
    } else {
        // The metadata exception is fixed to GCP's link-local endpoint; it is
        // never taken from a secret reference or an upstream response.
        let url = reqwest::Url::parse(
            "http://169.254.169.254/computeMetadata/v1/instance/service-accounts/default/token",
        )
        .expect("metadata URL");
        if !store.allow_private {
            return Err(Failure("explicitly allow GCP metadata access"));
        }
        let c = egress
            .client(provider, crate::egress::Purpose::OAuth)
            .map_err(|_| Failure("egress unavailable"))?;
        let v = send(
            c.get(url)
                .timeout(Duration::from_secs(10))
                .header("metadata-flavor", "Google"),
        )
        .await?;
        Ok(text(&v, "access_token")?.into())
    }
}
fn gcp_path(path: &str) -> Result<String> {
    let p: Vec<_> = path.split('/').collect();
    if !(p.len() == 4 || p.len() == 6)
        || p[0] != "projects"
        || p[2] != "secrets"
        || (p.len() == 6 && p[4] != "versions")
        || p.iter().any(|s| {
            s.is_empty()
                || !s
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_/".contains(&c))
        })
    {
        return Err(Failure("invalid GCP secret path"));
    }
    Ok(if p.len() == 4 {
        format!("{path}/versions/latest")
    } else {
        path.into()
    })
}
async fn gcp_resolve(
    store: &Store,
    auth: &Value,
    egress: &Egress,
    provider: Option<&str>,
    r: &Reference<'_>,
) -> Result<SecretValue> {
    let path = gcp_path(r.path)?;
    let url = endpoint(store, &format!("v1/{path}:access"))?;
    let c = client(store, egress, provider, &url).await?;
    let token = gcp_token(store, auth, egress, provider).await?;
    let v = send(
        c.get(url)
            .header("authorization", header(&format!("Bearer {token}"))?),
    )
    .await?;
    let bytes = STANDARD
        .decode(
            v.pointer("/payload/data")
                .and_then(Value::as_str)
                .ok_or(Failure("GCP payload unavailable"))?,
        )
        .map_err(|_| Failure("invalid GCP payload"))?;
    select(
        String::from_utf8(bytes).map_err(|_| Failure("GCP secret is not text"))?,
        r.field,
    )
}

fn bws_api(store: &Store, path: &str) -> Result<reqwest::Url> {
    if let Some(address) = store.option("api_url") {
        return endpoint(
            &Store {
                address,
                ..store.clone()
            },
            path,
        );
    }
    let cloud = matches!(
        reqwest::Url::parse(&store.address)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .as_deref(),
        Some("api.bitwarden.com" | "api.bitwarden.eu")
    );
    endpoint(store, &format!("{}{path}", if cloud { "" } else { "api/" }))
}
async fn bws_login(
    store: &Store,
    auth: &Value,
    egress: &Egress,
    provider: Option<&str>,
) -> Result<(String, Vec<u8>, String)> {
    use super::bws;
    let raw = text(auth, "access_token")?;
    let (identity, seed) = raw
        .split_once(':')
        .ok_or(Failure("invalid BWS access token"))?;
    let p: Vec<_> = identity.split('.').collect();
    if p.len() != 3 || p[0] != "0" || p[1].is_empty() || p[2].is_empty() {
        return Err(Failure("invalid BWS access token"));
    }
    let key = bws::derive(&bws::decode(seed)?)?;
    let identity_url = store.option("identity_url").unwrap_or_else(|| {
        if store.address == "https://api.bitwarden.com" {
            "https://identity.bitwarden.com/connect/token".into()
        } else if store.address == "https://api.bitwarden.eu" {
            "https://identity.bitwarden.eu/connect/token".into()
        } else {
            format!(
                "{}/identity/connect/token",
                store.address.trim_end_matches('/')
            )
        }
    });
    let mut identity_store = store.clone();
    identity_store.address = identity_url;
    let url = endpoint(&identity_store, "")?;
    // Do not add a trailing slash to the identity route.
    let url = reqwest::Url::parse(url.as_str().trim_end_matches('/'))
        .map_err(|_| Failure("invalid BWS identity address"))?;
    let c = client(&identity_store, egress, provider, &url).await?;
    let v = send(c.post(url).form(&[
        ("grant_type", "client_credentials"),
        ("scope", "api.secrets"),
        ("client_id", p[1]),
        ("client_secret", p[2]),
    ]))
    .await?;
    let payload = bws::decrypt(&key, text(&v, "encrypted_payload")?)?;
    let payload: Value =
        serde_json::from_slice(&payload).map_err(|_| Failure("invalid BWS payload"))?;
    let org_key = bws::decode(text(&payload, "encryptionKey")?)?;
    if org_key.len() != 64 {
        return Err(Failure("invalid BWS organization key"));
    }
    let token = text(&v, "access_token")?.to_owned();
    let jwt = token
        .split('.')
        .nth(1)
        .ok_or(Failure("invalid BWS login"))?;
    let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(jwt)
        .map_err(|_| Failure("invalid BWS login"))?;
    let claims: Value =
        serde_json::from_slice(&claims).map_err(|_| Failure("invalid BWS login"))?;
    Ok((token, org_key, text(&claims, "organization")?.into()))
}
async fn bws_resolve(
    store: &Store,
    auth: &Value,
    egress: &Egress,
    provider: Option<&str>,
    r: &Reference<'_>,
) -> Result<SecretValue> {
    let (token, key, _) = bws_login(store, auth, egress, provider).await?;
    let url = bws_api(
        store,
        &format!("secrets/{}", crate::sigv4::encode(r.path, false)),
    )?;
    let c = client(store, egress, provider, &url).await?;
    let v = send(
        c.get(url)
            .header("authorization", header(&format!("Bearer {token}"))?),
    )
    .await?;
    let bytes = super::bws::decrypt(&key, text(&v, "value")?)?;
    select(
        String::from_utf8(bytes).map_err(|_| Failure("BWS secret is not text"))?,
        r.field,
    )
}

#[derive(Serialize)]
pub struct Choice {
    pub id: String,
    pub label: String,
}
#[derive(Serialize)]
pub struct Page {
    pub items: Vec<Choice>,
    pub next: Option<String>,
}
pub async fn browse(
    store: &Store,
    key: &SecretKey,
    egress: &Egress,
    path: &str,
    cursor: &str,
    q: &str,
) -> Result<Page> {
    if cursor.len() > 2048 || q.len() > 200 || path.len() > 2048 {
        return Err(Failure("picker query too long"));
    }
    let auth = store.auth(key)?;
    let prefix = format!("{}@{}:", store.kind, store.name);
    let mut items = Vec::new();
    let mut next = None;
    let mut local = true;
    match store.kind.as_str() {
        "vault" => {
            let root = if path.is_empty() {
                store.option("browse_root").unwrap_or_else(|| "kv".into())
            } else {
                path.into()
            };
            Reference::parse(&format!("{prefix}{}", root.trim_end_matches('/')))?;
            let reference = format!("{prefix}{}/_", root.trim_end_matches('/'));
            let mut r = Reference::parse(&reference)?;
            r.path = root.trim_end_matches('/');
            let v = vault(store, &auth, egress, None, &r, true).await?;
            for name in v
                .get("keys")
                .and_then(Value::as_array)
                .ok_or(Failure("Vault list unavailable"))?
            {
                if let Some(name) = name.as_str() {
                    items.push(Choice {
                        id: format!("{prefix}{}/{name}", root.trim_end_matches('/')),
                        label: name.into(),
                    });
                }
            }
        }
        "aws-sm" => {
            local = false;
            let mut body = json!({"MaxResults":20});
            if !cursor.is_empty() {
                body["NextToken"] = json!(cursor);
            }
            if !q.is_empty() {
                body["Filters"] = json!([{"Key":"name","Values":[q]}]);
            }
            let region = store
                .option("region")
                .ok_or(Failure("set the store AWS region to browse"))?;
            let v = aws(store, &auth, egress, None, &region, "ListSecrets", body).await?;
            next = v
                .get("NextToken")
                .and_then(Value::as_str)
                .map(str::to_owned);
            for s in v
                .get("SecretList")
                .and_then(Value::as_array)
                .ok_or(Failure("AWS list unavailable"))?
            {
                items.push(Choice {
                    id: format!("{prefix}{}", text(s, "ARN")?),
                    label: text(s, "Name")?.into(),
                });
            }
        }
        "gcp-sm" => {
            local = false;
            let project = store
                .option("project")
                .ok_or(Failure("set the store GCP project to browse"))?;
            if !project
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            {
                return Err(Failure("invalid GCP project"));
            }
            let mut url = endpoint(store, &format!("v1/projects/{project}/secrets"))?;
            url.query_pairs_mut().append_pair("pageSize", "20");
            if !cursor.is_empty() {
                url.query_pairs_mut().append_pair("pageToken", cursor);
            }
            if !q.is_empty() {
                url.query_pairs_mut()
                    .append_pair("filter", &format!("name:{q}"));
            }
            let c = client(store, egress, None, &url).await?;
            let token = gcp_token(store, &auth, egress, None).await?;
            let v = send(
                c.get(url)
                    .header("authorization", header(&format!("Bearer {token}"))?),
            )
            .await?;
            next = v
                .get("nextPageToken")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .map(str::to_owned);
            for s in v
                .get("secrets")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let name = text(s, "name")?;
                items.push(Choice {
                    id: format!("{prefix}{name}"),
                    label: name.into(),
                });
            }
        }
        "bws" => {
            let (token, key, org) = bws_login(store, &auth, egress, None).await?;
            let url = bws_api(
                store,
                &format!(
                    "organizations/{}/secrets",
                    crate::sigv4::encode(&org, false)
                ),
            )?;
            let c = client(store, egress, None, &url).await?;
            let v = send(
                c.get(url)
                    .header("authorization", header(&format!("Bearer {token}"))?),
            )
            .await?;
            for s in v
                .get("data")
                .and_then(Value::as_array)
                .ok_or(Failure("BWS list unavailable"))?
            {
                let name = String::from_utf8(super::bws::decrypt(&key, text(s, "key")?)?)
                    .map_err(|_| Failure("invalid BWS secret name"))?;
                items.push(Choice {
                    id: format!("{prefix}{}", text(s, "id")?),
                    label: name,
                });
            }
        }
        "bw" => {
            let mut url = endpoint(store, "list/object/items")?;
            url.query_pairs_mut().append_pair("search", q);
            let c = client(store, egress, None, &url).await?;
            let v = send(c.get(url))
                .await
                .map_err(|_| Failure("vault locked or list unavailable"))?;
            let list = v
                .pointer("/data/data")
                .or_else(|| v.get("data"))
                .and_then(Value::as_array)
                .ok_or(Failure("vault locked or list unavailable"))?;
            for s in list {
                items.push(Choice {
                    id: format!("{prefix}{}", text(s, "id")?),
                    label: text(s, "name")?.into(),
                });
            }
        }
        _ => return Err(Failure("unknown secret store")),
    }
    if local {
        items.retain(|s| s.label.to_lowercase().contains(&q.to_lowercase()));
        items.sort_by(|a, b| a.label.cmp(&b.label));
        let offset = if cursor.is_empty() {
            0
        } else {
            cursor
                .parse::<usize>()
                .map_err(|_| Failure("invalid picker cursor"))?
        };
        if items.len() > offset.saturating_add(20) {
            next = Some(offset.saturating_add(20).to_string());
        }
        items = items.into_iter().skip(offset).take(20).collect();
    }
    if items.len() > 20 || next.as_ref().is_some_and(|s| s.len() > 2048) {
        return Err(Failure("invalid picker page"));
    }
    Ok(Page { items, next })
}
pub async fn fields(
    store: &Store,
    key: &SecretKey,
    egress: &Egress,
    reference: &str,
) -> Result<Vec<String>> {
    let r = Reference::parse(reference)?;
    let auth = store.auth(key)?;
    if store.kind == "vault" {
        let data = vault(store, &auth, egress, None, &r, false).await?;
        return Ok(data
            .as_object()
            .ok_or(Failure("Vault fields unavailable"))?
            .keys()
            .take(200)
            .cloned()
            .collect());
    }
    if store.kind == "bw" {
        let url = endpoint(
            store,
            &format!("object/item/{}", crate::sigv4::encode(r.path, false)),
        )?;
        let c = client(store, egress, None, &url).await?;
        let item = send(c.get(url))
            .await
            .map_err(|_| Failure("vault locked or item unavailable"))?;
        let item = item
            .pointer("/data/template")
            .or_else(|| item.get("data"))
            .unwrap_or(&item);
        let mut fields = vec![
            "password".into(),
            "username".into(),
            "notes".into(),
            "totp".into(),
            "uri".into(),
        ];
        for field in item
            .get("fields")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .take(200)
        {
            if let Some(name) = field.get("name").and_then(Value::as_str) {
                fields.push(name.into());
            }
        }
        return Ok(fields);
    }
    let value = resolve(store, key, egress, None, reference).await?;
    let data: Value = serde_json::from_str(value.expose()).unwrap_or(Value::Null);
    let mut fields = vec![String::new()];
    if let Some(map) = data.as_object() {
        fields.extend(map.keys().take(200).cloned());
    }
    Ok(fields)
}
