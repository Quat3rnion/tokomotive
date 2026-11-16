//! Structured hook management. Saved secrets are never read back into a page.
use super::*;
use crate::hooks::Hook;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/hooks", get(page).post(save))
        .route("/admin/hooks/{id}/delete", post(delete))
        .route("/admin/hooks/{id}/rotate", post(rotate))
        .route("/admin/hooks/{id}/attach", post(attach))
        .route("/admin/hooks/attachments/{id}/delete", post(detach))
        .route("/admin/hooks/{id}/test", post(test))
}
#[derive(sqlx::FromRow)]
struct Attachment {
    id: i64,
    hook_id: i64,
    target: String,
}
#[derive(Template)]
#[template(path = "hooks.html")]
struct Page {
    base: Base,
    hooks: Vec<Hook>,
    attachments: Vec<Attachment>,
    routes: Option<Vec<db::ControlChoice>>,
    teams: Option<Vec<db::ControlChoice>>,
    keys: Vec<db::KeyRow>,
    pager: Pager,
}
async fn page(
    _: Admin,
    State(st): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Query(q): Query<PageQuery>,
) -> Response {
    let mut hooks: Vec<Hook> = tryr!(
        sqlx::query_as("SELECT * FROM hooks ORDER BY priority,name LIMIT ? OFFSET ?")
            .bind(q.limit() + 1)
            .bind(q.offset())
            .fetch_all(&st.db)
            .await
    );
    let pager = Pager::offset(&uri.to_string(), &q, hooks.len());
    hooks.truncate(q.limit() as usize);
    let attachments = tryr!(sqlx::query_as("SELECT a.id,a.hook_id,CASE WHEN a.global=1 THEN 'Global' WHEN a.route IS NOT NULL THEN 'Route: '||a.route WHEN a.team_id IS NOT NULL THEN 'Team: '||t.name ELSE 'Virtual Key: '||k.label||' (#'||k.id||')' END AS target FROM hook_attachments a JOIN hooks h ON h.id=a.hook_id LEFT JOIN teams t ON t.id=a.team_id LEFT JOIN virtual_keys k ON k.id=a.key_id WHERE h.id IN (SELECT id FROM hooks ORDER BY priority,name LIMIT ? OFFSET ?) ORDER BY a.id")
        .bind(q.limit()).bind(q.offset()).fetch_all(&st.db).await);
    render(Page {
        base: Base::new(&st),
        hooks,
        attachments,
        routes: tryr!(db::small_choices(&st.db, "routes").await),
        teams: tryr!(db::small_choices(&st.db, "teams").await),
        keys: tryr!(db::list_keys(&st.db).await),
        pager,
    })
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct HookForm {
    id: Option<i64>,
    name: String,
    url: String,
    pre_request: String,
    post_response: String,
    event: String,
    include_bodies: String,
    timeout_ms: Option<i64>,
    fail_closed: String,
    enabled: String,
    allow_private: String,
    priority: Option<i64>,
}
async fn save(_: Admin, State(st): State<AppState>, Form(f): Form<HookForm>) -> Response {
    let name = f.name.trim();
    if name.is_empty() || name.len() > 100 || name.chars().any(char::is_control) {
        return form_error("Use a name with 1 to 100 bytes and no control characters.");
    }
    if f.pre_request.is_empty() && f.post_response.is_empty() && f.event.is_empty() {
        return form_error("Select at least one stage.");
    }
    let timeout = f.timeout_ms.unwrap_or(2000);
    if !(1..=30000).contains(&timeout) {
        return form_error("Use a timeout from 1 to 30000 ms.");
    }
    let url = match reqwest::Url::parse(f.url.trim()) {
        Ok(v) => v,
        Err(_) => return form_error("Use a valid HTTPS URL."),
    };
    if crate::secrets::adapters::webhook_client(&url, !f.allow_private.is_empty(), &st.egress)
        .await
        .is_err()
    {
        return form_error("URL denied. Use HTTPS; private HTTP needs explicit permission.");
    }
    let new = f.id.is_none();
    if new
        && tryr!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM hooks")
                .fetch_one(&st.db)
                .await
        ) >= 64
    {
        return form_error("At most 64 hooks can be stored. Delete an unused hook first.");
    }
    let secret = crate::hooks::new_secret();
    let encrypted = tryr!(st.secret_key.encrypt(&secret));
    let result = if let Some(id) = f.id {
        sqlx::query("UPDATE hooks SET name=?,url=?,pre_request=?,post_response=?,event=?,include_bodies=?,timeout_ms=?,fail_closed=?,enabled=?,allow_private=?,priority=? WHERE id=?")
            .bind(name).bind(url.as_str()).bind(!f.pre_request.is_empty()).bind(!f.post_response.is_empty()).bind(!f.event.is_empty())
            .bind(!f.include_bodies.is_empty()).bind(timeout).bind(!f.fail_closed.is_empty()).bind(!f.enabled.is_empty())
            .bind(!f.allow_private.is_empty()).bind(f.priority.unwrap_or(0)).bind(id).execute(&st.db).await
    } else {
        sqlx::query("INSERT INTO hooks(name,url,pre_request,post_response,event,include_bodies,timeout_ms,fail_closed,enabled,allow_private,priority,secret) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)")
            .bind(name).bind(url.as_str()).bind(!f.pre_request.is_empty()).bind(!f.post_response.is_empty()).bind(!f.event.is_empty())
            .bind(!f.include_bodies.is_empty()).bind(timeout).bind(!f.fail_closed.is_empty()).bind(!f.enabled.is_empty())
            .bind(!f.allow_private.is_empty()).bind(f.priority.unwrap_or(0)).bind(encrypted).execute(&st.db).await
    };
    let result = match result {
        Ok(r) => r,
        Err(_) => return form_error("Cannot save hook. Use a unique name."),
    };
    if new {
        secret_response(&st, result.last_insert_rowid(), &secret)
    } else if result.rows_affected() == 0 {
        StatusCode::NOT_FOUND.into_response()
    } else {
        saved("/admin/hooks", "Hook saved.")
    }
}
#[derive(Template)]
#[template(path = "hook_secret.html")]
struct SecretPage<'a> {
    base: Base,
    id: i64,
    secret: &'a str,
}
fn secret_response(st: &AppState, id: i64, secret: &str) -> Response {
    let mut r = render(SecretPage {
        base: Base::new(st),
        id,
        secret,
    });
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    r
}
async fn delete(_: Admin, State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    tryr!(
        sqlx::query("DELETE FROM hooks WHERE id=?")
            .bind(id)
            .execute(&st.db)
            .await
    );
    saved("/admin/hooks", "Hook deleted.")
}
async fn rotate(_: Admin, State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    let secret = crate::hooks::new_secret();
    let result = tryr!(
        sqlx::query("UPDATE hooks SET secret=? WHERE id=?")
            .bind(tryr!(st.secret_key.encrypt(&secret)))
            .bind(id)
            .execute(&st.db)
            .await
    );
    if result.rows_affected() == 0 {
        return form_error("Select a listed hook.");
    }
    secret_response(&st, id, &secret)
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct AttachForm {
    route_name: String,
    team_name: String,
    key_id: String,
    global: String,
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
    let global = !f.global.is_empty();
    if [!route.is_empty(), !team.is_empty(), !key.is_empty(), global]
        .into_iter()
        .filter(|v| *v)
        .count()
        != 1
    {
        return form_error("Select exactly one target.");
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
    let key_id = if key.is_empty() {
        None
    } else {
        match key.parse::<i64>() {
            Ok(v) => Some(v),
            Err(_) => return form_error("Select a listed Virtual Key."),
        }
    };
    if sqlx::query(
        "INSERT INTO hook_attachments(hook_id,route,team_id,key_id,global) VALUES(?,?,?,?,?)",
    )
    .bind(id)
    .bind((!route.is_empty()).then_some(route))
    .bind(team_id)
    .bind(key_id)
    .bind(global)
    .execute(&st.db)
    .await
    .is_err()
    {
        return form_error("Select a listed hook and attachment target.");
    }
    saved("/admin/hooks", "Hook attached.")
}
async fn detach(_: Admin, State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    tryr!(
        sqlx::query("DELETE FROM hook_attachments WHERE id=?")
            .bind(id)
            .execute(&st.db)
            .await
    );
    saved("/admin/hooks", "Attachment deleted.")
}
#[derive(Deserialize)]
struct TestForm {
    stage: String,
}
async fn test(
    _: Admin,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<TestForm>,
) -> Response {
    let hook: Hook = match tryr!(
        sqlx::query_as("SELECT * FROM hooks WHERE id=?")
            .bind(id)
            .fetch_optional(&st.db)
            .await
    ) {
        Some(v) => v,
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    if !hook.stage(&f.stage) {
        return form_error("Select a stage enabled on this hook.");
    }
    let data = match f.stage.as_str() {
        "pre_request" => {
            serde_json::json!({"endpoint":"chat_completions","route":"sample","model":"sample","body":{"model":"sample","messages":[{"role":"user","content":"Hook test"}]},"key_id":0,"team_id":null,"user":"sample","metadata":{},"test":true})
        }
        "post_response" => {
            serde_json::json!({"body":{"choices":[{"message":{"role":"assistant","content":"Hook test"}}]},"status":200,"test":true})
        }
        _ => {
            serde_json::json!({"ledger":{"status":200,"usage":{"input":0,"output":0},"quote":{"cost_micros":0,"price_micros":0}},"test":true})
        }
    };
    let call = crate::hooks::call(&st, &hook, &f.stage, &crate::hooks::new_secret(), data).await;
    axum::Json(serde_json::json!({"status":call.status,"latency_ms":call.ms,"outcome":call.outcome,"decision":call.decision})).into_response()
}
