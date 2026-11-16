use axum::{
    Json,
    extract::State,
    http::HeaderMap,
    routing::{get, post},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokomotive::{config::Config, state::AppState};

#[derive(Default)]
struct Vendor {
    calls: Mutex<Vec<(String, HeaderMap, Value)>>,
    statuses: Mutex<std::collections::VecDeque<u16>>,
    arrivals: Mutex<Vec<std::time::Instant>>,
    vault_key: Mutex<String>,
    blocked: std::sync::atomic::AtomicBool,
    gate: tokio::sync::Notify,
}
struct Env {
    _dir: tempfile::TempDir,
    st: AppState,
    url: String,
    virtual_key: String,
    vendor: String,
    mock: Arc<Vendor>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Env {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
async fn intake(
    State(mock): State<Arc<Vendor>>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    Json(value): Json<Value>,
) -> axum::http::StatusCode {
    mock.calls
        .lock()
        .unwrap()
        .push((uri.path().into(), headers, value));
    mock.arrivals
        .lock()
        .unwrap()
        .push(std::time::Instant::now());
    let notified = mock.gate.notified();
    if mock.blocked.load(std::sync::atomic::Ordering::SeqCst) {
        notified.await;
    }
    axum::http::StatusCode::from_u16(mock.statuses.lock().unwrap().pop_front().unwrap_or(202))
        .unwrap()
}
async fn upstream() -> Json<Value> {
    Json(
        json!({"id":"c","model":"m","choices":[{"message":{"role":"assistant","content":"private answer"}}],"usage":{"prompt_tokens":2,"completion_tokens":3}}),
    )
}
async fn vault(State(mock): State<Arc<Vendor>>) -> Json<Value> {
    Json(json!({"data":{"data":{"api_key":*mock.vault_key.lock().unwrap()}}}))
}
async fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let mock = Arc::new(Vendor::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route("/api/intake/llm-obs/v1/trace/spans", post(intake))
        .route("/api/v2/series", post(intake))
        .route("/v1/logs/batch", post(intake))
        .route("/v1/kv/data/exporter", get(vault))
        .route("/v1/chat/completions", post(upstream))
        .with_state(mock.clone());
    let vendor_task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let provider_key = tokomotive::hooks::new_secret();
    let virtual_key = format!("sk-{}", tokomotive::hooks::new_secret());
    let cfg: Config = toml::from_str(&format!(
        "[server]\ndatabase='sqlite://{}'\nmode='simple'\nadmin_password='password123'\n[[providers]]\nname='p'\nkind='openai'\nbase_url='http://{addr}'\napi_key='{provider_key}'\n[[models]]\nname='m'\nprovider='p'\ncost={{input=1,output=2}}\n[[routes]]\nname='chat'\ndeployments=[{{provider='p',model='m'}}]\n[[users]]\nname='alice'\nkeys=['{virtual_key}']",
        dir.path().join("export.sqlite").display()
    )).unwrap();
    static METRICS: std::sync::OnceLock<metrics_exporter_prometheus::PrometheusHandle> =
        std::sync::OnceLock::new();
    let metrics = METRICS
        .get_or_init(|| {
            metrics_exporter_prometheus::PrometheusBuilder::new()
                .install_recorder()
                .unwrap()
        })
        .clone();
    let st = tokomotive::init(&cfg, Some(metrics)).await.unwrap();
    tokomotive::db::create_session(&st.db, "export-session", 1)
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = tokomotive::router(st.clone());
    let proxy = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Env {
        _dir: dir,
        st,
        url,
        virtual_key,
        vendor: format!("http://{addr}"),
        mock,
        tasks: vec![vendor_task, proxy],
    }
}
impl Env {
    fn metric(&self, metric: &str, exporter: &str) -> u64 {
        self.st
            .metrics
            .as_ref()
            .unwrap()
            .render()
            .lines()
            .find(|line| {
                line.starts_with(metric) && line.contains(&format!("exporter=\"{exporter}\""))
            })
            .and_then(|line| line.split_whitespace().last()?.parse().ok())
            .unwrap_or(0)
    }
    fn client(&self) -> reqwest::Client {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
    }
    async fn admin(&self, path: &str, form: &[(&str, &str)]) -> reqwest::Response {
        self.post_session("export-session", path, form).await
    }
    async fn post_session(
        &self,
        session: &str,
        path: &str,
        form: &[(&str, &str)],
    ) -> reqwest::Response {
        self.client()
            .post(format!("{}{path}", self.url))
            .header("cookie", format!("tokomotive_session={session}"))
            .header("origin", &self.url)
            .form(form)
            .send()
            .await
            .unwrap()
    }
    async fn create(&self, key: &str) {
        self.create_with(key, &[]).await;
    }
    async fn create_with(&self, key: &str, changes: &[(&str, &str)]) {
        let mut fields = vec![
            ("name", "datadog"),
            ("kind", "datadog"),
            ("site", "US1"),
            ("endpoint", &self.vendor),
            ("allow_private", "on"),
            ("api_key", key),
            ("spans", "on"),
            ("enabled", "on"),
            ("sample_rate", "1"),
            ("batch_size", "16"),
            ("interval_ms", "100"),
            ("tag_allowlist", "route,model,provider"),
        ];
        for &(key, value) in changes {
            fields.retain(|(k, _)| *k != key);
            fields.push((key, value));
        }
        let response = self.admin("/admin/settings/exporters", &fields).await;
        assert_eq!(response.status(), 303, "{}", response.text().await.unwrap());
    }
    async fn send(&self) {
        let response = self.request().await;
        assert_eq!(response.status(), 200);
        response.bytes().await.unwrap();
    }
    async fn request(&self) -> reqwest::Response {
        self.client()
            .post(format!("{}/v1/chat/completions", self.url))
            .bearer_auth(&self.virtual_key)
            .json(&json!({"model":"chat","messages":[{"role":"user","content":"private prompt"}]}))
            .send()
            .await
            .unwrap()
    }
    async fn guardrail(&self, action: &str, direction: &str) {
        let id = sqlx::query("INSERT INTO guardrail_sets(name,rules) VALUES('privacy',?)")
            .bind(json!([{"name":"confidential","kind":{"type":"keywords","words":["private"],"case_sensitive":false},"action":action,"direction":direction}]).to_string())
            .execute(&self.st.db).await.unwrap().last_insert_rowid();
        sqlx::query("INSERT INTO guardrail_attachments(set_id,route) VALUES(?,'chat')")
            .bind(id)
            .execute(&self.st.db)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn reference_backed_exporter_bindings_and_body_permission_are_owner_only() {
    let e = env().await;
    e.create("env:DEMO_DD_KEY").await;
    sqlx::query("INSERT INTO admin_accounts(username,role,password_hash) VALUES('ref-admin','admin','fixture')")
        .execute(&e.st.db).await.unwrap();
    let account = tokomotive::admin::by_username(&e.st.db, "ref-admin")
        .await
        .unwrap()
        .unwrap();
    tokomotive::admin::create_session(&e.st.db, "ref-admin", 1, &account)
        .await
        .unwrap();
    let base = vec![
        ("id", "1"),
        ("name", "datadog"),
        ("kind", "datadog"),
        ("site", "US1"),
        ("spans", "on"),
        ("endpoint", &e.vendor),
        ("allow_private", "on"),
        ("api_key", ""),
    ];
    for (field, value) in [
        ("api_key", "env:ANOTHER_DD_KEY"),
        ("api_key", "vault@team:kv/exporter#api_key"),
        ("api_key", "replacement-literal"),
        ("endpoint", "http://127.0.0.1:9"),
        ("site", "EU1"),
        ("include_bodies", "on"),
        ("allow_private", ""),
        ("repo_id", "another-repository"),
    ] {
        let mut form = base.clone();
        form.retain(|(k, _)| *k != field);
        form.push((field, value));
        let response = e
            .post_session("ref-admin", "/admin/settings/exporters", &form)
            .await;
        assert_eq!(response.status(), 403, "Admin must not change {field}");
    }
    let response = e
        .post_session(
            "ref-admin",
            "/admin/settings/exporters",
            &[
                ("name", "new-ref"),
                ("kind", "datadog"),
                ("site", "US1"),
                ("spans", "on"),
                ("endpoint", &e.vendor),
                ("allow_private", "on"),
                ("api_key", "env:ANOTHER_DD_KEY"),
            ],
        )
        .await;
    assert_eq!(response.status(), 403);
    let mut safe = base.clone();
    safe.extend([
        ("sample_rate", "0.5"),
        ("batch_size", "2"),
        ("interval_ms", "10"),
        ("enabled", "on"),
    ]);
    assert_eq!(
        e.post_session("ref-admin", "/admin/settings/exporters", &safe)
            .await
            .status(),
        303
    );
    e.create_with("env:DEMO_DD_KEY", &[("id", "1"), ("include_bodies", "on")])
        .await;
    let page = e
        .client()
        .get(format!("{}/admin/settings", e.url))
        .header("cookie", "tokomotive_session=ref-admin")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("data-reference-locked=\"true\""));
    assert!(page.contains("name=\"include_bodies\" checked disabled"));
    let mut preserved = base.clone();
    preserved.push(("include_bodies", "on"));
    assert_eq!(
        e.post_session("ref-admin", "/admin/settings/exporters", &preserved)
            .await
            .status(),
        303,
        "Admin can preserve an Owner's body permission without changing it"
    );
}

#[tokio::test]
async fn reference_backed_test_ignores_unsaved_destination_and_body_values() {
    let e = env().await;
    let key = tokomotive::hooks::new_secret();
    *e.mock.vault_key.lock().unwrap() = key.clone();
    let bootstrap = tokomotive::hooks::new_secret();
    assert_eq!(
        e.admin(
            "/admin/settings/secrets",
            &[
                ("name", "export-store"),
                ("kind", "vault"),
                ("address", &e.vendor),
                ("typed", "true"),
                ("vault_auth", "token"),
                ("vault_token", &bootstrap),
                ("vault_kv_version", "2"),
                ("allow_private", "on"),
            ]
        )
        .await
        .status(),
        303
    );
    e.create("vault@export-store:kv/exporter#api_key").await;
    sqlx::query("INSERT INTO admin_accounts(username,role,password_hash) VALUES('test-admin','admin','fixture')")
        .execute(&e.st.db).await.unwrap();
    let account = tokomotive::admin::by_username(&e.st.db, "test-admin")
        .await
        .unwrap()
        .unwrap();
    tokomotive::admin::create_session(&e.st.db, "test-admin", 1, &account)
        .await
        .unwrap();
    let result: Value = e
        .post_session(
            "test-admin",
            "/admin/settings/exporters/1/test",
            &[
                ("endpoint", "http://127.0.0.1:9"),
                ("site", "gov"),
                ("kind", "maxim"),
                ("repo_id", "unsaved"),
                ("include_bodies", "on"),
                ("api_key", "unsaved"),
            ],
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(result["status"], 202);
    assert_eq!(result["outcome"], "sent");
    let calls = e.mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "/api/intake/llm-obs/v1/trace/spans");
    assert_eq!(calls[0].1["dd-api-key"], key);
    assert!(
        calls[0].2["data"]["attributes"]["spans"][0]["meta"]
            .get("input")
            .is_none()
    );
}

#[tokio::test]
async fn batches_flush_at_size_and_interval() {
    let e = env().await;
    e.create_with(
        &tokomotive::hooks::new_secret(),
        &[("batch_size", "2"), ("interval_ms", "1500")],
    )
    .await;
    e.send().await;
    e.send().await;
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if !e.mock.calls.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("full batch must not wait for the interval");
    {
        let calls = e.mock.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].2["data"]["attributes"]["spans"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }
    e.send().await;
    e.st.flush().await;
    let calls = e.mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[1].2["data"]["attributes"]["spans"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn maxim_exports_native_trace_batch_with_repository_and_key_header() {
    let e = env().await;
    let key = tokomotive::hooks::new_secret();
    e.create_with(
        &key,
        &[
            ("kind", "maxim"),
            ("name", "maxim"),
            ("repo_id", "repository-123"),
        ],
    )
    .await;
    e.send().await;
    e.st.flush().await;
    let calls = e.mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let (path, headers, value) = &calls[0];
    assert_eq!(path, "/v1/logs/batch");
    assert_eq!(headers["x-maxim-api-key"], key);
    assert!(headers.get("dd-api-key").is_none());
    assert_eq!(value["repoId"], "repository-123");
    let trace = &value["logs"][0];
    assert_eq!(trace["type"], "trace");
    assert!(!trace["id"].as_str().unwrap().is_empty());
    assert_eq!(trace["tags"]["route"], "chat");
    assert_eq!(trace["metrics"]["input_tokens"], 2);
    assert_eq!(trace["metrics"]["output_tokens"], 3);
    assert!(trace.get("input").is_none());
    assert!(trace.get("output").is_none());
}

#[tokio::test]
async fn retry_429_and_5xx_with_backoff_then_drop_at_limit() {
    for (name, statuses, sent, failed, dropped) in [
        ("retry-success", vec![429, 500, 202], 1, 2, 0),
        ("retry-exhausted", vec![503, 503, 503, 202], 0, 3, 1),
    ] {
        let e = env().await;
        *e.mock.statuses.lock().unwrap() = statuses.into();
        e.create_with(&tokomotive::hooks::new_secret(), &[("name", name)])
            .await;
        e.send().await;
        e.st.flush().await;
        assert_eq!(e.mock.calls.lock().unwrap().len(), 3);
        let times = e.mock.arrivals.lock().unwrap();
        assert!(times[1].duration_since(times[0]) >= std::time::Duration::from_millis(90));
        assert!(times[2].duration_since(times[1]) >= std::time::Duration::from_millis(190));
        assert_eq!(e.metric("llm_exporter_sent_total", name), sent);
        assert_eq!(e.metric("llm_exporter_failed_total", name), failed);
        assert_eq!(e.metric("llm_exporter_dropped_total", name), dropped);
    }
}

#[tokio::test]
async fn sampling_zero_disables_export_and_fractional_rate_selects_a_subset() {
    for rate in ["0", "0.5"] {
        let e = env().await;
        e.create_with(&tokomotive::hooks::new_secret(), &[("sample_rate", rate)])
            .await;
        for _ in 0..64 {
            e.send().await;
        }
        e.st.flush().await;
        let count: usize = e
            .mock
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|(_, _, v)| v["data"]["attributes"]["spans"].as_array().unwrap().len())
            .sum();
        if rate == "0" {
            assert_eq!(count, 0);
        } else {
            assert!(count > 0 && count < 64, "sampled {count} of 64");
        }
    }
}

#[tokio::test]
async fn opted_in_bodies_are_final_and_rejected_bodies_are_always_omitted() {
    for kind in ["datadog", "maxim"] {
        for (action, direction) in [
            ("redact", "request"),
            ("block", "request"),
            ("block", "response"),
        ] {
            let e = env().await;
            e.create_with(
                &tokomotive::hooks::new_secret(),
                &[
                    ("kind", kind),
                    ("repo_id", "repository"),
                    ("include_bodies", "on"),
                ],
            )
            .await;
            e.guardrail(action, direction).await;
            let r = e.request().await;
            assert_eq!(
                r.status().as_u16(),
                if action == "block" { 400 } else { 200 }
            );
            r.bytes().await.unwrap();
            e.st.flush().await;
            let calls = e.mock.calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            let value = &calls[0].2;
            let (input, output) = if kind == "datadog" {
                let meta = &value["data"]["attributes"]["spans"][0]["meta"];
                (meta.get("input"), meta.get("output"))
            } else {
                (
                    value["logs"][0].get("input"),
                    value["logs"][0].get("output"),
                )
            };
            if action == "block" {
                assert!(input.is_none());
                assert!(output.is_none());
                assert!(!value.to_string().contains("private"));
            } else {
                assert!(input.unwrap().to_string().contains("[REDACTED"));
                assert!(!input.unwrap().to_string().contains("private prompt"));
                assert!(output.unwrap().to_string().contains("private answer"));
            }
        }
    }
}

#[tokio::test]
async fn datadog_optional_series_carry_usage_cost_latency_and_allowlisted_names() {
    let e = env().await;
    sqlx::query("INSERT INTO teams(name,created_at) VALUES('research',0)")
        .execute(&e.st.db)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET team_id=(SELECT id FROM teams WHERE name='research')")
        .execute(&e.st.db)
        .await
        .unwrap();
    sqlx::query("UPDATE virtual_keys SET label='application-key'")
        .execute(&e.st.db)
        .await
        .unwrap();
    e.create_with(
        &tokomotive::hooks::new_secret(),
        &[
            ("metrics", "on"),
            ("tag_allowlist", "route,model,provider,team,key_name"),
        ],
    )
    .await;
    e.send().await;
    e.st.flush().await;
    let calls = e.mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    let series = &calls
        .iter()
        .find(|(path, _, _)| path == "/api/v2/series")
        .expect("metrics series request")
        .2["series"];
    let rows = series.as_array().unwrap();
    assert_eq!(rows.len(), 4);
    for (metric, kind, value) in [
        ("tokomotive.requests", 1, 1.0),
        ("tokomotive.tokens", 1, 5.0),
        ("tokomotive.cost", 1, 0.000008),
    ] {
        let row = rows.iter().find(|r| r["metric"] == metric).unwrap();
        assert_eq!(row["type"], kind);
        assert_eq!(row["points"][0]["value"], value);
        assert!(row["points"][0]["timestamp"].as_i64().unwrap() > 0);
        assert_eq!(
            row["tags"],
            json!([
                "key_name:application-key",
                "model:m",
                "provider:p",
                "route:chat",
                "team:research"
            ])
        );
    }
    assert!(
        rows.iter()
            .find(|r| r["metric"] == "tokomotive.latency")
            .unwrap()["points"][0]["value"]
            .as_f64()
            .unwrap()
            >= 0.0
    );
}

#[tokio::test]
async fn settings_forms_test_button_audit_and_roles_keep_api_keys_private() {
    let e = env().await;
    let first = tokomotive::hooks::new_secret();
    let second = tokomotive::hooks::new_secret();
    e.create(&first).await;
    e.create_with(
        &second,
        &[("id", "1"), ("enabled", ""), ("sample_rate", "0")],
    )
    .await;
    let page = e
        .client()
        .get(format!("{}/admin/settings", e.url))
        .header("cookie", "tokomotive_session=export-session")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("id=\"exporters\""));
    assert!(page.contains("name=\"api_key\""));
    assert!(page.contains("type=\"password\""));
    for field in [
        "kind",
        "site",
        "repo_id",
        "sample_rate",
        "tag_allowlist",
        "batch_size",
        "interval_ms",
        "include_bodies",
    ] {
        assert!(page.contains(&format!("name=\"{field}\"")));
    }
    assert!(!page.contains(&first) && !page.contains(&second));
    let response = e.admin("/admin/settings/exporters/1/test", &[]).await;
    assert_eq!(response.status(), 200);
    let result: Value = response.json().await.unwrap();
    assert_eq!(result["status"], 202);
    assert_eq!(result["outcome"], "sent");
    assert!(!result.to_string().contains(&second));
    assert_eq!(
        e.mock.calls.lock().unwrap().len(),
        1,
        "test bypasses disabled and sample rate"
    );
    let audit: Vec<String> = sqlx::query_scalar(
        "SELECT before_json||after_json FROM admin_audit WHERE target_type='exporters'",
    )
    .fetch_all(&e.st.db)
    .await
    .unwrap();
    assert_eq!(audit.len(), 3);
    assert!(
        audit[0].contains("sample_rate"),
        "audit includes public exporter settings"
    );
    assert!(
        audit
            .iter()
            .all(|s| !s.contains(&first) && !s.contains(&second))
    );
    let stored: String = sqlx::query_scalar("SELECT api_key FROM exporters WHERE id=1")
        .fetch_one(&e.st.db)
        .await
        .unwrap();
    assert_ne!(stored, second);
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&e.st.db)
        .await
        .unwrap();
    for file in std::fs::read_dir(e._dir.path()).unwrap() {
        let bytes = std::fs::read(file.unwrap().path()).unwrap();
        for key in [&first, &second] {
            assert!(!bytes.windows(key.len()).any(|w| w == key.as_bytes()));
        }
    }
    for role in ["viewer", "admin"] {
        let username = format!("export-{role}");
        sqlx::query(
            "INSERT INTO admin_accounts(username,role,password_hash) VALUES(?,?,'fixture')",
        )
        .bind(&username)
        .bind(role)
        .execute(&e.st.db)
        .await
        .unwrap();
        let account = tokomotive::admin::by_username(&e.st.db, &username)
            .await
            .unwrap()
            .unwrap();
        tokomotive::admin::create_session(&e.st.db, role, 1, &account)
            .await
            .unwrap();
        for path in [
            "/admin/settings/exporters",
            "/admin/settings/exporters/1/test",
            "/admin/settings/exporters/1/delete",
        ] {
            let response = e
                .client()
                .post(format!("{}{path}", e.url))
                .header("origin", &e.url)
                .header("cookie", format!("tokomotive_session={role}"))
                .form(&[
                    ("id", "1"),
                    ("name", "datadog"),
                    ("kind", "datadog"),
                    ("site", "US1"),
                    ("spans", "on"),
                    ("endpoint", &e.vendor),
                    ("allow_private", "on"),
                ])
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status().as_u16(),
                if role == "viewer" {
                    403
                } else if path.ends_with("/test") {
                    200
                } else {
                    303
                },
                "{role}: {path}"
            );
        }
    }
    assert!(
        tokomotive::exporters::list(&e.st.db)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn datadog_sites_map_to_documented_hosts_and_gov_is_metrics_only() {
    let e = env().await;
    let key = tokomotive::hooks::new_secret();
    let response = e
        .admin(
            "/admin/settings/exporters",
            &[
                ("name", "gov"),
                ("kind", "datadog"),
                ("site", "gov"),
                ("spans", "on"),
                ("metrics", "on"),
                ("endpoint", &e.vendor),
                ("allow_private", "on"),
                ("api_key", &key),
            ],
        )
        .await;
    assert_eq!(response.status(), 400);
    let error = response.text().await.unwrap();
    assert!(error.contains("https://docs.datadoghq.com/llm_observability/setup/api/"));
    e.create_with(&key, &[("site", "gov"), ("spans", ""), ("metrics", "on")])
        .await;
    e.send().await;
    e.st.flush().await;
    assert_eq!(e.mock.calls.lock().unwrap()[0].0, "/api/v2/series");
    let page = e
        .client()
        .get(format!("{}/admin/settings", e.url))
        .header("cookie", "tokomotive_session=export-session")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("name=\"spans\" disabled"));
    let mut config = tokomotive::exporters::list(&e.st.db)
        .await
        .unwrap()
        .remove(0);
    config.endpoint.clear();
    for (site, url) in [
        ("US1", "https://api.datadoghq.com"),
        ("US3", "https://api.us3.datadoghq.com"),
        ("US5", "https://api.us5.datadoghq.com"),
        ("EU1", "https://api.datadoghq.eu"),
        ("AP1", "https://api.ap1.datadoghq.com"),
        ("gov", "https://api.ddog-gov.com"),
    ] {
        config.site = site.into();
        assert_eq!(config.base_url().unwrap(), url);
    }
}

#[tokio::test]
async fn public_http_private_without_opt_in_and_invalid_settings_are_refused() {
    let e = env().await;
    let key = tokomotive::hooks::new_secret();
    for (field, value) in [
        ("site", "invalid"),
        ("sample_rate", "NaN"),
        ("batch_size", "0"),
        ("interval_ms", "0"),
        ("tag_allowlist", "authorization"),
        ("spans", ""),
        ("endpoint", "http://8.8.8.8"),
        ("allow_private", ""),
        ("endpoint", "http://127.0.0.1:8081/?api_key=not-allowed"),
    ] {
        let mut fields = vec![
            ("name", "invalid"),
            ("kind", "datadog"),
            ("site", "US1"),
            ("spans", "on"),
            ("endpoint", &e.vendor),
            ("allow_private", "on"),
            ("api_key", &key),
        ];
        fields.retain(|(k, _)| *k != field);
        fields.push((field, value));
        let response = e.admin("/admin/settings/exporters", &fields).await;
        assert_eq!(response.status(), 400, "{field}={value}");
    }
    assert!(
        tokomotive::exporters::list(&e.st.db)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn secret_store_key_reference_is_resolved_by_real_proxy_export() {
    let e = env().await;
    let key = tokomotive::hooks::new_secret();
    *e.mock.vault_key.lock().unwrap() = key.clone();
    let bootstrap = tokomotive::hooks::new_secret();
    let response = e
        .admin(
            "/admin/settings/secrets",
            &[
                ("name", "export-store"),
                ("kind", "vault"),
                ("address", &e.vendor),
                ("typed", "true"),
                ("vault_auth", "token"),
                ("vault_token", &bootstrap),
                ("vault_kv_version", "2"),
                ("allow_private", "on"),
            ],
        )
        .await;
    assert_eq!(response.status(), 303, "{}", response.text().await.unwrap());
    e.create_with(
        "vault@export-store:kv/exporter#api_key",
        &[
            ("kind", "maxim"),
            ("repo_id", "repo"),
            ("tag_allowlist", ""),
        ],
    )
    .await;
    e.send().await;
    e.st.flush().await;
    let calls = e.mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1["x-maxim-api-key"], key);
    assert_eq!(calls[0].2["logs"][0]["tags"], json!({}));
}

#[tokio::test]
async fn environment_key_reference_is_resolved_without_persisting_the_value() {
    const ENV: &str = "TOKO_EXPORT_CHILD_KEY";
    if let Ok(key) = std::env::var(ENV) {
        let e = env().await;
        e.create(&format!("env:{ENV}")).await;
        e.send().await;
        e.st.flush().await;
        assert_eq!(e.mock.calls.lock().unwrap()[0].1["dd-api-key"], key);
        let stored: String = sqlx::query_scalar("SELECT api_key FROM exporters")
            .fetch_one(&e.st.db)
            .await
            .unwrap();
        assert_eq!(stored, format!("env:{ENV}"));
    } else {
        // Give this process its own environment; never mutate the parallel test runner.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "environment_key_reference_is_resolved_without_persisting_the_value",
            ])
            .env(ENV, tokomotive::hooks::new_secret())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[tokio::test]
async fn slow_vendor_does_not_delay_proxy_and_shared_queue_is_bounded() {
    use std::sync::atomic::Ordering;
    let e = env().await;
    let name = "slow-bounded";
    e.mock.blocked.store(true, Ordering::SeqCst);
    e.create_with(
        &tokomotive::hooks::new_secret(),
        &[("name", name), ("batch_size", "1")],
    )
    .await;
    let start = std::time::Instant::now();
    tokio::time::timeout(std::time::Duration::from_secs(1), e.send())
        .await
        .expect("the Proxy must not wait for the vendor");
    let elapsed = start.elapsed();
    eprintln!("Proxy response with blocked exporter: {elapsed:?}");
    assert!(elapsed < std::time::Duration::from_millis(500));
    for _ in 1..100 {
        e.send().await;
    }
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if e.metric("llm_exporter_dropped_total", name) >= 32 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("4 active plus 64 waiting must drop excess events");
    assert_eq!(e.mock.calls.lock().unwrap().len(), 4);
    e.mock.blocked.store(false, Ordering::SeqCst);
    e.mock.gate.notify_waiters();
    e.st.flush().await;
    assert_eq!(e.metric("llm_exporter_sent_total", name), 68);
    assert_eq!(e.metric("llm_exporter_dropped_total", name), 32);
}

#[tokio::test]
async fn datadog_exports_documented_spans_from_real_proxy_without_bodies() {
    let e = env().await;
    let key = tokomotive::hooks::new_secret();
    e.create(&key).await;
    e.send().await;
    e.st.flush().await;
    let calls = e.mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let (path, headers, payload) = &calls[0];
    assert_eq!(path, "/api/intake/llm-obs/v1/trace/spans");
    assert_eq!(headers["dd-api-key"], key);
    assert_eq!(payload["data"]["type"], "span");
    assert_eq!(payload["data"]["attributes"]["ml_app"], "tokomotive");
    let span = &payload["data"]["attributes"]["spans"][0];
    assert_eq!(span["parent_id"], "undefined");
    assert_eq!(span["meta"]["kind"], "llm");
    assert_eq!(span["meta"]["model_name"], "m");
    assert_eq!(span["metrics"]["input_tokens"], 2);
    assert_eq!(span["metrics"]["output_tokens"], 3);
    assert!(span["start_ns"].as_u64().unwrap() > 0);
    assert!(span["span_id"].as_str().unwrap().parse::<u64>().is_ok());
    assert!(span["meta"].get("input").is_none());
    assert!(!payload.to_string().contains("private prompt"));
    assert!(!payload.to_string().contains("private answer"));
}
