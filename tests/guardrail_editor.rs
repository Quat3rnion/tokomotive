//! Issue #14: the Guardrails page edits rules with form fields, not JSON.
use serde_json::Value;
use tokomotive::{config::Config, state::AppState};

const SESSION: &str = "tokomotive_session=editor-session";

struct App {
    _dir: tempfile::TempDir,
    st: AppState,
    url: String,
    c: reqwest::Client,
}

async fn app() -> App {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg: Config = toml::from_str("").unwrap();
    cfg.server.database = format!("sqlite://{}/db.sqlite", dir.path().display());
    let st = tokomotive::init(&cfg, None).await.unwrap();
    tokomotive::settings::put(&st.db, "admin_password_hash", "fixture-password-hash")
        .await
        .unwrap();
    tokomotive::db::create_session(&st.db, "editor-session", 1)
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = tokomotive::router(st.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let c = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    App {
        _dir: dir,
        st,
        url,
        c,
    }
}

type Pairs<'a> = Vec<(&'a str, &'a str)>;

impl App {
    async fn post(&self, path: &str, form: &Pairs<'_>, htmx: bool) -> reqwest::Response {
        let mut r = self
            .c
            .post(format!("{}{path}", self.url))
            .header("cookie", SESSION)
            .header("origin", &self.url)
            .form(form);
        if htmx {
            r = r.header("hx-request", "true");
        }
        r.send().await.unwrap()
    }
    async fn page(&self) -> String {
        let r = self
            .c
            .get(format!("{}/admin/guardrails", self.url))
            .header("cookie", SESSION)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        r.text().await.unwrap()
    }
    /// The stored rules of the rule set with this name, as JSON values.
    async fn rules(&self, name: &str) -> Vec<Value> {
        let text: String = sqlx::query_scalar("SELECT rules FROM guardrail_sets WHERE name=?")
            .bind(name)
            .fetch_one(&self.st.db)
            .await
            .unwrap();
        serde_json::from_str(&text).unwrap()
    }
    async fn id(&self, name: &str) -> i64 {
        sqlx::query_scalar("SELECT id FROM guardrail_sets WHERE name=?")
            .bind(name)
            .fetch_one(&self.st.db)
            .await
            .unwrap()
    }
    async fn sets(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM guardrail_sets")
            .fetch_one(&self.st.db)
            .await
            .unwrap()
    }
}

fn set_fields(name: &str) -> Pairs<'_> {
    vec![
        ("name", name),
        ("max_text_bytes", "1048576"),
        ("hook_url", ""),
    ]
}

/// One rule per kind, plus an untouched empty row of the no-JS form.
fn three_kinds(name: &str) -> Pairs<'_> {
    let mut f = set_fields(name);
    f.extend([
        ("rule[0][name]", "EMAIL"),
        ("rule[0][kind]", "detector"),
        ("rule[0][detector]", "email"),
        ("rule[0][pattern]", ""),
        ("rule[0][words]", ""),
        ("rule[0][action]", "redact"),
        ("rule[0][direction]", "both"),
        ("rule[1][name]", "ticket"),
        ("rule[1][kind]", "regex"),
        ("rule[1][detector]", ""),
        ("rule[1][pattern]", r"\bPRJ-[0-9]{4}\b"),
        ("rule[1][words]", ""),
        ("rule[1][action]", "block"),
        ("rule[1][direction]", "request"),
        ("rule[2][name]", "codewords"),
        ("rule[2][kind]", "keywords"),
        ("rule[2][detector]", ""),
        ("rule[2][pattern]", ""),
        ("rule[2][words]", "alpha\r\n  beta  \r\n\r\nGamma Ray\r\n"),
        ("rule[2][whole_word]", "on"),
        ("rule[2][action]", "log"),
        ("rule[2][direction]", "response"),
        ("rule[3][name]", ""),
        ("rule[3][kind]", "detector"),
        ("rule[3][detector]", ""),
        ("rule[3][pattern]", ""),
        ("rule[3][words]", ""),
        ("rule[3][action]", "redact"),
        ("rule[3][direction]", "both"),
    ]);
    f
}

#[tokio::test]
async fn form_fields_for_each_kind_create_the_right_rules() {
    let a = app().await;
    let r = a
        .post("/admin/guardrails", &three_kinds("Kinds"), false)
        .await;
    assert_eq!(r.status(), 303);
    let rules = a.rules("Kinds").await;
    assert_eq!(rules.len(), 3, "the empty row is skipped: {rules:?}");
    assert_eq!(
        rules[0],
        serde_json::json!({"name":"EMAIL","kind":{"type":"detector","detector":"email"},"action":"redact","direction":"both"})
    );
    assert_eq!(
        rules[1],
        serde_json::json!({"name":"ticket","kind":{"type":"regex","pattern":r"\bPRJ-[0-9]{4}\b"},"action":"block","direction":"request"})
    );
    assert_eq!(
        rules[2],
        serde_json::json!({"name":"codewords","kind":{"type":"keywords","words":["alpha","beta","Gamma Ray"],"whole_word":true,"case_sensitive":false},"action":"log","direction":"response"})
    );
    // htmx saves navigate.
    let r = a
        .post("/admin/guardrails", &three_kinds("Kinds 2"), true)
        .await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["hx-redirect"], "/admin/guardrails");
}

#[tokio::test]
async fn editing_reordering_and_removing_rules() {
    let a = app().await;
    assert_eq!(
        a.post("/admin/guardrails", &three_kinds("Edit me"), false)
            .await
            .status(),
        303
    );
    let id = a.id("Edit me").await.to_string();
    // The JS moves rows in the page and keeps their indexes: the field order
    // is the rule order. A row with `remove` is deleted.
    let mut f = set_fields("Edited");
    f.extend([
        ("id", id.as_str()),
        ("rule[7][name]", "codewords"),
        ("rule[7][kind]", "keywords"),
        ("rule[7][words]", "alpha"),
        ("rule[7][case_sensitive]", "on"),
        ("rule[7][action]", "block"),
        ("rule[7][direction]", "both"),
        ("rule[0][name]", "EMAIL-ADDR"),
        ("rule[0][kind]", "detector"),
        ("rule[0][detector]", "phone"),
        ("rule[0][action]", "log"),
        ("rule[0][direction]", "request"),
        ("rule[1][name]", "ticket"),
        ("rule[1][kind]", "regex"),
        ("rule[1][pattern]", "PRJ"),
        ("rule[1][remove]", "on"),
        ("rule[1][action]", "block"),
        ("rule[1][direction]", "both"),
    ]);
    assert_eq!(a.post("/admin/guardrails", &f, true).await.status(), 200);
    assert_eq!(a.sets().await, 1);
    let rules = a.rules("Edited").await;
    let names: Vec<_> = rules.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["codewords", "EMAIL-ADDR"]);
    assert_eq!(rules[0]["kind"]["words"], serde_json::json!(["alpha"]));
    assert_eq!(rules[0]["kind"]["whole_word"], false);
    assert_eq!(rules[0]["kind"]["case_sensitive"], true);
    assert_eq!(rules[1]["kind"]["detector"], "phone");
    assert_eq!(rules[1]["action"], "log");
    assert_eq!(rules[1]["direction"], "request");
}

fn with<'a>(mut f: Pairs<'a>, key: &str, value: &'a str) -> Pairs<'a> {
    for pair in &mut f {
        if pair.0 == key {
            pair.1 = value;
        }
    }
    f
}

#[tokio::test]
async fn validation_errors_name_the_field_and_keep_typed_input() {
    let a = app().await;
    for (form, field, words) in [
        (
            with(three_kinds("Bad"), "rule[1][pattern]", "PRJ-(unclosed"),
            "rule[1][pattern]",
            &["Rule 2", "pattern"][..],
        ),
        (
            with(three_kinds("Bad"), "rule[1][pattern]", "x*"),
            "rule[1][pattern]",
            &["Rule 2", "empty text"][..],
        ),
        (
            with(three_kinds("Bad"), "rule[2][words]", " \r\n \r\n"),
            "rule[2][words]",
            &["Rule 3", "keyword"][..],
        ),
        (
            with(three_kinds("Bad"), "rule[2][name]", "EMAIL"),
            "rule[2][name]",
            &["Rule 3", "EMAIL", "Rule 1"][..],
        ),
        (
            with(three_kinds("Bad"), "rule[0][detector]", ""),
            "rule[0][detector]",
            &["Rule 1", "detector"][..],
        ),
        (
            with(three_kinds("Bad"), "rule[1][name]", ""),
            "rule[1][name]",
            &["Rule 2", "name"][..],
        ),
        (
            with(three_kinds("Bad"), "max_text_bytes", "0"),
            "max_text_bytes",
            &["Text limit"][..],
        ),
        (
            with(three_kinds("Bad"), "hook_url", "ftp://user:pw@x"),
            "hook_url",
            &["URL"][..],
        ),
        (with(three_kinds(""), "name", ""), "name", &["name"][..]),
    ] {
        // htmx: the message goes to the form's error slot and names the field.
        let r = a.post("/admin/guardrails", &form, true).await;
        assert_eq!(r.status(), 400, "{field}");
        let body = r.text().await.unwrap();
        assert!(
            body.contains(&format!("data-field=\"{field}\"")),
            "{field}: {body}"
        );
        for w in words {
            assert!(body.contains(w), "{field}: {w} in {body}");
        }
        // Without JS: the page again, with the typed values and the error.
        let r = a.post("/admin/guardrails", &form, false).await;
        assert_eq!(r.status(), 400, "{field}");
        let html = r.text().await.unwrap();
        assert!(html.contains("<h1>Guardrails</h1>"), "{field}");
        assert!(html.contains("class=\"form-error\""), "{field}");
        assert!(html.contains(words[0]), "{field}");
        assert!(
            html.contains("aria-invalid=\"true\""),
            "{field}: the field is marked"
        );
        if field != "rule[1][name]" {
            assert!(
                html.contains("value=\"ticket\""),
                "{field}: typed name kept"
            );
        }
        if field != "rule[2][words]" {
            assert!(html.contains("Gamma Ray"), "{field}: typed keywords kept");
        }
        assert_eq!(a.sets().await, 0, "{field}: nothing saved");
    }
    // A typed pattern is shown again, escaped.
    let r = a
        .post(
            "/admin/guardrails",
            &with(three_kinds("Bad"), "rule[1][pattern]", "<b>(x"),
            false,
        )
        .await;
    let html = r.text().await.unwrap();
    assert!(html.contains("value=\"&lt;b&gt;(x\"") || html.contains("value=\"&#60;b&#62;(x\""));
    assert!(!html.contains("<b>(x"));
}

#[tokio::test]
async fn rule_count_is_capped() {
    let a = app().await;
    let names: Vec<(String, String, String, String, String)> = (0..65)
        .map(|i| {
            (
                format!("rule[{i}][name]"),
                format!("r{i}"),
                format!("rule[{i}][kind]"),
                format!("rule[{i}][detector]"),
                format!("rule[{i}][action]"),
            )
        })
        .collect();
    let mut f = set_fields("Many");
    for (n, v, k, d, act) in &names {
        f.extend([
            (n.as_str(), v.as_str()),
            (k.as_str(), "detector"),
            (d.as_str(), "email"),
            (act.as_str(), "log"),
        ]);
    }
    let r = a.post("/admin/guardrails", &f, true).await;
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("64"));
    assert_eq!(a.sets().await, 0);
}

const JSON_RULES: &str = r#"[{"name":"EMAIL","kind":{"type":"detector","detector":"email"},"action":"redact","direction":"both"},{"name":"restricted","kind":{"type":"keywords","words":["restricted"],"whole_word":true,"case_sensitive":false},"action":"block","direction":"request"}]"#;

#[tokio::test]
async fn json_post_and_json_import_still_work() {
    let a = app().await;
    // The old script format: one `rules` field with a JSON array.
    let mut f = set_fields("Scripted");
    f.push(("rules", JSON_RULES));
    assert_eq!(a.post("/admin/guardrails", &f, false).await.status(), 303);
    let rules = a.rules("Scripted").await;
    assert_eq!(rules.len(), 2);
    assert_eq!(rules[1]["kind"]["words"], serde_json::json!(["restricted"]));
    let id = a.id("Scripted").await.to_string();

    // The page shows the saved JSON read-only with Copy, and an import box.
    let html = a.page().await;
    assert!(html.contains("Advanced: import or export JSON"));
    assert!(html.contains("data-copy=\"["));
    assert!(html.contains("&quot;restricted&quot;") || html.contains("&#34;restricted&#34;"));

    // Import replaces the rows of the form.
    let mut f = with(three_kinds("Scripted"), "name", "Scripted");
    f.push(("id", id.as_str()));
    f.push((
        "rules",
        r#"[{"name":"only","kind":{"type":"regex","pattern":"abc"},"action":"log","direction":"both"}]"#,
    ));
    assert_eq!(a.post("/admin/guardrails", &f, true).await.status(), 200);
    let rules = a.rules("Scripted").await;
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0]["name"], "only");

    // Bad JSON and invalid rules in JSON: an error on the import field.
    for (json, word) in [
        ("[{", "JSON array"),
        (
            r#"[{"name":"a","kind":{"type":"regex","pattern":"("},"action":"log","direction":"both"}]"#,
            "Rule 1",
        ),
        (
            r#"[{"name":"a","kind":{"type":"detector","detector":"email"},"action":"log","direction":"both"},{"name":"a","kind":{"type":"detector","detector":"phone"},"action":"log","direction":"both"}]"#,
            "Rule 2",
        ),
    ] {
        let mut f = set_fields("Scripted");
        f.push(("id", id.as_str()));
        f.push(("rules", json));
        let r = a.post("/admin/guardrails", &f, true).await;
        assert_eq!(r.status(), 400, "{json}");
        let body = r.text().await.unwrap();
        assert!(body.contains("data-field=\"rules\""), "{body}");
        assert!(body.contains(word), "{word} in {body}");
    }
    assert_eq!(a.rules("Scripted").await.len(), 1);
}

#[tokio::test]
async fn page_without_js_shows_rules_and_empty_rows() {
    let a = app().await;
    let mut f = set_fields("Contact");
    f.push(("rules", JSON_RULES));
    assert_eq!(a.post("/admin/guardrails", &f, false).await.status(), 303);
    let id = a.id("Contact").await;
    let html = a.page().await;
    // The saved rules as fields, in order.
    assert!(
        html.contains("name=\"rule[0][name]\" value=\"EMAIL\""),
        "{html}"
    );
    assert!(html.contains("name=\"rule[1][name]\" value=\"restricted\""));
    assert!(html.contains("restricted</textarea>"));
    // Empty rows for new rules, in the saved set and in the add form.
    assert!(html.contains(&format!("id=\"g{id}-r3-name\"")));
    assert!(html.contains("id=\"gnew-r2-name\""));
    // Each rule is a fieldset with a legend; each control has a label.
    assert!(html.contains("<fieldset class=\"rule\""));
    assert!(html.contains("<legend>Rule 1"));
    assert!(html.contains(&format!("for=\"g{id}-r0-name\"")));
    assert!(html.contains(&format!("for=\"g{id}-r0-pattern\"")));
    assert!(html.contains(&format!("for=\"g{id}-r0-words\"")));
    // Kind is a radio group; without JS, the kind fields all show.
    assert!(html.contains("name=\"rule[0][kind]\" value=\"detector\" checked"));
    assert!(html.contains("name=\"rule[1][kind]\" value=\"keywords\" checked"));
    // A no-JS remove control, and the JS row template and quick starts.
    assert!(html.contains("name=\"rule[0][remove]\""));
    assert!(html.contains("<template id=\"guard-rule-tpl\">"));
    assert!(html.contains("Redact emails and phone numbers"));
    assert!(html.contains("Block secrets and API keys"));
    assert!(html.contains("Redact credit cards and IBANs"));
    // The JSON textarea is no longer the main rule field.
    assert!(!html.contains("Rules (JSON)"));
}

#[tokio::test]
async fn test_box_lists_matches_per_rule_with_the_redacted_preview() {
    let a = app().await;
    let mut f = set_fields("Contact");
    f.push(("rules", JSON_RULES));
    assert_eq!(a.post("/admin/guardrails", &f, false).await.status(), 303);
    let id = a.id("Contact").await.to_string();
    let r = a
        .post(
            "/admin/guardrails/test",
            &vec![
                ("set_id", id.as_str()),
                ("text", "Mail a@example.com or b@example.org."),
                ("direction", "request"),
            ],
            true,
        )
        .await;
    assert_eq!(r.status(), 200);
    let html = r.text().await.unwrap();
    assert!(html.contains("Not blocked."), "{html}");
    assert!(html.contains("<strong>EMAIL</strong>"), "{html}");
    assert!(html.contains("2 matches"), "{html}");
    assert!(html.contains("Text after the rules"), "{html}");
    assert!(html.contains("Mail [REDACTED:EMAIL] or [REDACTED:EMAIL]."));
    assert!(!html.contains("a@example.com"));
}
