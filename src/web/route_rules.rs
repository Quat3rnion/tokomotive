//! A structured rule list. Form order is rule order; IDs survive moves and saves.
use super::*;
use crate::route_rules::{Action, Canary, Conditions, Rule};
use std::collections::BTreeMap;

pub(super) fn router() -> Router<AppState> {
    Router::new().route("/admin/routes/{name}/rules", post(save))
}

#[derive(Clone, Default)]
struct Row {
    key: String,
    fields: BTreeMap<String, Vec<String>>,
}
impl Row {
    fn summary_fields(&self) -> Vec<(&'static str, String)> {
        [
            ("key_id", "Virtual Key id"),
            ("user_id", "User id"),
            ("team_id", "Team id"),
            ("header_name", "Header name"),
            ("header_op", "Header operator"),
            ("header_value", "Header value"),
            ("model", "Requested model"),
            ("model_op", "Model operator"),
            ("endpoint", "Endpoint"),
            ("format", "Client format"),
            ("stream", "Stream"),
            ("min_tokens", "Minimum input tokens"),
            ("max_tokens", "Maximum input tokens"),
            ("has_tools", "Has tools"),
            ("has_images", "Has images"),
            ("deployments", "Deployment set"),
            ("target_route", "Target Route"),
            ("message", "Rejection message"),
            ("no_fallback", "No Fallback"),
            ("canary_action", "Canary action"),
            ("canary_deployments", "Canary Deployment set"),
            ("canary_route", "Canary Route"),
        ]
        .into_iter()
        .filter(|(field, _)| {
            !self.value(field).is_empty()
                && !(*field == "header_op" && self.value("header_name").is_empty())
                && !(*field == "model_op" && self.value("model").is_empty())
                && !(field.starts_with("canary_") && self.value("canary_percent").is_empty())
        })
        .map(|(field, label)| (label, self.fields[field].join(", ")))
        .collect()
    }
    fn value(&self, field: &str) -> &str {
        self.fields
            .get(field)
            .and_then(|v| v.first())
            .map(String::as_str)
            .unwrap_or_default()
    }
    fn selected(&self, field: &str, value: &str) -> bool {
        self.fields
            .get(field)
            .is_some_and(|vs| vs.iter().any(|v| v == value))
    }
    fn put(&mut self, field: &str, value: impl ToString) {
        self.fields.insert(field.into(), vec![value.to_string()]);
    }
    fn blank(key: usize) -> Self {
        let mut row = Self {
            key: key.to_string(),
            ..Default::default()
        };
        for (field, value) in [
            ("enabled", "on"),
            ("action", "deployments"),
            ("canary_action", "deployments"),
            ("header_op", "equals"),
            ("model_op", "equals"),
        ] {
            row.put(field, value);
        }
        row
    }
    fn from_rule(key: usize, rule: &Rule) -> anyhow::Result<Self> {
        let mut row = Self::blank(key);
        row.put("id", &rule.id);
        row.put("name", &rule.name);
        row.put("enabled", if rule.enabled { "on" } else { "off" });
        row.put("no_fallback", if rule.no_fallback { "on" } else { "off" });
        if let serde_json::Value::Object(fields) = serde_json::to_value(&rule.conditions)? {
            for (field, value) in fields {
                if !value.is_null() {
                    row.put(
                        &field,
                        value
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| value.to_string()),
                    );
                }
            }
        }
        row.set_action(&rule.action, false)?;
        if let Some(canary) = &rule.canary {
            row.put(
                "canary_percent",
                format!("{}.{:02}", canary.percent_bp / 100, canary.percent_bp % 100),
            );
            row.put("salt", &canary.salt);
            row.set_action(&canary.action, true)?;
        }
        Ok(row)
    }
    fn set_action(&mut self, action: &Action, canary: bool) -> anyhow::Result<()> {
        match action {
            Action::Deployments { deployments } => {
                self.put(
                    if canary { "canary_action" } else { "action" },
                    "deployments",
                );
                self.fields.insert(
                    if canary {
                        "canary_deployments"
                    } else {
                        "deployments"
                    }
                    .into(),
                    deployments
                        .iter()
                        .map(serde_json::to_string)
                        .collect::<Result<_, _>>()?,
                );
            }
            Action::Route { route } => {
                self.put(if canary { "canary_action" } else { "action" }, "route");
                self.put(
                    if canary {
                        "canary_route"
                    } else {
                        "target_route"
                    },
                    route,
                );
            }
            Action::Reject { message } => {
                self.put("action", "reject");
                self.put("message", message);
            }
        }
        Ok(())
    }
}

fn rows(pairs: Vec<(String, String)>) -> anyhow::Result<Vec<Row>> {
    let mut rows: Vec<Row> = Vec::new();
    for (key, value) in pairs {
        let Some((n, field)) = key
            .strip_prefix("rule[")
            .and_then(|k| k.split_once("]["))
            .and_then(|(n, f)| Some((n, f.strip_suffix(']')?)))
        else {
            continue;
        };
        anyhow::ensure!(
            n.len() <= 6 && n.bytes().all(|c| c.is_ascii_digit()),
            "Use valid rule rows."
        );
        let pos = if let Some(pos) = rows.iter().position(|r| r.key == n) {
            pos
        } else {
            anyhow::ensure!(rows.len() < 64, "Use at most 32 rules.");
            rows.push(Row {
                key: n.into(),
                ..Default::default()
            });
            rows.len() - 1
        };
        rows[pos]
            .fields
            .entry(field.into())
            .or_default()
            .push(value);
    }
    Ok(rows)
}

#[derive(Template)]
#[template(path = "route_rules.html")]
pub(super) struct Editor {
    route: String,
    rows: Vec<Row>,
    keys: Vec<(String, String)>,
    users: Vec<(String, String)>,
    teams: Vec<(String, String)>,
    routes: Vec<String>,
    deployments: Vec<(String, String)>,
    error: String,
}
impl Editor {
    fn label<'a>(&self, choices: &'a [(String, String)], id: &'a str) -> &'a str {
        choices
            .iter()
            .find(|(key, _)| key == id)
            .map_or(id, |(_, label)| label.as_str())
    }
    pub(super) async fn load(st: &AppState, route: &str) -> anyhow::Result<Self> {
        let rules = crate::route_rules::load(&st.db, route).await?;
        let mut rows = rules
            .iter()
            .enumerate()
            .map(|(i, r)| Row::from_rule(i, r))
            .collect::<anyhow::Result<Vec<_>>>()?;
        rows.push(Row::blank(rows.len()));
        let table = st.router_table();
        let route_cfg = table
            .routes
            .iter()
            .find(|r| r.name == route)
            .ok_or_else(|| anyhow::anyhow!("Route not found."))?;
        Ok(Self {
            route:route.into(),rows,error:String::new(),
            keys:sqlx::query_as("SELECT CAST(k.id AS TEXT),u.name || ' / ' || k.label FROM virtual_keys k JOIN users u ON u.id=k.user_id ORDER BY u.name,k.id LIMIT 200").fetch_all(&st.db).await?,
            users:sqlx::query_as("SELECT CAST(id AS TEXT),name FROM users ORDER BY name LIMIT 200").fetch_all(&st.db).await?,
            teams:sqlx::query_as("SELECT CAST(id AS TEXT),name FROM teams ORDER BY name LIMIT 200").fetch_all(&st.db).await?,
            routes:table.routes.iter().map(|r|r.name.clone()).collect(),
            deployments:route_cfg.deployments.iter().map(|d|Ok((serde_json::to_string(&(&d.provider,&d.model))?,format!("{} / {}",d.provider,d.model)))).collect::<anyhow::Result<_>>()?,
        })
    }
}

#[derive(Template)]
#[template(path = "route_rule_error.html")]
struct ErrorPage {
    base: Base,
    editor: String,
}

async fn save(
    _: Admin,
    State(st): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let _guard = st.settings_write.lock().await;
    let rows = match rows(pairs) {
        Ok(r) => r,
        Err(e) => return form_error(e),
    };
    let existing = tryr!(crate::route_rules::load(&st.db, &name).await);
    let table = st.router_table();
    let Some(route) = table.routes.iter().find(|r| r.name == name) else {
        return form_error("Route not found.");
    };
    let result = match parse(&rows, &existing, &table, route) {
        Ok(rules) => match crate::route_rules::validate_graph(&st.db, &name, &rules).await {
            Ok(()) => Ok(rules),
            Err(e) => Err(e),
        },
        Err(e) => Err(e),
    };
    let rules = match result {
        Ok(rules) => rules,
        Err(e) => {
            if headers.get("hx-request").is_some_and(|v| v == "true") {
                return form_error(e);
            }
            let mut editor = tryr!(Editor::load(&st, &name).await);
            editor.rows = rows;
            editor.error = e.to_string();
            let page = ErrorPage {
                base: Base::new(&st),
                editor: tryr!(editor.render()),
            };
            return (StatusCode::BAD_REQUEST, render(page)).into_response();
        }
    };
    let raw = tryr!(serde_json::to_string(&rules));
    tryr!(
        sqlx::query("UPDATE routes SET rules=? WHERE name=?")
            .bind(raw)
            .bind(&name)
            .execute(&st.db)
            .await
    );
    saved("/admin/routes", &format!("Route {name} rules saved."))
}

fn parse(
    rows: &[Row],
    existing: &[Rule],
    table: &crate::routing::RouterTable,
    route: &crate::routing::Route,
) -> anyhow::Result<Vec<Rule>> {
    let mut rules: Vec<Rule> = Vec::new();
    for row in rows {
        let get = |k: &str| row.value(k);
        if get("remove") == "on" {
            continue;
        }
        if get("name").trim().is_empty() {
            anyhow::ensure!(
                [
                    "deployments",
                    "target_route",
                    "message",
                    "header_name",
                    "header_value",
                    "model",
                    "key_id",
                    "user_id",
                    "team_id",
                    "canary_percent"
                ]
                .iter()
                .all(|k| get(k).is_empty()),
                "Enter a name for each rule."
            );
            continue;
        }
        anyhow::ensure!(
            rules.len() < 32 && get("name").len() <= 80,
            "Use at most 32 rules and names of at most 80 characters."
        );
        let primary_action = action(row, false, table, route)?;
        let mut conditions = serde_json::Map::new();
        for k in [
            "key_id",
            "user_id",
            "team_id",
            "header_name",
            "header_op",
            "header_value",
            "model",
            "model_op",
            "endpoint",
            "format",
            "stream",
            "min_tokens",
            "max_tokens",
            "has_tools",
            "has_images",
        ] {
            let v = get(k);
            if v.is_empty() {
                continue;
            }
            let value = if matches!(
                k,
                "key_id"
                    | "user_id"
                    | "team_id"
                    | "min_tokens"
                    | "max_tokens"
                    | "stream"
                    | "has_tools"
                    | "has_images"
            ) {
                serde_json::from_str(v)
                    .map_err(|_| anyhow::anyhow!("Use a valid value for {k}."))?
            } else {
                serde_json::Value::String(v.into())
            };
            conditions.insert(k.into(), value);
        }
        let conditions: Conditions = serde_json::from_value(serde_json::Value::Object(conditions))?;
        conditions.validate()?;
        let original = existing.iter().find(|r| r.id == get("id"));
        anyhow::ensure!(
            get("id").is_empty() || original.is_some(),
            "The rule changed. Reload the Route."
        );
        let id = original
            .map(|r| r.id.clone())
            .unwrap_or_else(|| hex::encode(rand::random::<[u8; 16]>()));
        anyhow::ensure!(
            !rules
                .iter()
                .any(|r| r.id == id || r.name == get("name").trim()),
            "Use a different name for each rule."
        );
        let canary = if get("canary_percent").is_empty() {
            None
        } else {
            let p = get("canary_percent").parse::<f64>().unwrap_or(f64::NAN);
            anyhow::ensure!(
                p.is_finite()
                    && (0.0..=100.0).contains(&p)
                    && (p * 100.0 - (p * 100.0).round()).abs() < 0.000001,
                "Use a Canary percentage from 0 to 100, with at most two decimal places."
            );
            let salt = if get("salt").is_empty() {
                original
                    .and_then(|r| r.canary.as_ref())
                    .map(|c| c.salt.clone())
                    .unwrap_or_else(|| hex::encode(rand::random::<[u8; 16]>()))
            } else {
                get("salt").into()
            };
            anyhow::ensure!(
                salt.len() <= 128,
                "Use at most 128 characters for the Canary salt."
            );
            Some(Canary {
                percent_bp: (p * 100.0).round() as u16,
                salt,
                action: action(row, true, table, route)?,
            })
        };
        rules.push(Rule {
            id,
            name: get("name").trim().into(),
            enabled: get("enabled") == "on",
            conditions,
            action: primary_action,
            no_fallback: get("no_fallback") == "on",
            canary,
        });
    }
    Ok(rules)
}
fn action(
    row: &Row,
    canary: bool,
    table: &crate::routing::RouterTable,
    route: &crate::routing::Route,
) -> anyhow::Result<Action> {
    match row.value(if canary { "canary_action" } else { "action" }) {
        "deployments" => {
            let deployments: Vec<(String, String)> = row
                .fields
                .get(if canary {
                    "canary_deployments"
                } else {
                    "deployments"
                })
                .into_iter()
                .flatten()
                .filter(|v| !v.is_empty())
                .map(|v| serde_json::from_str(v))
                .collect::<Result<_, _>>()?;
            anyhow::ensure!(
                !deployments.is_empty()
                    && deployments.iter().all(|(p, m)| route
                        .deployments
                        .iter()
                        .any(|d| &d.provider == p && &d.model == m)),
                "Choose Deployments from this Route."
            );
            Ok(Action::Deployments { deployments })
        }
        "route" => {
            let target = row.value(if canary {
                "canary_route"
            } else {
                "target_route"
            });
            anyhow::ensure!(
                table.routes.iter().any(|r| r.name == target),
                "Choose an existing target Route."
            );
            Ok(Action::Route {
                route: target.into(),
            })
        }
        "reject" if !canary => {
            let message = row.value("message");
            anyhow::ensure!(
                !message.trim().is_empty() && message.len() <= 500,
                "Enter a rejection message of at most 500 characters."
            );
            Ok(Action::Reject {
                message: message.into(),
            })
        }
        _ => anyhow::bail!("Choose a Deployment set, Route or rejection action."),
    }
}
