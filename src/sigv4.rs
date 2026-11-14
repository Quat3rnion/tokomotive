//! AWS Signature Version 4, with double URI encoding (not S3).
//! https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-create-signed-request.html
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
}
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credentials([REDACTED])")
    }
}
impl Credentials {
    pub fn new(access_key: &str, secret_key: &str, session_token: Option<String>) -> Self {
        Self {
            access_key: access_key.into(),
            secret_key: secret_key.into(),
            session_token,
        }
    }
}
pub struct Signature {
    pub authorization: String,
    pub canonical_request: String,
    pub string_to_sign: String,
}
pub fn encode(value: &str, preserve_slash: bool) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'~')
            || (preserve_slash && b == b'/')
        {
            out.push(char::from(b));
        } else {
            use std::fmt::Write;
            write!(out, "%{b:02X}").expect("string write");
        }
    }
    out
}
pub fn hash(body: &[u8]) -> String {
    hex::encode(Sha256::digest(body))
}
fn mac(key: &[u8], message: &str) -> Vec<u8> {
    let mut h = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key");
    h.update(message.as_bytes());
    h.finalize().into_bytes().to_vec()
}
/// Headers must have lowercase names. The URL path is the encoded wire path;
/// query is already AWS-encoded and sorted (Bedrock has no query).
#[allow(clippy::too_many_arguments)]
pub fn sign(
    creds: &Credentials,
    region: &str,
    service: &str,
    method: &str,
    path: &str,
    query: &str,
    headers: &BTreeMap<String, String>,
    body: &[u8],
) -> Result<Signature, &'static str> {
    let date = headers.get("x-amz-date").ok_or("missing signing date")?;
    if date.len() != 16 || !date.is_ascii() || !date[..8].bytes().all(|b| b.is_ascii_digit()) {
        return Err("invalid signing date");
    }
    if creds.access_key.is_empty() || creds.secret_key.is_empty() || !headers.contains_key("host") {
        return Err("missing AWS credentials or host");
    }
    let mut canonical_headers = String::new();
    for (name, value) in headers {
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b == b'-' || b.is_ascii_digit())
            || value.contains(['\r', '\n'])
        {
            return Err("invalid signing header");
        }
        canonical_headers.push_str(&format!(
            "{name}:{}\n",
            value.split_whitespace().collect::<Vec<_>>().join(" ")
        ));
    }
    let signed_headers = headers.keys().cloned().collect::<Vec<_>>().join(";");
    let canonical_request = format!(
        "{method}\n{}\n{query}\n{canonical_headers}\n{signed_headers}\n{}",
        encode(path, true),
        hash(body)
    );
    let scope = format!("{}/{region}/{service}/aws4_request", &date[..8]);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{date}\n{scope}\n{}",
        hash(canonical_request.as_bytes())
    );
    let k = mac(format!("AWS4{}", creds.secret_key).as_bytes(), &date[..8]);
    let k = mac(&k, region);
    let k = mac(&k, service);
    let k = mac(&k, "aws4_request");
    let signature = hex::encode(mac(&k, &string_to_sign));
    Ok(Signature {
        authorization: format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            creds.access_key
        ),
        canonical_request,
        string_to_sign,
    })
}

/// Only a region is accepted, never an arbitrary host or URL.
pub fn host(region: &str) -> Result<String, &'static str> {
    let pieces: Vec<_> = region.split('-').collect();
    if region.len() > 40
        || pieces.len() < 3
        || pieces.len() > 5
        || pieces[0].len() != 2
        || !pieces[0].bytes().all(|b| b.is_ascii_lowercase())
        || !pieces[1..pieces.len() - 1]
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase()))
        || pieces
            .last()
            .is_none_or(|p| p.len() != 1 || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err("invalid AWS region");
    }
    Ok(format!("bedrock-runtime.{region}.amazonaws.com"))
}
pub fn region_from_base(base: &str) -> Result<&str, &'static str> {
    let region = base
        .strip_prefix("https://bedrock-runtime.")
        .and_then(|s| s.strip_suffix(".amazonaws.com"))
        .ok_or("Bedrock needs the fixed HTTPS runtime host")?;
    host(region)?;
    Ok(region)
}
/// Builds only the trusted host. No caller headers enter the signature.
pub fn request(
    client: &reqwest::Client,
    creds: &Credentials,
    region: &str,
    path: &str,
    body: &[u8],
    date: &str,
) -> Result<reqwest::RequestBuilder, &'static str> {
    let host = host(region)?;
    if !path.starts_with("/model/")
        || !(path.ends_with("/converse") || path.ends_with("/converse-stream"))
        || path.contains(['?', '#', '\r', '\n'])
    {
        return Err("invalid Bedrock path");
    }
    let mut headers = BTreeMap::from([
        ("host".into(), host.clone()),
        ("content-type".into(), "application/json".into()),
        ("x-amz-date".into(), date.into()),
        ("x-amz-content-sha256".into(), hash(body)),
    ]);
    if let Some(token) = &creds.session_token {
        headers.insert("x-amz-security-token".into(), token.clone());
    }
    let signature = sign(creds, region, "bedrock", "POST", path, "", &headers, body)?;
    let mut request = client.post(format!("https://{host}{path}"));
    for (name, value) in headers {
        let mut value =
            reqwest::header::HeaderValue::from_str(&value).map_err(|_| "invalid AWS header")?;
        value.set_sensitive(name == "x-amz-security-token");
        request = request.header(name, value);
    }
    let mut authorization = reqwest::header::HeaderValue::from_str(&signature.authorization)
        .map_err(|_| "invalid AWS Authorization")?;
    authorization.set_sensitive(true);
    Ok(request.header("authorization", authorization))
}
