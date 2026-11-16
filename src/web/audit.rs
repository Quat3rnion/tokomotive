//! Audit at the dashboard boundary. Never copy request bodies or secret columns.
use super::*;
use serde_json::{Value, json};

/// JSON fields allowed in Route, Card and runtime-setting snapshots. Unknown
/// fields are dropped, so a new secret field is private by default.
fn public_json(value: Value) -> Value {
    const FIELDS: &[&str] = &[
        "name",
        "strategy",
        "deployments",
        "provider",
        "model",
        "weight",
        "fallbacks",
        "session_affinity",
        "upstream_timeout_secs",
        "reserve_output_tokens",
        "session_days",
        "mode",
        "router",
        "failure_threshold",
        "cooldown_secs",
        "max_attempts",
        "session_affinity_ttl_secs",
        "session_affinity_capacity",
        "input",
        "output",
        "cache_read",
        "cache_write_5m",
        "cache_write_1h",
        "image_input",
        "image_output",
        "base",
        "tiers",
        "above_input_tokens",
        "rates",
        "kind",
        "markup_bp",
    ];
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(k, _)| FIELDS.contains(&k.as_str()))
                .map(|(k, v)| (k, public_json(v)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(public_json).collect()),
        v => v,
    }
}

async fn rows(st: &AppState, table: &str, fields: &[&str], extra: &str) -> anyhow::Result<Value> {
    // All SQL fragments come from the static allowlists below, never the request.
    let columns = fields
        .iter()
        .map(|f| format!("'{f}',{f}"))
        .collect::<Vec<_>>()
        .join(",");
    let values: Vec<String> = sqlx::query_scalar(&format!(
        "SELECT json_object({columns}) FROM {table} {extra}"
    ))
    .fetch_all(&st.db)
    .await?;
    let mut values = values
        .into_iter()
        .map(|v| serde_json::from_str::<Value>(&v))
        .collect::<Result<Vec<_>, _>>()?;
    for row in &mut values {
        for field in ["config_json", "spec", "runtime"] {
            if let Some(value) = row.get_mut(field) {
                *value = public_json(serde_json::from_str(value.as_str().unwrap_or("null"))?);
            }
        }
    }
    Ok(Value::Array(values))
}
async fn snapshot(st: &AppState, target: &str) -> anyhow::Result<String> {
    let mut value = serde_json::Map::new();
    macro_rules! table { ($table:expr, [$($field:expr),*], $extra:expr) => {
        value.insert($table.into(),rows(st,$table,&[$($field),*],$extra).await?);
    }; }
    match target {
        "exporters" => {
            table!(
                "exporters",
                [
                    "id",
                    "name",
                    "kind",
                    "site",
                    "repo_id",
                    "spans",
                    "metrics",
                    "enabled",
                    "sample_rate",
                    "include_bodies",
                    "tag_allowlist",
                    "batch_size",
                    "interval_ms",
                    "allow_private"
                ],
                "ORDER BY id"
            );
        }
        "hooks" => {
            table!(
                "hooks",
                [
                    "id",
                    "name",
                    "pre_request",
                    "post_response",
                    "event",
                    "include_bodies",
                    "timeout_ms",
                    "fail_closed",
                    "enabled",
                    "allow_private",
                    "priority"
                ],
                "ORDER BY id"
            );
            table!(
                "hook_attachments",
                ["id", "hook_id", "route", "team_id", "key_id", "global"],
                "ORDER BY id"
            );
        }
        "users" => {
            table!(
                "users",
                [
                    "id",
                    "name",
                    "team_id",
                    "budget_micros",
                    "budget_period",
                    "rpm_limit",
                    "tpm_limit",
                    "disabled"
                ],
                "ORDER BY id"
            );
        }
        "teams" => {
            table!(
                "teams",
                ["id", "name", "budget_micros", "budget_period"],
                "ORDER BY id"
            );
        }
        "keys" => {
            table!(
                "virtual_keys",
                [
                    "id",
                    "user_id",
                    "label",
                    "allowed_models",
                    "allowed_providers",
                    "rpm_limit",
                    "tpm_limit",
                    "budget_micros",
                    "budget_period",
                    "expires_at",
                    "revoked_at"
                ],
                "ORDER BY id"
            );
        }
        "providers" => {
            table!(
                "providers",
                [
                    "name",
                    "kind",
                    "auth",
                    "include_usage",
                    "key_header",
                    "project_id",
                    "region",
                    "preset"
                ],
                "ORDER BY name"
            );
            table!(
                "oauth_credentials",
                ["provider", "flow"],
                "ORDER BY provider,flow"
            );
            let counts:Vec<(String,i64,bool)>=sqlx::query_as("SELECT name,json_array_length(keys_json),bedrock_credentials IS NOT NULL FROM providers ORDER BY name").fetch_all(&st.db).await?;
            value.insert("credential_metadata".into(), json!(counts));
        }
        "routes" => {
            table!("routes", ["name", "config_json"], "ORDER BY name");
        }
        "models" => {
            table!(
                "models",
                [
                    "id",
                    "name",
                    "provider",
                    "aliases",
                    "models_dev_provider",
                    "models_dev_model",
                    "cost_pinned"
                ],
                "ORDER BY id"
            );
            table!(
                "cards",
                ["id", "model_id", "team_id", "kind", "spec", "source"],
                "ORDER BY id"
            );
        }
        "reprice" => {
            let counts:(i64,i64,i64,i64)=sqlx::query_as("SELECT count(*),coalesce(sum(unpriced),0),coalesce(sum(price_micros),0),coalesce(sum(cost_micros),0) FROM ledger").fetch_one(&st.db).await?;
            value.insert("ledger_totals".into(), json!(counts));
        }
        "login" | "logout" => {
            let counts: Vec<(Option<i64>, i64)> = sqlx::query_as(
                "SELECT account_id,count(*) FROM sessions GROUP BY account_id ORDER BY account_id",
            )
            .fetch_all(&st.db)
            .await?;
            value.insert("sessions".into(), json!(counts));
        }
        _ => {
            table!(
                "admin_accounts",
                ["id", "username", "role", "session_version"],
                "ORDER BY id"
            );
            table!(
                "alert_webhooks",
                ["id", "host", "format", "enabled"],
                "ORDER BY id"
            );
            // Only these scalar settings and the explicitly scrubbed runtime
            // document can enter the log. URLs, tokens and hashes cannot.
            table!(
                "settings",
                ["key", "value"],
                "WHERE key IN ('default_markup_bp','audit_retention_days','models_dev_enabled','alert_thresholds') ORDER BY key"
            );
            let runtime = crate::settings::value(&st.db, "runtime_settings")
                .await?
                .unwrap_or_else(|| "null".into());
            value.insert(
                "runtime".into(),
                public_json(serde_json::from_str(&runtime)?),
            );
        }
    }
    Ok(serde_json::to_string(&value)?)
}

/// Find the affected identity for forms whose URL has no path ID.
fn changed_id(target: &str, before: &str, after: &str) -> Option<String> {
    let (table, key) = match target {
        "users" => ("users", "id"),
        "teams" => ("teams", "id"),
        "keys" => ("virtual_keys", "id"),
        "models" => ("models", "id"),
        "providers" => ("providers", "name"),
        "routes" => ("routes", "name"),
        "accounts" => ("admin_accounts", "id"),
        "alerts" => ("alert_webhooks", "id"),
        "hooks" => ("hooks", "id"),
        "exporters" => ("exporters", "id"),
        _ => return None,
    };
    let before: Value = serde_json::from_str(before).ok()?;
    let after: Value = serde_json::from_str(after).ok()?;
    fn keyed<'a>(rows: &'a [Value], key: &str) -> std::collections::BTreeMap<String, &'a Value> {
        rows.iter()
            .filter_map(|row| {
                row.get(key).map(|id| {
                    (
                        id.as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| id.to_string()),
                        row,
                    )
                })
            })
            .collect()
    }
    let before = keyed(before.get(table)?.as_array()?, key);
    let after = keyed(after.get(table)?.as_array()?, key);
    let ids: std::collections::BTreeSet<String> = before
        .keys()
        .chain(after.keys())
        .filter(|id| before.get(*id) != after.get(*id))
        .cloned()
        .collect();
    if ids.len() == 1 {
        ids.into_iter().next()
    } else {
        None
    }
}

pub(super) async fn run(
    st: &AppState,
    req: Request,
    next: Next,
    account: Option<crate::admin::Account>,
) -> Response {
    let matched = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|p| p.as_str())
        .unwrap_or("unknown")
        .to_owned();
    let path = req.uri().path();
    let target = if matched.starts_with("/admin/settings/exporters") {
        "exporters"
    } else if matched.starts_with("/admin/settings/accounts") {
        "accounts"
    } else if matched.starts_with("/admin/settings/alerts") {
        "alerts"
    } else if path == "/setup" {
        "setup"
    } else {
        path.split('/').nth(2).unwrap_or("dashboard")
    }
    .to_owned();
    let id = matched
        .split('/')
        .zip(path.split('/'))
        .find_map(|(pattern, value)| pattern.starts_with('{').then(|| value.to_owned()))
        .unwrap_or_default();
    let action = format!("{} {matched}", req.method());
    let scope = if matched == "/admin/settings/models-dev/refresh" {
        "models"
    } else {
        &target
    };
    let before = tryr!(snapshot(st, scope).await);
    if target == "login" || target == "setup" {
        // Authentication failures do not mutate dashboard state. Do not let
        // an unauthenticated flood fill the durable audit log.
        let response = next.run(req).await;
        if let Some(account) = response.extensions().get::<crate::admin::Account>() {
            let after = tryr!(snapshot(st, scope).await);
            tryr!(sqlx::query("INSERT INTO admin_audit(account_id,account,action,target_type,target_id,summary,before_json,after_json) VALUES(?,?,?,?,?,?,?,?)")
                .bind(account.id).bind(&account.username).bind(&action).bind(&target).bind(account.id.to_string()).bind(if target=="login" {"Local password login succeeded."} else {"Setup completed."}).bind(&before).bind(after).execute(&st.db).await);
        }
        return response;
    }
    // Reserve a durable entry before dispatch. A crash leaves an explicit
    // interrupted entry instead of silently losing the dashboard operation.
    let audit_id=tryr!(sqlx::query("INSERT INTO admin_audit(account_id,account,action,target_type,target_id,summary,before_json,after_json) VALUES(?,?,?,?,?,'Started; outcome not recorded.',?,'null')")
        .bind(account.as_ref().map(|a|a.id)).bind(account.as_ref().map_or("anonymous",|a|a.username.as_str()))
        .bind(&action).bind(&target).bind(&id).bind(&before).execute(&st.db).await).last_insert_rowid();
    let response = next.run(req).await;
    let account = response
        .extensions()
        .get::<crate::admin::Account>()
        .or(account.as_ref());
    let after = tryr!(snapshot(st, scope).await);
    let id = if id.is_empty() {
        changed_id(&target, &before, &after).unwrap_or_default()
    } else {
        id
    };
    let summary = format!(
        "HTTP {}. {}",
        response.status().as_u16(),
        if before == after {
            "No public fields changed; secrets are omitted."
        } else {
            "Public fields changed; secrets are omitted."
        }
    );
    tryr!(
        sqlx::query(
            "UPDATE admin_audit SET account_id=?,account=?,summary=?,after_json=?,target_id=? WHERE id=?"
        )
        .bind(account.map(|a| a.id))
        .bind(account.map_or("anonymous", |a| a.username.as_str()))
        .bind(summary)
        .bind(after)
        .bind(id)
        .bind(audit_id)
        .execute(&st.db)
        .await
    );
    response
}

#[derive(Default, Deserialize)]
pub(super) struct AuditQuery {
    #[serde(flatten)]
    page: PageQuery,
    #[serde(default)]
    account: String,
    #[serde(default)]
    action: String,
    #[serde(default)]
    target: String,
    #[serde(default)]
    since: String,
    #[serde(default)]
    until: String,
}
#[derive(sqlx::FromRow)]
struct AuditRow {
    id: i64,
    time: i64,
    account: String,
    action: String,
    target_type: String,
    target_id: String,
    summary: String,
    before_json: String,
    after_json: String,
}
impl AuditRow {
    fn result(&self) -> &str {
        self.summary.split('.').next().unwrap_or(&self.summary)
    }
    fn stamp(&self) -> String {
        chrono::DateTime::from_timestamp(self.time, 0)
            .map(|d| d.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_default()
    }
}
#[derive(Template)]
#[template(path = "audit.html")]
struct AuditPage {
    base: Base,
    rows: Vec<AuditRow>,
    q: AuditQuery,
    pager: Pager,
}

pub(super) async fn page(
    _: Admin,
    State(st): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Query(mut q): Query<AuditQuery>,
) -> Response {
    q.page.limit.get_or_insert(25);
    let time = |s: &str| -> anyhow::Result<Option<i64>> {
        if s.is_empty() {
            return Ok(None);
        }
        let date = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M")
            .map_err(|_| anyhow::anyhow!("Use a UTC date and time."))?;
        Ok(Some(date.and_utc().timestamp()))
    };
    let since = tryform!(time(&q.since));
    let until = tryform!(time(&q.until));
    let mut rows=tryr!(sqlx::query_as("SELECT id,time,account,action,target_type,target_id,summary,before_json,after_json FROM admin_audit WHERE (?='' OR account=?) AND (?='' OR instr(action,?)>0) AND (?='' OR target_type=? OR target_id=?) AND (? IS NULL OR time>=?) AND (? IS NULL OR time<=?) ORDER BY id DESC LIMIT ? OFFSET ?")
        .bind(&q.account).bind(&q.account).bind(&q.action).bind(&q.action).bind(&q.target).bind(&q.target).bind(&q.target)
        .bind(since).bind(since).bind(until).bind(until).bind(q.page.limit()+1).bind(q.page.offset()).fetch_all(&st.db).await);
    let pager = Pager::offset(&uri.to_string(), &q.page, rows.len());
    rows.truncate(q.page.limit() as usize);
    render(AuditPage {
        base: Base::new(&st),
        rows,
        q,
        pager,
    })
}
#[derive(Deserialize)]
pub(super) struct RetentionForm {
    days: i64,
}
pub(super) async fn retention(
    _: Admin,
    State(st): State<AppState>,
    Form(f): Form<RetentionForm>,
) -> Response {
    if !(1..=36500).contains(&f.days) {
        return bad("Use 1 to 36500 days for audit retention.");
    }
    tryr!(crate::settings::put(&st.db, "audit_retention_days", &f.days.to_string()).await);
    saved("/admin/settings#audit", "Audit retention saved.")
}
