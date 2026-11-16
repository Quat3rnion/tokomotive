//! Operations pages: Ledger, Providers, Routes and Settings.

use tokomotive::{
    catalog::Quote,
    config::Config,
    db::{self, NewEntry},
    ratelimit::Limits,
    state::AppState,
    usage::Usage,
};

struct App {
    _dir: tempfile::TempDir,
    st: AppState,
    url: String,
    c: reqwest::Client,
    cookie: String,
}

async fn app() -> App {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg: Config = toml::from_str("").unwrap();
    cfg.server.database = format!("sqlite://{}/db.sqlite", dir.path().display());
    let st = tokomotive::init(&cfg, None).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = tokomotive::router(st.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let c = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let r = c
        .post(format!("{url}/setup"))
        .form(&[("password", "long-password"), ("mode", "full")])
        .send()
        .await
        .unwrap();
    let cookie = r
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("tokomotive_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    App {
        _dir: dir,
        st,
        url,
        c,
        cookie,
    }
}

impl App {
    async fn get(&self, path: &str) -> reqwest::Response {
        self.c
            .get(format!("{}{path}", self.url))
            .header("cookie", &self.cookie)
            .send()
            .await
            .unwrap()
    }

    async fn page(&self, path: &str) -> String {
        let r = self.get(path).await;
        assert_eq!(r.status(), 200, "{path}");
        r.text().await.unwrap()
    }

    async fn post(&self, path: &str, form: &[(&str, &str)]) -> reqwest::Response {
        self.c
            .post(format!("{}{path}", self.url))
            .header("cookie", &self.cookie)
            .form(form)
            .send()
            .await
            .unwrap()
    }

    async fn post_htmx(&self, path: &str, form: &[(&str, &str)]) -> reqwest::Response {
        self.c
            .post(format!("{}{path}", self.url))
            .header("cookie", &self.cookie)
            .header("hx-request", "true")
            .form(form)
            .send()
            .await
            .unwrap()
    }

    async fn provider(&self, name: &str, key: &str) {
        let r = self
            .post(
                "/admin/providers",
                &[
                    ("name", name),
                    ("kind", "openai"),
                    ("base_url", "http://127.0.0.1:9"),
                    ("api_key", key),
                ],
            )
            .await;
        assert_eq!(r.status(), 303);
    }

    async fn user(&self, name: &str) -> i64 {
        db::create_user_with_limits(&self.st.db, name, None, None, false, Limits::default())
            .await
            .unwrap()
    }

    async fn entry(
        &self,
        user: i64,
        provider: &str,
        model: &str,
        ts_ms: i64,
        unpriced: bool,
    ) -> i64 {
        let (key, _) = db::create_key_with_limits(&self.st.db, user, "", None, Limits::default())
            .await
            .unwrap();
        db::insert_entry(
            &self.st.db,
            &NewEntry {
                route_rule: None,
                cache_hit: false,
                session_affinity: None,
                reported_cost_micros: None,
                ts_ms,
                user_id: user,
                key_id: key,
                team_id: None,
                provider: provider.into(),
                subscription: false,
                endpoint: "chat_completions".into(),
                requested_model: Some(model.into()),
                model: Some(model.into()),
                status: 200,
                streamed: false,
                usage: Usage {
                    input: 1200,
                    output: 340,
                    cache_read: 56,
                    ..Default::default()
                },
                quote: Quote {
                    unpriced,
                    ..Default::default()
                },
                estimated: false,
                latency_ms: 812,
                ttft_ms: Some(120),
                client_aborted: false,
                provider_key: Some(0),
                attempts: 1,
            },
        )
        .await
        .unwrap()
        .expect("fixture Ledger Entry is written directly")
    }

    async fn set_simple(&self) {
        let r = self
            .post("/admin/settings/mode", &[("mode", "simple")])
            .await;
        assert_eq!(r.status(), 303);
    }
}

/// The text of the first `<thead>` of the page.
fn thead(html: &str) -> &str {
    html.split("<thead>")
        .nth(1)
        .unwrap()
        .split("</thead>")
        .next()
        .unwrap()
}

// ------------------------------------------------------------------ Ledger

#[tokio::test]
async fn ledger_puts_flags_next_to_status_and_groups_tokens() {
    let a = app().await;
    let u = a.user("maria").await;
    a.entry(u, "openai", "gpt-4o", 1_767_225_600_000, true)
        .await;
    let html = a.page("/admin/ledger").await;
    let head = thead(&html);
    let status = head.find(">Status<").expect("Status column");
    let flags = head.find(">Flags<").expect("Flags column");
    assert!(flags > status);
    assert_eq!(head[status..flags].matches("<th").count(), 1, "{head}");
    assert!(
        head.matches("<th").count() <= 12,
        "too many columns: {head}"
    );
    assert!(head.contains("Time (UTC)"), "{head}");
    assert!(head.contains("Tokens in / out"), "{head}");
    assert!(
        html.contains("<time datetime=\"2026-01-01T00:00:00Z\">2026-01-01 00:00:00</time>"),
        "{html}"
    );
    assert!(html.contains("1,200 / 340"));
    // The full breakdown stays available in the row details.
    assert!(html.contains("Cache write 5m") && html.contains("Attempts"));
    assert!(html.contains("Chat Completions"), "readable endpoint");
    assert!(!html.contains("(1 tries)") && !html.contains(">#0<"));
    assert!(html.contains("table class=\"ledger cards-sm\""));
}

#[tokio::test]
async fn ledger_filters_by_user_provider_model_and_date() {
    let a = app().await;
    let maria = a.user("maria").await;
    let jonas = a.user("jonas").await;
    // 2026-01-01 and 2026-02-01, UTC.
    a.entry(maria, "openai", "gpt-4o", 1_767_225_600_000, false)
        .await;
    a.entry(jonas, "anthropic", "claude-x", 1_769_904_000_000, false)
        .await;
    let html = a.page(&format!("/admin/ledger?user={jonas}")).await;
    assert!(
        html.contains("claude-x") && !html.contains(">gpt-4o<"),
        "{html}"
    );
    let html = a.page("/admin/ledger?provider=openai").await;
    assert!(html.contains(">gpt-4o<") && !html.contains("claude-x"));
    let html = a.page("/admin/ledger?model=claude-x").await;
    assert!(html.contains("claude-x") && !html.contains(">gpt-4o<"));
    let html = a.page("/admin/ledger?from=2026-01-15").await;
    assert!(html.contains("claude-x") && !html.contains(">gpt-4o<"));
    let html = a.page("/admin/ledger?to=2026-01-01").await;
    assert!(html.contains(">gpt-4o<") && !html.contains("claude-x"));
    // The filter bar keeps the values and labels each control.
    let html = a.page("/admin/ledger?model=claude-x&from=2026-01-15").await;
    assert!(html.contains("<form method=\"get\" action=\"/admin/ledger\""));
    assert!(
        html.contains("id=\"lf-model\" name=\"model\" value=\"claude-x\""),
        "{html}"
    );
    assert!(html.contains("<label for=\"lf-user\">User</label>"));
    // A bad date is explained on the page.
    let html = a.page("/admin/ledger?from=13/01/2026").await;
    assert!(html.contains("Use the date format YYYY-MM-DD."), "{html}");
}

#[tokio::test]
async fn ledger_pages_to_older_entries() {
    let a = app().await;
    let u = a.user("maria").await;
    let mut ids = vec![];
    for i in 0..101 {
        ids.push(
            a.entry(u, "openai", &format!("m{i}"), 1_767_225_600_000 + i, false)
                .await,
        );
    }
    let html = a.page("/admin/ledger?provider=openai").await;
    assert!(html.contains(">m100<") && !html.contains(">m0<"));
    let older = format!("/admin/ledger?provider=openai&#38;before={}", ids[1]);
    assert!(html.contains(&older), "{html}");
    let html = a
        .page(&format!("/admin/ledger?provider=openai&before={}", ids[1]))
        .await;
    assert!(html.contains(">m0<") && !html.contains(">m1<"));
    assert!(html.contains("rel=\"prev\">Previous</a>"));
}

#[tokio::test]
async fn reprice_redirects_and_names_models_without_cards() {
    let a = app().await;
    let u = a.user("maria").await;
    a.entry(u, "openai", "nonexistent", 1_767_225_600_000, true)
        .await;
    let r = a.post("/admin/reprice", &[]).await;
    assert_eq!(r.status(), 303);
    assert_eq!(r.headers()["location"], "/admin/ledger?filter=unpriced");
    let html = a.page("/admin/ledger?filter=unpriced").await;
    assert!(
        html.contains("Models with no Cost Card: nonexistent."),
        "{html}"
    );
}

#[tokio::test]
async fn simple_mode_redirects_money_filters() {
    let a = app().await;
    a.set_simple().await;
    let r = a.get("/admin/ledger?filter=unpriced").await;
    assert_eq!(r.status(), 303);
    assert_eq!(r.headers()["location"], "/admin/ledger");
    assert_eq!(a.get("/admin/ledger?filter=errors").await.status(), 200);
}

// --------------------------------------------------------------- Providers

#[tokio::test]
async fn env_keys_show_their_name_and_whether_they_are_set() {
    let a = app().await;
    a.provider("openai", "env:TOKO_UX_SURELY_UNSET_VAR").await;
    let r = a
        .post("/admin/providers/openai/keys", &[("api_key", "env:PATH")])
        .await;
    assert_eq!(r.status(), 303);
    let r = a
        .post(
            "/admin/providers/openai/keys",
            &[("api_key", "sk-plain-4b8e")],
        )
        .await;
    assert_eq!(r.status(), 303);
    let html = a.page("/admin/providers").await;
    assert!(
        html.contains("<code>env:TOKO_UX_SURELY_UNSET_VAR</code>"),
        "{html}"
    );
    assert!(html.contains("<code>env:PATH</code>"));
    assert!(html.contains("<span class=\"tag warn\">not set</span>"));
    assert!(html.contains("<span class=\"tag good\">set</span>"));
    assert!(html.contains("<code>****4b8e</code>"));
    assert!(!html.contains("<code>****</code>"));
}

#[tokio::test]
async fn key_counts_use_plurals_and_warn_when_empty() {
    let a = app().await;
    a.provider("one", "sk-one-1111").await;
    a.provider("none", "").await;
    a.provider("two", "sk-two-2222").await;
    a.post("/admin/providers/two/keys", &[("api_key", "sk-two-3333")])
        .await;
    let html = a.page("/admin/providers").await;
    assert!(!html.contains("1 keys"), "{html}");
    assert!(html.contains("1 Provider Key."));
    assert!(html.contains("2 Provider Keys."));
    assert!(html.contains("No Provider Key. Requests to this Provider fail."));
}

#[tokio::test]
async fn provider_results_and_sign_in_show_inside_the_provider() {
    let a = app().await;
    a.provider("openai", "sk-one-1111").await;
    let r = a
        .post(
            "/admin/providers",
            &[
                ("name", "claude-max"),
                ("kind", "anthropic"),
                ("auth", "claude"),
                // A trailing /v1 is accepted for a Subscription Provider.
                ("base_url", "https://api.anthropic.com/v1"),
            ],
        )
        .await;
    assert_eq!(r.status(), 303);
    let html = a.page("/admin/providers").await;
    assert!(!html.contains("id=\"errors\""));
    assert!(html.contains("hx-target=\"find .test-result\""), "{html}");
    assert!(html.contains("hx-target=\"next .signin\""));
    assert!(html.contains("<label for=\"p-openai-kind\">API format</label>"));
    assert!(html.contains("Claude Pro/Max"));
    assert!(!html.contains("Authentication: claude"));
    // Disconnected: a badge, Sign in, and no Disconnect button.
    assert!(html.contains("<span class=\"tag warn\">Disconnected</span>"));
    assert!(!html.contains("oauth/disconnect"));
    // The sign-in panel names the Provider and lets the Operator check the paste.
    let r = a
        .post_htmx("/admin/providers/claude-max/oauth/start", &[])
        .await;
    assert_eq!(r.status(), 200);
    let panel = r.text().await.unwrap();
    assert!(
        panel.contains("Sign in to claude-max (Claude Pro/Max)"),
        "{panel}"
    );
    assert!(panel.contains("(opens in a new tab)"));
    assert!(panel.contains("type=\"text\""));
    assert!(panel.contains("spellcheck=\"false\""));
    assert!(panel.contains("<ol"));
    assert!(!panel.contains("type=\"password\""));
}

#[tokio::test]
async fn provider_errors_keep_the_input() {
    let a = app().await;
    let r = a
        .post_htmx(
            "/admin/providers",
            &[
                ("name", "bad name"),
                ("kind", "openai"),
                ("base_url", "http://127.0.0.1:9"),
            ],
        )
        .await;
    assert_eq!(r.status(), 400);
    // The message goes into the form's own slot; the form is not replaced.
    assert_eq!(r.headers()["hx-retarget"], "find .form-error");
    assert!(r.text().await.unwrap().contains("Names must contain only"));
}

// ------------------------------------------------------------------ Routes

#[tokio::test]
async fn unknown_fallbacks_are_rejected_inline() {
    let a = app().await;
    a.provider("openai", "sk-one-1111").await;
    let route = |name: &'static str, fallbacks: &'static str| {
        vec![
            ("name", name),
            ("strategy", "priority"),
            ("fallbacks", fallbacks),
            ("deployment_provider", "openai"),
            ("deployment_model", "gpt-4o"),
            ("deployment_weight", "1"),
        ]
    };
    let r = a
        .post_htmx("/admin/routes", &route("fast", "does-not-exist"))
        .await;
    assert_eq!(r.status(), 400);
    assert_eq!(r.headers()["hx-retarget"], "find .form-error");
    assert!(
        r.text()
            .await
            .unwrap()
            .contains("Unknown Fallback: does-not-exist."),
    );
    assert!(a.st.router_table().routes.is_empty());
    let r = a.post_htmx("/admin/routes", &route("fast", "fast")).await;
    assert_eq!(r.status(), 400);
    // A Route and a catalog Model are valid Fallbacks.
    assert_eq!(
        a.post("/admin/routes", &route("cheap", "")).await.status(),
        303
    );
    db::create_model(&a.st.db, "gpt-4o-mini", "openai", &[])
        .await
        .unwrap();
    a.st.reload_catalog().await.unwrap();
    let r = a
        .post("/admin/routes", &route("fast", "cheap, gpt-4o-mini"))
        .await;
    assert_eq!(r.status(), 303);
    let html = a.page("/admin/routes").await;
    assert!(!html.contains("id=\"errors\""));
    assert!(html.contains("data-confirm=\"Delete Route fast?"));
}

// ---------------------------------------------------------------- Settings

#[tokio::test]
async fn mode_change_has_its_own_confirmed_form() {
    let a = app().await;
    let html = a.page("/admin/settings").await;
    assert!(html.contains("action=\"/admin/settings/mode\""), "{html}");
    assert!(html.contains("data-confirm=\"Switch to Simple Mode?"));
    assert!(html.contains("<code class=\"url\">http://127.0.0.1"));
    assert!(!html.contains("id=\"errors\""));
    // The server settings form no longer changes the mode.
    let main = html.split("action=\"/admin/settings\"").nth(1).unwrap();
    let main = main.split("</form>").next().unwrap();
    assert!(!main.contains("name=\"mode\""));
    a.set_simple().await;
    assert_eq!(a.st.settings().mode, "simple");
    let html = a.page("/admin/settings").await;
    assert!(html.contains("data-confirm=\"Switch to Full Mode?"));
    let r = a.post("/admin/settings/mode", &[("mode", "other")]).await;
    assert_eq!(r.status(), 400);
    assert_eq!(a.st.settings().mode, "simple");
}

#[tokio::test]
async fn saving_settings_keeps_the_mode() {
    let a = app().await;
    a.set_simple().await;
    let s = a.st.settings();
    let r = a
        .post(
            "/admin/settings",
            &[
                ("upstream_timeout_secs", "77"),
                ("session_days", &s.session_days.to_string()),
                ("failure_threshold", &s.router.failure_threshold.to_string()),
                ("cooldown_secs", &s.router.cooldown_secs.to_string()),
                ("max_attempts", &s.router.max_attempts.to_string()),
            ],
        )
        .await;
    assert_eq!(r.status(), 303);
    assert_eq!(a.st.settings().upstream_timeout_secs, 77);
    assert_eq!(a.st.settings().mode, "simple");
}

#[tokio::test]
async fn default_markup_is_read_only_in_settings() {
    let a = app().await;
    let r = a
        .post(
            "/admin/settings/markup",
            &[("default_markup_percent", "20")],
        )
        .await;
    assert_eq!(r.status(), 303);
    let html = a.page("/admin/settings").await;
    assert!(html.contains("Default Markup: 20%"), "{html}");
    assert!(html.contains("href=\"/admin/models#markup\">Change on Models</a>"));
    assert!(!html.contains("name=\"default_markup_percent\""));
    // Old scripts that still send the field keep working.
    let s = a.st.settings();
    let r = a
        .post(
            "/admin/settings",
            &[
                ("default_markup_percent", "15"),
                (
                    "upstream_timeout_secs",
                    &s.upstream_timeout_secs.to_string(),
                ),
                (
                    "reserve_output_tokens",
                    &s.reserve_output_tokens.to_string(),
                ),
                ("session_days", &s.session_days.to_string()),
                ("failure_threshold", &s.router.failure_threshold.to_string()),
                ("cooldown_secs", &s.router.cooldown_secs.to_string()),
                ("max_attempts", &s.router.max_attempts.to_string()),
            ],
        )
        .await;
    assert_eq!(r.status(), 303);
    assert_eq!(a.st.catalog().default_markup_bp, 1500);
    a.set_simple().await;
    let html = a.page("/admin/settings").await;
    assert!(!html.contains("Default Markup"));
    assert!(!html.contains("/admin/models#markup"));
}
