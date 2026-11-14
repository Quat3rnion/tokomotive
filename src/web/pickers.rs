//! Entity picker search (base.html item 12): `GET /admin/search/{kind}?q=`
//! for Users, Teams, Providers, Routes and models.dev providers and models.
//!
//! - At most [`LIMIT`] rows. An empty query lists the first rows by name.
//! - Names that start with the query come first, then other matches.
//! - `X-Total-Count` is the number of all matches, so the picker can say
//!   how many more a search finds.
//! - `<li role="option">` HTML for the picker (Accept: text/html or htmx),
//!   JSON otherwise. The `name` is the value that the form submits.
use super::*;
use serde_json::Value;
use sqlx::Row;

const LIMIT: i64 = 20;

/// One choice of an entity picker.
#[derive(serde::Serialize)]
pub(super) struct Choice {
    /// The value that the form submits (a name or a models.dev ID).
    pub name: String,
    /// The database id, for Users and Teams.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    /// Context for the Operator, for example "Team ops, 2 keys".
    pub detail: String,
    /// A short tag, for example "subscription" or "disabled".
    #[serde(skip_serializing_if = "str::is_empty")]
    pub tag: &'static str,
    /// models.dev models: the models.dev provider ID of the model. The picker
    /// fills an empty provider field with it.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub scope: String,
}

#[derive(Template)]
#[template(path = "entity_options.html")]
struct EntityOptions {
    hits: Vec<Choice>,
}

#[derive(Deserialize, Default)]
pub(super) struct EntityQ {
    #[serde(default)]
    q: String,
    /// models.dev models: limit to this models.dev provider ID.
    #[serde(default)]
    provider: String,
}

fn plural(n: i64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn format_label(kind: &str) -> &'static str {
    match kind {
        "anthropic" => "Anthropic format",
        "gemini" => "Gemini format",
        "bedrock" => "AWS Bedrock format",
        "cohere" => "Cohere format",
        _ => "OpenAI format",
    }
}

fn strategy_label(s: &str) -> &'static str {
    match s {
        "round_robin" => "Round robin",
        "weighted" => "Weighted",
        _ => "Priority",
    }
}

/// The host of a URL, without the scheme and path.
fn host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?']).next().unwrap_or(rest)
}

/// Rows and the number of all matches. `q` is bound, never SQL text.
async fn find(
    st: &AppState,
    kind: &str,
    q: &str,
    provider: &str,
) -> anyhow::Result<(Vec<Choice>, i64)> {
    let db = &st.db;
    // ?1 is the query. Rank 0: the name starts with it; 1: other matches.
    let rank = |col: &str| format!("CASE WHEN instr(lower({col}),lower(?1))=1 THEN 0 ELSE 1 END");
    let (rows_sql, count_sql) = match kind {
        "users" => (
            format!(
                "SELECT u.id,u.name,t.name AS team,u.disabled,
                 (SELECT COUNT(*) FROM virtual_keys k WHERE k.user_id=u.id AND k.revoked_at IS NULL) AS keys
                 FROM users u LEFT JOIN teams t ON t.id=u.team_id
                 WHERE instr(lower(u.name),lower(?1))>0 OR instr(lower(coalesce(t.name,'')),lower(?1))>0
                 ORDER BY {}, (instr(lower(u.name),lower(?1))=0), u.name COLLATE NOCASE LIMIT ?2",
                rank("u.name")
            ),
            "SELECT COUNT(*) FROM users u LEFT JOIN teams t ON t.id=u.team_id
             WHERE instr(lower(u.name),lower(?1))>0 OR instr(lower(coalesce(t.name,'')),lower(?1))>0",
        ),
        "teams" => (
            format!(
                "SELECT t.id,t.name,(SELECT COUNT(*) FROM users u WHERE u.team_id=t.id) AS members
                 FROM teams t WHERE instr(lower(t.name),lower(?1))>0
                 ORDER BY {}, t.name COLLATE NOCASE LIMIT ?2",
                rank("t.name")
            ),
            "SELECT COUNT(*) FROM teams WHERE instr(lower(name),lower(?1))>0",
        ),
        "providers" => (
            format!(
                "SELECT name,kind,auth,base_url FROM providers WHERE instr(lower(name),lower(?1))>0
                 ORDER BY {}, name COLLATE NOCASE LIMIT ?2",
                rank("name")
            ),
            "SELECT COUNT(*) FROM providers WHERE instr(lower(name),lower(?1))>0",
        ),
        "routes" => (
            format!(
                "SELECT name,coalesce(json_extract(config_json,'$.strategy'),'priority') AS strategy,
                 coalesce(json_array_length(config_json,'$.deployments'),0) AS deployments
                 FROM routes WHERE instr(lower(name),lower(?1))>0
                 ORDER BY {}, name COLLATE NOCASE LIMIT ?2",
                rank("name")
            ),
            "SELECT COUNT(*) FROM routes WHERE instr(lower(name),lower(?1))>0",
        ),
        "models-dev-providers" => (
            "SELECT p.id,p.name,p.api,(SELECT COUNT(*) FROM models_dev_models m WHERE m.provider=p.id) AS models
                 FROM models_dev_providers p WHERE instr(lower(p.id),lower(?1))>0 OR instr(lower(p.name),lower(?1))>0
                 ORDER BY CASE WHEN instr(lower(p.id),lower(?1))=1 THEN 0 WHEN instr(lower(p.name),lower(?1))=1 THEN 1 ELSE 2 END,
                 p.id LIMIT ?2"
                .to_owned(),
            "SELECT COUNT(*) FROM models_dev_providers p WHERE instr(lower(p.id),lower(?1))>0 OR instr(lower(p.name),lower(?1))>0",
        ),
        "models-dev-models" => (
            "SELECT m.id,m.name,m.provider,m.context FROM models_dev_models m
             WHERE (?3='' OR m.provider=?3) AND (instr(lower(m.id),lower(?1))>0 OR instr(lower(m.name),lower(?1))>0)
             ORDER BY CASE WHEN lower(m.id)=lower(?1) THEN 0 WHEN instr(lower(m.id),lower(?1))=1 THEN 1 ELSE 2 END,
             m.id,m.provider LIMIT ?2"
                .to_owned(),
            "SELECT COUNT(*) FROM models_dev_models m
             WHERE (?2='' OR m.provider=?2) AND (instr(lower(m.id),lower(?1))>0 OR instr(lower(m.name),lower(?1))>0)",
        ),
        _ => anyhow::bail!("unknown picker kind"),
    };
    // Only models.dev models use the provider ID: ?2 to count, ?3 for rows.
    let scoped = kind == "models-dev-models";
    let mut count = sqlx::query_scalar::<_, i64>(count_sql).bind(q);
    if scoped {
        count = count.bind(provider);
    }
    let total = count.fetch_one(db).await?;
    let mut rows = sqlx::query(&rows_sql).bind(q).bind(LIMIT);
    if scoped {
        rows = rows.bind(provider);
    }
    let rows = rows.fetch_all(db).await?;
    let hits = rows
        .into_iter()
        .map(|r| match kind {
            "users" => {
                let team: Option<String> = r.get("team");
                let team = team.map_or_else(|| "No Team".to_owned(), |t| format!("Team {t}"));
                Choice {
                    id: Some(r.get("id")),
                    name: r.get("name"),
                    detail: format!("{team}, {}", plural(r.get("keys"), "key", "keys")),
                    tag: if r.get::<bool, _>("disabled") {
                        "disabled"
                    } else {
                        ""
                    },
                    scope: String::new(),
                }
            }
            "teams" => Choice {
                id: Some(r.get("id")),
                name: r.get("name"),
                detail: plural(r.get("members"), "User", "Users"),
                tag: "",
                scope: String::new(),
            },
            "providers" => {
                let auth: String = r.get("auth");
                let auth: crate::provider::ProviderAuth =
                    serde_json::from_value(Value::String(auth)).unwrap_or_default();
                // Stored as JSON text, for example "\"anthropic\"".
                let kind: String = r.get("kind");
                let kind = kind.trim_matches('"');
                Choice {
                    id: None,
                    name: r.get("name"),
                    detail: format!("{}, {}", format_label(kind), settings::auth_label(auth)),
                    tag: if auth.is_subscription() {
                        "subscription"
                    } else {
                        ""
                    },
                    scope: String::new(),
                }
            }
            "routes" => {
                let strategy: String = r.get("strategy");
                Choice {
                    id: None,
                    name: r.get("name"),
                    detail: format!(
                        "{}, {}",
                        strategy_label(&strategy),
                        plural(r.get("deployments"), "Deployment", "Deployments")
                    ),
                    tag: "",
                    scope: String::new(),
                }
            }
            "models-dev-providers" => {
                let name: String = r.get("name");
                let api: String = r.get("api");
                let mut detail = vec![name];
                if !api.is_empty() {
                    detail.push(host(&api).to_owned());
                }
                detail.push(plural(r.get("models"), "model", "models"));
                Choice {
                    id: None,
                    name: r.get("id"),
                    detail: detail.join(", "),
                    tag: "",
                    scope: String::new(),
                }
            }
            _ => {
                let id: String = r.get("id");
                let name: String = r.get("name");
                let dev: String = r.get("provider");
                let mut detail = vec![];
                if name != id {
                    detail.push(name);
                }
                if provider.is_empty() {
                    detail.push(dev.clone());
                }
                if let Some(c) = r
                    .get::<Option<i64>, _>("context")
                    .and_then(|c| u64::try_from(c).ok())
                {
                    detail.push(format!("{} context", context_text(c)));
                }
                Choice {
                    id: None,
                    name: id,
                    detail: detail.join(", "),
                    tag: "",
                    scope: dev,
                }
            }
        })
        .collect();
    Ok((hits, total))
}

pub(super) async fn search(
    _: Admin,
    State(st): State<AppState>,
    Path(kind): Path<String>,
    headers: HeaderMap,
    Query(q): Query<EntityQ>,
) -> Response {
    if !matches!(
        kind.as_str(),
        "users" | "teams" | "providers" | "routes" | "models-dev-providers" | "models-dev-models"
    ) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let text: String = q.q.trim().chars().take(200).collect();
    let provider: String = q.provider.trim().chars().take(200).collect();
    let (hits, total) = tryr!(find(&st, &kind, &text, &provider).await);
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let mut r = if !headers.contains_key("hx-request") && !accept.contains("text/html") {
        axum::Json(hits).into_response()
    } else {
        render(EntityOptions { hits })
    };
    let h = r.headers_mut();
    h.insert("x-total-count", HeaderValue::from(total));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::VARY, HeaderValue::from_static("Accept, HX-Request"));
    r
}
