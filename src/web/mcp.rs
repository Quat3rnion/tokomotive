//! MCP administration. Credentials leave storage only in upstream requests.
use super::*;
use crate::mcp::{
    data::{self, Server},
    upstream::{self, Connection},
};
use serde_json::Value;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/mcp", get(index).post(create))
        .route("/admin/mcp/log", get(log))
        .route("/admin/mcp/retention", post(retention))
        .route("/admin/mcp/search", get(search))
        .route("/admin/mcp/{id}", post(update))
        .route("/admin/mcp/{id}/delete", post(delete))
        .route("/admin/mcp/{id}/test", post(test))
        .route("/admin/keys/{id}/mcp", post(key_access))
        .route("/admin/teams/{id}/mcp", post(team_access))
}
struct ServerView {
    server: Server,
    tools: Vec<ToolView>,
}
struct ToolView {
    name: String,
    description: String,
}
#[derive(Template)]
#[template(path = "mcp.html")]
struct ServersPage {
    base: Base,
    servers: Vec<ServerView>,
}
async fn index(_: Admin, State(st): State<AppState>) -> Response {
    let mut servers = Vec::new();
    for server in tryr!(data::servers(&st.db).await) {
        let raw: String = tryr!(
            sqlx::query_scalar("SELECT tools_json FROM mcp_servers WHERE id=?")
                .bind(server.id)
                .fetch_one(&st.db)
                .await
        );
        let tools: Vec<Value> = tryr!(serde_json::from_str(&raw));
        let tools = tools
            .into_iter()
            .map(|t| ToolView {
                name: t["name"].as_str().unwrap_or("").into(),
                description: t["description"].as_str().unwrap_or("").into(),
            })
            .collect();
        servers.push(ServerView { server, tools });
    }
    render(ServersPage {
        base: Base::new(&st),
        servers,
    })
}
#[derive(Deserialize)]
struct ServerForm {
    name: String,
    url: String,
    auth: String,
    #[serde(default, rename = "credential_header")]
    header_name: String,
    #[serde(default)]
    secret: String,
    allow_private: Option<String>,
    enabled: Option<String>,
}
async fn create(admin: Admin, State(st): State<AppState>, Form(f): Form<ServerForm>) -> Response {
    save(admin, &st, None, f).await
}
async fn update(
    admin: Admin,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<ServerForm>,
) -> Response {
    save(admin, &st, Some(id), f).await
}
async fn save(Admin(account): Admin, st: &AppState, id: Option<i64>, f: ServerForm) -> Response {
    let _lock = st.settings_write.lock().await;
    let name = f.name.trim();
    if !data::valid_server_name(name) {
        return bad(
            "Use 1 to 32 letters, digits, dots, hyphens or underscores. Do not start or end with an underscore, use two underscores together, or use only dots.",
        );
    }
    let url = match upstream::valid_url(f.url.trim()) {
        Ok(u) => u,
        Err(_) => return bad("Use an HTTP or HTTPS URL without credentials or a fragment."),
    };
    if url.scheme() == "http" && f.allow_private.is_none() {
        return bad("HTTP needs private network access. Use HTTPS for a public server.");
    }
    if url.query().is_some() {
        return bad(
            "Do not put credentials or query parameters in the URL. Use a header for authentication.",
        );
    }
    if !matches!(f.auth.as_str(), "none" | "bearer" | "header")
        || (f.auth == "header" && !upstream::valid_header(f.header_name.trim()))
    {
        return bad("Choose a supported authentication method and a valid custom header.");
    }
    let servers = tryr!(data::servers(&st.db).await);
    if id.is_none() && servers.len() >= 64 {
        return bad("The limit is 64 MCP servers.");
    }
    let old = id.and_then(|id| servers.iter().find(|s| s.id == id));
    if id.is_some() && old.is_none() {
        return bad("MCP server not found.");
    }
    if account.role != "owner"
        && f.auth != "none"
        && f.secret.is_empty()
        && old.is_some_and(|s| {
            s.secret.is_some()
                && (upstream::valid_url(&s.url).ok().as_ref() != Some(&url)
                    || s.auth != f.auth
                    || !s.header_name.eq_ignore_ascii_case(f.header_name.trim())
                    || s.allow_private != f.allow_private.is_some())
        })
    {
        return page_error(
            StatusCode::FORBIDDEN,
            "Only an Owner can change the destination or authentication while keeping an MCP credential. Enter a new credential.",
        );
    }
    let secret = if f.auth == "none" {
        None
    } else if f.secret.is_empty() {
        old.and_then(|s| s.secret.clone())
    } else {
        if f.secret.len() > 8192 || reqwest::header::HeaderValue::from_str(&f.secret).is_err() {
            return bad("Use a valid header value of at most 8192 bytes.");
        }
        Some(tryr!(st.secret_key.encrypt(&f.secret)))
    };
    if f.auth != "none" && secret.is_none() {
        return bad("Enter a credential for this server.");
    }
    let query = if id.is_some() {
        "UPDATE mcp_servers SET name=?,url=?,auth=?,header_name=?,secret=?,allow_private=?,enabled=?,tools_json='[]' WHERE id=?"
    } else {
        "INSERT INTO mcp_servers(name,url,auth,header_name,secret,allow_private,enabled,id) VALUES(?,?,?,?,?,?,?,?)"
    };
    let result = sqlx::query(query)
        .bind(name)
        .bind(url.as_str())
        .bind(&f.auth)
        .bind(f.header_name.trim())
        .bind(secret)
        .bind(f.allow_private.is_some())
        .bind(f.enabled.is_some())
        .bind(id)
        .execute(&st.db)
        .await;
    if let Err(e) = result {
        let e: anyhow::Error = e.into();
        if is_unique_violation(&e) {
            return bad("This MCP server name is in use.");
        }
        return internal(e);
    }
    saved(
        "/admin/mcp",
        "MCP server saved. Run a connection test to update the tool list.",
    )
}
async fn delete(_: Admin, State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    tryr!(
        sqlx::query("DELETE FROM mcp_servers WHERE id=?")
            .bind(id)
            .execute(&st.db)
            .await
    );
    saved(
        "/admin/mcp",
        "MCP server deleted. Its tool calls stay in the log.",
    )
}
async fn test(_: Admin, State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    let servers = tryr!(data::servers(&st.db).await);
    let Some(server) = servers.iter().find(|s| s.id == id) else {
        return bad("MCP server not found.");
    };
    match Connection::default().tools(&st, server).await {
        Ok(tools) => {
            tryr!(data::cache_tools(&st, id, &tools).await);
            saved(
                "/admin/mcp",
                &format!("Connection test passed. {} tools found.", tools.len()),
            )
        }
        Err(e) => form_error(format!(
            "Connection test failed: {}. Check the URL, network access and credentials.",
            e.0
        )),
    }
}

#[derive(Deserialize)]
struct AccessForm {
    mcp_all: Option<String>,
    #[serde(default)]
    mcp_allow: String,
}
async fn key_access(
    _: Admin,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<AccessForm>,
) -> Response {
    save_access(&st, "virtual_keys", id, f).await
}
async fn team_access(
    _: Admin,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<AccessForm>,
) -> Response {
    save_access(&st, "teams", id, f).await
}
async fn save_access(st: &AppState, table: &str, id: i64, f: AccessForm) -> Response {
    if f.mcp_allow.len() > 32768 {
        return bad("Choose fewer MCP tools.");
    }
    let choices = tryr!(choices(st).await);
    let patterns: Vec<String> = f
        .mcp_allow
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    if patterns.len() > 256
        || patterns
            .iter()
            .any(|s| !choices.iter().any(|c| &c.name == s))
    {
        return bad(
            "Choose MCP servers or tools from the list. Run a connection test if a tool is missing.",
        );
    }
    let count = tryr!(
        sqlx::query(&format!(
            "UPDATE {table} SET mcp_all=?,mcp_allow=? WHERE id=?"
        ))
        .bind(f.mcp_all.is_some())
        .bind(tryr!(serde_json::to_string(&patterns)))
        .bind(id)
        .execute(&st.db)
        .await
    )
    .rows_affected();
    if count == 0 {
        return bad("Key or Team not found.");
    }
    saved(
        if table == "teams" {
            "/admin/teams"
        } else {
            "/admin/keys"
        },
        "MCP access saved.",
    )
}
async fn choices(st: &AppState) -> anyhow::Result<Vec<pickers::Choice>> {
    let mut choices = Vec::new();
    for server in data::servers(&st.db).await? {
        let make = |name: String, detail: String| pickers::Choice {
            name,
            id: None,
            detail,
            tag: "",
            scope: String::new(),
        };
        choices.push(make(
            format!("{}/*", server.name),
            "All tools on this server".into(),
        ));
        let raw: String = sqlx::query_scalar("SELECT tools_json FROM mcp_servers WHERE id=?")
            .bind(server.id)
            .fetch_one(&st.db)
            .await?;
        for tool in serde_json::from_str::<Vec<Value>>(&raw)? {
            if let Some(name) = tool["name"].as_str() {
                choices.push(make(
                    format!("{}/{name}", server.name),
                    tool["description"].as_str().unwrap_or("").into(),
                ));
            }
        }
    }
    Ok(choices)
}
#[derive(Template)]
#[template(path = "entity_options.html")]
struct Options {
    hits: Vec<pickers::Choice>,
}
async fn search(
    _: Admin,
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<PageQuery>,
) -> Response {
    let text = q.q.chars().take(200).collect::<String>().to_lowercase();
    let mut hits = tryr!(choices(&st).await);
    hits.retain(|h| {
        h.name.to_lowercase().contains(&text) || h.detail.to_lowercase().contains(&text)
    });
    let count = hits.len();
    hits.truncate(20);
    let html = headers.contains_key("hx-request")
        || headers
            .get(header::ACCEPT)
            .and_then(|h| h.to_str().ok())
            .is_some_and(|h| h.contains("text/html"));
    let mut r = if html {
        render(Options { hits })
    } else {
        axum::Json(hits).into_response()
    };
    r.headers_mut()
        .insert("x-total-count", HeaderValue::from(count));
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r.headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Accept, HX-Request"));
    r
}

#[derive(sqlx::FromRow)]
struct CallView {
    id: i64,
    ts_ms: i64,
    key_name: String,
    user_name: String,
    server: String,
    tool: String,
    status: String,
    latency_ms: i64,
    error_class: String,
}
impl CallView {
    fn time(&self) -> String {
        chrono::DateTime::from_timestamp_millis(self.ts_ms)
            .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_default()
    }
}
#[derive(Template)]
#[template(path = "mcp_log.html")]
struct LogPage {
    base: Base,
    calls: Vec<CallView>,
    pager: Pager,
    retention: data::LogRetention,
}
async fn log(
    _: Admin,
    State(st): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Query(q): Query<PageQuery>,
) -> Response {
    let mut query = sqlx::QueryBuilder::new(
        "SELECT c.*,COALESCE(NULLIF(k.label,''),k.prefix,CAST(c.key_id AS TEXT)) AS key_name,COALESCE(u.name,CAST(c.user_id AS TEXT)) AS user_name FROM mcp_calls c LEFT JOIN virtual_keys k ON k.id=c.key_id LEFT JOIN users u ON u.id=c.user_id WHERE 1=1",
    );
    if !q.q.is_empty() {
        query
            .push(" AND (instr(c.server,")
            .push_bind(q.q.clone())
            .push(")>0 OR instr(c.tool,")
            .push_bind(q.q.clone())
            .push(")>0)");
    }
    q.keyset_desc(&mut query, "c.id");
    let rows = tryr!(query.build_query_as::<CallView>().fetch_all(&st.db).await);
    let page = crate::pagination::Page::new(rows, &q, |c| c.id);
    let pager = Pager::keyset(&uri.to_string(), &q, &page);
    render(LogPage {
        base: Base::new(&st),
        calls: page.items,
        pager,
        retention: tryr!(data::LogRetention::load(&st.db).await),
    })
}

#[derive(Deserialize)]
struct RetentionForm {
    days: i64,
    max_rows: i64,
}
async fn retention(_: Admin, State(st): State<AppState>, Form(f): Form<RetentionForm>) -> Response {
    if !(1..=3650).contains(&f.days) || !(1..=1_000_000).contains(&f.max_rows) {
        return bad("Use 1 to 3650 days and 1 to 1000000 rows for the MCP call log.");
    }
    let _lock = st.settings_write.lock().await;
    let mut tx = tryr!(crate::db::begin_write(&st.db, "mcp_retention_settings").await);
    for (key, value) in [("mcp_retention_days", f.days), ("mcp_max_rows", f.max_rows)] {
        tryr!(sqlx::query("INSERT INTO settings(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
            .bind(key).bind(value.to_string()).execute(&mut *tx).await);
    }
    tryr!(tx.commit().await);
    saved("/admin/mcp/log#retention", "MCP call log retention saved.")
}
