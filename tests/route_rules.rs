use axum::{Json, routing::post};
use serde_json::{Value, json};
use tokomotive::{config::Config, state::AppState};

struct App {
    st: AppState,
    url: String,
    c: reqwest::Client,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for App {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
async fn app() -> App {
    let up = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", up.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let router = axum::Router::new().route("/v1/chat/completions", post(|Json(v): Json<Value>| async move {
            if v["model"].as_str().is_some_and(|s|s.starts_with("down")) { return (axum::http::StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":{"message":"down"}}))); }
            (axum::http::StatusCode::OK, Json(json!({"model":v["model"],"choices":[{"message":{"role":"assistant","content":"ok"}}],"usage":{"prompt_tokens":3,"completion_tokens":1}})))
        }));
        axum::serve(up, router).await.unwrap();
    });
    let cfg: Config = toml::from_str(&format!(r#"
[server]
database='sqlite::memory:'
mode='simple'
admin_password='password123'
[[providers]]
name='up'
kind='openai'
base_url='{upstream}'
api_key='upstream-key'
[[teams]]
name='engineering'
[[users]]
name='alice'
team='engineering'
keys=['sk-alice']
[[routes]]
name='primary'
session_affinity=true
fallbacks=['backup']
deployments=[{{provider='up',model='normal'}},{{provider='up',model='canary'}},{{provider='up',model='down'}},{{provider='up',model='down2'}}]
[[routes]]
name='backup'
deployments=[{{provider='up',model='backup'}}]
"#)).unwrap();
    static METRICS: std::sync::OnceLock<metrics_exporter_prometheus::PrometheusHandle> =
        std::sync::OnceLock::new();
    let metrics = METRICS
        .get_or_init(|| tokomotive::metrics::install().unwrap())
        .clone();
    let st = tokomotive::init(&cfg, Some(metrics)).await.unwrap();
    tokomotive::db::create_session(&st.db, "rules-session", 1)
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = tokomotive::router(st.clone());
    let proxy = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    App {
        st,
        url,
        c: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        tasks: vec![task, proxy],
    }
}
impl App {
    async fn try_model(&self, session: &str) -> String {
        let r = self
            .c
            .post(format!("{}/admin/routes/try", self.url))
            .header("cookie", "tokomotive_session=rules-session")
            .header("origin", &self.url)
            .form(&[
                ("model", "primary"),
                ("key_id", "1"),
                ("session_id", session),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        r.text().await.unwrap()
    }
    async fn save(&self, route: &str, fields: &[(&str, &str)]) -> reqwest::Response {
        self.c
            .post(format!("{}/admin/routes/{route}/rules", self.url))
            .header("cookie", "tokomotive_session=rules-session")
            .header("origin", &self.url)
            .form(fields)
            .send()
            .await
            .unwrap()
    }
    async fn chat(&self, body: Value) -> reqwest::Response {
        self.c
            .post(format!("{}/v1/chat/completions", self.url))
            .bearer_auth("sk-alice")
            .json(&body)
            .send()
            .await
            .unwrap()
    }
}
fn rule() -> Vec<(&'static str, &'static str)> {
    vec![
        ("rule[0][name]", "pilot"),
        ("rule[0][enabled]", "on"),
        ("rule[0][action]", "deployments"),
        ("rule[0][deployments]", r#"["up","canary"]"#),
    ]
}
#[tokio::test]
async fn subset_action_uses_only_selected_deployments() {
    let a = app().await;
    let r = a.save("primary", &rule()).await;
    assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    let r = a
        .chat(json!({"model":"primary","messages":[{"role":"user","content":"hi"}]}))
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["model"], "canary");
}

#[tokio::test]
async fn reject_action_has_native_error_formats_and_first_match_wins() {
    let a = app().await;
    let mut fields = vec![
        ("rule[0][name]", "deny"),
        ("rule[0][enabled]", "on"),
        ("rule[0][action]", "reject"),
        ("rule[0][message]", "This Route is closed."),
    ];
    fields.extend([
        ("rule[1][name]", "later"),
        ("rule[1][enabled]", "on"),
        ("rule[1][action]", "deployments"),
        ("rule[1][deployments]", r#"["up","normal"]"#),
    ]);
    let r = a.save("primary", &fields).await;
    assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    for (path, body, kind) in [
        (
            "/v1/chat/completions",
            json!({"model":"primary","messages":[{"role":"user","content":"hi"}]}),
            "openai",
        ),
        (
            "/v1/messages",
            json!({"model":"primary","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}),
            "anthropic",
        ),
        (
            "/v1/models/primary:generateContent",
            json!({"contents":[{"role":"user","parts":[{"text":"hi"}]}]}),
            "gemini",
        ),
        (
            "/model/primary/converse",
            json!({"messages":[{"role":"user","content":[{"text":"hi"}]}]}),
            "bedrock",
        ),
    ] {
        let r =
            a.c.post(format!("{}{path}", a.url))
                .bearer_auth("sk-alice")
                .json(&body)
                .send()
                .await
                .unwrap();
        assert_eq!(r.status(), 403, "{kind}: {}", r.text().await.unwrap());
        if kind == "bedrock" {
            assert_eq!(r.headers()["x-amzn-errortype"], "AccessDeniedException");
        }
        let v: Value = r.json().await.unwrap();
        match kind {
            "anthropic" => assert_eq!(v["type"], "error"),
            "gemini" => assert_eq!(v["error"]["status"], "PERMISSION_DENIED"),
            "bedrock" => assert_eq!(v["message"], "This Route is closed."),
            _ => assert_eq!(v["error"]["message"], "This Route is closed."),
        }
    }
    a.st.flush().await;
    let ledger =
        a.c.get(format!("{}/admin/ledger?rule=deny", a.url))
            .header("cookie", "tokomotive_session=rules-session")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
    assert!(ledger.contains("rule: deny"));
    let metrics =
        a.c.get(format!("{}/metrics", a.url))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
    assert!(
        metrics
            .lines()
            .any(|l| l.starts_with("tokomotive_route_rule_matches_total{")
                && l.contains("route=\"primary\"")
                && l.contains("side=\"primary\""))
    );
}

#[tokio::test]
async fn each_condition_matches_only_its_request_attributes() {
    let a = app().await;
    let body = json!({"model":"primary","messages":[{"role":"user","content":"hello"}]});
    for (field, yes, no) in [
        ("key_id", "1", "999"),
        ("user_id", "1", "999"),
        ("team_id", "1", "999"),
        ("model", "primary", "other"),
        ("endpoint", "chat_completions", "responses"),
        ("format", "openai", "anthropic"),
        ("stream", "false", "true"),
        ("min_tokens", "1", "100000"),
        ("max_tokens", "100000", "0"),
        ("has_tools", "false", "true"),
        ("has_images", "false", "true"),
    ] {
        for (value, status) in [(yes, 403), (no, 200)] {
            let key = format!("rule[0][{field}]");
            let f = vec![
                ("rule[0][name]", "conditional"),
                ("rule[0][enabled]", "on"),
                ("rule[0][action]", "reject"),
                ("rule[0][message]", "Matched"),
                (key.as_str(), value),
            ];
            let r = a.save("primary", &f).await;
            assert_eq!(r.status(), 303, "{field}: {}", r.text().await.unwrap());
            let r = a.chat(body.clone()).await;
            assert_eq!(
                r.status(),
                status,
                "{field}={value}: {}",
                r.text().await.unwrap()
            );
        }
    }
    for (op, pattern) in [
        ("equals", "pilot-blue"),
        ("contains", "blue"),
        ("regex", "^pilot-[a-z]+$"),
    ] {
        let f = vec![
            ("rule[0][name]", "header"),
            ("rule[0][enabled]", "on"),
            ("rule[0][action]", "reject"),
            ("rule[0][message]", "Matched"),
            ("rule[0][header_name]", "X-Cohort"),
            ("rule[0][header_op]", op),
            ("rule[0][header_value]", pattern),
        ];
        assert_eq!(a.save("primary", &f).await.status(), 303);
        for (value, status) in [("pilot-blue", 403), ("other", 200)] {
            let r =
                a.c.post(format!("{}/v1/chat/completions", a.url))
                    .bearer_auth("sk-alice")
                    .header("x-cohort", value)
                    .json(&body)
                    .send()
                    .await
                    .unwrap();
            assert_eq!(r.status(), status, "{op} {value}");
        }
    }
    let f = vec![
        ("rule[0][name]", "prefix"),
        ("rule[0][enabled]", "on"),
        ("rule[0][action]", "reject"),
        ("rule[0][message]", "Matched"),
        ("rule[0][model]", "prim"),
        ("rule[0][model_op]", "prefix"),
    ];
    assert_eq!(a.save("primary", &f).await.status(), 303);
    assert_eq!(a.chat(body).await.status(), 403);
}

#[tokio::test]
async fn route_action_and_cycle_rejection_include_canary_edges() {
    let a = app().await;
    let f = vec![
        ("rule[0][name]", "redirect"),
        ("rule[0][enabled]", "on"),
        ("rule[0][action]", "route"),
        ("rule[0][target_route]", "backup"),
    ];
    let r = a.save("primary", &f).await;
    assert_eq!(r.status(), 303, "{}", r.text().await.unwrap());
    let r = a
        .chat(json!({"model":"primary","messages":[{"role":"user","content":"hi"}]}))
        .await;
    assert_eq!(r.json::<Value>().await.unwrap()["model"], "backup");
    let cyclic = vec![
        ("rule[0][name]", "cycle"),
        ("rule[0][enabled]", "on"),
        ("rule[0][action]", "route"),
        ("rule[0][target_route]", "primary"),
    ];
    let r = a.save("backup", &cyclic).await;
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("cycle"));
}

#[tokio::test]
async fn chosen_set_failure_uses_original_fallback_unless_disabled() {
    let a = app().await;
    let mut f = vec![
        ("rule[0][name]", "down-set"),
        ("rule[0][enabled]", "on"),
        ("rule[0][action]", "deployments"),
        ("rule[0][deployments]", r#"["up","down"]"#),
    ];
    assert_eq!(a.save("primary", &f).await.status(), 303);
    let body = json!({"model":"primary","messages":[{"role":"user","content":"hi"}]});
    let r = a.chat(body.clone()).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["model"], "backup");
    f.push(("rule[0][no_fallback]", "on"));
    assert_eq!(a.save("primary", &f).await.status(), 303);
    assert_eq!(a.chat(body).await.status(), 503);
}

#[tokio::test]
async fn canary_split_is_sticky_balanced_and_visible_in_try_box() {
    let a = app().await;
    let mut f = rule();
    // Primary uses normal; a quarter of conversations use canary.
    f[3].1 = r#"["up","normal"]"#;
    f.extend([
        ("rule[0][canary_percent]", "25"),
        ("rule[0][canary_action]", "deployments"),
        ("rule[0][canary_deployments]", r#"["up","canary"]"#),
    ]);
    assert_eq!(a.save("primary", &f).await.status(), 303);
    let mut canary = 0;
    for n in 0..10_000 {
        let text = a.try_model(&format!("conversation-{n}")).await;
        assert!(text.contains("Matched rule: pilot"), "{text}");
        if text.contains("Canary side: canary") {
            canary += 1;
        }
    }
    assert!((2200..=2800).contains(&canary), "{canary}/10000");
    for n in 0..10 {
        let session = format!("conversation-{n}");
        let first = a.try_model(&session).await;
        assert_eq!(first, a.try_model(&session).await);
        let r =
            a.c.post(format!("{}/v1/chat/completions", a.url))
                .bearer_auth("sk-alice")
                .header("x-session-id", &session)
                .json(&json!({"model":"primary","messages":[{"role":"user","content":"hi"}]}))
                .send()
                .await
                .unwrap();
        assert_eq!(r.status(), 200);
        let v: Value = r.json().await.unwrap();
        assert_eq!(
            v["model"],
            if first.contains("Canary side: canary") {
                "canary"
            } else {
                "normal"
            }
        );
    }
    let cyclic = vec![
        ("rule[0][name]", "cycle"),
        ("rule[0][enabled]", "on"),
        ("rule[0][action]", "route"),
        ("rule[0][target_route]", "backup"),
        ("rule[0][canary_percent]", "10"),
        ("rule[0][canary_action]", "route"),
        ("rule[0][canary_route]", "primary"),
    ];
    assert_eq!(a.save("primary", &cyclic).await.status(), 400);
}

#[tokio::test]
async fn ledger_and_attempts_record_rule_side_and_filter_by_rule() {
    let a = app().await;
    assert_eq!(a.save("primary", &rule()).await.status(), 303);
    let body = json!({"model":"primary","messages":[{"role":"user","content":"hi"}]});
    assert_eq!(a.chat(body).await.status(), 200);
    assert_eq!(
        a.chat(json!({"model":"backup","messages":[{"role":"user","content":"other"}]}))
            .await
            .status(),
        200
    );
    a.st.flush().await;
    let get = |path: String| {
        a.c.get(format!("{}{path}", a.url))
            .header("cookie", "tokomotive_session=rules-session")
    };
    let page = get("/admin/ledger?rule=pilot&entry=1".into())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("Rule: pilot / primary"), "{page}");
    assert!(page.contains("rule: pilot"));
    assert!(page.contains("id=\"e1\""));
    assert!(!page.contains("id=\"e2\""));
    let empty = get("/admin/ledger?rule=missing".into())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!empty.contains("id=\"e1\""));
}

#[tokio::test]
async fn structured_editor_preserves_bad_regex_and_central_role_matrix() {
    let a = app().await;
    let page =
        a.c.get(format!("{}/admin/routes", a.url))
            .header("cookie", "tokomotive_session=rules-session")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
    assert!(page.contains("Request rules"));
    assert!(page.contains("rule[0][header_value]"));
    assert!(page.contains("name=\"session_id\""));
    let f = vec![
        ("rule[0][name]", "typed-rule"),
        ("rule[0][enabled]", "on"),
        ("rule[0][action]", "reject"),
        ("rule[0][message]", "blocked"),
        ("rule[0][header_name]", "x-cohort"),
        ("rule[0][header_op]", "regex"),
        ("rule[0][header_value]", "[broken"),
    ];
    let r = a.save("primary", &f).await;
    assert_eq!(r.status(), 400);
    let text = r.text().await.unwrap();
    assert!(text.contains("value=\"[broken\""), "{text}");
    assert!(text.contains("value=\"typed-rule\""));
    sqlx::query("INSERT INTO admin_accounts(username,password_hash,role) SELECT 'keeper',password_hash,'owner' FROM admin_accounts LIMIT 1").execute(&a.st.db).await.unwrap();
    for role in ["admin", "viewer"] {
        sqlx::query("UPDATE admin_accounts SET role=? WHERE id=1")
            .bind(role)
            .execute(&a.st.db)
            .await
            .unwrap();
        sqlx::query("DELETE FROM sessions")
            .execute(&a.st.db)
            .await
            .unwrap();
        tokomotive::db::create_session(&a.st.db, "rules-session", 1)
            .await
            .unwrap();
        let r = a.save("primary", &rule()).await;
        assert_eq!(r.status(), if role == "admin" { 303 } else { 403 });
        let page =
            a.c.get(format!("{}/admin/routes", a.url))
                .header("cookie", "tokomotive_session=rules-session")
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
        assert_eq!(
            page.contains("action=\"/admin/routes/primary/rules\""),
            role == "admin"
        );
        assert!(a.try_model("viewer-read").await.contains("Matched rule"));
    }
}

#[tokio::test]
async fn old_fallback_pin_cannot_override_a_healthy_rule_set() {
    let a = app().await;
    let mut fields = rule();
    fields[3].1 = r#"["up","down"]"#;
    fields.push(("rule[0][deployments]", r#"["up","down2"]"#));
    assert_eq!(a.save("primary", &fields).await.status(), 303);
    let body =
        json!({"model":"primary","messages":[{"role":"user","content":"pinned conversation"}]} );
    assert_eq!(
        a.chat(body.clone()).await.json::<Value>().await.unwrap()["model"],
        "backup"
    );
    fields[3].1 = r#"["up","normal"]"#;
    fields[4].1 = r#"["up","canary"]"#;
    assert_eq!(a.save("primary", &fields).await.status(), 303);
    assert_ne!(
        a.chat(body).await.json::<Value>().await.unwrap()["model"],
        "backup"
    );
}

#[tokio::test]
async fn native_clients_share_tool_image_and_stream_attributes() {
    let a = app().await;
    let f = vec![
        ("rule[0][name]", "multimodal-tools"),
        ("rule[0][enabled]", "on"),
        ("rule[0][action]", "reject"),
        ("rule[0][message]", "Matched both"),
        ("rule[0][has_tools]", "true"),
        ("rule[0][has_images]", "true"),
    ];
    assert_eq!(a.save("primary", &f).await.status(), 303);
    for (path, body) in [
        (
            "/v1/chat/completions",
            json!({"model":"primary","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,aGk="}}]}],"tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object"}}}]}),
        ),
        (
            "/v1/messages",
            json!({"model":"primary","max_tokens":5,"messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGk="}}]}],"tools":[{"name":"f","input_schema":{"type":"object"}}]}),
        ),
        (
            "/v1/models/primary:generateContent",
            json!({"contents":[{"role":"user","parts":[{"inlineData":{"mimeType":"image/png","data":"aGk="}}]}],"tools":[{"functionDeclarations":[{"name":"f","parameters":{"type":"object"}}]}]}),
        ),
        (
            "/model/primary/converse",
            json!({"messages":[{"role":"user","content":[{"image":{"format":"png","source":{"bytes":"aGk="}}}]}],"toolConfig":{"tools":[{"toolSpec":{"name":"f","inputSchema":{"json":{"type":"object"}}}}]}}),
        ),
    ] {
        let r =
            a.c.post(format!("{}{path}", a.url))
                .bearer_auth("sk-alice")
                .json(&body)
                .send()
                .await
                .unwrap();
        assert_eq!(r.status(), 403, "{path}: {}", r.text().await.unwrap());
    }
    assert_eq!(
        a.chat(
            json!({"model":"primary","messages":[{"role":"user","content":"no images or tools"}]})
        )
        .await
        .status(),
        200
    );
    let stream = vec![
        ("rule[0][name]", "stream"),
        ("rule[0][enabled]", "on"),
        ("rule[0][action]", "reject"),
        ("rule[0][message]", "No stream"),
        ("rule[0][stream]", "true"),
    ];
    assert_eq!(a.save("primary", &stream).await.status(), 303);
    let r =
        a.c.post(format!("{}/v1/models/primary:streamGenerateContent", a.url))
            .bearer_auth("sk-alice")
            .json(&json!({"contents":[{"role":"user","parts":[{"text":"hi"}]}]}))
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 403);
}

#[tokio::test]
async fn route_edits_cannot_remove_rule_deployments_or_referenced_routes() {
    let a = app().await;
    assert_eq!(a.save("primary", &rule()).await.status(), 303);
    let r =
        a.c.post(format!("{}/admin/routes/primary", a.url))
            .header("cookie", "tokomotive_session=rules-session")
            .header("origin", &a.url)
            .form(&[
                ("name", "primary"),
                ("strategy", "priority"),
                ("deployment_provider", "up"),
                ("deployment_model", "normal"),
                ("deployment_weight", "1"),
            ])
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 400, "{}", r.text().await.unwrap());
    let f = vec![
        ("rule[0][name]", "redirect"),
        ("rule[0][enabled]", "on"),
        ("rule[0][action]", "route"),
        ("rule[0][target_route]", "backup"),
    ];
    assert_eq!(a.save("primary", &f).await.status(), 303);
    let r =
        a.c.post(format!("{}/admin/routes/backup/delete", a.url))
            .header("cookie", "tokomotive_session=rules-session")
            .header("origin", &a.url)
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn canary_extremes_route_targets_prefix_stickiness_and_save_identity() {
    let a = app().await;
    let mut f = rule();
    f[3].1 = r#"["up","normal"]"#;
    f.extend([
        ("rule[0][canary_percent]", "100"),
        ("rule[0][canary_action]", "route"),
        ("rule[0][canary_route]", "backup"),
    ]);
    assert_eq!(a.save("primary", &f).await.status(), 303);
    for n in 0..5 {
        assert!(
            a.try_model(&format!("s{n}"))
                .await
                .contains("Canary side: canary")
        );
    }
    let body = json!({"model":"primary","messages":[{"role":"user","content":"first prompt"}]} );
    assert_eq!(
        a.chat(body.clone()).await.json::<Value>().await.unwrap()["model"],
        "backup"
    );
    f[4].1 = "0";
    assert_eq!(a.save("primary", &f).await.status(), 303);
    for n in 0..5 {
        assert!(
            a.try_model(&format!("s{n}"))
                .await
                .contains("Canary side: primary")
        );
    }
    assert_eq!(
        a.chat(body.clone()).await.json::<Value>().await.unwrap()["model"],
        "normal"
    );
    f[4].1 = "50";
    assert_eq!(a.save("primary", &f).await.status(), 303);
    let before = a.try_model("same-conversation").await;
    let page =
        a.c.get(format!("{}/admin/routes", a.url))
            .header("cookie", "tokomotive_session=rules-session")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
    let form = page
        .split("action=\"/admin/routes/primary/rules\"")
        .nth(1)
        .unwrap();
    let id = regex::Regex::new(r#"name="rule\[0\]\[id\]" value="([a-f0-9]+)""#)
        .unwrap()
        .captures(form)
        .unwrap()[1]
        .to_owned();
    f.push(("rule[0][id]", &id));
    assert_eq!(a.save("primary", &f).await.status(), 303);
    assert_eq!(before, a.try_model("same-conversation").await);
    let first = a.chat(body.clone()).await.json::<Value>().await.unwrap()["model"].clone();
    let mut next = body;
    next["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":"reply"}),
        json!({"role":"user","content":"second turn"}),
    ]);
    assert_eq!(
        a.chat(next).await.json::<Value>().await.unwrap()["model"],
        first
    );
}

#[tokio::test]
async fn disabled_rules_are_skipped_and_provider_access_is_not_expanded() {
    let a = app().await;
    let mut fields = vec![
        ("rule[0][name]", "disabled"),
        ("rule[0][action]", "reject"),
        ("rule[0][message]", "must not run"),
    ];
    fields.extend([
        ("rule[1][name]", "first-match"),
        ("rule[1][enabled]", "on"),
        ("rule[1][action]", "deployments"),
        ("rule[1][deployments]", r#"["up","canary"]"#),
    ]);
    assert_eq!(a.save("primary", &fields).await.status(), 303);
    let body = json!({"model":"primary","messages":[{"role":"user","content":"hello"}]} );
    assert_eq!(
        a.chat(body.clone()).await.json::<Value>().await.unwrap()["model"],
        "canary"
    );
    tokomotive::db::update_key_providers(&a.st.db, 1, &["not-up".into()])
        .await
        .unwrap();
    assert_eq!(a.chat(body).await.status(), 403);
}

/// The rule summary on the Routes page names Deployments as `provider /
/// model`, not as JSON.
#[tokio::test]
async fn rule_summary_shows_readable_deployments() {
    let a = app().await;
    let r = a.save("primary", &rule()).await;
    assert_eq!(r.status(), 303);
    let html =
        a.c.get(format!("{}/admin/routes", a.url))
            .header("cookie", "tokomotive_session=rules-session")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
    let summary = html
        .split("<dt>Deployment set</dt><dd>")
        .nth(1)
        .expect("summary");
    let summary = &summary[..summary.find("</dd>").unwrap()];
    assert!(summary.contains("up / canary"), "{summary}");
    assert!(
        !summary.contains("[&quot;") && !summary.contains("[\""),
        "{summary}"
    );
}
