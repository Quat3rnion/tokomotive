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
    base: Base,
    settings: std::sync::Arc<store::Settings>,
    markup: f64,
    metrics_set: bool,
}
async fn settings_page(_: Admin, State(st): State<AppState>) -> Response {
    let settings = st.settings();
    render(SettingsPage {
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
    base: Base,
    routes: Vec<RouteView>,
    providers: Vec<String>,
    /// Route and Model names offered as Fallback suggestions.
    fallback_names: Vec<String>,
    /// Upstream model names offered in the Deployment rows.
    upstream_models: Vec<String>,
}
impl RoutesPage {
    /// The Add route form starts with no deployments.
    fn no_deployments(&self) -> &'static [DeploymentCfg] {
        &[]
    }
}
struct RouteView {
    name: String,
    strategy: String,
    deployments: Vec<DeploymentCfg>,
    fallbacks: String,
}

async fn routes(_: Admin, State(st): State<AppState>) -> Response {
    let cfg = tryr!(runtime(&st).await);
    let routes = cfg
        .routes
        .iter()
        .map(|r| -> anyhow::Result<RouteView> {
            Ok(RouteView {
                name: r.name.clone(),
                strategy: serde_json::to_value(r.strategy)?
                    .as_str()
                    .unwrap_or("priority")
                    .into(),
                deployments: r.deployments.clone(),
                fallbacks: r.fallbacks.join(", "),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>();
    let mut fallback_names: Vec<String> = cfg
        .routes
        .iter()
        .map(|r| r.name.clone())
        .chain(st.catalog().models.iter().map(|m| m.name.clone()))
        .collect();
    fallback_names.sort();
    fallback_names.dedup();
    let mut upstream_models: Vec<String> = cfg
        .routes
        .iter()
        .flat_map(|r| r.deployments.iter().map(|d| d.model.clone()))
        .chain(st.catalog().models.iter().map(|m| m.name.clone()))
        .collect();
    upstream_models.sort();
    upstream_models.dedup();
    render(RoutesPage {
        upstream_models,
        base: Base::new(&st),
        routes: tryr!(routes),
        providers: cfg.providers.into_iter().map(|p| p.name).collect(),
        fallback_names,
    })
}

/// The route form. Deployment rows arrive as repeated
/// `deployment_provider`, `deployment_model` and `deployment_weight` fields.
#[derive(Default)]
struct RouteForm {
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
        if out.is_empty() {
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
    let route = RouteCfg {
        name: f.name,
        strategy,
        deployments,
        fallbacks: csv(&f.fallbacks),
    };
    let mut cfg = tryr!(runtime(&st).await);
    if let Some(name) = &old {
        if !cfg.routes.iter().any(|r| &r.name == name) {
            return form_error("Route not found.");
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
    base: Base,
    providers: Vec<ProviderView>,
}
struct ProviderView {
    name: String,
    kind: String,
    base_url: String,
    keys: Vec<KeyView>,
    auth: String,
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

fn key_view(st: &AppState, stored: &str) -> anyhow::Result<KeyView> {
    Ok(match stored.strip_prefix("env:") {
        Some(env) => KeyView {
            label: stored.into(),
            env_set: Some(env_value(env).is_some()),
        },
        None => KeyView {
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
) -> anyhow::Result<std::collections::HashMap<String, Vec<String>>> {
    let mut out = std::collections::HashMap::new();
    for row in sqlx::query("SELECT name, keys_json FROM providers")
        .fetch_all(&st.db)
        .await?
    {
        let mut labels = Vec::new();
        for stored in serde_json::from_str::<Vec<String>>(row.get("keys_json"))? {
            match stored.strip_prefix("env:") {
                Some(env) if env_value(env).is_none() => {}
                Some(_) => labels.push(stored.clone()),
                None => labels.push(last4(&st.secret_key.decrypt(&stored)?)),
            }
        }
        out.insert(row.get("name"), labels);
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
        crate::provider::ProviderAuth::Claude => "Claude Pro/Max",
    }
}

async fn providers(_: Admin, State(st): State<AppState>) -> Response {
    let mut providers = Vec::new();
    for row in tryr!(
        sqlx::query("SELECT * FROM providers ORDER BY name")
            .fetch_all(&st.db)
            .await
    ) {
        let mut keys = Vec::new();
        for value in tryr!(serde_json::from_str::<Vec<String>>(row.get("keys_json"))) {
            keys.push(tryr!(key_view(&st, &value)));
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
            name,
            auth: auth.as_str().into(),
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
            keys,
        });
    }
    render(ProvidersPage {
        base: Base::new(&st),
        providers,
    })
}

#[derive(Deserialize)]
struct ProviderForm {
    name: String,
    /// A Subscription Provider has a fixed API format; the form may omit it.
    #[serde(default)]
    kind: ProviderKind,
    #[serde(default)]
    auth: crate::provider::ProviderAuth,
    base_url: String,
    #[serde(default)]
    api_key: String,
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
        ProviderAuth::ApiKey => {}
        ProviderAuth::Chatgpt => f.kind = ProviderKind::Openai,
        ProviderAuth::Claude => f.kind = ProviderKind::Anthropic,
    }
    if f.auth != ProviderAuth::ApiKey {
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
        sqlx::query("UPDATE providers SET name=?,kind=?,base_url=?,keys_json=?,auth=? WHERE name=?")
            .bind(&f.name)
            .bind(tryr!(serde_json::to_string(&f.kind)))
            .bind(f.base_url.trim_end_matches('/'))
            .bind(tryr!(serde_json::to_string(&keys)))
            .bind(f.auth.as_str())
            .bind(name)
            .execute(&mut *tx)
            .await
    } else {
        sqlx::query("INSERT INTO providers(name,kind,base_url,keys_json,auth) VALUES(?,?,?,?,?)")
            .bind(&f.name)
            .bind(tryr!(serde_json::to_string(&f.kind)))
            .bind(f.base_url.trim_end_matches('/'))
            .bind(tryr!(serde_json::to_string(&keys)))
            .bind(f.auth.as_str())
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
    let url = format!(
        "{}/v1/models",
        p.base_url.trim_end_matches('/').trim_end_matches("/v1")
    );
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
    let req = match p.kind {
        ProviderKind::Openai => req.bearer_auth(key),
        ProviderKind::Anthropic => req
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01"),
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
            "The Provider answered 404. Check the base URL; use the address without /v1.".into(),
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
