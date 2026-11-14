//! Live configuration forms. No secret is sent back to the browser.
use super::*;
use crate::config::{Config, DeploymentCfg, ProviderCfg, RouteCfg, Strategy};
use crate::provider::ProviderKind;
use crate::settings as store;
use anyhow::Context;
use sqlx::Row;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/setup", get(setup_page).post(setup))
        .route("/admin/providers", get(providers).post(create_provider))
        .route("/admin/providers/{name}", post(update_provider))
        .route("/admin/providers/{name}/delete", post(delete_provider))
        .route("/admin/providers/{name}/keys", post(add_key))
        .route(
            "/admin/providers/{name}/keys/{index}/delete",
            post(delete_key),
        )
        .route("/admin/providers/{name}/test", post(test_provider))
        .route("/admin/routes", get(routes).post(create_route))
        .route("/admin/routes/{name}", post(update_route))
        .route("/admin/routes/{name}/delete", post(delete_route))
        .route("/admin/settings", get(settings_page).post(save_settings))
        .route("/admin/settings/password", post(change_password))
        .route("/admin/settings/mode", post(save_mode))
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsPage {
    models_dev_url: String,
    models_dev_enabled: bool,
    models_dev_error: String,
    models_dev_updated: String,
    base: Base,
    settings: std::sync::Arc<store::Settings>,
    markup: f64,
    metrics_set: bool,
}
async fn settings_page(_: Admin, State(st): State<AppState>) -> Response {
    let settings = st.settings();
    render(SettingsPage {
        models_dev_url: tryr!(store::value(&st.db, "models_dev_url").await).unwrap_or_default(),
        models_dev_enabled: tryr!(store::value(&st.db, "models_dev_enabled").await).as_deref()
            != Some("false"),
        models_dev_error: tryr!(store::value(&st.db, "models_dev_error").await).unwrap_or_default(),
        models_dev_updated: tryr!(store::value(&st.db, "models_dev_updated").await)
            .and_then(|s| s.parse().ok())
            .and_then(super::views::stamp_opt)
            .map(|s| s.text)
            .unwrap_or_else(|| "Not yet".into()),
        base: Base::new(&st),
        metrics_set: settings.metrics_token.is_some(),
        settings,
        markup: st.catalog().default_markup_bp as f64 / 100.0,
    })
}

#[derive(Deserialize)]
struct ServerForm {
    default_markup_percent: Option<String>,
    upstream_timeout_secs: String,
    reserve_output_tokens: Option<String>,
    session_days: String,
    failure_threshold: String,
    cooldown_secs: String,
    max_attempts: String,
    session_affinity_ttl_secs: Option<String>,
    session_affinity_capacity: Option<String>,
    /// The dashboard changes the mode with its own form. Scripts that still
    /// send the mode here keep working; without it the mode stays.
    mode: Option<String>,
    #[serde(default)]
    metrics_token: String,
    clear_metrics_token: Option<String>,
}
impl ServerForm {
    fn parse(&self, mut cfg: Config, mode: &str) -> anyhow::Result<(Config, store::Settings)> {
        let mode = self.mode.as_deref().unwrap_or(mode);
        if !matches!(mode, "full" | "simple") {
            anyhow::bail!("Choose Full Mode or Simple Mode.");
        }
        if let Some(value) = &self.default_markup_percent {
            cfg.server.default_markup_percent = value
                .trim()
                .parse()
                .context("Enter the Markup as a number, for example 20.")?;
        }
        cfg.server.upstream_timeout_secs = self
            .upstream_timeout_secs
            .parse()
            .context("Enter the upstream timeout as a whole number of seconds, 1 or more.")?;
        if let Some(value) = &self.reserve_output_tokens {
            cfg.server.reserve_output_tokens = value
                .parse()
                .context("Enter the reserved output tokens as a whole number, 0 or more.")?;
        }
        cfg.server.session_days = self
            .session_days
            .parse()
            .context("Enter the session days as a whole number from 1 to 3650.")?;
        cfg.router.failure_threshold = self
            .failure_threshold
            .parse()
            .context("Enter the failure threshold as a whole number, 1 or more.")?;
        cfg.router.cooldown_secs = self
            .cooldown_secs
            .parse()
            .context("Enter the cooldown as a whole number of seconds, 0 or more.")?;
        cfg.router.max_attempts = self
            .max_attempts
            .parse()
            .context("Enter the maximum attempts as a whole number, 1 or more.")?;
        if let Some(value) = &self.session_affinity_ttl_secs {
            cfg.router.session_affinity_ttl_secs = value
                .parse()
                .context("Enter the session affinity lifetime as whole seconds, 1 or more.")?;
        }
        if let Some(value) = &self.session_affinity_capacity {
            cfg.router.session_affinity_capacity = value
                .parse()
                .context("Enter the session affinity capacity as a whole number, 1 or more.")?;
        }
        cfg.validate()?;
        let settings = store::Settings {
            upstream_timeout_secs: cfg.server.upstream_timeout_secs,
            reserve_output_tokens: cfg.server.reserve_output_tokens,
            session_days: cfg.server.session_days,
            mode: mode.to_owned(),
            router: cfg.router.clone(),
            metrics_token: None,
        };
        Ok((cfg, settings))
    }
}
async fn save_settings(
    _: Admin,
    State(st): State<AppState>,
    Form(f): Form<ServerForm>,
) -> Response {
    let _guard = st.settings_write.lock().await;
    let mut cfg = tryr!(runtime(&st).await);
    cfg.server.default_markup_percent = st.catalog().default_markup_bp as f64 / 100.0;
    cfg.server.reserve_output_tokens = st.settings().reserve_output_tokens;
    let (cfg, mut settings) = match f.parse(cfg, &st.settings().mode) {
        Ok(v) => v,
        Err(e) => return form_error(e),
    };
    let token = if f.clear_metrics_token.is_some() {
        None
    } else if !f.metrics_token.is_empty() {
        Some(f.metrics_token)
    } else {
        st.settings().metrics_token.clone()
    };
    settings.metrics_token = tryr!(token.map(|v| st.secret_key.encrypt(&v)).transpose());
    let mut tx = tryr!(st.db.begin().await);
    tryr!(
        sqlx::query("UPDATE settings SET value=? WHERE key='runtime_settings'")
            .bind(tryr!(serde_json::to_string(&settings)))
            .execute(&mut *tx)
            .await
    );
    tryr!(
        sqlx::query("UPDATE settings SET value=? WHERE key='default_markup_bp'")
            .bind(percent_to_bp(cfg.server.default_markup_percent).to_string())
            .execute(&mut *tx)
            .await
    );
    tryr!(tx.commit().await);
    tryr!(st.reload_settings().await);
    saved("/admin/settings", "Settings saved.")
}

#[derive(Deserialize)]
struct ModeForm {
    #[serde(default)]
    mode: String,
}
/// Changes only the mode. The page asks for confirmation first.
async fn save_mode(_: Admin, State(st): State<AppState>, Form(f): Form<ModeForm>) -> Response {
    let _guard = st.settings_write.lock().await;
    let message = match f.mode.as_str() {
        "full" => "Full Mode is on. The Proxy applies Prices, Budgets and Rate Limits.",
        "simple" => "Simple Mode is on. The Proxy routes requests and records usage only.",
        _ => return form_error("Choose Full Mode or Simple Mode."),
    };
    // Change only the mode in the stored value; the token stays encrypted.
    let mut settings: store::Settings = tryr!(serde_json::from_str(
        &tryr!(store::value(&st.db, "runtime_settings").await).unwrap_or_default()
    ));
    settings.mode = f.mode;
    tryr!(
        sqlx::query("UPDATE settings SET value=? WHERE key='runtime_settings'")
            .bind(tryr!(serde_json::to_string(&settings)))
            .execute(&st.db)
            .await
    );
    tryr!(st.reload_settings().await);
    saved("/admin/settings", message)
}

#[derive(Deserialize)]
struct PasswordForm {
    current_password: String,
    password: String,
}
async fn change_password(
    _: Admin,
    State(st): State<AppState>,
    Form(f): Form<PasswordForm>,
) -> Response {
    let _guard = st.settings_write.lock().await;
    if f.password.chars().count() < 8 {
        return form_error("Use at least 8 characters for the new password.");
    }
    let old = tryr!(store::value(&st.db, "admin_password_hash").await).unwrap_or_default();
    if !store::verify_password(&f.current_password, &old) {
        return form_error("Incorrect current password.");
    }
    let hash = tryr!(store::hash_password(&f.password));
    let mut tx = tryr!(st.db.begin().await);
    tryr!(
        sqlx::query("UPDATE settings SET value=? WHERE key='admin_password_hash'")
            .bind(hash)
            .execute(&mut *tx)
            .await
    );
    tryr!(sqlx::query("DELETE FROM sessions").execute(&mut *tx).await);
    tryr!(tx.commit().await);
    saved(
        "/admin/login",
        "Password changed. Log in with the new password.",
    )
}

#[derive(Template)]
#[template(path = "routes.html")]
struct RoutesPage {
    provider_choices: Option<Vec<db::ControlChoice>>,
    pager: Pager,
    base: Base,
    routes: Vec<RouteView>,
    providers: Vec<String>,
}
impl RoutesPage {
    /// The Add route form starts with no deployments.
    fn no_deployments(&self) -> &'static [DeploymentCfg] {
        &[]
    }
}
struct RouteView {
    session_affinity: bool,
    active_sessions: usize,
    deployment_pager: Pager,
    deployment_offset: i64,
    deployment_count: usize,
    name: String,
    strategy: String,
    deployments: Vec<DeploymentCfg>,
    fallbacks: String,
}

#[derive(Deserialize)]
struct RoutesQ {
    #[serde(flatten)]
    page: PageQuery,
    deployments_page: Option<i64>,
    deployments_route: Option<String>,
}

async fn routes(
    _: Admin,
    State(st): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Query(query): Query<RoutesQ>,
) -> Response {
    let q = query.page;
    let deployment_q = PageQuery {
        page: query.deployments_page,
        limit: Some(50),
        ..Default::default()
    };
    // Routes are small editable configuration records; offset is not used on logs.
    let records: Vec<String> = tryr!(sqlx::query_scalar("SELECT json_set(config_json,'$.deployments',json('[]')) FROM routes WHERE instr(lower(name),lower(?)) > 0 ORDER BY name LIMIT ? OFFSET ?")
        .bind(&q.q).bind(q.limit()+1).bind(q.offset()).fetch_all(&st.db).await);
    let pager = Pager::offset(&uri.to_string(), &q, records.len());
    let records = records
        .into_iter()
        .take(q.limit() as usize)
        .map(|text| serde_json::from_str::<RouteCfg>(&text))
        .collect::<Result<Vec<_>, _>>();
    let records = tryr!(records);
    let mut routes = Vec::new();
    let active_sessions = st.active_sessions();
    for (index, r) in records.into_iter().enumerate() {
        let deployment_q = if query
            .deployments_route
            .as_ref()
            .map_or(index == 0, |name| name == &r.name)
        {
            deployment_q.clone()
        } else {
            PageQuery {
                limit: Some(50),
                ..Default::default()
            }
        };
        let deployments:Vec<String>=tryr!(sqlx::query_scalar("SELECT j.value FROM routes r,json_each(r.config_json,'$.deployments') j WHERE r.name=? ORDER BY CAST(j.key AS INTEGER) LIMIT 51 OFFSET ?")
            .bind(&r.name).bind(deployment_q.offset()).fetch_all(&st.db).await);
        let mut deployment_pager =
            Pager::offset(&uri.to_string(), &deployment_q, deployments.len());
        deployment_pager.label = format!("Deployments of {}", r.name);
        let n = deployment_q.page.unwrap_or(1).max(1);
        deployment_pager.next = deployment_pager.next.map(|_| {
            crate::pagination::link(
                &uri.to_string(),
                &[
                    ("deployments_page", Some(n.saturating_add(1).to_string())),
                    ("deployments_route", Some(r.name.clone())),
                ],
            )
        });
        deployment_pager.prev = deployment_pager.prev.map(|_| {
            crate::pagination::link(
                &uri.to_string(),
                &[
                    ("deployments_page", Some((n - 1).to_string())),
                    ("deployments_route", Some(r.name.clone())),
                ],
            )
        });
        let deployments = tryr!(
            deployments
                .into_iter()
                .take(50)
                .map(|text| serde_json::from_str::<DeploymentCfg>(&text))
                .collect::<Result<Vec<_>, _>>()
        );
        routes.push(RouteView {
            session_affinity: r.session_affinity,
            active_sessions: active_sessions.get(&r.name).copied().unwrap_or(0),
            deployment_pager,
            deployment_offset: deployment_q.offset(),
            deployment_count: deployments.len(),
            name: r.name.clone(),
            strategy: tryr!(serde_json::to_value(r.strategy))
                .as_str()
                .unwrap_or("priority")
                .into(),
            deployments,
            fallbacks: r.fallbacks.join(", "),
        });
    }
    let providers = tryr!(db::search_names(&st.db, "providers", "").await)
        .into_iter()
        .map(|p| p.name)
        .collect();
    render(RoutesPage {
        provider_choices: tryr!(db::small_choices(&st.db, "providers").await),
        pager,
        base: Base::new(&st),
        routes,
        providers,
    })
}

/// The route form. Deployment rows arrive as repeated
/// `deployment_provider`, `deployment_model` and `deployment_weight` fields.
#[derive(Default)]
struct RouteForm {
    session_affinity: Option<String>,
    deployment_offset: String,
    deployment_count: String,
    name: String,
    strategy: String,
    fallbacks: String,
    providers: Vec<String>,
    models: Vec<String>,
    weights: Vec<String>,
}
impl RouteForm {
    fn from_pairs(pairs: Vec<(String, String)>) -> Self {
        let mut f = Self::default();
        for (key, value) in pairs {
            match key.as_str() {
                "session_affinity" => f.session_affinity = Some(value),
                "deployment_offset" => f.deployment_offset = value,
                "deployment_count" => f.deployment_count = value,
                "name" => f.name = value,
                "strategy" => f.strategy = value,
                "fallbacks" => f.fallbacks = value,
                "deployment_provider" => f.providers.push(value),
                "deployment_model" => f.models.push(value),
                "deployment_weight" => f.weights.push(value),
                _ => {}
            }
        }
        f
    }

    /// Builds deployments from the rows. Blank rows are ignored.
    /// `Config::validate` checks providers and weights after this.
    fn deployments(&self) -> Result<Vec<DeploymentCfg>, &'static str> {
        let rows = self.providers.len();
        if self.models.len() != rows || self.weights.len() != rows {
            return Err("Each Deployment needs a Provider, a model and a weight. Reload the page.");
        }
        let mut out = Vec::new();
        for ((provider, model), weight) in
            self.providers.iter().zip(&self.models).zip(&self.weights)
        {
            let (provider, model, weight) = (provider.trim(), model.trim(), weight.trim());
            if provider.is_empty() && model.is_empty() {
                continue;
            }
            if provider.is_empty() {
                return Err("Choose a Provider for each Deployment.");
            }
            let weight = if weight.is_empty() {
                1
            } else {
                weight
                    .parse()
                    .map_err(|_| "Weight must be a positive integer.")?
            };
            out.push(DeploymentCfg {
                provider: provider.into(),
                model: model.into(),
                weight,
            });
        }
        if out.is_empty() && self.deployment_count.is_empty() {
            return Err("Add at least one Deployment.");
        }
        Ok(out)
    }
}
async fn create_route(
    admin: Admin,
    State(st): State<AppState>,
    Form(f): Form<Vec<(String, String)>>,
) -> Response {
    save_route(admin, st, None, RouteForm::from_pairs(f)).await
}
async fn update_route(
    admin: Admin,
    State(st): State<AppState>,
    Path(name): Path<String>,
    Form(f): Form<Vec<(String, String)>>,
) -> Response {
    save_route(admin, st, Some(name), RouteForm::from_pairs(f)).await
}
async fn save_route(_: Admin, st: AppState, old: Option<String>, f: RouteForm) -> Response {
    let _guard = st.settings_write.lock().await;
    let deployments = match f.deployments() {
        Ok(v) => v,
        Err(e) => return form_error(e),
    };
    let strategy: Strategy = match serde_json::from_value(serde_json::Value::String(f.strategy)) {
        Ok(v) => v,
        Err(_) => return form_error("Choose the strategy Priority, Round robin or Weighted."),
    };
    let added = old.is_none();
    let session_affinity = match f.session_affinity.as_deref() {
        Some("on") => true,
        Some("off") | None => false,
        _ => return form_error("Choose Session affinity On or Off."),
    };
    let mut route = RouteCfg {
        name: f.name,
        strategy,
        session_affinity,
        deployments,
        fallbacks: csv(&f.fallbacks),
    };
    let mut cfg = tryr!(runtime(&st).await);
    if let Some(name) = &old {
        let Some(original) = cfg.routes.iter().find(|r| &r.name == name) else {
            return form_error("Route not found.");
        };
        if f.session_affinity.is_none() {
            route.session_affinity = original.session_affinity;
        }
        if !f.deployment_offset.is_empty() || !f.deployment_count.is_empty() {
            let window = f
                .deployment_offset
                .parse::<usize>()
                .ok()
                .zip(f.deployment_count.parse::<usize>().ok())
                .filter(|(offset, count)| {
                    *count <= 50
                        && offset
                            .checked_add(*count)
                            .is_some_and(|end| end <= original.deployments.len())
                });
            let Some((offset, count)) = window else {
                return form_error("The Deployment page changed. Reload the Route and try again.");
            };
            let mut all = original.deployments.clone();
            all.splice(offset..offset + count, route.deployments);
            route.deployments = all;
        }
        if name != &route.name && cfg.routes.iter().any(|r| r.fallbacks.contains(name)) {
            return form_error(
                "Another Route uses this Route as a Fallback. Remove that Fallback before you rename it.",
            );
        }
        cfg.routes.retain(|r| &r.name != name);
    }
    cfg.routes.push(route.clone());
    if let Err(e) = cfg.validate() {
        return form_error(e);
    }
    // A Fallback names another Route or a catalog Model (or alias).
    let catalog = st.catalog();
    for name in &route.fallbacks {
        if name == &route.name {
            return form_error(format!(
                "Route {name} cannot be its own Fallback. Name another Route or a Model."
            ));
        }
        if !cfg.routes.iter().any(|r| &r.name == name) && catalog.resolve(name).is_none() {
            return form_error(format!(
                "Unknown Fallback: {name}. Use the name of another Route or of a Model on the Models page."
            ));
        }
    }
    let mut tx = tryr!(st.db.begin().await);
    if let Some(name) = old {
        tryr!(
            sqlx::query("DELETE FROM routes WHERE name=?")
                .bind(name)
                .execute(&mut *tx)
                .await
        );
    }
    if let Err(e) = sqlx::query("INSERT INTO routes(name,config_json) VALUES(?,?)")
        .bind(&route.name)
        .bind(tryr!(serde_json::to_string(&route)))
        .execute(&mut *tx)
        .await
    {
        let e = anyhow::Error::from(e);
        if is_unique_violation(&e) {
            return form_error(format!("A Route named {} already exists.", route.name));
        }
        return internal(e);
    }
    tryr!(tx.commit().await);
    tryr!(st.reload_settings().await);
    let verb = if added { "added" } else { "saved" };
    saved("/admin/routes", &format!("Route {} {verb}.", route.name))
}
async fn delete_route(_: Admin, State(st): State<AppState>, Path(name): Path<String>) -> Response {
    let _guard = st.settings_write.lock().await;
    let cfg = tryr!(runtime(&st).await);
    if cfg.routes.iter().any(|r| r.fallbacks.contains(&name)) {
        return form_error(
            "Another Route uses this Route as a Fallback. Remove that Fallback first.",
        );
    }
    tryr!(
        sqlx::query("DELETE FROM routes WHERE name=?")
            .bind(&name)
            .execute(&st.db)
            .await
    );
    tryr!(st.reload_settings().await);
    saved("/admin/routes", &format!("Route {name} deleted."))
}

#[derive(Template)]
#[template(path = "providers.html")]
struct ProvidersPage {
    pager: Pager,
    base: Base,
    providers: Vec<ProviderView>,
    presets: Vec<crate::presets::PresetChoice>,
    preset: crate::presets::PresetChoice,
}
struct ProviderView {
    key_pager: Pager,
    name: String,
    kind: String,
    base_url: String,
    include_usage: bool,
    key_header: &'static str,
    keys: Vec<KeyView>,
    auth: String,
    openrouter: bool,
    /// "API key", "ChatGPT subscription" or "Claude Pro/Max".
    auth_label: &'static str,
    status: Option<StatusView>,
    usage_status: Option<StatusView>,
    usage: Vec<UsageView>,
}
impl ProviderView {
    fn has_unset_env(&self) -> bool {
        self.keys.iter().any(|k| k.env_set == Some(false))
    }
}
/// One entry of a Key Pool. Never more than the last 4 characters.
struct KeyView {
    index: i64,
    /// `env:NAME` or `****abcd`.
    label: String,
    /// For `env:NAME`: whether the variable is set and not empty.
    env_set: Option<bool>,
}
/// The state of a subscription or usage login.
struct StatusView {
    connected: bool,
    account: Option<String>,
    plan: Option<String>,
    expires: Option<super::views::Stamp>,
    last_refresh: Option<super::views::Stamp>,
    last_error: Option<String>,
}
struct UsageView {
    label: String,
    /// 0 to 100, for `<meter>`.
    percent: f64,
    percent_text: String,
    resets: Option<super::views::Stamp>,
}

fn last4(plain: &str) -> String {
    let tail: String = plain
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("****{tail}")
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn key_view(st: &AppState, stored: &str, index: i64) -> anyhow::Result<KeyView> {
    Ok(match stored.strip_prefix("env:") {
        Some(env) => KeyView {
            index,
            label: stored.into(),
            env_set: Some(env_value(env).is_some()),
        },
        None => KeyView {
            index,
            label: last4(&st.secret_key.decrypt(stored)?),
            env_set: None,
        },
    })
}

/// The display text of each Key Pool entry, in the order that the Proxy
/// uses (unset `env:NAME` references are not in the pool). The Ledger shows
/// these for the Provider Key index of an entry.
pub(super) async fn key_labels(
    st: &AppState,
    rows: &[db::LedgerRow],
) -> anyhow::Result<std::collections::HashMap<(String, i64), String>> {
    let mut out = std::collections::HashMap::new();
    let mut needed = std::collections::BTreeMap::<&str, std::collections::BTreeSet<i64>>::new();
    for row in rows {
        if let Some(index) = row.provider_key {
            needed.entry(&row.provider).or_default().insert(index);
        }
    }
    for (provider, indices) in needed {
        let max = indices.last().copied().unwrap_or(0);
        let (mut offset, mut index) = (0, 0);
        loop {
            let stored:Vec<String> = sqlx::query_scalar("SELECT j.value FROM providers p,json_each(p.keys_json) j WHERE p.name=? ORDER BY CAST(j.key AS INTEGER) LIMIT 200 OFFSET ?")
                .bind(provider).bind(offset).fetch_all(&st.db).await?;
            let n = stored.len();
            for value in stored {
                if value
                    .strip_prefix("env:")
                    .is_some_and(|env| env_value(env).is_none())
                {
                    continue;
                }
                if indices.contains(&index) {
                    out.insert(
                        (provider.to_owned(), index),
                        if value.starts_with("env:") {
                            value
                        } else {
                            last4(&st.secret_key.decrypt(&value)?)
                        },
                    );
                }
                index += 1;
                if index > max {
                    break;
                }
            }
            if n < 200 || index > max {
                break;
            }
            offset += 200;
        }
    }
    Ok(out)
}

fn status_view(status: crate::oauth::Status) -> StatusView {
    let stamp = |ms: Option<i64>| ms.and_then(|ms| super::views::stamp_opt(ms.div_euclid(1000)));
    StatusView {
        connected: status.connected,
        account: status.account,
        plan: status.plan,
        expires: stamp(status.expires_at),
        last_refresh: stamp(status.last_refresh),
        last_error: status.last_error,
    }
}

fn auth_label(auth: crate::provider::ProviderAuth) -> &'static str {
    match auth {
        crate::provider::ProviderAuth::ApiKey => "API key",
        crate::provider::ProviderAuth::Chatgpt => "ChatGPT subscription",
        crate::provider::ProviderAuth::Codex => "ChatGPT via Codex",
        crate::provider::ProviderAuth::Claude => "Claude Pro/Max",
        crate::provider::ProviderAuth::HuggingFace => "Hugging Face OAuth",
        crate::provider::ProviderAuth::Entra => "Azure Entra ID",
        crate::provider::ProviderAuth::GithubCopilot => "GitHub Copilot",
        crate::provider::ProviderAuth::QwenCode => "Qwen Code",
        crate::provider::ProviderAuth::KimiCode => "Kimi Code",
        crate::provider::ProviderAuth::GoogleVertex => {
            "Google Vertex: sign-in ready, adapter pending"
        }
    }
}

#[derive(Deserialize)]
struct ProvidersQ {
    #[serde(flatten)]
    page: PageQuery,
    pool_page: Option<i64>,
    pool_provider: Option<String>,
    preset: Option<String>,
}

async fn providers(
    _: Admin,
    State(st): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Query(query): Query<ProvidersQ>,
) -> Response {
    let q = query.page;
    let pool_q = PageQuery {
        page: query.pool_page,
        limit: Some(50),
        ..Default::default()
    };
    let mut providers = Vec::new();
    let records = tryr!(
        sqlx::query("SELECT name,kind,auth,base_url,include_usage,key_header FROM providers WHERE instr(lower(name),lower(?)) > 0 ORDER BY name LIMIT ? OFFSET ?")
            .bind(&q.q).bind(q.limit()+1).bind(q.offset())
            .fetch_all(&st.db)
            .await
    );
    // Providers are admin-sized, mutable configuration rows, not an append log.
    let pager = Pager::offset(&uri.to_string(), &q, records.len());
    for (index, row) in records.into_iter().take(q.limit() as usize).enumerate() {
        let pool_q = if query
            .pool_provider
            .as_ref()
            .map_or(index == 0, |name| name == row.get::<&str, _>("name"))
        {
            pool_q.clone()
        } else {
            PageQuery {
                limit: Some(50),
                ..Default::default()
            }
        };
        let mut keys = Vec::new();
        let key_rows=tryr!(sqlx::query("SELECT CAST(j.key AS INTEGER) AS idx,j.value FROM providers p,json_each(p.keys_json) j WHERE p.name=? ORDER BY CAST(j.key AS INTEGER) LIMIT 51 OFFSET ?")
            .bind(row.get::<String,_>("name")).bind(pool_q.offset()).fetch_all(&st.db).await);
        let mut key_pager = Pager::offset(&uri.to_string(), &pool_q, key_rows.len());
        key_pager.label = format!("Provider Keys of {}", row.get::<String, _>("name"));
        let pool_page = pool_q.page.unwrap_or(1).max(1);
        key_pager.next = key_pager.next.map(|_| {
            crate::pagination::link(
                &uri.to_string(),
                &[
                    ("pool_page", Some(pool_page.saturating_add(1).to_string())),
                    ("pool_provider", Some(row.get("name"))),
                ],
            )
        });
        key_pager.prev = key_pager.prev.map(|_| {
            crate::pagination::link(
                &uri.to_string(),
                &[
                    ("pool_page", Some((pool_page - 1).to_string())),
                    ("pool_provider", Some(row.get("name"))),
                ],
            )
        });
        for key in key_rows.into_iter().take(50) {
            keys.push(tryr!(key_view(&st, key.get("value"), key.get("idx"))));
        }
        let kind: ProviderKind = tryr!(serde_json::from_str(row.get("kind")));
        let auth: crate::provider::ProviderAuth = tryr!(serde_json::from_value(serde_json::json!(
            row.get::<String, _>("auth")
        )));
        let name: String = row.get("name");
        let status = match crate::oauth::Flow::from_auth(auth) {
            Some(flow) => Some(status_view(tryr!(st.oauth.status(&name, flow).await))),
            None => None,
        };
        let chatgpt = auth == crate::provider::ProviderAuth::Chatgpt;
        let usage_status = if chatgpt {
            Some(status_view(tryr!(
                st.oauth.status(&name, crate::oauth::Flow::Codex).await
            )))
        } else {
            None
        };
        let usage = if chatgpt {
            st.oauth
                .usage(&name)
                .await
                .map(|u| {
                    u.windows
                        .iter()
                        .map(|w| UsageView {
                            label: w.label.clone(),
                            percent: w.used_percent.clamp(0.0, 100.0),
                            percent_text: format!("{:.0}%", w.used_percent),
                            resets: w
                                .resets_at
                                .and_then(|ms| super::views::stamp_opt(ms.div_euclid(1000))),
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            vec![]
        };
        providers.push(ProviderView {
            key_pager,
            name,
            auth: auth.as_str().into(),
            openrouter: auth == crate::provider::ProviderAuth::ApiKey
                && matches!(
                    row.get::<&str, _>("base_url").trim_end_matches('/'),
                    "https://openrouter.ai/api" | "https://openrouter.ai/api/v1"
                ),
            auth_label: auth_label(auth),
            status,
            usage_status,
            usage,
            kind: match kind {
                ProviderKind::Openai => "openai",
                ProviderKind::Anthropic => "anthropic",
            }
            .into(),
            base_url: row.get("base_url"),
            include_usage: row.get("include_usage"),
            key_header: tryr!(
                row.get::<Option<String>, _>("key_header")
                    .map(|h| serde_json::from_value::<crate::provider::KeyHeader>(
                        serde_json::json!(h)
                    ))
                    .transpose()
            )
            .unwrap_or_else(|| crate::provider::KeyHeader::for_kind(kind))
            .as_str(),
            keys,
        });
    }
    let presets = tryr!(crate::presets::merged(&st.db).await);
    let preset = presets
        .iter()
        .find(|p| Some(p.name.as_str()) == query.preset.as_deref())
        .unwrap_or(&presets[0])
        .clone();
    render(ProvidersPage {
        pager,
        base: Base::new(&st),
        providers,
        presets,
        preset,
    })
}

#[derive(Deserialize)]
struct ProviderForm {
    preset: Option<String>,
    name: String,
    /// A Subscription Provider has a fixed API format; the form may omit it.
    #[serde(default)]
    kind: ProviderKind,
    #[serde(default)]
    auth: crate::provider::ProviderAuth,
    base_url: String,
    #[serde(default)]
    api_key: String,
    #[serde(default = "crate::provider::default_include_usage")]
    include_usage: bool,
    #[serde(default)]
    key_header: Option<crate::provider::KeyHeader>,
}

async fn runtime(st: &AppState) -> anyhow::Result<Config> {
    Ok(store::load(&st.db, &st.secret_key).await?.0)
}

fn encode_key(st: &AppState, key: &str) -> anyhow::Result<String> {
    if let Some(env) = key.strip_prefix("env:") {
        if env.is_empty() || !env.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
            anyhow::bail!("Use env:NAME for an environment variable reference.");
        }
        Ok(key.into())
    } else {
        st.secret_key.encrypt(key)
    }
}

async fn create_provider(
    admin: Admin,
    State(st): State<AppState>,
    Form(f): Form<ProviderForm>,
) -> Response {
    save_provider(admin, st, None, f).await
}
async fn update_provider(
    admin: Admin,
    State(st): State<AppState>,
    Path(name): Path<String>,
    Form(f): Form<ProviderForm>,
) -> Response {
    save_provider(admin, st, Some(name), f).await
}
async fn save_provider(
    _: Admin,
    st: AppState,
    old: Option<String>,
    mut f: ProviderForm,
) -> Response {
    use crate::provider::ProviderAuth;
    f.name = f.name.trim().into();
    f.base_url = f.base_url.trim().trim_end_matches('/').into();
    // A Subscription Provider has one fixed address and API format. Accept
    // the address with /v1, as the Provider documentation often shows it.
    match f.auth {
        ProviderAuth::ApiKey | ProviderAuth::GithubCopilot | ProviderAuth::KimiCode => {}
        ProviderAuth::Chatgpt
        | ProviderAuth::Codex
        | ProviderAuth::HuggingFace
        | ProviderAuth::Entra
        | ProviderAuth::GoogleVertex
        | ProviderAuth::QwenCode => f.kind = ProviderKind::Openai,
        ProviderAuth::Claude => f.kind = ProviderKind::Anthropic,
    }
    if !matches!(
        f.auth,
        ProviderAuth::ApiKey | ProviderAuth::QwenCode | ProviderAuth::KimiCode
    ) {
        f.base_url = f.base_url.trim_end_matches("/v1").into();
    }
    let _guard = st.settings_write.lock().await;
    let added = old.is_none();
    let mut cfg = tryr!(runtime(&st).await);
    if let Some(name) = &old {
        cfg.providers.retain(|p| &p.name != name);
    }
    cfg.providers.push(ProviderCfg {
        name: f.name.clone(),
        kind: f.kind,
        auth: f.auth,
        base_url: f.base_url.clone(),
        include_usage: f.include_usage,
        key_header: f.key_header,
        api_key: None,
        api_key_env: None,
        api_keys: vec![],
        api_keys_env: vec![],
    });
    // Renaming a used provider requires updating its routes first.
    if let Err(e) = cfg.validate() {
        return form_error(e);
    }
    if old.as_ref().is_some_and(|name| name != &f.name)
        && st
            .catalog()
            .models
            .iter()
            .any(|m| Some(&m.provider) == old.as_ref())
    {
        return form_error("A Model uses this Provider. Change that Model first.");
    }
    let _oauth_guard = if let Some(name) = &old {
        Some(st.oauth.provider_write(name).await)
    } else {
        None
    };
    let mut keys: Vec<String> = match &old {
        Some(name) => {
            let old_auth: String = tryr!(
                sqlx::query_scalar("SELECT auth FROM providers WHERE name=?")
                    .bind(name)
                    .fetch_one(&st.db)
                    .await
            );
            if old_auth != f.auth.as_str() {
                return form_error("Create a new Provider to change its authentication type.");
            }
            let raw: Option<String> = tryr!(
                sqlx::query_scalar("SELECT keys_json FROM providers WHERE name=?")
                    .bind(name)
                    .fetch_optional(&st.db)
                    .await
            );
            let Some(raw) = raw else {
                return form_error("Provider not found.");
            };
            tryr!(serde_json::from_str(&raw))
        }
        None => vec![],
    };
    if !f.api_key.trim().is_empty() {
        if f.auth != crate::provider::ProviderAuth::ApiKey {
            return form_error("Subscription Providers do not use API keys.");
        }
        keys.push(match encode_key(&st, f.api_key.trim()) {
            Ok(v) => v,
            Err(e) => return form_error(e),
        });
    }
    let mut tx = tryr!(st.db.begin().await);
    let result = if let Some(name) = old {
        sqlx::query("UPDATE providers SET name=?,kind=?,base_url=?,keys_json=?,auth=?,include_usage=?,key_header=? WHERE name=?")
            .bind(&f.name)
            .bind(tryr!(serde_json::to_string(&f.kind)))
            .bind(f.base_url.trim_end_matches('/'))
            .bind(tryr!(serde_json::to_string(&keys)))
            .bind(f.auth.as_str())
            .bind(f.include_usage)
            .bind(f.key_header.map(|h| h.as_str()))
            .bind(name)
            .execute(&mut *tx)
            .await
    } else {
        sqlx::query("INSERT INTO providers(name,kind,base_url,keys_json,auth,include_usage,key_header) VALUES(?,?,?,?,?,?,?)")
            .bind(&f.name)
            .bind(tryr!(serde_json::to_string(&f.kind)))
            .bind(f.base_url.trim_end_matches('/'))
            .bind(tryr!(serde_json::to_string(&keys)))
            .bind(f.auth.as_str())
            .bind(f.include_usage)
            .bind(f.key_header.map(|h| h.as_str()))
            .execute(&mut *tx)
            .await
    };
    if let Err(e) = result {
        let e = anyhow::Error::from(e);
        if is_unique_violation(&e) {
            return form_error(format!("A Provider named {} already exists.", f.name));
        }
        return internal(e);
    }
    if let Some(preset) = &f.preset {
        tryr!(
            sqlx::query("UPDATE providers SET preset=? WHERE name=?")
                .bind(preset)
                .bind(&f.name)
                .execute(&mut *tx)
                .await
        );
    }
    tryr!(tx.commit().await);
    tryr!(st.reload_settings().await);
    let verb = if added { "added" } else { "saved" };
    saved("/admin/providers", &format!("Provider {} {verb}.", f.name))
}

async fn delete_provider(
    _: Admin,
    State(st): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    let _guard = st.settings_write.lock().await;
    let cfg = tryr!(runtime(&st).await);
    if cfg
        .routes
        .iter()
        .any(|r| r.deployments.iter().any(|d| d.provider == name))
        || st.catalog().models.iter().any(|m| m.provider == name)
    {
        return form_error("A Route or a Model uses this Provider. Change them first.");
    }
    let _oauth_guard = st.oauth.provider_write(&name).await;
    tryr!(
        sqlx::query("DELETE FROM providers WHERE name=?")
            .bind(&name)
            .execute(&st.db)
            .await
    );
    tryr!(st.reload_settings().await);
    saved("/admin/providers", &format!("Provider {name} deleted."))
}

#[derive(Deserialize)]
struct KeyForm {
    api_key: String,
}
async fn add_key(
    _: Admin,
    State(st): State<AppState>,
    Path(name): Path<String>,
    Form(f): Form<KeyForm>,
) -> Response {
    if f.api_key.trim().is_empty() {
        return form_error("Enter a Provider Key or env:NAME.");
    }
    let encoded = match encode_key(&st, f.api_key.trim()) {
        Ok(v) => v,
        Err(e) => return form_error(e),
    };
    change_keys(st, name, None, Some(encoded)).await
}
async fn delete_key(
    _: Admin,
    State(st): State<AppState>,
    Path((name, index)): Path<(String, usize)>,
) -> Response {
    change_keys(st, name, Some(index), None).await
}
async fn change_keys(
    st: AppState,
    name: String,
    remove: Option<usize>,
    add: Option<String>,
) -> Response {
    let _guard = st.settings_write.lock().await;
    let auth: Option<String> = tryr!(
        sqlx::query_scalar("SELECT auth FROM providers WHERE name=?")
            .bind(&name)
            .fetch_optional(&st.db)
            .await
    );
    if auth.as_deref() != Some("api_key") {
        return form_error("Only API-key Providers have a Key Pool.");
    }
    let raw: Option<String> = tryr!(
        sqlx::query_scalar("SELECT keys_json FROM providers WHERE name=?")
            .bind(&name)
            .fetch_optional(&st.db)
            .await
    );
    let Some(raw) = raw else {
        return form_error("Provider not found.");
    };
    let mut keys: Vec<String> = tryr!(serde_json::from_str(&raw));
    if let Some(i) = remove {
        if i >= keys.len() {
            return form_error("Key not found. Reload the page.");
        }
        keys.remove(i);
    }
    if let Some(key) = add {
        keys.push(key);
    }
    tryr!(
        sqlx::query("UPDATE providers SET keys_json=? WHERE name=?")
            .bind(tryr!(serde_json::to_string(&keys)))
            .bind(name)
            .execute(&st.db)
            .await
    );
    tryr!(st.reload_settings().await);
    let message = if remove.is_some() {
        "Provider Key removed."
    } else {
        "Provider Key added."
    };
    saved("/admin/providers", message)
}

async fn test_provider(_: Admin, State(st): State<AppState>, Path(name): Path<String>) -> Response {
    let table = st.router_table();
    let Some(p) = table.providers.iter().find(|p| p.name == name) else {
        return form_error("Provider not found.");
    };
    let url = crate::provider::upstream_url(&p.base_url, "/v1/models");
    if p.auth != crate::provider::ProviderAuth::ApiKey {
        return form_error(
            "Use Sign in for this Subscription Provider. API-key tests do not apply.",
        );
    }
    let Some(key) = p.keys.first() else {
        return form_error(
            "This Provider has no Provider Key. Add a key, or set its environment variable, and test again.",
        );
    };
    let req = st
        .http()
        .get(url)
        .timeout(std::time::Duration::from_secs(15));
    let req = p.key_header.apply(req, key);
    let req = if p.kind == ProviderKind::Anthropic {
        req.header("anthropic-version", "2023-06-01")
    } else {
        req
    };
    let (class, text) = match req.send().await {
        Err(_) => (
            "bad",
            "Cannot reach the Provider. Check the base URL and the network.".to_owned(),
        ),
        Ok(r) => test_message(r.status().as_u16()),
    };
    Html(format!(
        "<p><span class=\"tag {class}\">{}</span> {}</p>",
        match class {
            "good" => "OK",
            "warn" => "Warning",
            _ => "Failed",
        },
        esc(&text)
    ))
    .into_response()
}

/// A plain explanation of the status of the Provider's model list.
fn test_message(code: u16) -> (&'static str, String) {
    match code {
        200..=299 => (
            "good",
            format!("Connected. The Provider answered {code} and accepted the first Provider Key."),
        ),
        401 | 403 => (
            "bad",
            format!("The Provider refused the key ({code}). Check the first Provider Key."),
        ),
        404 => (
            "bad",
            "The Provider answered 404. Check the API base URL in the Provider documentation.".into(),
        ),
        429 => (
            "warn",
            "The Provider answered 429 (rate limited or out of credit). The key is valid, but requests can fail.".into(),
        ),
        500..=599 => (
            "warn",
            format!("The Provider has a problem ({code}). Try again later."),
        ),
        _ => (
            "warn",
            format!("The Provider answered {code}. Check the base URL and the Provider Key."),
        ),
    }
}

#[derive(Template)]
#[template(path = "setup.html")]
struct SetupPage {
    base: Base,
    mode: String,
}

fn setup_done() -> Response {
    page_error_with(
        StatusCode::FORBIDDEN,
        "Setup is complete",
        "Setup runs only once. Log in with the dashboard password. To change the password, use Settings after you log in.",
        ("/admin/login", "Log in"),
    )
}

async fn setup_page(State(st): State<AppState>) -> Response {
    if tryr!(crate::settings::value(&st.db, "admin_password_hash").await).is_some() {
        return setup_done();
    }
    render(SetupPage {
        base: Base::anonymous(),
        mode: st.settings().mode.clone(),
    })
}

#[derive(Deserialize)]
struct SetupForm {
    password: String,
    /// Optional, so that scripted Setup with one field keeps working.
    password_confirm: Option<String>,
    mode: Option<String>,
}

async fn setup(State(st): State<AppState>, Form(f): Form<SetupForm>) -> Response {
    let _guard = st.settings_write.lock().await;
    if tryr!(store::value(&st.db, "admin_password_hash").await).is_some() {
        return setup_done();
    }
    if f.password.chars().count() < 8 {
        return bad("Use at least 8 characters for the password.");
    }
    if f.password_confirm
        .as_ref()
        .is_some_and(|c| c != &f.password)
    {
        return bad("The passwords do not match. Type the same password in both fields.");
    }
    let mode = f.mode.unwrap_or_else(|| st.settings().mode.clone());
    if !matches!(mode.as_str(), "full" | "simple") {
        return bad("Choose Full Mode or Simple Mode.");
    }
    let mut settings: store::Settings = tryr!(serde_json::from_str(
        &tryr!(store::value(&st.db, "runtime_settings").await).unwrap_or_default()
    ));
    settings.mode = mode;
    let hash = tryr!(crate::settings::hash_password(&f.password));
    let mut tx = tryr!(st.db.begin().await);
    let result = tryr!(
        sqlx::query("INSERT OR IGNORE INTO settings(key,value) VALUES('admin_password_hash',?)")
            .bind(&hash)
            .execute(&mut *tx)
            .await
    );
    if result.rows_affected() == 0 {
        return setup_done();
    }
    tryr!(
        sqlx::query("UPDATE settings SET value=? WHERE key='runtime_settings'")
            .bind(tryr!(serde_json::to_string(&settings)))
            .execute(&mut *tx)
            .await
    );
    tryr!(tx.commit().await);
    tryr!(st.reload_settings().await);
    // Log the Operator in, so the password is not typed twice.
    let session = tryr!(start_session(&st, &hash).await);
    let mut r = saved(
        "/admin",
        "Setup is complete. Follow the steps below to connect your first client.",
    );
    r.headers_mut()
        .append(header::SET_COOKIE, tryr!(HeaderValue::from_str(&session)));
    r
}
