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
    mode: String,
    #[serde(default)]
    metrics_token: String,
    clear_metrics_token: Option<String>,
}
impl ServerForm {
    fn parse(&self, mut cfg: Config) -> anyhow::Result<(Config, store::Settings)> {
        if !matches!(self.mode.as_str(), "full" | "simple") {
            anyhow::bail!("Choose full or simple mode.");
        }
        if let Some(value) = &self.default_markup_percent {
            cfg.server.default_markup_percent =
                value.parse().context("Markup must be a number.")?;
        }
        cfg.server.upstream_timeout_secs = self
            .upstream_timeout_secs
            .parse()
            .context("Timeout must be a positive integer.")?;
        if let Some(value) = &self.reserve_output_tokens {
            cfg.server.reserve_output_tokens = value
                .parse()
                .context("Reserved tokens must be a non-negative integer.")?;
        }
        cfg.server.session_days = self
            .session_days
            .parse()
            .context("Session days must be a positive integer.")?;
        cfg.router.failure_threshold = self
            .failure_threshold
            .parse()
            .context("Failure threshold must be a positive integer.")?;
        cfg.router.cooldown_secs = self
            .cooldown_secs
            .parse()
            .context("Cooldown must be a non-negative integer.")?;
        cfg.router.max_attempts = self
            .max_attempts
            .parse()
            .context("Maximum attempts must be a positive integer.")?;
        cfg.validate()?;
        let settings = store::Settings {
            upstream_timeout_secs: cfg.server.upstream_timeout_secs,
            reserve_output_tokens: cfg.server.reserve_output_tokens,
            session_days: cfg.server.session_days,
            mode: self.mode.clone(),
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
    let (cfg, mut settings) = match f.parse(cfg) {
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
    render(RoutesPage {
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
            return Err("Each deployment needs a provider, model and weight. Reload the page.");
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
                return Err("Choose a provider for each deployment.");
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
            return Err("Add at least one deployment.");
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
        Err(_) => return form_error("Choose priority, round_robin or weighted."),
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
                "This route is used as a fallback. Remove that reference before renaming it.",
            );
        }
        cfg.routes.retain(|r| &r.name != name);
    }
    cfg.routes.push(route.clone());
    if let Err(e) = cfg.validate() {
        return form_error(e);
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
        return form_error("This route is used as a fallback. Remove that reference first.");
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
    keys: Vec<String>,
    auth: String,
    status: String,
    usage_status: String,
    usage: String,
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
            let plain = if let Some(env) = value.strip_prefix("env:") {
                std::env::var(env).unwrap_or_default()
            } else {
                tryr!(st.secret_key.decrypt(&value))
            };
            keys.push(format!(
                "****{}",
                plain
                    .chars()
                    .rev()
                    .take(4)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<String>()
            ));
        }
        let kind: ProviderKind = tryr!(serde_json::from_str(row.get("kind")));
        let auth: crate::provider::ProviderAuth = tryr!(serde_json::from_value(serde_json::json!(
            row.get::<String, _>("auth")
        )));
        let name: String = row.get("name");
        let status = if let Some(flow) = crate::oauth::Flow::from_auth(auth) {
            status_text(tryr!(st.oauth.status(&name, flow).await))
        } else {
            String::new()
        };
        let usage_status = if auth == crate::provider::ProviderAuth::Chatgpt {
            status_text(tryr!(
                st.oauth.status(&name, crate::oauth::Flow::Codex).await
            ))
        } else {
            String::new()
        };
        let usage = if auth == crate::provider::ProviderAuth::Chatgpt {
            st.oauth
                .usage(&name)
                .await
                .map(|u| {
                    u.windows
                        .iter()
                        .map(|w| {
                            format!(
                                "{}: {}%; resets {}",
                                w.label,
                                w.used_percent,
                                w.resets_at
                                    .map(super::views::utc_ms)
                                    .filter(|s| !s.is_empty())
                                    .unwrap_or_else(|| "not available".into())
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .unwrap_or_else(|| "not available".into())
        } else {
            String::new()
        };
        providers.push(ProviderView {
            name,
            auth: auth.as_str().into(),
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
async fn save_provider(_: Admin, st: AppState, old: Option<String>, f: ProviderForm) -> Response {
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
        return form_error("This provider is used by a model. Update that model first.");
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

fn status_text(status: crate::oauth::Status) -> String {
    let date = |ms: Option<i64>| {
        ms.map(super::views::utc_ms)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "not available".into())
    };
    format!(
        "{}; account: {}; plan: {}; expires: {}; last refresh: {}; last error: {}",
        if status.connected {
            "Connected"
        } else {
            "Disconnected"
        },
        status.account.as_deref().unwrap_or("not available"),
        status.plan.as_deref().unwrap_or("not available"),
        date(status.expires_at),
        date(status.last_refresh),
        status.last_error.as_deref().unwrap_or("none")
    )
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
        return form_error(
            "This provider is used by a route or model. Remove those references first.",
        );
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
    let mut req = st
        .http()
        .get(url)
        .timeout(std::time::Duration::from_secs(15));
    if let Some(key) = p.keys.first() {
        req = match p.kind {
            ProviderKind::Openai => req.bearer_auth(key),
            ProviderKind::Anthropic => req
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01"),
        };
    }
    match req.send().await {
        Ok(r) => Html(format!("<p>Upstream status: {}</p>", r.status().as_u16())).into_response(),
        Err(_) => form_error("Cannot reach the provider. Check its URL and credentials."),
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
