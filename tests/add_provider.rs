//! Add a Provider: the provider-type search, the guided form for each
//! category, "Add and sign in" and the old form fields.

use serde_json::Value;
use tokomotive::{config::Config, db, oauth::OAuthEndpoints, state::AppState};

struct App {
    _dir: Option<tempfile::TempDir>,
    st: AppState,
    url: String,
    c: reqwest::Client,
    cookie: String,
}

async fn serve(st: AppState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = tokomotive::router(st);
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    url
}

async fn login(c: &reqwest::Client, url: &str) -> String {
    let r = c
        .post(format!("{url}/setup"))
        .form(&[("password", "long-password"), ("mode", "full")])
        .send()
        .await
        .unwrap();
    r.headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("tokomotive_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

async fn app() -> App {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg: Config = toml::from_str("").unwrap();
    cfg.server.database = format!("sqlite://{}/db.sqlite", dir.path().display());
    let st = tokomotive::init(&cfg, None).await.unwrap();
    let url = serve(st.clone()).await;
    let c = client();
    let cookie = login(&c, &url).await;
    App {
        _dir: Some(dir),
        st,
        url,
        c,
        cookie,
    }
}

impl App {
    async fn get(&self, path: &str, accept: &str) -> reqwest::Response {
        self.c
            .get(format!("{}{path}", self.url))
            .header("cookie", &self.cookie)
            .header("accept", accept)
            .send()
            .await
            .unwrap()
    }
    async fn page(&self, path: &str) -> String {
        let r = self.get(path, "text/html").await;
        assert_eq!(r.status(), 200, "{path}");
        r.text().await.unwrap()
    }
    async fn ids(&self, q: &str) -> Vec<String> {
        self.types(q)
            .await
            .iter()
            .map(|v| v["id"].as_str().unwrap().to_owned())
            .collect()
    }
    async fn types(&self, q: &str) -> Vec<Value> {
        let path = format!("/admin/search/provider-types?q={}", q.replace(' ', "%20"));
        let r = self.get(&path, "application/json").await;
        assert_eq!(r.status(), 200, "{path}");
        r.json::<Value>().await.unwrap().as_array().unwrap().clone()
    }
    async fn post(&self, path: &str, form: &[(&str, &str)], htmx: bool) -> reqwest::Response {
        let mut r = self
            .c
            .post(format!("{}{path}", self.url))
            .header("cookie", &self.cookie)
            .form(form);
        if htmx {
            r = r.header("hx-request", "true");
        }
        r.send().await.unwrap()
    }
    async fn models_dev(&self, id: &str, name: &str, api: &str) {
        sqlx::query("INSERT INTO models_dev_providers(id,name,api,npm,doc) VALUES(?,?,?,?,?)")
            .bind(id)
            .bind(name)
            .bind(api)
            .bind("@ai-sdk/openai-compatible")
            .bind("https://docs.example/keys")
            .execute(&self.st.db)
            .await
            .unwrap();
    }
}

const POPULAR: [&str; 12] = [
    "openai",
    "anthropic",
    "openrouter",
    "gemini",
    "deepseek",
    "kimi",
    "zai",
    "chatgpt",
    "claude",
    "github-copilot",
    "local",
    "custom-openai",
];

#[tokio::test]
async fn empty_query_returns_the_popular_choices() {
    let a = app().await;
    assert_eq!(a.ids("").await, POPULAR);
    let html = a.page("/admin/search/provider-types?q=").await;
    assert_eq!(html.matches("role=\"option\"").count(), 12);
}

#[tokio::test]
async fn search_matches_aliases_and_hosts_and_ranks_groups() {
    let a = app().await;
    a.models_dev("acme", "Acme AI", "https://llm.acme-cloud.example/v1")
        .await;
    a.models_dev(
        "deepseek-mirror",
        "DeepSeek Mirror",
        "https://mirror.example/v1",
    )
    .await;

    // Aliases.
    let glm = a.ids("glm").await;
    for id in ["zai", "zai-coding", "zai-messages", "zai-china-messages"] {
        assert!(glm.contains(&id.to_owned()), "glm finds {id}: {glm:?}");
    }
    let kimi = a.ids("kimi").await;
    assert_eq!(kimi[0], "kimi", "{kimi:?}");
    assert!(kimi.contains(&"kimi-code".to_owned()), "{kimi:?}");
    assert!(a.ids("grok").await.contains(&"xai".to_owned()));
    assert!(a.ids("ollama").await.contains(&"local".to_owned()));

    // Hosts of the base URL, for presets and models.dev entries.
    assert!(
        a.ids("api.deepseek.com")
            .await
            .contains(&"deepseek".to_owned())
    );
    let acme = a.types("acme-cloud").await;
    assert_eq!(acme[0]["id"], "acme");
    assert_eq!(acme[0]["source"], "models_dev");
    assert_eq!(acme[0]["host"], "llm.acme-cloud.example");

    // Exact first, then verified presets, then models.dev.
    let ds = a.ids("deepseek").await;
    assert_eq!(ds[0], "deepseek", "{ds:?}");
    let verified = ds.iter().position(|x| x == "deepseek-messages").unwrap();
    let mirror = ds.iter().position(|x| x == "deepseek-mirror").unwrap();
    assert!(verified < mirror, "{ds:?}");

    // Verified presets before subscription sign-ins.
    let openai = a.ids("openai").await;
    assert_eq!(openai[0], "openai", "{openai:?}");
    assert!(openai.contains(&"chatgpt".to_owned()), "{openai:?}");
    let messages = kimi.iter().position(|x| x == "kimi-messages").unwrap();
    let code = kimi.iter().position(|x| x == "kimi-code").unwrap();
    assert!(messages < code, "{kimi:?}");

    // Each result names its tagline, source and API format.
    let z = a.types("zai-coding").await;
    assert_eq!(z[0]["id"], "zai-coding");
    assert_eq!(z[0]["label"], "Z.ai Coding Plan");
    assert_eq!(z[0]["tagline"], "API key");
    assert_eq!(z[0]["source"], "verified");
    assert_eq!(z[0]["format"], "OpenAI");
    let html = a.page("/admin/search/provider-types?q=zai-coding").await;
    assert!(html.contains("data-value=\"zai-coding\""), "{html}");
    assert!(html.contains("Z.ai Coding Plan"));
    assert!(html.contains("Verified"));
    assert!(html.contains("OpenAI format"));
    assert!(html.contains("api.z.ai"));
}

#[tokio::test]
async fn search_caps_at_twenty_and_requires_admin() {
    let a = app().await;
    for i in 0..30 {
        a.models_dev(
            &format!("zeta-{i}"),
            &format!("Zeta {i}"),
            &format!("https://zeta{i}.example/v1"),
        )
        .await;
    }
    assert_eq!(a.types("zeta").await.len(), 20);
    let html = a.page("/admin/search/provider-types?q=zeta").await;
    assert_eq!(html.matches("role=\"option\"").count(), 20);
    let r =
        a.c.get(format!("{}/admin/search/provider-types?q=open", a.url))
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 303);
    assert_eq!(r.headers()["location"], "/admin/login");
}

#[tokio::test]
async fn every_auth_type_and_native_kind_is_findable() {
    let a = app().await;
    for (q, id, tagline) in [
        ("chatgpt subscription", "chatgpt", "Subscription sign-in"),
        ("codex", "codex", "Device code"),
        ("claude pro", "claude", "Subscription sign-in"),
        ("hugging face oauth", "hf-oauth", "Device code"),
        ("entra", "entra", "Device code"),
        ("vertex", "vertex", "Google sign-in"),
        ("vertex", "vertex-sa", "Service account"),
        ("copilot", "github-copilot", "Device code"),
        ("qwen code", "qwen-code", "Device code"),
        ("kimi code", "kimi-code", "Device code"),
        ("gemini cli", "gemini-cli", "Subscription sign-in"),
        ("gemini", "gemini", "API key"),
        ("bedrock", "bedrock-native", "AWS credentials"),
        ("cohere", "cohere", "API key"),
        ("custom", "custom-openai", "Any compatible API"),
        ("custom", "custom-anthropic", "Any compatible API"),
        ("local", "local", "Self-hosted"),
    ] {
        let found = a.types(q).await;
        let hit = found
            .iter()
            .find(|v| v["id"] == id)
            .unwrap_or_else(|| panic!("{q} finds {id}: {found:?}"));
        assert_eq!(hit["tagline"], tagline, "{id}");
    }
}

#[tokio::test]
async fn add_parameter_renders_the_guided_form_for_each_category() {
    let a = app().await;
    // API key: prefilled, locked base URL with an Edit toggle, key help,
    // docs link and Advanced settings.
    let html = a.page("/admin/providers?add=deepseek").await;
    assert!(html.contains("id=\"add-provider\""));
    assert!(html.contains("name=\"preset\" value=\"deepseek\""));
    assert!(html.contains("name=\"name\" value=\"deepseek\""));
    assert!(html.contains("value=\"https://api.deepseek.com\""));
    assert!(html.contains("data-lock"));
    assert!(html.contains("data-unlock=\"np-url\""));
    assert!(html.contains("name=\"api_key\""));
    assert!(html.contains("Where to get a key"));
    assert!(html.contains("https://api-docs.deepseek.com/api/create-chat-completion/"));
    assert!(html.contains("<summary>Advanced</summary>"));
    assert!(html.contains("name=\"key_header\""));
    assert!(html.contains("name=\"include_usage\""));
    assert!(html.contains("OpenAI format · Bearer key · api.deepseek.com"));
    assert!(html.contains("Change provider"));
    assert!(!html.contains("name=\"aws_region\""));
    assert!(html.contains(">Add provider</button>"));

    // Subscription: explanation, risk note with the ADR, Add and sign in.
    let html = a.page("/admin/providers?add=github-copilot").await;
    assert!(html.contains("name=\"auth\" value=\"github_copilot\""));
    assert!(html.contains("0006-github-copilot.md"));
    assert!(html.contains("Add and sign in"));
    assert!(html.contains("device code"));
    assert!(html.contains("name=\"then\" value=\"signin\""));
    assert!(html.contains("hx-target=\"#np-signin\""));
    assert!(!html.contains("name=\"api_key\""));
    assert!(!html.contains("name=\"preset\" value=\"github-copilot\""));
    let html = a.page("/admin/providers?add=chatgpt").await;
    assert!(html.contains("name=\"auth\" value=\"chatgpt\""));
    assert!(html.contains("Add and sign in"));
    assert!(html.contains("0003-subscription-providers.md"));

    // Bedrock native: region and AWS credentials.
    let html = a.page("/admin/providers?add=bedrock-native").await;
    for field in [
        "aws_region",
        "aws_access_key",
        "aws_secret_key",
        "aws_session_token",
    ] {
        assert!(html.contains(&format!("name=\"{field}\"")), "{field}");
    }
    assert!(!html.contains("name=\"api_key\""));

    // Vertex: project, region, user OAuth client or service account.
    let html = a.page("/admin/providers?add=vertex").await;
    for field in ["project_id", "region", "client_id", "client_secret"] {
        assert!(html.contains(&format!("name=\"{field}\"")), "{field}");
    }
    assert!(html.contains("Add and sign in"));
    let html = a.page("/admin/providers?add=vertex-sa").await;
    assert!(html.contains("name=\"service_account\""));
    assert!(html.contains("type=\"file\""));
    assert!(!html.contains("name=\"client_id\""));

    // Entra: tenant, client ID and the Azure resource URL.
    let html = a.page("/admin/providers?add=entra").await;
    for field in ["tenant", "client_id", "base_url"] {
        assert!(html.contains(&format!("name=\"{field}\"")), "{field}");
    }
    assert!(html.contains("Azure resource URL"));

    // Hugging Face OAuth: client ID or CIMD.
    let html = a.page("/admin/providers?add=hf-oauth").await;
    assert!(html.contains("name=\"client_id\""));
    assert!(html.contains("/.well-known/oauth-cimd"));

    // Local server: examples.
    let html = a.page("/admin/providers?add=local").await;
    assert!(html.contains("http://localhost:11434/v1"));
    assert!(html.contains("http://localhost:1234/v1"));
    assert!(html.contains("http://localhost:8000/v1"));
    assert!(html.contains("name=\"base_url\" value=\"\""));

    // Custom: the full field set, all visible.
    let html = a.page("/admin/providers?add=custom-anthropic").await;
    for field in ["kind", "base_url", "api_key", "key_header", "include_usage"] {
        assert!(html.contains(&format!("name=\"{field}\"")), "{field}");
    }
    assert!(html.contains("value=\"anthropic\" selected"));
    assert!(!html.contains("<summary>Advanced</summary>"));

    // The fragment endpoint renders the same form and keeps a typed name.
    let html = a
        .page("/admin/provider-form?type=deepseek&name=my-deepseek")
        .await;
    assert!(html.contains("name=\"name\" value=\"my-deepseek\""));
    assert!(!html.contains("<html"));

    // An unknown type shows the picker and says so.
    let html = a.page("/admin/providers?add=nope").await;
    assert!(html.contains("No Provider type"));
}

#[tokio::test]
async fn no_javascript_select_submits_a_preset() {
    let a = app().await;
    let html = a.page("/admin/providers").await;
    assert!(html.contains("<select id=\"np-type\" name=\"preset\""));
    assert!(html.contains("<optgroup label=\"Verified API-key Providers\">"));
    assert!(html.contains("<optgroup label=\"Subscriptions and sign-in\">"));
    assert!(html.contains("<option value=\"deepseek\">"));
    // No Providers yet: the panel is open and at the top.
    assert!(!html.contains("data-collapse id=\"add-provider-panel\""));
    let html = a.page("/admin/providers?preset=deepseek").await;
    assert!(html.contains("name=\"preset\" value=\"deepseek\""));
    let r = a
        .post(
            "/admin/providers",
            &[
                ("preset", "deepseek"),
                ("then", "card"),
                ("auth", "api_key"),
                ("name", "deepseek"),
                ("kind", "openai"),
                ("base_url", "https://api.deepseek.com"),
                ("api_key", "sk-deepseek-1234"),
                ("key_header", "bearer"),
                ("include_usage", "false"),
            ],
            false,
        )
        .await;
    assert_eq!(r.status(), 303);
    assert_eq!(
        r.headers()["location"],
        "/admin/providers?added=deepseek#p-deepseek"
    );
    let preset: Option<String> =
        sqlx::query_scalar("SELECT preset FROM providers WHERE name='deepseek'")
            .fetch_one(&a.st.db)
            .await
            .unwrap();
    assert_eq!(preset.as_deref(), Some("deepseek"));

    // With Providers, the panel is first and collapsed behind a button.
    let html = a.page("/admin/providers").await;
    assert!(html.contains("data-toggle=\"add-provider-panel\""));
    assert!(html.contains("data-collapse id=\"add-provider-panel\""));
    let add = html.find("id=\"add-provider-panel\"").unwrap();
    let card = html.find("id=\"p-deepseek\"").unwrap();
    assert!(add < card);
    // ?add= opens it.
    let html = a.page("/admin/providers?add=").await;
    assert!(!html.contains("data-collapse id=\"add-provider-panel\""));
}

#[tokio::test]
async fn added_card_shows_next_steps_and_the_model_link_preselects_it() {
    let a = app().await;
    let r = a
        .post(
            "/admin/providers",
            &[
                ("then", "card"),
                ("name", "ds"),
                ("kind", "openai"),
                ("base_url", "https://api.deepseek.com"),
                ("api_key", "sk-deepseek-1234"),
            ],
            true,
        )
        .await;
    assert_eq!(r.headers()["hx-redirect"], "/admin/providers?added=ds#p-ds");
    let html = a.page("/admin/providers?added=ds").await;
    let card = &html[html.find("id=\"p-ds\"").unwrap()..];
    assert!(card.contains("data-added"));
    assert!(card.contains("Test connection"));
    assert!(card.contains("/admin/models?provider=ds#add-model-h"));
    let html = a.page("/admin/models?provider=ds").await;
    assert!(
        html.contains("<option value=\"ds\" selected>ds</option>"),
        "{html}"
    );
}

/// HX-Refresh reloads the current page after HX-Redirect sets the new
/// address, so it is only correct when only the #fragment changes.
#[tokio::test]
async fn htmx_redirect_to_another_page_with_a_fragment_does_not_refresh() {
    let a = app().await;
    let post = |name: &'static str, current: String| {
        a.c.post(format!("{}/admin/providers", a.url))
            .header("cookie", &a.cookie)
            .header("hx-request", "true")
            .header("hx-current-url", current)
            .form(&[
                ("then", "card"),
                ("name", name),
                ("kind", "openai"),
                ("base_url", "https://api.deepseek.com"),
            ])
            .send()
    };
    let r = post(
        "one",
        format!("{}/admin/providers?add=deepseek#add-provider-h", a.url),
    )
    .await
    .unwrap();
    assert_eq!(
        r.headers()["hx-redirect"],
        "/admin/providers?added=one#p-one"
    );
    assert!(r.headers().get("hx-refresh").is_none());
    let r = post("two", format!("{}/admin/providers?added=two", a.url))
        .await
        .unwrap();
    assert_eq!(r.headers()["hx-refresh"], "true");
}

#[tokio::test]
async fn name_suggestion_is_unique() {
    let a = app().await;
    for name in ["deepseek", "deepseek-2"] {
        let r = a
            .post(
                "/admin/providers",
                &[
                    ("name", name),
                    ("kind", "openai"),
                    ("base_url", "https://api.deepseek.com"),
                ],
                false,
            )
            .await;
        assert_eq!(r.status(), 303);
    }
    let html = a.page("/admin/providers?add=deepseek").await;
    assert!(
        html.contains("name=\"name\" value=\"deepseek-3\""),
        "{html}"
    );
    let html = a.page("/admin/providers?add=custom-openai").await;
    assert!(html.contains("name=\"name\" value=\"custom\""));
}

#[tokio::test]
async fn old_form_fields_still_create_providers() {
    let a = app().await;
    let r = a
        .post(
            "/admin/providers",
            &[
                ("preset", "openai"),
                ("name", "legacy"),
                ("auth", "api_key"),
                ("kind", "openai"),
                ("base_url", "https://api.openai.com"),
                ("api_key", "sk-legacy-1234"),
                ("key_header", "bearer"),
                ("include_usage", "true"),
                ("project_id", ""),
                ("region", "global"),
                ("aws_region", "us-east-1"),
            ],
            false,
        )
        .await;
    assert_eq!(r.status(), 303);
    assert_eq!(r.headers()["location"], "/admin/providers");
    let r = a
        .post(
            "/admin/providers",
            &[
                ("name", "sub"),
                ("auth", "claude"),
                ("base_url", "https://api.anthropic.com/v1"),
            ],
            false,
        )
        .await;
    assert_eq!(r.status(), 303);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM providers")
        .fetch_one(&a.st.db)
        .await
        .unwrap();
    assert_eq!(n, 2);
}

#[tokio::test]
async fn add_and_sign_in_creates_the_provider_and_returns_the_sign_in_panel() {
    let a = app().await;
    // A paste flow (ChatGPT): the panel has the sign-in link.
    let r = a
        .post(
            "/admin/providers",
            &[
                ("then", "signin"),
                ("auth", "chatgpt"),
                ("name", "chatgpt"),
                ("base_url", "https://api.openai.com"),
            ],
            true,
        )
        .await;
    assert_eq!(r.status(), 200);
    let html = r.text().await.unwrap();
    assert!(html.contains("Provider chatgpt added."), "{html}");
    assert!(
        html.contains("signin-panel") || html.contains("auth/callback"),
        "{html}"
    );
    assert!(html.contains("/admin/providers?added=chatgpt#p-chatgpt"));
    let auth: String = sqlx::query_scalar("SELECT auth FROM providers WHERE name='chatgpt'")
        .fetch_one(&a.st.db)
        .await
        .unwrap();
    assert_eq!(auth, "chatgpt");

    // Without JavaScript: the card opens, and its Sign in button is next.
    let r = a
        .post(
            "/admin/providers",
            &[
                ("then", "signin"),
                ("auth", "claude"),
                ("name", "claude"),
                ("base_url", "https://api.anthropic.com"),
            ],
            false,
        )
        .await;
    assert_eq!(r.status(), 303);
    assert_eq!(
        r.headers()["location"],
        "/admin/providers?added=claude#p-claude"
    );

    // A refused OAuth client removes the new Provider again.
    let r = a
        .post(
            "/admin/providers",
            &[
                ("then", "signin"),
                ("auth", "hugging_face"),
                ("name", "hf"),
                ("base_url", "https://router.huggingface.co"),
                ("client_id", "bad\nclient"),
            ],
            true,
        )
        .await;
    assert_eq!(r.status(), 400);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM providers WHERE name='hf'")
        .fetch_one(&a.st.db)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

async fn copilot_app() -> (App, tokio::task::JoinHandle<()>) {
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use serde_json::json;
    let mock = Router::new()
        .route(
            "/device",
            post(|| async {
                Json(json!({"device_code":"dev","user_code":"WXYZ-1234","verification_uri":"https://github.com/login/device","interval":1,"expires_in":900}))
            }),
        )
        .route(
            "/token",
            post(|| async { Json(json!({"error":"authorization_pending"})) }),
        )
        .route("/user", get(|| async { Json(json!({"chat_enabled":true})) }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(l, mock).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let mut cfg: Config = toml::from_str("").unwrap();
    cfg.server.database = format!("sqlite://{}/db.sqlite", dir.path().display());
    let database = db::connect(&cfg.server.database).await.unwrap();
    db::seed(&database, &cfg).await.unwrap();
    let st = AppState::new_with_oauth_endpoints(
        &cfg,
        database,
        None,
        OAuthEndpoints {
            copilot_device: format!("{base}/device"),
            copilot_token: format!("{base}/token"),
            copilot_user: format!("{base}/user"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let url = serve(st.clone()).await;
    let c = client();
    let cookie = login(&c, &url).await;
    (
        App {
            _dir: Some(dir),
            st,
            url,
            c,
            cookie,
        },
        task,
    )
}

#[tokio::test]
async fn add_and_sign_in_starts_a_device_code_for_copilot() {
    let (a, mock) = copilot_app().await;
    let r = a
        .post(
            "/admin/providers",
            &[
                ("then", "signin"),
                ("auth", "github_copilot"),
                ("kind", "anthropic"),
                ("name", "copilot"),
                ("base_url", "https://api.githubcopilot.com"),
            ],
            true,
        )
        .await;
    assert_eq!(r.status(), 200);
    let html = r.text().await.unwrap();
    assert!(html.contains("WXYZ-1234"), "{html}");
    assert!(html.contains("device/status"));
    assert!(!html.contains("\"dev\""));
    let kind: String = sqlx::query_scalar("SELECT kind FROM providers WHERE name='copilot'")
        .fetch_one(&a.st.db)
        .await
        .unwrap();
    assert_eq!(kind, "\"anthropic\"");
    mock.abort();
}

/// Not a test: serves a dashboard with a mock GitHub device endpoint for the
/// Playwright check. Run with
/// `TOKO_SHOTS_PORT=18431 cargo test --test add_provider serve_for_screenshots -- --ignored`.
#[tokio::test]
#[ignore]
async fn serve_for_screenshots() {
    let (a, _mock) = copilot_app().await;
    let port = std::env::var("TOKO_SHOTS_PORT").unwrap_or_else(|_| "18431".into());
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    println!("cookie {}", a.cookie);
    axum::serve(listener, tokomotive::router(a.st.clone()))
        .await
        .unwrap();
}
