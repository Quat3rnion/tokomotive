//! Bound all untrusted transport input; never follow redirects or MCP URLs.
//! https://modelcontextprotocol.io/specification/2025-06-18/basic/transports
use super::{VERSION, data::Server};
use crate::state::AppState;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    time::Duration,
};

pub const JSON_CAP: usize = 4 * 1024 * 1024;
const EVENT_CAP: usize = 1024 * 1024;
pub const MAX_TOOLS: usize = 2048;
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Failure(pub &'static str);
type Result<T> = std::result::Result<T, Failure>;

pub fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
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
                && !(a == 198 && (b == 18 || b == 19))
                && !(a == 192 && b == 0)
        }
        IpAddr::V6(v) => v
            .to_ipv4_mapped()
            .map(|v| public(v.into()))
            .unwrap_or_else(|| {
                let s = v.segments();
                s[0] & 0xe000 == 0x2000
                    && s[0] != 0x2002
                    && !(s[0] == 0x2001 && (s[1] < 0x200 || s[1] == 0xdb8))
            }),
    }
}
pub fn valid_url(raw: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(raw).map_err(|_| Failure("invalid_url"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.as_str().len() > 4096
    {
        return Err(Failure("invalid_url"));
    }
    Ok(url)
}
async fn client(server: &Server) -> Result<(reqwest::Client, reqwest::Url)> {
    let url = valid_url(&server.url)?;
    let host = url
        .host_str()
        .ok_or(Failure("invalid_url"))?
        .trim_matches(['[', ']']);
    let port = url.port_or_known_default().ok_or(Failure("invalid_url"))?;
    let addresses: Vec<SocketAddr> = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::lookup_host((host, port)),
    )
    .await
    .map_err(|_| Failure("dns_timeout"))?
    .map_err(|_| Failure("dns"))?
    .take(33)
    .collect();
    if addresses.is_empty()
        || addresses.len() > 32
        || addresses.iter().any(|a| {
            let ip = a.ip();
            ip.is_unspecified()
                || ip.is_multicast()
                || (!public(ip) && !server.allow_private)
                || (url.scheme() == "http" && (!server.allow_private || public(ip)))
        })
    {
        return Err(Failure("address_denied"));
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
        .resolve_to_addrs(host, &addresses)
        .build()
        .map_err(|_| Failure("client"))?;
    Ok((client, url))
}
#[derive(Default)]
pub struct Connection {
    config: Option<Server>,
    session: Option<String>,
    sequence: u64,
}
impl Connection {
    async fn send(
        &mut self,
        st: &AppState,
        server: &Server,
        method: &str,
        params: Value,
        notification: bool,
    ) -> Result<Value> {
        let (client, url) = client(server).await?;
        self.sequence += 1;
        let id = self.sequence;
        let mut body = json!({"jsonrpc":"2.0","method":method,"params":params});
        if !notification {
            body["id"] = json!(id);
        }
        let mut req = client
            .post(url)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", VERSION)
            .json(&body);
        if let Some(session) = &self.session {
            req = req.header("mcp-session-id", session);
        }
        if server.auth != "none" {
            let secret = st
                .secret_key
                .decrypt(server.secret.as_deref().ok_or(Failure("credentials"))?)
                .map_err(|_| Failure("credentials"))?;
            req = match server.auth.as_str() {
                "bearer" => req.bearer_auth(secret),
                "header" if valid_header(&server.header_name) => {
                    req.header(&server.header_name, secret)
                }
                _ => return Err(Failure("credentials")),
            };
        }
        let response = req.send().await.map_err(|_| Failure("transport"))?;
        if !response.status().is_success() {
            if response.status() == 404 {
                self.session = None;
                self.config = None;
            }
            return Err(Failure("upstream_http"));
        }
        if method == "initialize" {
            self.session = response
                .headers()
                .get("mcp-session-id")
                .map(|h| {
                    h.to_str()
                        .map(str::to_owned)
                        .map_err(|_| Failure("session"))
                })
                .transpose()?;
            if self.session.as_ref().is_some_and(|s| {
                s.is_empty() || s.len() > 256 || !s.bytes().all(|b| (0x21..=0x7e).contains(&b))
            }) {
                return Err(Failure("session"));
            }
        }
        if notification {
            return Ok(Value::Null);
        }
        decode(response, id).await
    }
    pub async fn request(
        &mut self,
        st: &AppState,
        server: &Server,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        if self.config.as_ref() != Some(server) {
            self.session = None;
            self.config = None;
            let initialized = self.send(st,server,"initialize",json!({"protocolVersion":VERSION,"capabilities":{},"clientInfo":{"name":"Tokomotive","version":env!("CARGO_PKG_VERSION")}}),false).await?;
            if initialized["result"]["protocolVersion"] != VERSION
                || !initialized["result"]["capabilities"]["tools"].is_object()
            {
                return Err(Failure("protocol"));
            }
            self.send(st, server, "notifications/initialized", json!({}), true)
                .await?;
            self.config = Some(server.clone());
        }
        self.send(st, server, method, params, false).await
    }
    pub async fn tools(&mut self, st: &AppState, server: &Server) -> Result<Vec<Value>> {
        let mut tools = Vec::new();
        let mut cursor = None;
        let mut seen = HashSet::new();
        let mut names = HashSet::new();
        let mut bytes = 0;
        for _ in 0..16 {
            let params = cursor
                .as_ref()
                .map(|c| json!({"cursor":c}))
                .unwrap_or(json!({}));
            let response = self.request(st, server, "tools/list", params).await?;
            let page = response["result"]["tools"]
                .as_array()
                .ok_or(Failure("protocol"))?;
            for tool in page {
                let name = tool["name"].as_str().ok_or(Failure("tool_name"))?;
                if !super::data::valid_name(name, 128)
                    || !names.insert(name.to_owned())
                    || !tool["inputSchema"].is_object()
                {
                    return Err(Failure("tool_name"));
                }
                bytes += tool.to_string().len();
                if tools.len() >= MAX_TOOLS || bytes > JSON_CAP {
                    return Err(Failure("tools_cap"));
                }
                tools.push(tool.clone());
            }
            match response["result"].get("nextCursor") {
                None => return Ok(tools),
                Some(Value::String(c))
                    if !c.is_empty() && c.len() <= 4096 && seen.insert(c.clone()) =>
                {
                    cursor = Some(c.clone())
                }
                _ => return Err(Failure("pagination")),
            }
        }
        Err(Failure("pagination"))
    }
}
pub fn valid_header(name: &str) -> bool {
    reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_ok()
        && ![
            "authorization",
            "host",
            "connection",
            "te",
            "trailer",
            "upgrade",
            "keep-alive",
            "proxy-connection",
            "content-type",
            "content-length",
            "accept",
            "transfer-encoding",
            "proxy-authorization",
            "cookie",
            "mcp-session-id",
            "mcp-protocol-version",
        ]
        .iter()
        .any(|n| name.eq_ignore_ascii_case(n))
        && !name.to_ascii_lowercase().starts_with("sec-")
}
fn rpc(raw: &[u8], id: u64) -> Result<Option<Value>> {
    let value: Value = serde_json::from_slice(raw).map_err(|_| Failure("invalid_json"))?;
    if value["jsonrpc"] != "2.0" {
        return Err(Failure("protocol"));
    }
    if value.get("id") == Some(&json!(id)) {
        if value.get("result").is_some() == value.get("error").is_some()
            || value.get("method").is_some()
        {
            return Err(Failure("protocol"));
        }
        return Ok(Some(value));
    }
    // Notifications and server requests are not executed or relayed.
    Ok(None)
}
async fn decode(response: reqwest::Response, id: u64) -> Result<Value> {
    let content = response
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let sse = content
        .split(';')
        .next()
        .is_some_and(|s| s.trim() == "text/event-stream");
    if !sse
        && content
            .split(';')
            .next()
            .is_none_or(|s| s.trim() != "application/json")
    {
        return Err(Failure("content_type"));
    }
    if response
        .content_length()
        .is_some_and(|n| n > JSON_CAP as u64)
    {
        return Err(Failure("response_cap"));
    }
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut total = 0;
    let mut events = 0;
    let mut scan = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| Failure("transport"))?;
        total += chunk.len();
        if total > JSON_CAP {
            return Err(Failure("response_cap"));
        }
        buffer.extend_from_slice(&chunk);
        if sse {
            while let Some((end, width)) = buffer[scan..]
                .windows(2)
                .position(|w| w == b"\n\n")
                .map(|p| (scan + p, 2))
                .into_iter()
                .chain(
                    buffer[scan..]
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .map(|p| (scan + p, 4)),
                )
                .min_by_key(|(p, _)| *p)
            {
                if end > EVENT_CAP {
                    return Err(Failure("event_cap"));
                }
                events += 1;
                if events > 128 {
                    return Err(Failure("events_cap"));
                }
                let event =
                    std::str::from_utf8(&buffer[..end]).map_err(|_| Failure("invalid_utf8"))?;
                let data = event
                    .lines()
                    .filter_map(|l| {
                        l.strip_prefix("data:")
                            .map(|s| s.strip_prefix(' ').unwrap_or(s))
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !data.is_empty()
                    && let Some(v) = rpc(data.as_bytes(), id)?
                {
                    return Ok(v);
                }
                buffer.drain(..end + width);
                scan = 0;
            }
            scan = buffer.len().saturating_sub(3);
            if buffer.len() > EVENT_CAP {
                return Err(Failure("event_cap"));
            }
        }
    }
    if sse {
        return Err(Failure("missing_response"));
    }
    rpc(&buffer, id)?.ok_or(Failure("missing_response"))
}
