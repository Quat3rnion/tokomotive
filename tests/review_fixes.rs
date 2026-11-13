use llm_accounting::translate::{Direction, translate_request};
use serde_json::json;
use llm_accounting::{config::Config, state::AppState};

struct Env {
    _dir: tempfile::TempDir,
    st: AppState,
    url: String,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Env {
    fn drop(&mut self) { for task in &self.tasks { task.abort(); } }
}

#[tokio::test]
async fn seeded_provider_keeps_env_first_literal_fallback() {
    if std::env::var_os("LLMA_REVIEW_SEED_CHILD").is_none() {
        for value in [None, Some(""), Some("environment-key")] {
            let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
            cmd.args(["--exact", "seeded_provider_keeps_env_first_literal_fallback", "--nocapture"])
                .env("LLMA_REVIEW_SEED_CHILD", "1").env_remove("LLMA_REVIEW_PROVIDER_KEY");
            if let Some(value) = value { cmd.env("LLMA_REVIEW_PROVIDER_KEY", value); }
            let out = cmd.output().unwrap();
            assert!(out.status.success(), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        }
        return;
    }
    let cfg: Config = toml::from_str(r#"
[server]
database = "sqlite::memory:"
[[providers]]
name = "test"
kind = "openai"
base_url = "http://localhost"
api_key_env = "LLMA_REVIEW_PROVIDER_KEY"
api_key = "literal-key"
"#).unwrap();
    let expected = cfg.providers[0].keys();
    let st = llm_accounting::init(&cfg, None).await.unwrap();
    assert_eq!(*st.router_table().providers[0].keys, expected, "seed dropped the env-first literal fallback");
}

#[test]
fn blank_secret_key_uses_the_persistent_generated_key() {
    if std::env::var_os("LLMA_REVIEW_SECRET_CHILD").is_none() {
        for value in ["", "  \t\n"] {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "blank_secret_key_uses_the_persistent_generated_key", "--nocapture"])
                .env("LLMA_REVIEW_SECRET_CHILD", "1").env("LLM_ACCOUNTING_SECRET_KEY", value)
                .output().unwrap();
            assert!(out.status.success(), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        }
        return;
    }
    let dir = tempfile::tempdir_in("/arboretum").unwrap();
    let database = format!("sqlite://{}/db.sqlite", dir.path().display());
    let key = llm_accounting::settings::SecretKey::load(&database)
        .expect("blank secret key must use generated key instead of failing startup");
    let encrypted = key.encrypt("provider-key").unwrap();
    assert_eq!(llm_accounting::settings::SecretKey::load(&database).unwrap().decrypt(&encrypted).unwrap(), "provider-key");
    assert!(dir.path().join("secret.key").exists());
}

async fn env(routes: &str) -> Env {
    let upstream = axum::Router::new()
        .route("/v1/chat/completions", axum::routing::post(|| async {
            (axum::http::StatusCode::TOO_MANY_REQUESTS, axum::Json(json!({"error":{"message":"rate limited"}})))
        }))
        .route("/v1/messages", axum::routing::post(|| async {
            axum::Json(json!({"type":"message","model":"claude","content":[],"usage":{"input_tokens":0,"output_tokens":1}}))
        }));
    let ul = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_url = format!("http://{}", ul.local_addr().unwrap());
    let ut = tokio::spawn(async move { axum::serve(ul, upstream).await.unwrap() });
    let dir = tempfile::tempdir_in("/arboretum").unwrap();
    let cfg: Config = toml::from_str(&format!(r#"
[server]
database = "sqlite://{}/db.sqlite"
admin_password = "long-password"
admin_password_env = "LLMA_REVIEW_UNSET_ADMIN"
default_markup_percent = 0
[[providers]]
name = "anthropic"
kind = "anthropic"
base_url = "{upstream_url}"
[[providers]]
name = "openai"
kind = "openai"
base_url = "{upstream_url}"
[[models]]
name = "claude"
provider = "anthropic"
cost = {{ output = 1 }}
[[models]]
name = "gpt"
provider = "openai"
cost = {{ output = 1 }}
[[users]]
name = "budgeted"
budget_usd = 0.005
keys = ["sk-review"]
{routes}
"#, dir.path().display())).unwrap();
    let st = llm_accounting::init(&cfg, None).await.unwrap();
    let pl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", pl.local_addr().unwrap());
    let app = llm_accounting::router(st.clone());
    let pt = tokio::spawn(async move { axum::serve(pl, app).await.unwrap() });
    Env { _dir: dir, st, url, tasks: vec![ut, pt] }
}

#[tokio::test]
async fn conflicting_chat_limits_are_rejected_before_reservation() {
    let e = env("").await;
    let response = reqwest::Client::new().post(format!("{}/v1/chat/completions", e.url))
        .bearer_auth("sk-review").json(&json!({"model":"claude","messages":[],
            "max_tokens":1,"max_completion_tokens":10000})).send().await.unwrap();
    assert_eq!(response.status(), 400, "conflicting output limits must not bypass the budget");
    assert_eq!(response.json::<serde_json::Value>().await.unwrap()["error"]["type"], "invalid_request_error");
}

#[tokio::test]
async fn reasoning_budget_reserves_translated_output_limit() {
    let e = env(r#"
[[routes]]
name = "mixed"
deployments = [{ provider = "openai", model = "gpt" }, { provider = "anthropic", model = "claude" }]
"#).await;
    let response = reqwest::Client::new().post(format!("{}/v1/chat/completions", e.url))
        .bearer_auth("sk-review").json(&json!({"model":"mixed","messages":[],
            "reasoning_effort":"high"})).send().await.unwrap();
    assert_eq!(response.status(), 402, "budget only covers the untranslated 4096 token default");
}

#[tokio::test]
async fn sessions_cannot_outlive_the_password_they_verified() {
    let e = env("").await;
    let old_hash = llm_accounting::settings::value(&e.st.db, "admin_password_hash").await.unwrap().unwrap();
    llm_accounting::db::create_session(&e.st.db, "old-login", 1).await.unwrap();
    assert!(llm_accounting::db::session_valid(&e.st.db, "old-login").await.unwrap());
    let new_hash = llm_accounting::settings::hash_password("new-password").unwrap();
    llm_accounting::settings::put(&e.st.db, "admin_password_hash", &new_hash).await.unwrap();
    assert!(!llm_accounting::db::session_valid(&e.st.db, "old-login").await.unwrap(),
        "a session verified against the old password survived rotation");
    // A login that verified before rotation can finish inserting afterwards.
    llm_accounting::db::create_session_for_password(&e.st.db, "late-login", 1, Some(&old_hash)).await.unwrap();
    assert!(!llm_accounting::db::session_valid(&e.st.db, "late-login").await.unwrap());
    llm_accounting::db::create_session_for_password(&e.st.db, "new-login", 1, Some(&new_hash)).await.unwrap();
    assert!(llm_accounting::db::session_valid(&e.st.db, "new-login").await.unwrap());
}

#[test]
fn tool_result_images_follow_all_consecutive_tool_replies() {
    let image = json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}});
    let out = translate_request(Direction::MessagesToChat, &json!({"messages":[
        {"role":"assistant","content":[
            {"type":"tool_use","id":"a","name":"lookup","input":{}},
            {"type":"tool_use","id":"b","name":"lookup","input":{}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":[{"type":"text","text":"first"},image]}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"b","content":[image]}]},
        {"role":"assistant","content":"Done"}
    ]})).unwrap();
    let messages = out["messages"].as_array().unwrap();
    assert_eq!(messages[1]["content"], "first", "tool replies must be text-only");
    assert_eq!(messages[2]["role"], "tool");
    assert_eq!(messages[2]["content"], "");
    assert_eq!(messages[3]["role"], "user");
    assert_eq!(messages[3]["content"][0]["text"], "Images from tool result a:");
    assert_eq!(messages[3]["content"][1]["type"], "image_url");
    assert_eq!(messages[3]["content"][2]["text"], "Images from tool result b:");
    assert_eq!(messages[3]["content"][3]["type"], "image_url");
    assert_eq!(messages[4]["content"], "Done");
}

#[test]
fn empty_assistant_text_keeps_only_tool_use() {
    for content in [json!(""), json!([{"type":"text","text":""}])] {
        let out = translate_request(Direction::ChatToMessages, &json!({"messages":[{
            "role":"assistant","content":content,"tool_calls":[{
                "id":"call1","function":{"name":"lookup","arguments":"{}"}
            }]
        }]})).unwrap();
        assert_eq!(out["messages"][0]["content"].as_array().unwrap().len(), 1,
            "empty Anthropic text block must be omitted");
        assert_eq!(out["messages"][0]["content"][0]["type"], "tool_use");
    }
}

#[test]
fn null_chat_limits_use_non_null_fallback_or_default() {
    for (limits, expected) in [
        (json!({"max_tokens":null}), 4096),
        (json!({"max_completion_tokens":null}), 4096),
        (json!({"max_completion_tokens":null,"max_tokens":123}), 123),
    ] {
        let mut body = limits;
        body["messages"] = json!([{"role":"user","content":"Hi"}]);
        let out = translate_request(Direction::ChatToMessages, &body).unwrap();
        assert_eq!(out["max_tokens"], expected);
    }
}
