//! Operator rule sets and attachments. Pasted test text is not stored.
use super::*;
use crate::guardrails::{Direction, Engine, Rule, RuleSet};

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/guardrails", get(page).post(save))
        .route("/admin/guardrails/test", post(test))
        .route("/admin/guardrails/{id}/delete", post(delete))
        .route("/admin/guardrails/{id}/attach", post(attach))
        .route("/admin/guardrails/attachments/{id}/delete", post(detach))
}

#[derive(sqlx::FromRow)]
struct Set {
    id: i64,
    name: String,
    rules: String,
    hook_url: String,
    fail_closed: bool,
    max_text_bytes: i64,
}
impl Set {
    fn config(&self) -> anyhow::Result<RuleSet> {
        Ok(RuleSet {
            name: self.name.clone(),
            rules: serde_json::from_str(&self.rules)?,
            hook_url: self.hook_url.clone(),
            fail_closed: self.fail_closed,
            max_text_bytes: usize::try_from(self.max_text_bytes)?,
        })
    }
}
#[derive(sqlx::FromRow)]
struct Attachment {
    id: i64,
    set_id: i64,
    target: String,
}
#[derive(Template)]
#[template(path = "guardrails.html")]
struct Page {
    base: Base,
    sets: Vec<Set>,
    attachments: Vec<Attachment>,
    routes: Option<Vec<db::ControlChoice>>,
    teams: Option<Vec<db::ControlChoice>>,
    keys: Vec<db::KeyRow>,
}
async fn page(_: Admin, State(st): State<AppState>) -> Response {
    let sets = tryr!(
        sqlx::query_as("SELECT * FROM guardrail_sets ORDER BY name LIMIT 64")
            .fetch_all(&st.db)
            .await
    );
    let attachments = tryr!(sqlx::query_as("SELECT a.id,a.set_id,CASE WHEN a.route IS NOT NULL THEN 'Route: '||a.route WHEN a.team_id IS NOT NULL THEN 'Team: '||t.name ELSE 'Virtual Key: '||k.label||' (#'||k.id||')' END AS target FROM guardrail_attachments a LEFT JOIN teams t ON t.id=a.team_id LEFT JOIN virtual_keys k ON k.id=a.key_id ORDER BY a.id LIMIT 1000").fetch_all(&st.db).await);
    Html(tryr!(
        Page {
            base: Base::new(&st),
            sets,
            attachments,
            routes: tryr!(db::small_choices(&st.db, "routes").await),
            teams: tryr!(db::small_choices(&st.db, "teams").await),
            keys: tryr!(db::list_keys(&st.db).await)
        }
        .render()
    ))
    .into_response()
}
#[derive(Deserialize)]
struct SetForm {
    id: Option<i64>,
    name: String,
    rules: String,
    #[serde(default)]
    hook_url: String,
    fail_closed: Option<String>,
    max_text_bytes: usize,
}
async fn save(_: Admin, State(st): State<AppState>, Form(f): Form<SetForm>) -> Response {
    let name = f.name.trim();
    if name.is_empty() || name.len() > 100 || name.chars().any(char::is_control) {
        return form_error("Use a rule set name with 1 to 100 bytes, without control characters.");
    }
    let rules: Vec<Rule> = match serde_json::from_str(&f.rules) {
        Ok(v) => v,
        Err(_) => return form_error("Use a JSON array of rules. See the example below."),
    };
    let set = RuleSet {
        name: name.into(),
        rules,
        hook_url: f.hook_url.trim().into(),
        fail_closed: f.fail_closed.is_some(),
        max_text_bytes: f.max_text_bytes,
    };
    if Engine::new(vec![set.clone()]).is_err() {
        return form_error(
            "Check the rule names, patterns, detector names, text limit and hook URL. Patterns must not match empty text.",
        );
    }
    let rules = tryr!(serde_json::to_string_pretty(&set.rules));
    let _guard = st.settings_write.lock().await;
    let result = if let Some(id) = f.id {
        sqlx::query("UPDATE guardrail_sets SET name=?,rules=?,hook_url=?,fail_closed=?,max_text_bytes=? WHERE id=?").bind(name).bind(rules).bind(set.hook_url).bind(set.fail_closed).bind(set.max_text_bytes as i64).bind(id).execute(&st.db).await
    } else {
        let count: i64 = tryr!(
            sqlx::query_scalar("SELECT count(*) FROM guardrail_sets")
                .fetch_one(&st.db)
                .await
        );
        if count >= 64 {
            return form_error("Delete a rule set before you add another. The limit is 64.");
        }
        sqlx::query("INSERT INTO guardrail_sets(name,rules,hook_url,fail_closed,max_text_bytes) VALUES(?,?,?,?,?)").bind(name).bind(rules).bind(set.hook_url).bind(set.fail_closed).bind(set.max_text_bytes as i64).execute(&st.db).await
    };
    if let Err(e) = result {
        let e = e.into();
        return if is_unique_violation(&e) {
            form_error("This rule set name is in use. Use another name.")
        } else {
            internal(e)
        };
    }
    saved("/admin/guardrails", "Guardrail rule set saved.")
}
async fn delete(_: Admin, State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    tryr!(
        sqlx::query("DELETE FROM guardrail_sets WHERE id=?")
            .bind(id)
            .execute(&st.db)
            .await
    );
    saved("/admin/guardrails", "Guardrail rule set deleted.")
}
#[derive(Deserialize)]
struct AttachForm {
    #[serde(default)]
    route_name: String,
    #[serde(default)]
    team_name: String,
    #[serde(default)]
    key_id: String,
}
async fn attach(
    _: Admin,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<AttachForm>,
) -> Response {
    let route = f.route_name.trim();
    let team = f.team_name.trim();
    let key = f.key_id.trim();
    if [!route.is_empty(), !team.is_empty(), !key.is_empty()]
        .into_iter()
        .filter(|v| *v)
        .count()
        != 1
    {
        return form_error("Select one Route, Team or Virtual Key.");
    }
    let team_id: Option<i64> = if team.is_empty() {
        None
    } else {
        let found = tryr!(
            sqlx::query_scalar("SELECT id FROM teams WHERE name=?")
                .bind(team)
                .fetch_optional(&st.db)
                .await
        );
        if found.is_none() {
            return form_error("Select a listed Team.");
        }
        found
    };
    let key_id: Option<i64> = if key.is_empty() {
        None
    } else {
        match key.parse() {
            Ok(v) => Some(v),
            Err(_) => return form_error("Select a listed Virtual Key."),
        }
    };
    let result = sqlx::query(
        "INSERT INTO guardrail_attachments(set_id,route,team_id,key_id) VALUES(?,?,?,?)",
    )
    .bind(id)
    .bind((!route.is_empty()).then_some(route))
    .bind(team_id)
    .bind(key_id)
    .execute(&st.db)
    .await;
    if result.is_err() {
        return form_error("Select a listed rule set and a listed Route, Team or Virtual Key.");
    }
    saved("/admin/guardrails", "Guardrail attachment added.")
}
async fn detach(_: Admin, State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    tryr!(
        sqlx::query("DELETE FROM guardrail_attachments WHERE id=?")
            .bind(id)
            .execute(&st.db)
            .await
    );
    saved("/admin/guardrails", "Guardrail attachment deleted.")
}
#[derive(Deserialize)]
struct TestForm {
    set_id: i64,
    text: String,
    direction: Direction,
}
#[derive(Template)]
#[template(path = "guardrail_test.html")]
struct TestResult {
    verdict: crate::guardrails::Verdict,
}
async fn test(_: Admin, State(st): State<AppState>, Form(f): Form<TestForm>) -> Response {
    let set: Option<Set> = tryr!(
        sqlx::query_as("SELECT * FROM guardrail_sets WHERE id=?")
            .bind(f.set_id)
            .fetch_optional(&st.db)
            .await
    );
    let Some(set) = set else {
        return form_error("Select a listed rule set.");
    };
    if f.direction == Direction::Both {
        return form_error("Select request or response.");
    }
    let mut config = tryr!(set.config());
    // A test must not send pasted text to an external service.
    config.hook_url.clear();
    let engine = tryr!(Engine::new(vec![config]));
    Html(tryr!(
        TestResult {
            verdict: engine.inspect(f.direction, &f.text).await
        }
        .render()
    ))
    .into_response()
}
