use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokomotive::egress::ProxyUrl;

#[derive(Clone)]
struct LogBuffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn mock_http() -> (
    String,
    tokio::sync::mpsc::Receiver<String>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    let task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut data = vec![];
                loop {
                    let b = socket.read_u8().await.unwrap();
                    data.push(b);
                    if data.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                tx.send(String::from_utf8(data).unwrap()).await.unwrap();
                socket.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://never-follow.invalid/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            });
        }
    });
    (address, rx, task)
}

#[tokio::test]
async fn provider_override_none_and_reload_route_requests_without_redirects() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg: tokomotive::config::Config = toml::from_str("").unwrap();
    cfg.server.database = format!("sqlite://{}/db.sqlite", dir.path().display());
    let st = tokomotive::init(&cfg, None).await.unwrap();
    let (global, mut global_rx, global_task) = mock_http().await;
    let (custom, mut custom_rx, custom_task) = mock_http().await;
    let (direct, mut direct_rx, direct_task) = mock_http().await;
    for name in ["global", "custom", "direct"] {
        sqlx::query("INSERT INTO providers(name,kind,base_url,keys_json) VALUES(?, '\"openai\"', 'http://upstream.invalid', '[]')")
            .bind(name).execute(&st.db).await.unwrap();
    }
    tokomotive::settings::put(
        &st.db,
        "egress_proxy",
        &st.secret_key.encrypt(&global).unwrap(),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE providers SET proxy_mode='custom',proxy_url=? WHERE name='custom'")
        .bind(st.secret_key.encrypt(&custom).unwrap())
        .execute(&st.db)
        .await
        .unwrap();
    sqlx::query("UPDATE providers SET proxy_mode='none' WHERE name='direct'")
        .execute(&st.db)
        .await
        .unwrap();
    st.reload_settings().await.unwrap();
    assert_eq!(
        st.provider_http("global")
            .get("http://upstream.invalid/first")
            .send()
            .await
            .unwrap()
            .status(),
        302
    );
    assert!(
        global_rx
            .recv()
            .await
            .unwrap()
            .starts_with("GET http://upstream.invalid/first ")
    );
    assert_eq!(
        st.provider_http("custom")
            .get("http://upstream.invalid/second")
            .send()
            .await
            .unwrap()
            .status(),
        302
    );
    assert!(
        custom_rx
            .recv()
            .await
            .unwrap()
            .starts_with("GET http://upstream.invalid/second ")
    );
    st.provider_http("direct")
        .get(&direct)
        .send()
        .await
        .unwrap();
    assert!(direct_rx.recv().await.unwrap().starts_with("GET / "));
    sqlx::query("UPDATE providers SET proxy_mode='none' WHERE name='custom'")
        .execute(&st.db)
        .await
        .unwrap();
    st.reload_settings().await.unwrap();
    st.provider_http("custom")
        .get(&direct)
        .send()
        .await
        .unwrap();
    assert!(direct_rx.recv().await.unwrap().starts_with("GET / "));
    assert!(global_rx.try_recv().is_err());
    assert!(custom_rx.try_recv().is_err());
    global_task.abort();
    custom_task.abort();
    direct_task.abort();
}

#[tokio::test]
async fn proxy_forms_encrypt_and_redact_secrets_and_report_fixed_failures() {
    let logs = LogBuffer(Default::default());
    let writer = logs.clone();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .try_init()
        .unwrap();
    let metrics = tokomotive::metrics::install().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg: tokomotive::config::Config = toml::from_str("").unwrap();
    cfg.server.database = format!("sqlite://{}/db.sqlite", dir.path().display());
    let st = tokomotive::init(&cfg, None).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = tokomotive::router(st.clone());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let c = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    c.post(format!("{url}/setup"))
        .form(&[
            ("password", "test-password-long"),
            ("confirm", "test-password-long"),
            ("mode", "full"),
        ])
        .send()
        .await
        .unwrap();
    let response = c
        .post(format!("{url}/admin/login"))
        .form(&[("password", "test-password-long")])
        .send()
        .await
        .unwrap();
    let cookie = response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    sqlx::query("INSERT INTO providers(name,kind,base_url,keys_json) VALUES('p', '\"openai\"', 'http://upstream.invalid', '[]')").execute(&st.db).await.unwrap();
    let secret = st.secret_key.encrypt("key").unwrap();
    sqlx::query("UPDATE providers SET keys_json=? WHERE name='p'")
        .bind(serde_json::to_string(&[secret]).unwrap())
        .execute(&st.db)
        .await
        .unwrap();
    st.reload_settings().await.unwrap();
    for path in ["/admin/settings/egress", "/admin/providers/p/egress"] {
        let response = c
            .post(format!("{url}{path}"))
            .header("cookie", &cookie)
            .form(&[
                ("mode", "custom"),
                ("proxy_url", "http://PRIVATEUSER:PRIVATEPASS@127.0.0.1:1"),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 303, "{}", response.text().await.unwrap());
        let response = c
            .post(format!("{url}{path}"))
            .header("cookie", &cookie)
            .form(&[
                ("mode", "custom"),
                ("proxy_url", "http://PRIVATEUSER:PRIVATEPASS@host:80/path"),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        assert!(!response.text().await.unwrap().contains("PRIVATE"));
    }
    let raw: String = sqlx::query_scalar("SELECT proxy_url FROM providers WHERE name='p'")
        .fetch_one(&st.db)
        .await
        .unwrap();
    assert!(!raw.contains("PRIVATE"));
    assert_eq!(
        st.secret_key.decrypt(&raw).unwrap(),
        "http://PRIVATEUSER:PRIVATEPASS@127.0.0.1:1"
    );
    let global = tokomotive::settings::value(&st.db, "egress_proxy")
        .await
        .unwrap()
        .unwrap();
    assert!(!global.contains("PRIVATE"));
    for path in ["/admin/providers", "/admin/settings"] {
        let page = c
            .get(format!("{url}{path}"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(!page.contains("PRIVATE"), "{path}");
        assert!(page.contains("http://127.0.0.1:1"));
    }
    let result = c
        .post(format!("{url}/admin/providers/p/test"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(result.contains("via proxy 127.0.0.1:1"), "{result}");
    assert!(
        result.contains("Cannot connect to the egress proxy"),
        "{result}"
    );
    assert!(!result.contains("PRIVATE"));
    let output = metrics.render();
    assert!(
        output.contains("tokomotive_proxy_connect_errors_total{provider=\"p\"} 1"),
        "{output}"
    );
    assert!(!output.contains("PRIVATE"));
    assert!(
        !String::from_utf8_lossy(&logs.0.lock().unwrap()).contains("PRIVATE"),
        "Proxy credentials must not enter trace logs"
    );
    task.abort();
}

#[test]
fn proxy_urls_are_strict_and_display_never_contains_credentials() {
    for scheme in ["http", "https", "socks5", "socks5h"] {
        let proxy = ProxyUrl::parse(&format!(
            "{scheme}://PRIVATEUSER:PRIVATEPASS@localhost:1080"
        ))
        .unwrap();
        assert_eq!(proxy.label(), format!("{scheme}://localhost:1080"));
        assert!(!format!("{proxy:?}").contains("PRIVATE"));
    }
    for invalid in [
        "ftp://localhost:21",
        "http://localhost/path",
        "http://localhost/?x=y",
        "http://localhost/#secret",
        "http://localhost",
        "http://localhost:0",
        " http://localhost:80",
        "http://localhost:80\\path",
        "http://localhost:80/../",
        "http://localhost:80\n",
        "socks5://:1080",
    ] {
        let error = ProxyUrl::parse(invalid).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Use http, https, socks5 or socks5h with a host and port, without a path, query or fragment."
        );
    }
}

#[tokio::test]
async fn socks5_authentication_remote_dns_and_redirect_policy() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        for expected in ["upstream.invalid", "127.0.0.1"] {
            let (mut socket, _) = listener.accept().await.unwrap();
            assert_eq!(socket.read_u8().await.unwrap(), 5);
            let n = socket.read_u8().await.unwrap() as usize;
            let mut methods = vec![0; n];
            socket.read_exact(&mut methods).await.unwrap();
            assert!(methods.contains(&2));
            socket.write_all(&[5, 2]).await.unwrap();
            assert_eq!(socket.read_u8().await.unwrap(), 1);
            let n = socket.read_u8().await.unwrap() as usize;
            let mut user = vec![0; n];
            socket.read_exact(&mut user).await.unwrap();
            assert_eq!(user, b"PRIVATEUSER");
            let n = socket.read_u8().await.unwrap() as usize;
            let mut password = vec![0; n];
            socket.read_exact(&mut password).await.unwrap();
            assert_eq!(password, b"PRIVATEPASS");
            socket.write_all(&[1, 0]).await.unwrap();
            assert_eq!(socket.read_u8().await.unwrap(), 5);
            assert_eq!(socket.read_u8().await.unwrap(), 1);
            assert_eq!(socket.read_u8().await.unwrap(), 0);
            let host = match socket.read_u8().await.unwrap() {
                3 => {
                    let n = socket.read_u8().await.unwrap() as usize;
                    let mut host = vec![0; n];
                    socket.read_exact(&mut host).await.unwrap();
                    String::from_utf8(host).unwrap()
                }
                1 => {
                    let mut host = [0; 4];
                    socket.read_exact(&mut host).await.unwrap();
                    std::net::Ipv4Addr::from(host).to_string()
                }
                other => panic!("unexpected SOCKS address type {other}"),
            };
            assert_eq!(host, expected);
            assert_eq!(socket.read_u16().await.unwrap(), 80);
            socket
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
                .await
                .unwrap();
            let mut request = vec![];
            while !request.ends_with(b"\r\n\r\n") {
                request.push(socket.read_u8().await.unwrap());
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with("GET /test "));
            assert!(!request.contains("PRIVATE"));
            assert!(!request.to_lowercase().contains("proxy-authorization"));
            socket.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://never-follow.invalid/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        }
    });
    for (scheme, host) in [("socks5h", "upstream.invalid"), ("socks5", "127.0.0.1")] {
        let policy = tokomotive::egress::Policy::new(
            Some(format!("{scheme}://PRIVATEUSER:PRIVATEPASS@{address}")),
            Default::default(),
            5,
        )
        .unwrap();
        let egress = tokomotive::egress::Egress::new(policy);
        let r = egress
            .client(Some("p"), tokomotive::egress::Purpose::Upstream)
            .get(format!("http://{host}/test"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 302);
    }
    task.await.unwrap();
}

#[tokio::test]
async fn http_connect_auth_tunnels_and_tls_verification_stays_enabled() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let cert = rcgen::generate_simple_self_signed(vec!["upstream.invalid".into()]).unwrap();
    let key =
        tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());
    let tls = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert.der().clone()], key.into())
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(tls));
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut header = vec![];
        while !header.ends_with(b"\r\n\r\n") {
            header.push(socket.read_u8().await.unwrap());
        }
        let header = String::from_utf8(header).unwrap();
        assert!(header.starts_with("CONNECT upstream.invalid:443 HTTP/1.1\r\n"));
        use base64::Engine;
        let auth = base64::engine::general_purpose::STANDARD.encode("PRIVATEUSER:PRIVATEPASS");
        assert!(header.to_lowercase().contains(&format!(
            "proxy-authorization: basic {}",
            auth.to_lowercase()
        )));
        socket
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .unwrap();
        assert!(
            acceptor.accept(socket).await.is_err(),
            "Untrusted upstream certificate must be refused"
        );
    });
    let policy = tokomotive::egress::Policy::new(
        Some(format!("http://PRIVATEUSER:PRIVATEPASS@{address}")),
        Default::default(),
        5,
    )
    .unwrap();
    let egress = tokomotive::egress::Egress::new(policy);
    let error = egress
        .client(Some("p"), tokomotive::egress::Purpose::Upstream)
        .get("https://upstream.invalid/test")
        .send()
        .await
        .unwrap_err();
    assert!(!format!("{error:?}").contains("PRIVATE"));
    assert!(
        !egress.connect_error("p", &error),
        "An upstream TLS failure is not a proxy connect failure"
    );
    task.await.unwrap();
}

#[tokio::test]
async fn https_proxy_tls_failure_is_a_proxy_connection_failure() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let key =
        tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());
    let tls = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert.der().clone()], key.into())
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(tls));
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        assert!(acceptor.accept(socket).await.is_err());
    });
    let policy = tokomotive::egress::Policy::new(
        Some(format!("https://PRIVATEUSER:PRIVATEPASS@{address}")),
        Default::default(),
        5,
    )
    .unwrap();
    let egress = tokomotive::egress::Egress::new(policy);
    let error = egress
        .client(Some("p"), tokomotive::egress::Purpose::Upstream)
        .get("https://upstream.invalid/test")
        .send()
        .await
        .unwrap_err();
    assert!(egress.connect_error("https-proxy-tls-check", &error));
    assert!(!format!("{error:?}").contains("PRIVATE"));
    task.await.unwrap();
}

#[tokio::test]
async fn device_signin_and_token_refresh_use_provider_proxy() {
    use std::sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    };
    use tokomotive::oauth::{ClientOptions, Flow, OAuthEndpoints, OAuthManager};
    let dir = tempfile::tempdir().unwrap();
    let mut cfg: tokomotive::config::Config = toml::from_str("").unwrap();
    cfg.server.database = format!("sqlite://{}/db.sqlite", dir.path().display());
    let st = tokomotive::init(&cfg, None).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!(
        "http://PRIVATEUSER:PRIVATEPASS@{}",
        listener.local_addr().unwrap()
    );
    sqlx::query("INSERT INTO providers(name,kind,base_url,auth,proxy_mode,proxy_url) VALUES('hf','\"openai\"','https://router.huggingface.co','hugging_face','custom',?)")
        .bind(st.secret_key.encrypt(&proxy).unwrap()).execute(&st.db).await.unwrap();
    st.reload_settings().await.unwrap();
    let server = tokio::spawn(async move {
        for step in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut header = vec![];
            while !header.ends_with(b"\r\n\r\n") {
                header.push(socket.read_u8().await.unwrap());
            }
            let header = String::from_utf8(header).unwrap();
            let length: usize = header
                .lines()
                .find_map(|l| {
                    l.to_lowercase()
                        .strip_prefix("content-length: ")
                        .map(|v| v.parse().unwrap())
                })
                .unwrap();
            let mut body = vec![0; length];
            socket.read_exact(&mut body).await.unwrap();
            let body = String::from_utf8(body).unwrap();
            assert!(!body.contains("PRIVATE"));
            let response = if step == 0 {
                assert!(header.starts_with("POST http://issuer.invalid/device "));
                r#"{"device_code":"private-device","user_code":"PUBLIC","verification_uri":"https://huggingface.co/activate","interval":1,"expires_in":900}"#
            } else if step == 1 {
                assert!(header.starts_with("POST http://issuer.invalid/token "));
                assert!(body.contains("device_code=private-device"));
                r#"{"access_token":"access","refresh_token":"refresh","expires_in":3600}"#
            } else {
                assert!(header.starts_with("POST http://issuer.invalid/token "));
                assert!(body.contains("grant_type=refresh_token"));
                assert!(body.contains("refresh_token=refresh"));
                r#"{"access_token":"refreshed","expires_in":3600}"#
            };
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
        }
    });
    let clock = Arc::new(AtomicI64::new(1000));
    let c = clock.clone();
    let manager = OAuthManager::new(
        st.db.clone(),
        st.secret_key.clone(),
        OAuthEndpoints {
            hf_device: "http://issuer.invalid/device".into(),
            hf_token: "http://issuer.invalid/token".into(),
            ..Default::default()
        },
    )
    .unwrap()
    .with_egress(st.egress.clone())
    .with_clock(move || c.load(Ordering::SeqCst));
    manager
        .configure(
            "hf",
            Flow::HuggingFace,
            ClientOptions {
                client_id: "client".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let started = manager.start_device("hf", Flow::HuggingFace).await.unwrap();
    clock.store(2000, Ordering::SeqCst);
    assert!(
        !manager
            .poll_device("hf", Flow::HuggingFace, &started.attempt)
            .await
            .unwrap()
    );
    assert_eq!(
        manager
            .token("hf", Flow::HuggingFace, Some("access"))
            .await
            .unwrap(),
        "refreshed"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn inference_and_key_retry_use_provider_proxy() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg: tokomotive::config::Config = toml::from_str(
        r#"
[server]
mode='simple'
[[providers]]
name='p'
kind='openai'
base_url='http://upstream.invalid'
api_keys=['first-key', 'second-key']
[[models]]
name='m'
provider='p'
cost={input=0,output=0}
[[users]]
name='user'
keys=['vk-test']
"#,
    )
    .unwrap();
    cfg.server.database = format!("sqlite://{}/db.sqlite", dir.path().display());
    let st = tokomotive::init(&cfg, None).await.unwrap();
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", proxy_listener.local_addr().unwrap());
    tokomotive::settings::put(
        &st.db,
        "egress_proxy",
        &st.secret_key.encrypt("http://127.0.0.1:1").unwrap(),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE providers SET proxy_mode='custom',proxy_url=? WHERE name='p'")
        .bind(st.secret_key.encrypt(&proxy).unwrap())
        .execute(&st.db)
        .await
        .unwrap();
    st.reload_settings().await.unwrap();
    let upstream = tokio::spawn(async move {
        for (index, key) in ["first-key", "second-key"].iter().enumerate() {
            let (mut socket, _) = proxy_listener.accept().await.unwrap();
            let mut header = vec![];
            while !header.ends_with(b"\r\n\r\n") {
                header.push(socket.read_u8().await.unwrap());
            }
            let header = String::from_utf8(header).unwrap();
            assert!(header.starts_with("POST http://upstream.invalid/v1/chat/completions "));
            assert!(header.contains(&format!("authorization: Bearer {key}")));
            let length: usize = header
                .lines()
                .find_map(|l| {
                    l.to_lowercase()
                        .strip_prefix("content-length: ")
                        .map(|v| v.parse().unwrap())
                })
                .unwrap();
            let mut body = vec![0; length];
            socket.read_exact(&mut body).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&body).unwrap()["model"],
                "m"
            );
            let (status, response) = if index == 0 {
                (500, r#"{"error":{"message":"retry"}}"#)
            } else {
                (
                    200,
                    r#"{"id":"ok","model":"m","choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
                )
            };
            socket.write_all(format!("HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = tokomotive::router(st.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{url}/v1/chat/completions"))
        .bearer_auth("vk-test")
        .json(&serde_json::json!({"model":"m","messages":[{"role":"user","content":"Hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    upstream.await.unwrap();
    st.flush().await;
    server.abort();
}

#[test]
fn client_cache_bounds_distinct_proxies_not_provider_count() {
    let shared = (0..1000)
        .map(|i| (format!("p{i}"), Some("http://127.0.0.1:9000".to_owned())))
        .collect();
    assert!(tokomotive::egress::Policy::new(None, shared, 30).is_ok());
    let distinct = (0..65)
        .map(|i| {
            (
                format!("p{i}"),
                Some(format!("http://127.0.0.1:{}", 9000 + i)),
            )
        })
        .collect();
    assert!(tokomotive::egress::Policy::new(None, distinct, 30).is_err());
}
