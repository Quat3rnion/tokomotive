//! Live configuration forms. No secret is sent back to the browser.
use super::*;
use crate::config::{Config, DeploymentCfg, ProviderCfg, RouteCfg, Strategy};
use crate::provider::ProviderKind;
use crate::provider_types::ProviderType;
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
        .route("/admin/providers/{name}/key-strategy", post(key_strategy))
        .route(
            "/admin/providers/{name}/accounts/{account}/reenable",
            post(reenable_account),
        )
        .route(
            "/admin/providers/{name}/keys/{index}/delete",
            post(delete_key),
        )
        .route("/admin/providers/{name}/test", post(test_provider))
        .route("/admin/providers/{name}/egress", post(save_provider_egress))
        .route("/admin/search/provider-types", get(search_provider_types))
        .route("/admin/provider-form", get(provider_form))
        .route("/admin/routes", get(routes).post(create_route))
        .route("/admin/routes/try", post(try_model))
        .route("/admin/routes/{name}", post(update_route))
        .route("/admin/routes/{name}/delete", post(delete_route))
        .route("/admin/routes/{name}/scores", get(adaptive_scores))
        .route("/admin/settings", get(settings_page).post(save_settings))
        .route("/admin/settings/egress", post(save_global_egress))
        .route("/admin/settings/password", post(change_password))
        .route("/admin/settings/mode", post(save_mode))
        .route("/admin/settings/alerts", post(save_alert))
        .route("/admin/settings/alerts/{id}/test", post(test_alert))
        .route("/admin/settings/alerts/{id}/delete", post(delete_alert))
        .route("/admin/settings/observability", post(save_observability))
        .route("/admin/routes/{name}/capture", post(route_capture))
        .route("/admin/keys/{id}/capture", post(key_capture))
}

#[derive(Deserialize)]
struct AlertForm {
    id: Option<i64>,
    #[serde(default)]
    url: String,
    format: String,
    enabled: Option<String>,
    thresholds: Option<String>,
}

#[derive(Deserialize)]
struct ObservabilityForm {
    enabled: Option<String>,
    endpoint: String,
    headers: String,
    clear_headers: Option<String>,
    service_name: String,
    sample_ratio: f64,
    body_cap: usize,
    retention_days: i64,
}
async fn save_observability(
    _: Admin,
    State(st): State<AppState>,
    Form(f): Form<ObservabilityForm>,
) -> Response {
    let _guard = st.settings_write.lock().await;
    let old = st.observability.settings();
    let s = crate::observability::Settings {
        enabled: f.enabled.is_some(),
        endpoint: f.endpoint.trim().into(),
        headers: if f.clear_headers.is_some() {
            String::new()
        } else if f.headers.is_empty() {
            old.headers.clone()
        } else {
            f.headers
        },
        service_name: f.service_name.trim().into(),
        sample_ratio: f.sample_ratio,
        body_cap: f.body_cap,
        retention_days: f.retention_days,
    };
    if let Err(e) = crate::observability::save(&st.db, &st.secret_key, &s).await {
        return form_error(e);
    }
    st.observability.configure(s);
    saved(
        "/admin/settings#observability",
        "Observability settings saved.",
    )
}
#[derive(Deserialize)]
struct CaptureForm {
    enabled: Option<String>,
}
async fn route_capture(
    _: Admin,
    State(st): State<AppState>,
    Path(name): Path<String>,
    Form(f): Form<CaptureForm>,
) -> Response {
    tryr!(
        sqlx::query("UPDATE routes SET capture_bodies=? WHERE name=?")
            .bind(f.enabled.is_some())
            .bind(name)
            .execute(&st.db)
            .await
    );
    saved("/admin/routes", "Body capture setting saved.")
}
async fn key_capture(
    _: Admin,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<CaptureForm>,
) -> Response {
    tryr!(
        sqlx::query("UPDATE virtual_keys SET capture_bodies=? WHERE id=?")
            .bind(f.enabled.is_some())
            .bind(id)
            .execute(&st.db)
            .await
    );
    saved("/admin/keys", "Body capture setting saved.")
}
async fn save_alert(_: Admin, State(st): State<AppState>, Form(f): Form<AlertForm>) -> Response {
    let _guard = st.settings_write.lock().await;
    if let Some(value) = &f.thresholds
        && let Err(e) = crate::alerts::parse_thresholds(value)
    {
        return form_error(e);
    }
    if let Err(e) = crate::alerts::save(&st, f.id, &f.url, &f.format, f.enabled.is_some()).await {
        return form_error(e);
    }
    if let Some(value) = f.thresholds {
        tryr!(store::put(&st.db, "alert_thresholds", &value).await);
    }
    saved("/admin/settings#alerts", "Alert settings saved.")
}
async fn test_alert(_: Admin, State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    crate::alerts::emit_to(
        &st,
        crate::alerts::Event::new("test", "operator", "Tokomotive test alert"),
        Some(id),
    );
    saved(
        "/admin/settings#alerts",
        "Test alert queued. Reload to see the delivery status.",
    )
}
async fn delete_alert(_: Admin, State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    tryr!(
        sqlx::query("DELETE FROM alert_webhooks WHERE id=?")
            .bind(id)
            .execute(&st.db)
            .await
    );
    saved("/admin/settings#alerts", "Webhook deleted.")
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsPage {
    route_choices: Option<Vec<db::ControlChoice>>,
    egress_label: String,
    egress_set: bool,
    observability: std::sync::Arc<crate::observability::Settings>,
    export_headers_set: bool,
    audit_retention_days: String,
    accounts: Vec<accounts::AccountView>,
    webhooks: Vec<crate::alerts::Webhook>,
    alert_thresholds: String,
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
        route_choices: tryr!(db::small_choices(&st.db, "routes").await),
        egress_label: st
            .egress
            .label(None)
            .unwrap_or_else(|| "None (direct connection)".into()),
        egress_set: st.egress.label(None).is_some(),
        observability: st.observability.settings(),
        export_headers_set: !st.observability.settings().headers.is_empty(),
        audit_retention_days: tryr!(store::value(&st.db, "audit_retention_days").await)
            .unwrap_or_else(|| "365".into()),
        accounts: if ctx().role == "owner" {
            tryr!(accounts::list(&st).await)
        } else {
            vec![]
        },
        webhooks: tryr!(crate::alerts::list(&st.db).await),
        alert_thresholds: tryr!(crate::alerts::thresholds(&st.db).await),
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

#[derive(Default, Deserialize)]
struct ProviderLimitsForm {
    request_timeout_ms: Option<String>,
    connect_timeout_ms: Option<String>,
    stream_idle_timeout_ms: Option<String>,
    max_concurrent: Option<String>,
    max_wait_ms: Option<String>,
    max_retries: Option<String>,
    backoff_base_ms: Option<String>,
    backoff_max_ms: Option<String>,
    backoff_jitter: Option<String>,
}
impl ProviderLimitsForm {
    fn parse(
        &self,
        previous: &crate::provider_limits::Limits,
    ) -> anyhow::Result<crate::provider_limits::Limits> {
        fn number<T: std::str::FromStr + Copy>(
            value: &Option<String>,
            old: Option<T>,
        ) -> anyhow::Result<Option<T>> {
            match value.as_deref().map(str::trim) {
                None => Ok(old),
                Some("") => Ok(None),
                Some(value) => value.parse().map(Some).map_err(|_| {
                    anyhow::anyhow!("Enter a whole number, 0 or more, or leave the limit blank.")
                }),
            }
        }
        let limits = crate::provider_limits::Limits {
            request_timeout_ms: number(&self.request_timeout_ms, previous.request_timeout_ms)?,
            connect_timeout_ms: number(&self.connect_timeout_ms, previous.connect_timeout_ms)?,
            stream_idle_timeout_ms: number(
                &self.stream_idle_timeout_ms,
                previous.stream_idle_timeout_ms,
            )?,
            max_concurrent: number(&self.max_concurrent, previous.max_concurrent)?,
            max_wait_ms: number(&self.max_wait_ms, previous.max_wait_ms)?,
            max_retries: number(&self.max_retries, previous.max_retries)?,
            backoff_base_ms: number(&self.backoff_base_ms, previous.backoff_base_ms)?,
            backoff_max_ms: number(&self.backoff_max_ms, previous.backoff_max_ms)?,
            backoff_jitter: match self.backoff_jitter.as_deref() {
                None => previous.backoff_jitter,
                Some("") => None,
                Some("true") => Some(true),
                Some("false") => Some(false),
                _ => anyhow::bail!("Choose Inherit, On or Off for backoff jitter."),
            },
        };
        limits.validate()?;
        Ok(limits)
    }
}
struct LimitField {
    name: &'static str,
    label: &'static str,
    value: String,
    min: u8,
}
fn limit_fields(limits: &crate::provider_limits::Limits) -> Vec<LimitField> {
    [
        (
            "request_timeout_ms",
            "Request timeout (ms)",
            limits.request_timeout_ms,
            1,
        ),
        (
            "connect_timeout_ms",
            "Connect timeout (ms)",
            limits.connect_timeout_ms,
            0,
        ),
        (
            "stream_idle_timeout_ms",
            "Stream idle timeout (ms)",
            limits.stream_idle_timeout_ms,
            0,
        ),
        (
            "max_concurrent",
            "Maximum concurrent requests",
            limits.max_concurrent.map(|n| n as u64),
            0,
        ),
        (
            "max_wait_ms",
            "Maximum concurrency wait (ms)",
            limits.max_wait_ms,
            0,
        ),
        (
            "max_retries",
            "Maximum Provider retries",
            limits.max_retries.map(|n| n as u64),
            0,
        ),
        (
            "backoff_base_ms",
            "Backoff base (ms)",
            limits.backoff_base_ms,
            0,
        ),
        (
            "backoff_max_ms",
            "Backoff maximum (ms)",
            limits.backoff_max_ms,
            0,
        ),
    ]
    .into_iter()
    .map(|(name, label, value, min)| LimitField {
        name,
        label,
        value: value.map(|n| n.to_string()).unwrap_or_default(),
        min,
    })
    .collect()
}
fn jitter_value(limits: &crate::provider_limits::Limits) -> &'static str {
    match limits.backoff_jitter {
        Some(true) => "true",
        Some(false) => "false",
        None => "",
    }
}
impl SettingsPage {
    fn limit_fields(&self) -> Vec<LimitField> {
        limit_fields(&self.settings.provider_defaults)
    }
    fn jitter(&self) -> &'static str {
        jitter_value(&self.settings.provider_defaults)
    }
}

#[derive(Deserialize)]
struct ServerForm {
    #[serde(flatten)]
    provider_limits: ProviderLimitsForm,
    unknown_model_policy: Option<String>,
    unknown_model_route: Option<String>,
    writer_timeout_secs: Option<u64>,
    ledger_spool_max_bytes: Option<u64>,
    cache_max_entries: Option<u64>,
    cache_max_bytes: Option<u64>,
    cache_max_entry_bytes: Option<u64>,
    cache_hit_price_micros: Option<i64>,
    quota_exhausted_percent: Option<String>,
    adaptive_epsilon: Option<f64>,
    adaptive_half_life: Option<f64>,
    adaptive_prior_decay_minutes: Option<f64>,
    default_markup_percent: Option<String>,
    upstream_timeout_secs: String,
    reserve_output_tokens: Option<String>,
    audio_upload_limit_mib: Option<String>,
    file_upload_limit_mib: Option<String>,
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
        cfg.server.provider_defaults = self.provider_limits.parse(&cfg.server.provider_defaults)?;
        if let Some(value) = &self.reserve_output_tokens {
            cfg.server.reserve_output_tokens = value
                .parse()
                .context("Enter the reserved output tokens as a whole number, 0 or more.")?;
        }
        cfg.server.session_days = self
            .session_days
            .parse()
            .context("Enter the session days as a whole number from 1 to 3650.")?;
        if let Some(value) = &self.audio_upload_limit_mib {
            cfg.server.audio_upload_limit_mib = value
                .parse()
                .context("Enter the audio upload limit as whole MiB from 1 to 63.")?;
        }
        if let Some(value) = &self.file_upload_limit_mib {
            cfg.server.file_upload_limit_mib = value
                .parse()
                .context("Enter the file upload limit as whole MiB from 1 to 63.")?;
        }
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
        if let Some(value) = self.adaptive_epsilon {
            cfg.router.adaptive_epsilon = value;
        }
        if let Some(value) = self.adaptive_half_life {
            cfg.router.adaptive_half_life = value;
        }
        if let Some(value) = self.adaptive_prior_decay_minutes {
            cfg.router.adaptive_prior_decay_minutes = value;
        }
        if let Some(policy) = &self.unknown_model_policy {
            cfg.router.unknown_model_route = match policy.as_str() {
                "reject" => None,
                "route" => {
                    let name = self.unknown_model_route.as_deref().unwrap_or("").trim();
                    anyhow::ensure!(
                        cfg.routes.iter().any(|r| r.name == name),
                        "Choose an existing Route for unknown model names."
                    );
                    Some(name.into())
                }
                _ => anyhow::bail!("Choose Reject or Send to Route."),
            };
        }
        if let Some(value) = &self.quota_exhausted_percent {
            cfg.router.quota_exhausted_percent = value
                .parse()
                .context("Enter a quota threshold from 0 to 100 percent.")?;
        }
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
            writer_timeout_secs: self
                .writer_timeout_secs
                .unwrap_or(store::default_writer_timeout_secs()),
            ledger_spool_max_bytes: self
                .ledger_spool_max_bytes
                .unwrap_or(store::default_ledger_spool_max_bytes()),
            cache: Default::default(),
            upstream_timeout_secs: cfg.server.upstream_timeout_secs,
            provider_defaults: cfg.server.provider_defaults.clone(),
            reserve_output_tokens: cfg.server.reserve_output_tokens,
            audio_upload_limit_mib: cfg.server.audio_upload_limit_mib,
            file_upload_limit_mib: cfg.server.file_upload_limit_mib,
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
    cfg.server.audio_upload_limit_mib = st.settings().audio_upload_limit_mib;
    cfg.server.file_upload_limit_mib = st.settings().file_upload_limit_mib;
    let (cfg, mut settings) = match f.parse(cfg, &st.settings().mode) {
        Ok(v) => v,
        Err(e) => return form_error(e),
    };
    settings.cache = st.settings().cache.clone();
    settings.writer_timeout_secs = f
        .writer_timeout_secs
        .unwrap_or(st.settings().writer_timeout_secs);
    settings.ledger_spool_max_bytes = f
        .ledger_spool_max_bytes
        .unwrap_or(st.settings().ledger_spool_max_bytes);
    if !(1..=3600).contains(&settings.writer_timeout_secs)
        || !(1..=1_073_741_824).contains(&settings.ledger_spool_max_bytes)
    {
        return form_error("Use 1 to 3600 writer wait seconds and 1 to 1073741824 spool bytes.");
    }
    if let Some(n) = f.cache_max_entries {
        settings.cache.max_entries = n;
    }
    if let Some(n) = f.cache_max_bytes {
        settings.cache.max_bytes = n;
    }
    if let Some(n) = f.cache_max_entry_bytes {
        settings.cache.max_entry_bytes = n;
    }
    if let Some(n) = f.cache_hit_price_micros {
        if n < 0 {
            return form_error("Cache hit Price must be 0 or more microdollars.");
        }
        settings.cache.hit_price_micros = n;
    }
    let token = if f.clear_metrics_token.is_some() {
        None
    } else if !f.metrics_token.is_empty() {
        Some(f.metrics_token)
    } else {
        st.settings().metrics_token.clone()
    };
    settings.metrics_token = tryr!(token.map(|v| st.secret_key.encrypt(&v)).transpose());
    let mut tx = tryr!(db::begin_write(&st.db, "server_settings").await);
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
    tryr!(crate::cache::maintain(&st.db, &settings.cache).await);
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
    admin: Admin,
    State(st): State<AppState>,
    Form(f): Form<PasswordForm>,
) -> Response {
    let _guard = st.settings_write.lock().await;
    if f.password.chars().count() < 8 {
        return form_error("Use at least 8 characters for the new password.");
    }
    if !store::verify_password(&f.current_password, &admin.0.password_hash) {
        return form_error("Incorrect current password.");
    }
    let hash = tryr!(store::hash_password(&f.password));
    let mut tx = tryr!(db::begin_write(&st.db, "account_password").await);
    tryr!(
        sqlx::query("UPDATE admin_accounts SET password_hash=? WHERE id=?")
            .bind(&hash)
            .bind(admin.0.id)
            .execute(&mut *tx)
            .await
    );
    tryr!(tx.commit().await);
    saved(
        "/admin/login",
        "Password changed. Log in with the new password.",
    )
}

#[derive(Template)]
#[template(path = "routes.html")]
struct RoutesPage {
    key_choices: Vec<(i64, String)>,
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
    rules_html: String,
    capture_bodies: bool,
    cache: crate::cache::Policy,
    skips: Vec<String>,
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
    let table = st.router_table();
    let schedule = tryr!(crate::quota::Schedule::load(&st.db).await);
    let settings = st.settings();
    let now = chrono::Utc::now().timestamp();
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
            rules_html: tryr!(tryr!(super::route_rules::Editor::load(&st,&r.name).await).render()),
            capture_bodies: tryr!(sqlx::query_scalar("SELECT capture_bodies FROM routes WHERE name=?").bind(&r.name).fetch_one(&st.db).await),
            cache: r.cache.clone(),
            skips: deployments
                .iter()
                .flat_map(|d| {
                    let mut messages = Vec::new();
                    if let Some(p) = table.providers.iter().find(|p| p.name == d.provider) {
                    let indices: Vec<_> = if p.keys.is_empty() { vec![None] } else { (0..p.keys.len()).map(Some).collect() };
                    for i in indices {
                            if let Some(skip) = schedule.skip(
                                p,
                                i,
                                now,
                                settings.router.quota_exhausted_percent,
                                settings.router.cooldown_secs,
                            ) {
                                let reason = if let Some(t) = skip.until {
                                    format!(
                                        "Skipped until {} (rate limited)",
                                        super::views::stamp_opt(t)
                                            .map(|s| s.text)
                                            .unwrap_or_else(|| t.to_string())
                                    )
                                } else {
                                    "Disabled: credentials invalid".into()
                                };
                                messages.push(format!(
                                    "{}/{} - {}: {reason}",
                                    d.provider,
                                    d.model,
                                    crate::quota::account_id(p, i)
                                ));
                            }
                        }
                        let candidate = crate::routing::Candidate {
                            adaptive: None,
                            provider: p.clone(),
                            model: Some(d.model.clone()),
                            route: r.name.clone(),
                            rewrite: true,
                        };
                        if !st.deployment_available(&candidate, &table.settings) {
                            messages.push(format!(
                                "{}/{} - Circuit-breaker cooldown (transient failure or rate limit without reset)",
                                d.provider, d.model
                            ));
                        }
                    }
                    messages
                })
                .collect(),
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
        key_choices: tryr!(sqlx::query_as("SELECT k.id,u.name || ' / ' || k.label FROM virtual_keys k JOIN users u ON u.id=k.user_id ORDER BY u.name,k.label,k.id LIMIT 200").fetch_all(&st.db).await),
        provider_choices: tryr!(db::small_choices(&st.db, "providers").await),
        pager,
        base: Base::new(&st),
        routes,
        providers,
    })
}

#[derive(Template)]
#[template(path = "adaptive_scores.html")]
struct AdaptiveScores {
    rows: Vec<AdaptiveScoreView>,
}
struct AdaptiveScoreView {
    provider: String,
    model: String,
    success: String,
    latency: String,
    ttft: String,
    share: String,
    stream_share: String,
}
async fn adaptive_scores(
    _: Admin,
    State(st): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    let table = st.router_table();
    let Some(route) = table
        .routes
        .iter()
        .find(|r| r.name == name && matches!(r.strategy, Strategy::Adaptive))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let settings = st.settings();
    let scores = st.adaptive.scores(route, &settings.router, false);
    let streams = st.adaptive.scores(route, &settings.router, true);
    let total: f64 = scores.iter().map(|s| s.score).sum();
    let stream_total: f64 = streams.iter().map(|s| s.score).sum();
    let count = scores.len().max(1) as f64;
    let share = |score: f64, total: f64| {
        format!(
            "{:.1}",
            100.0
                * (settings.router.adaptive_epsilon / count
                    + (1.0 - settings.router.adaptive_epsilon)
                        * if total > 0.0 {
                            score / total
                        } else {
                            1.0 / count
                        })
        )
    };
    render(AdaptiveScores {
        rows: scores
            .into_iter()
            .zip(streams)
            .map(|(s, t)| AdaptiveScoreView {
                provider: s.provider,
                model: s.model,
                success: format!("{:.1}", s.success * 100.0),
                latency: format!("{:.1}", s.latency),
                ttft: format!("{:.1}", t.latency),
                share: share(s.score, total),
                stream_share: share(t.score, stream_total),
            })
            .collect(),
    })
}

#[derive(Deserialize)]
struct TryModelForm {
    model: String,
    key_id: Option<String>,
    session_id: Option<String>,
}

#[derive(Template)]
#[template(path = "resolution.html")]
struct ResolutionView {
    steps: Vec<String>,
    result: String,
}

async fn try_model(_: Admin, State(st): State<AppState>, Form(f): Form<TryModelForm>) -> Response {
    if f.model.len() > 200 {
        return bad("Use at most 200 characters for a model name.");
    }
    let mut identity = (0, 0, None);
    let (models, providers): (Vec<String>, Vec<String>) = if let Some(id) =
        f.key_id.filter(|id| !id.is_empty())
    {
        let id = match id.parse::<i64>() {
            Ok(id) => id,
            Err(_) => return bad("Choose a Virtual Key."),
        };
        let row: Option<(Option<String>, Option<String>)> = tryr!(
            sqlx::query_as("SELECT allowed_models,allowed_providers FROM virtual_keys WHERE id=?")
                .bind(id)
                .fetch_optional(&st.db)
                .await
        );
        let Some((models, providers)) = row else {
            return bad("Virtual Key not found.");
        };
        identity=tryr!(sqlx::query_as("SELECT k.id,k.user_id,u.team_id FROM virtual_keys k JOIN users u ON u.id=k.user_id WHERE k.id=?").bind(id).fetch_one(&st.db).await);
        (
            tryr!(serde_json::from_str(models.as_deref().unwrap_or("[]"))),
            tryr!(serde_json::from_str(providers.as_deref().unwrap_or("[]"))),
        )
    } else {
        (vec![], vec![])
    };
    let access = crate::resolve::Access {
        models: &models,
        providers: &providers,
    };
    let table = st.router_table();
    let catalog = st.catalog();
    let r = crate::resolve::resolve(
        &table,
        &catalog,
        &st.resolution_index.read().expect("resolution index"),
        f.model.trim(),
        access,
    );
    let mut rule_note = String::new();
    let mut selected_table = None;
    let mut rejection = None;
    if r.allowed(access, f.model.trim(), &catalog)
        && let Some(crate::resolve::Target::Route(route)) = &r.target
    {
        let rules = tryr!(crate::route_rules::load(&st.db, route).await);
        let mut headers = axum::http::HeaderMap::new();
        if let Some(session) = f.session_id.filter(|v| !v.is_empty()) {
            if session.len() > 512 {
                return form_error("Use at most 512 characters for a session id.");
            }
            let value = match session.parse() {
                Ok(v) => v,
                Err(_) => return form_error("Use a valid session id."),
            };
            headers.insert("x-session-id", value);
        }
        let attributes = crate::route_rules::Attributes {
            key: identity.0,
            user: identity.1,
            team: identity.2,
            headers: &headers,
            body: None,
            model: f.model.trim(),
            endpoint: "chat_completions",
            format: "openai",
        };
        if let Some(selection) = crate::route_rules::select(&rules, &attributes) {
            rule_note = format!(
                " Matched rule: {}. Canary side: {}.",
                selection.rule.name, selection.side
            );
            if let crate::route_rules::Action::Reject { message } = selection.action {
                rejection = Some(format!("403: {message}"));
            }
            selected_table = Some(crate::route_rules::apply(
                &table,
                route,
                selection.action,
                selection.rule.no_fallback,
            ));
        } else {
            rule_note = " Matched rule: none.".into();
        }
    }
    let table = selected_table.as_ref().unwrap_or(&table);
    let mut result = if let Some(message) = rejection {
        message
    } else if !r.allowed(access, f.model.trim(), &catalog) && r.target.is_some() {
        "403: this API key may not use this model".into()
    } else {
        match &r.target {
            Some(crate::resolve::Target::Route(route)) => {
                let providers = table
                    .reachable_providers(&catalog, route)
                    .iter()
                    .filter(|p| access.provider(p))
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                if providers.is_empty() {
                    "403: this API key may not use the Route's Providers".into()
                } else {
                    format!("Route: {route}. Providers: {providers}.")
                }
            }
            Some(crate::resolve::Target::Model { provider, model }) => {
                if table
                    .providers
                    .iter()
                    .any(|p| &p.name == provider && access.provider(p))
                {
                    format!("Provider: {provider}. Model: {model}.")
                } else {
                    "403: this API key may not use this Provider".into()
                }
            }
            None => format!(
                "404 model_not_found: {}",
                crate::resolve::message(
                    table,
                    &catalog,
                    &st.resolution_index.read().expect("resolution index"),
                    f.model.trim(),
                    access,
                    &r
                )
            ),
        }
    };
    result.push_str(&rule_note);
    render(ResolutionView {
        steps: r.steps,
        result,
    })
}

/// The route form. Deployment rows arrive as repeated
/// `deployment_provider`, `deployment_model` and `deployment_weight` fields.
#[derive(Default)]
struct RouteForm {
    cache_enabled: Option<String>,
    cache_ttl_secs: Option<String>,
    cache_scope: Option<String>,
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
                "cache_enabled" => f.cache_enabled = Some(value),
                "cache_ttl_secs" => f.cache_ttl_secs = Some(value),
                "cache_scope" => f.cache_scope = Some(value),
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
        cache: Default::default(),
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
        route.cache = original.cache.clone();
        if name != &route.name && tryr!(crate::route_rules::referenced(&st.db, name).await) {
            return form_error(
                "A Route rule uses this Route. Remove that action before you rename it.",
            );
        }
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
    if let Some(v) = f.cache_enabled.as_deref() {
        route.cache.enabled = match v {
            "on" => true,
            "off" => false,
            _ => return form_error("Choose Cache On or Off."),
        };
    }
    if let Some(v) = f.cache_ttl_secs.as_deref() {
        route.cache.ttl_secs = match v.parse::<u64>() {
            Ok(n) if n > 0 => n,
            _ => return form_error("Cache lifetime must be a whole number of seconds, 1 or more."),
        };
    }
    if let Some(v) = f.cache_scope {
        route.cache.scope = match serde_json::from_value(serde_json::Value::String(v)) {
            Ok(v) => v,
            Err(_) => return form_error("Choose cache scope Key, Team or Global."),
        };
    }
    cfg.routes.push(route.clone());
    if let Some(original_name) = &old {
        let rules = tryr!(crate::route_rules::load(&st.db, original_name).await);
        if let Err(e) = crate::route_rules::validate_deployments(&rules, &route.deployments) {
            return form_error(e);
        }
    }
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
    let mut tx = tryr!(db::begin_write(&st.db, "route_settings").await);
    let config_json = tryr!(serde_json::to_string(&route));
    // Keep independent Route controls when the configuration or name changes.
    let written = if let Some(name) = old {
        sqlx::query("UPDATE routes SET name=?,config_json=? WHERE name=?")
            .bind(&route.name)
            .bind(config_json)
            .bind(name)
            .execute(&mut *tx)
            .await
    } else {
        sqlx::query("INSERT INTO routes(name,config_json) VALUES(?,?)")
            .bind(&route.name)
            .bind(config_json)
            .execute(&mut *tx)
            .await
    };
    if let Err(e) = written {
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
    if tryr!(crate::route_rules::referenced(&st.db, &name).await) {
        return form_error(
            "A Route rule uses this Route. Remove that action before you delete it.",
        );
    }
    let cfg = tryr!(runtime(&st).await);
    if cfg.router.unknown_model_route.as_ref() == Some(&name) {
        return form_error(
            "This Route handles unknown model names. Select Reject or another Route in Settings first.",
        );
    }
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
    /// The no-JavaScript provider select: (group label, types).
    groups: Vec<(&'static str, Vec<ProviderType>)>,
    type_count: usize,
    /// The guided form of the chosen type (`?add=` or `?preset=`), as
    /// rendered provider_add.html.
    add_html: String,
    /// The Add a Provider panel starts open.
    add_open: bool,
    /// There is at least one Provider (the panel can collapse).
    any_provider: bool,
    add_error: String,
}
/// The guided Add a Provider form for one provider type.
struct AddForm {
    t: ProviderType,
    name: String,
}
impl AddForm {
    fn signin(&self) -> bool {
        use crate::provider_types::Category;
        matches!(
            self.t.category,
            Category::Subscription | Category::Vertex | Category::Entra | Category::HuggingFace
        )
    }
    fn api_key(&self) -> bool {
        use crate::provider_types::Category;
        matches!(
            self.t.category,
            Category::ApiKey | Category::Local | Category::Custom
        )
    }
    fn cat(&self) -> &'static str {
        self.t.category_name()
    }
}
#[derive(Template)]
#[template(path = "provider_add.html")]
struct AddFormFragment {
    form: AddForm,
}
#[derive(Template)]
#[template(path = "provider_type_options.html")]
struct TypeOptions {
    hits: Vec<ProviderType>,
}
#[derive(Template)]
#[template(path = "provider_added_signin.html")]
struct AddedSignin {
    name: String,
    panel: String,
}
struct ProviderView {
    limits: crate::provider_limits::Limits,
    default_model: String,
    priority: String,
    proxy_mode: String,
    proxy_label: String,
    key_strategy: String,
    quota: Vec<super::quota::AccountView>,
    /// Just added: the card shows the next steps and takes focus.
    added: bool,
    added_message: String,
    aws_region: String,
    aws_saved: bool,
    project_id: String,
    region: String,
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
    fn limit_fields(&self) -> Vec<LimitField> {
        limit_fields(&self.limits)
    }
    fn jitter(&self) -> &'static str {
        jitter_value(&self.limits)
    }
    fn default_model_id(&self) -> String {
        format!("p-{}-default-model", self.name)
    }
    fn has_unset_env(&self) -> bool {
        self.keys.iter().any(|k| k.env_set == Some(false))
    }
    fn disconnected(&self) -> bool {
        self.status.as_ref().is_some_and(|s| !s.connected)
    }
    fn skipped(&self) -> usize {
        self.quota.iter().filter(|a| a.skipped).count()
    }
    /// Quota and health start open only when they have observations to show:
    /// quota windows, or an account that is skipped, limited or disconnected.
    fn health_open(&self) -> bool {
        !self.quota.is_empty()
            && (self.disconnected()
                || self
                    .quota
                    .iter()
                    .any(|a| a.skipped || a.state != "ok" || !a.windows.is_empty()))
    }
    /// Tag class of the compact health status in the card.
    fn health_class(&self) -> &'static str {
        if self.skipped() > 0 || self.quota.iter().any(|a| a.state == "credentials_invalid") {
            "bad"
        } else if self.disconnected() || self.quota.iter().any(|a| a.state != "ok") {
            "warn"
        } else if self.quota.is_empty() {
            ""
        } else {
            "good"
        }
    }
    fn health_text(&self) -> String {
        let skipped = self.skipped();
        if skipped == 1 {
            "1 account skipped".into()
        } else if skipped > 1 {
            format!("{skipped} accounts skipped")
        } else if self.disconnected() {
            "Disabled".into()
        } else if self.quota.iter().any(|a| a.state != "ok") {
            "Rate limited".into()
        } else if self.quota.is_empty() {
            "No data yet".into()
        } else {
            let used = self
                .quota
                .iter()
                .flat_map(|a| a.windows.iter().filter_map(|w| w.percent))
                .fold(None, |m: Option<f64>, p| Some(m.map_or(p, |m| m.max(p))));
            match used {
                Some(p) => format!("Healthy, highest use {p:.0}%"),
                None => "Healthy".into(),
            }
        }
    }
}
/// One entry of a Key Pool. Never more than the last 4 characters.
struct KeyView {
    quota_id: Option<String>,
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
    next_refresh: Option<super::views::Stamp>,
    last_refresh_error: Option<String>,
    refresh_in_doubt: bool,
    refreshable: bool,
}
struct UsageView {
    label: String,
    /// 0 to 100, for `<meter>`.
    percent: f64,
    percent_text: String,
    resets: Option<super::views::Stamp>,
}

fn last4(plain: &str) -> String {
    // Show the tail only when at least 8 characters stay hidden.
    if plain.chars().count() < 12 {
        return "****".into();
    }
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
    crate::config::provider_env_value(name)
}

fn key_view(st: &AppState, stored: &str, index: i64) -> anyhow::Result<KeyView> {
    Ok(match stored.strip_prefix("env:") {
        Some(env) => KeyView {
            quota_id: env_value(env).as_deref().map(crate::quota::key_id),
            index,
            label: stored.into(),
            env_set: Some(env_value(env).is_some()),
        },
        None => {
            let plain = st.secret_key.decrypt(stored)?;
            KeyView {
                quota_id: Some(crate::quota::key_id(&plain)),
                index,
                label: last4(&plain),
                env_set: None,
            }
        }
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
        next_refresh: stamp(status.next_refresh_at),
        last_refresh_error: status.last_refresh_error,
        refresh_in_doubt: status.refresh_in_doubt,
        refreshable: status.next_refresh_at.is_some(),
    }
}

pub(super) fn auth_label(auth: crate::provider::ProviderAuth) -> &'static str {
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
        crate::provider::ProviderAuth::GeminiCli => "Gemini CLI (unofficial)",
        crate::provider::ProviderAuth::GoogleVertex => "Google Vertex AI",
    }
}

#[derive(Deserialize)]
struct ProvidersQ {
    #[serde(flatten)]
    page: PageQuery,
    pool_page: Option<i64>,
    pool_provider: Option<String>,
    preset: Option<String>,
    /// A provider type ID: opens the guided form (an empty value opens the
    /// picker).
    add: Option<String>,
    /// The name of a Provider that was just added.
    added: Option<String>,
}

async fn provider_types(st: &AppState) -> anyhow::Result<Vec<ProviderType>> {
    Ok(crate::provider_types::catalog(
        &crate::presets::merged(&st.db).await?,
    ))
}

/// The type's base name, or the base name with the first free `-N`.
async fn suggest_name(st: &AppState, base: &str) -> anyhow::Result<String> {
    let taken: std::collections::HashSet<String> =
        sqlx::query_scalar("SELECT name FROM providers WHERE substr(name,1,?)=?")
            .bind(base.len() as i64)
            .bind(base)
            .fetch_all(&st.db)
            .await?
            .into_iter()
            .collect();
    if !taken.contains(base) {
        return Ok(base.into());
    }
    Ok((2..)
        .map(|n| format!("{base}-{n}"))
        .find(|c| !taken.contains(c))
        .unwrap_or_else(|| base.into()))
}

async fn add_form(st: &AppState, t: ProviderType, name: Option<&str>) -> anyhow::Result<AddForm> {
    let name = match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => n.chars().take(64).collect(),
        None => suggest_name(st, &t.name_base()).await?,
    };
    Ok(AddForm { t, name })
}

#[derive(Deserialize)]
struct TypeSearchQ {
    #[serde(default)]
    q: String,
}

/// The provider picker search: `<li role="option">` items for the picker,
/// or JSON for `Accept: application/json`. At most 20 results.
async fn search_provider_types(
    _: Admin,
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<TypeSearchQ>,
) -> Response {
    let types = tryr!(provider_types(&st).await);
    let text: String = q.q.chars().take(200).collect();
    let hits: Vec<ProviderType> = crate::provider_types::search(&types, &text)
        .into_iter()
        .cloned()
        .collect();
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !headers.contains_key("hx-request") && !accept.contains("text/html") {
        return axum::Json(hits).into_response();
    }
    render(TypeOptions { hits })
}

#[derive(Deserialize)]
struct TypeFormQ {
    #[serde(default, rename = "type")]
    id: String,
    #[serde(default)]
    name: String,
}

/// The guided form of one provider type, for the picker (htmx).
async fn provider_form(
    _: Admin,
    State(st): State<AppState>,
    Query(q): Query<TypeFormQ>,
) -> Response {
    let types = tryr!(provider_types(&st).await);
    let Some(t) = types.into_iter().find(|t| t.id == q.id) else {
        return bad("This Provider type is not known. Search again.");
    };
    render(AddFormFragment {
        form: tryr!(add_form(&st, t, Some(&q.name)).await),
    })
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
        sqlx::query("SELECT name,kind,auth,base_url,include_usage,key_header,key_strategy,default_model,priority,bedrock_credentials IS NOT NULL AS aws_saved,project_id,region,proxy_mode,limits_json FROM providers WHERE instr(lower(name),lower(?)) > 0 ORDER BY name LIMIT ? OFFSET ?")
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
        let plan = if matches!(
            auth,
            crate::provider::ProviderAuth::Chatgpt
                | crate::provider::ProviderAuth::Codex
                | crate::provider::ProviderAuth::GithubCopilot
        ) {
            st.oauth.usage(&name).await
        } else {
            None
        };
        if let Some(plan) = &plan {
            tryr!(
                crate::quota::save_plan(
                    &st.db,
                    &name,
                    if chatgpt { "usage-signin" } else { "signin" },
                    plan,
                    plan.observed_at.div_euclid(1000)
                )
                .await
            );
        }
        let usage = if chatgpt {
            plan.map(|u| {
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
            limits: tryr!(serde_json::from_str(row.get("limits_json"))),
            default_model: row
                .get::<Option<String>, _>("default_model")
                .unwrap_or_default(),
            priority: row
                .get::<Option<i16>, _>("priority")
                .map(|n| n.to_string())
                .unwrap_or_default(),
            proxy_mode: row.get("proxy_mode"),
            proxy_label: st
                .egress
                .label(Some(&name))
                .unwrap_or_else(|| "None (direct connection)".into()),
            key_strategy: row.get("key_strategy"),
            quota: tryr!(super::quota::views(&st, Some(&name), 50).await),
            added: false,
            added_message: String::new(),
            aws_region: crate::sigv4::region_from_base(row.get::<&str, _>("base_url"))
                .unwrap_or("")
                .into(),
            aws_saved: row.get("aws_saved"),
            project_id: row.get("project_id"),
            region: row.get("region"),
            key_pager,
            name,
            auth: auth.as_str().into(),
            openrouter: auth == crate::provider::ProviderAuth::ApiKey
                && matches!(
                    row.get::<&str, _>("base_url").trim_end_matches('/'),
                    "https://openrouter.ai/api" | "https://openrouter.ai/api/v1"
                ),
            auth_label: if kind == ProviderKind::Bedrock {
                "AWS SigV4"
            } else {
                auth_label(auth)
            },
            status,
            usage_status,
            usage,
            kind: match kind {
                ProviderKind::Openai => "openai",
                ProviderKind::Anthropic => "anthropic",
                ProviderKind::Gemini => "gemini",
                ProviderKind::Bedrock => "bedrock",
                ProviderKind::Cohere => "cohere",
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
    let mut base = Base::new(&st);
    if let Some(name) = &query.added
        && let Some(p) = providers.iter_mut().find(|p| &p.name == name)
    {
        // The card shows the success message with the next steps.
        p.added = true;
        p.added_message = base
            .flash
            .take()
            .unwrap_or_else(|| format!("Provider {name} added."));
    }
    let any_provider: bool = tryr!(
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM providers)")
            .fetch_one(&st.db)
            .await
    );
    let types = tryr!(provider_types(&st).await);
    let wanted = query.add.or(query.preset);
    let mut add_error = String::new();
    let mut add_html = String::new();
    if let Some(id) = wanted.as_deref().map(str::trim).filter(|id| !id.is_empty()) {
        match types.iter().find(|t| t.id == id) {
            Some(t) => {
                let form = tryr!(add_form(&st, t.clone(), None).await);
                add_html = tryr!(AddFormFragment { form }.render());
            }
            None => {
                add_error =
                    format!("No Provider type has the ID {id}. Search for the Provider below.")
            }
        }
    }
    let groups = crate::provider_types::groups(&types)
        .into_iter()
        .map(|(label, list)| (label, list.into_iter().cloned().collect()))
        .collect();
    render(ProvidersPage {
        pager,
        base,
        providers,
        groups,
        type_count: types.len(),
        add_open: wanted.is_some() || !any_provider,
        add_html,
        any_provider,
        add_error,
    })
}

#[derive(Deserialize)]
struct ProviderForm {
    #[serde(flatten)]
    limits: ProviderLimitsForm,
    default_model: Option<String>,
    priority: Option<String>,
    /// The guided form: "card" opens the new card, "signin" also starts
    /// the sign-in. Empty for the old form (back to the list).
    #[serde(default)]
    then: String,
    /// OAuth client settings that the guided form saves with a new Provider.
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    client_secret: String,
    #[serde(default)]
    tenant: String,
    /// A Vertex service-account JSON key for a new Provider.
    #[serde(default)]
    service_account: String,
    #[serde(default)]
    aws_region: String,
    #[serde(default)]
    aws_access_key: String,
    #[serde(default)]
    aws_secret_key: String,
    #[serde(default)]
    aws_session_token: String,
    #[serde(default)]
    aws_clear_token: bool,
    #[serde(default)]
    project_id: String,
    #[serde(default = "crate::config::default_region")]
    region: String,
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
        crate::config::validate_provider_env(env)?;
        Ok(key.into())
    } else {
        st.secret_key.encrypt(key)
    }
}

async fn create_provider(
    admin: Admin,
    State(st): State<AppState>,
    headers: HeaderMap,
    Form(f): Form<ProviderForm>,
) -> Response {
    let hx = headers.contains_key("hx-request");
    save_provider(admin, st, None, f, hx).await
}
async fn update_provider(
    admin: Admin,
    State(st): State<AppState>,
    Path(name): Path<String>,
    Form(f): Form<ProviderForm>,
) -> Response {
    save_provider(admin, st, Some(name), f, false).await
}
async fn save_provider(
    Admin(account): Admin,
    st: AppState,
    old: Option<String>,
    mut f: ProviderForm,
    hx: bool,
) -> Response {
    use crate::provider::ProviderAuth;
    if account.role != "owner" && f.api_key.trim().starts_with("env:") {
        return page_error(
            StatusCode::FORBIDDEN,
            "Only an Owner can change environment key references.",
        );
    }
    f.name = f.name.trim().into();
    f.base_url = f.base_url.trim().trim_end_matches('/').into();
    if f.auth == ProviderAuth::GoogleVertex {
        if let Err(e) = crate::vertex::validate_project(&f.project_id) {
            return form_error(e);
        }
        f.base_url = match crate::vertex::base_url(&f.region) {
            Ok(v) => v,
            Err(e) => return form_error(e),
        };
    }
    // A Subscription Provider has one fixed address and API format. Accept
    // the address with /v1, as the Provider documentation often shows it.
    match f.auth {
        ProviderAuth::ApiKey | ProviderAuth::GithubCopilot | ProviderAuth::KimiCode => {}
        ProviderAuth::Chatgpt
        | ProviderAuth::Codex
        | ProviderAuth::HuggingFace
        | ProviderAuth::Entra
        | ProviderAuth::QwenCode => f.kind = ProviderKind::Openai,
        ProviderAuth::Claude => f.kind = ProviderKind::Anthropic,
        ProviderAuth::GeminiCli | ProviderAuth::GoogleVertex => f.kind = ProviderKind::Gemini,
    }
    if !matches!(
        f.auth,
        ProviderAuth::ApiKey | ProviderAuth::QwenCode | ProviderAuth::KimiCode
    ) {
        f.base_url = f.base_url.trim_end_matches("/v1").into();
    }
    let guard = st.settings_write.lock().await;
    if account.role != "owner"
        && let Some(name) = &old
    {
        let previous: Option<(String, String, String, Option<String>, String)> = tryr!(
            sqlx::query_as(
                "SELECT keys_json,base_url,kind,key_header,auth FROM providers WHERE name=?"
            )
            .bind(name)
            .fetch_optional(&st.db)
            .await
        );
        if let Some((raw, base, kind, header, auth)) = previous {
            let keys: Vec<String> = tryr!(serde_json::from_str(&raw));
            if keys.iter().any(|k| k.starts_with("env:"))
                && (base != f.base_url
                    || kind != tryr!(serde_json::to_string(&f.kind))
                    || header.as_deref() != f.key_header.map(|h| h.as_str())
                    || auth != f.auth.as_str())
            {
                return page_error(
                    StatusCode::FORBIDDEN,
                    "Only an Owner can change the address, format or authentication of an environment-backed Provider.",
                );
            }
        }
    }
    let added = old.is_none();
    let aws_credentials = if f.kind == ProviderKind::Bedrock {
        if f.auth != ProviderAuth::ApiKey || !f.api_key.is_empty() {
            return form_error("Native Bedrock uses AWS credentials only.");
        }
        let host = match crate::sigv4::host(&f.aws_region) {
            Ok(h) => h,
            Err(e) => return form_error(e),
        };
        f.base_url = format!("https://{host}");
        let existing = if let Some(name) = &old {
            store::bedrock_credentials(&st.db, &st.secret_key, name)
                .await
                .ok()
        } else {
            None
        };
        let access = if f.aws_access_key.is_empty() {
            existing
                .as_ref()
                .map(|c| c.access_key.clone())
                .unwrap_or_default()
        } else {
            f.aws_access_key.clone()
        };
        let secret = if f.aws_secret_key.is_empty() {
            existing
                .as_ref()
                .map(|c| c.secret_key.clone())
                .unwrap_or_default()
        } else {
            f.aws_secret_key.clone()
        };
        let token = if f.aws_clear_token {
            None
        } else if f.aws_session_token.is_empty() {
            existing.and_then(|c| c.session_token)
        } else {
            Some(f.aws_session_token.clone())
        };
        if access.is_empty()
            || secret.is_empty()
            || [Some(&access), Some(&secret), token.as_ref()]
                .into_iter()
                .flatten()
                .any(|s| s.len() > 16384 || s.chars().any(char::is_control))
        {
            return form_error("Enter valid AWS access and secret keys.");
        }
        Some(tryr!(st.secret_key.encrypt(&tryr!(serde_json::to_string(
            &crate::sigv4::Credentials::new(&access, &secret, token)
        )))))
    } else {
        None
    };
    let mut cfg = tryr!(runtime(&st).await);
    let previous = old
        .as_ref()
        .and_then(|name| cfg.providers.iter().find(|p| &p.name == name));
    let default_model = f
        .default_model
        .as_deref()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            f.default_model
                .is_none()
                .then(|| previous.and_then(|p| p.default_model.clone()))
                .flatten()
        });
    if default_model
        .as_ref()
        .is_some_and(|s| s.len() > 200 || s.chars().any(char::is_control))
    {
        return form_error(
            "Default model must have at most 200 characters and no control characters.",
        );
    }
    let priority = match f.priority.as_deref().map(str::trim) {
        Some("") => None,
        Some(n) => Some(match n.parse::<i16>() {
            Ok(n) => n,
            Err(_) => return form_error("Priority must be a whole number from -32768 to 32767."),
        }),
        None => previous.and_then(|p| p.priority),
    };
    let previous_limits = cfg
        .providers
        .iter()
        .find(|p| Some(&p.name) == old.as_ref())
        .map(|p| p.limits.clone())
        .unwrap_or_default();
    let limits = match f.limits.parse(&previous_limits) {
        Ok(limits) => limits,
        Err(e) => return form_error(e),
    };
    if let Some(name) = &old {
        cfg.providers.retain(|p| &p.name != name);
    }
    cfg.providers.push(ProviderCfg {
        limits: limits.clone(),
        default_model: default_model.clone(),
        priority,
        name: f.name.clone(),
        key_strategy: crate::config::KeyStrategy::default(),
        kind: f.kind,
        auth: f.auth,
        base_url: f.base_url.clone(),
        project_id: f.project_id.clone(),
        region: f.region.clone(),
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
    if f.kind == ProviderKind::Bedrock {
        keys.clear();
    }
    let mut tx = tryr!(db::begin_write(&st.db, "provider_settings").await);
    let result = if let Some(name) = old {
        sqlx::query("UPDATE providers SET name=?,kind=?,base_url=?,keys_json=?,auth=?,include_usage=?,key_header=?,project_id=?,region=? WHERE name=?")
            .bind(&f.name)
            .bind(tryr!(serde_json::to_string(&f.kind)))
            .bind(f.base_url.trim_end_matches('/'))
            .bind(tryr!(serde_json::to_string(&keys)))
            .bind(f.auth.as_str())
            .bind(f.include_usage)
            .bind(f.key_header.map(|h| h.as_str()))
            .bind(&f.project_id)
            .bind(&f.region)
            .bind(name)
            .execute(&mut *tx)
            .await
    } else {
        sqlx::query("INSERT INTO providers(name,kind,base_url,keys_json,auth,include_usage,key_header,project_id,region) VALUES(?,?,?,?,?,?,?,?,?)")
            .bind(&f.name)
            .bind(tryr!(serde_json::to_string(&f.kind)))
            .bind(f.base_url.trim_end_matches('/'))
            .bind(tryr!(serde_json::to_string(&keys)))
            .bind(f.auth.as_str())
            .bind(f.include_usage)
            .bind(f.key_header.map(|h| h.as_str()))
            .bind(&f.project_id)
            .bind(&f.region)
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
    tryr!(
        sqlx::query("UPDATE providers SET default_model=?,priority=?,limits_json=? WHERE name=?")
            .bind(default_model)
            .bind(priority)
            .bind(tryr!(serde_json::to_string(&limits)))
            .bind(&f.name)
            .execute(&mut *tx)
            .await
    );
    tryr!(
        sqlx::query("UPDATE providers SET bedrock_credentials=? WHERE name=?")
            .bind(aws_credentials)
            .bind(&f.name)
            .execute(&mut *tx)
            .await
    );
    tryr!(tx.commit().await);
    tryr!(st.reload_settings().await);
    if !added {
        if !f.api_key.trim().is_empty() {
            tryr!(recover_key(&st, &f.name, f.api_key.trim()).await);
        }
        if f.kind == ProviderKind::Bedrock
            && (!f.aws_access_key.is_empty()
                || !f.aws_secret_key.is_empty()
                || !f.aws_session_token.is_empty())
        {
            tryr!(crate::quota::recover_credentials(&st.db, &f.name, "default").await);
            crate::quota::emit_health_events(&st).await;
        }
        return saved("/admin/providers", &format!("Provider {} saved.", f.name));
    }
    drop(_oauth_guard);
    drop(guard);
    after_add(st, f, hx).await
}

/// Saves the credentials of the guided form, then opens the new card or
/// starts the sign-in. A refused credential removes the new Provider again.
async fn after_add(st: AppState, f: ProviderForm, hx: bool) -> Response {
    use crate::oauth::Flow;
    use crate::provider::ProviderAuth;
    let name = f.name.clone();
    let flow = Flow::from_auth(f.auth);
    let account = f.service_account.trim();
    let result = if f.auth == ProviderAuth::GoogleVertex && !account.is_empty() {
        st.oauth
            .configure_service_account(&name, account)
            .await
            .map_err(|_| "Check the service-account JSON key and its Google Cloud permissions.")
    } else if let Some(flow) = flow.filter(|_| {
        !f.client_id.trim().is_empty()
            || (f.auth == ProviderAuth::GithubCopilot && !f.tenant.trim().is_empty())
    }) {
        st.oauth
            .configure(
                &name,
                flow,
                crate::oauth::ClientOptions {
                    client_id: f.client_id.trim().into(),
                    client_secret: f.client_secret.trim().into(),
                    tenant: f.tenant.trim().into(),
                    scope: String::new(),
                },
            )
            .await
            .map_err(|_| "Check the client ID, secret and tenant settings. For CIMD, use the public HTTPS metadata URL with no secret.")
    } else {
        Ok(())
    };
    if let Err(message) = result {
        let _guard = st.settings_write.lock().await;
        tryr!(
            sqlx::query("DELETE FROM providers WHERE name=?")
                .bind(&name)
                .execute(&st.db)
                .await
        );
        tryr!(st.reload_settings().await);
        return form_error(format!("{message} The Provider was not added."));
    }
    let card = format!("/admin/providers?added={name}#p-{name}");
    let signin = f.then == "signin" && flow.is_some() && account.is_empty();
    if signin && hx {
        let panel = super::oauth::start_panel(st, name.clone()).await;
        if panel.status() == StatusCode::OK {
            let body = tryr!(axum::body::to_bytes(panel.into_body(), 1 << 20).await);
            return render(AddedSignin {
                name,
                panel: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        return saved(
            &card,
            &format!("Provider {name} added. Sign-in did not start. Select Sign in to try again."),
        );
    }
    match f.then.as_str() {
        "signin" if signin => saved(
            &card,
            &format!("Provider {name} added. Select Sign in to connect it."),
        ),
        "signin" | "card" => saved(&card, &format!("Provider {name} added.")),
        _ => saved("/admin/providers", &format!("Provider {name} added.")),
    }
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
    Admin(account): Admin,
    State(st): State<AppState>,
    Path(name): Path<String>,
    Form(f): Form<KeyForm>,
) -> Response {
    if account.role != "owner" && f.api_key.trim().starts_with("env:") {
        return page_error(
            StatusCode::FORBIDDEN,
            "Only an Owner can change environment key references.",
        );
    }
    if f.api_key.trim().is_empty() {
        return form_error("Enter a Provider Key or env:NAME.");
    }
    let encoded = match encode_key(&st, f.api_key.trim()) {
        Ok(v) => v,
        Err(e) => return form_error(e),
    };
    change_keys(st, name, None, Some(encoded), account.role == "owner").await
}
async fn delete_key(
    Admin(account): Admin,
    State(st): State<AppState>,
    Path((name, index)): Path<(String, usize)>,
) -> Response {
    change_keys(st, name, Some(index), None, account.role == "owner").await
}
async fn change_keys(
    st: AppState,
    name: String,
    remove: Option<usize>,
    add: Option<String>,
    owner: bool,
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
        if !owner && keys[i].starts_with("env:") {
            return page_error(
                StatusCode::FORBIDDEN,
                "Only an Owner can change environment key references.",
            );
        }
        keys.remove(i);
    }
    let recovery = add
        .as_ref()
        .map(|key| {
            if let Some(env) = key.strip_prefix("env:") {
                Ok(env_value(env))
            } else {
                st.secret_key.decrypt(key).map(Some)
            }
        })
        .transpose();
    let recovery = tryr!(recovery).flatten();
    if let Some(key) = add {
        keys.push(key);
    }
    tryr!(
        sqlx::query("UPDATE providers SET keys_json=? WHERE name=?")
            .bind(tryr!(serde_json::to_string(&keys)))
            .bind(&name)
            .execute(&st.db)
            .await
    );
    tryr!(st.reload_settings().await);
    if let Some(key) = recovery {
        tryr!(recover_key(&st, &name, &key).await);
    }
    let message = if remove.is_some() {
        "Provider Key removed."
    } else {
        "Provider Key added."
    };
    saved("/admin/providers", message)
}

async fn recover_key(st: &AppState, name: &str, key: &str) -> anyhow::Result<()> {
    let plain = key
        .strip_prefix("env:")
        .map_or_else(|| Some(key.to_owned()), env_value);
    if let Some(plain) = plain {
        crate::quota::recover_credentials(&st.db, name, &crate::quota::key_id(&plain)).await?;
    }
    crate::quota::emit_health_events(st).await;
    Ok(())
}

#[derive(Deserialize)]
struct KeyStrategyForm {
    strategy: crate::config::KeyStrategy,
}
async fn key_strategy(
    _: Admin,
    State(st): State<AppState>,
    Path(name): Path<String>,
    Form(f): Form<KeyStrategyForm>,
) -> Response {
    let _guard = st.settings_write.lock().await;
    let strategy = tryr!(serde_json::to_value(f.strategy));
    tryr!(
        sqlx::query("UPDATE providers SET key_strategy=? WHERE name=?")
            .bind(strategy.as_str())
            .bind(name)
            .execute(&st.db)
            .await
    );
    tryr!(st.reload_settings().await);
    saved("/admin/providers", "Key Pool strategy saved.")
}
async fn reenable_account(
    _: Admin,
    State(st): State<AppState>,
    Path((name, account)): Path<(String, String)>,
) -> Response {
    let _guard = st.settings_write.lock().await;
    let exists: bool = tryr!(
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM provider_health WHERE provider=? AND account=?)"
        )
        .bind(&name)
        .bind(&account)
        .fetch_one(&st.db)
        .await
    );
    if !exists {
        return form_error("Account not found. Reload the page.");
    }
    tryr!(crate::quota::reenable(&st, &name, &account).await);
    st.circuits
        .lock()
        .expect("circuits")
        .retain(|(provider, _), _| provider != &name);
    saved(
        "/admin/providers",
        "Account re-enabled. The next request tests access.",
    )
}

#[derive(Deserialize)]
struct EgressForm {
    mode: String,
    #[serde(default)]
    proxy_url: String,
}

async fn save_global_egress(
    _: Admin,
    State(st): State<AppState>,
    Form(f): Form<EgressForm>,
) -> Response {
    let _guard = st.settings_write.lock().await;
    let proxy = match f.mode.as_str() {
        "none" => Ok(None),
        "custom" if f.proxy_url.is_empty() => tryr!(store::value(&st.db, "egress_proxy").await)
            .map(|v| st.secret_key.decrypt(&v))
            .transpose(),
        "custom" => Ok(Some(f.proxy_url)),
        _ => return form_error("Choose None or Custom proxy."),
    };
    let proxy = tryr!(proxy);
    if f.mode == "custom" && proxy.is_none() {
        return form_error("Enter a proxy URL.");
    }
    if let Some(value) = &proxy
        && let Err(e) = crate::egress::ProxyUrl::parse(value)
    {
        return form_error(e);
    }
    let policy = match crate::egress::Policy::prepare(
        &st.db,
        &st.secret_key,
        st.settings().upstream_timeout_secs,
        Some(proxy.clone()),
        None,
    )
    .await
    {
        Ok(policy) => policy,
        Err(e) => return form_error(e),
    };
    if let Some(proxy) = proxy {
        tryr!(
            store::put(
                &st.db,
                "egress_proxy",
                &tryr!(st.secret_key.encrypt(&proxy))
            )
            .await
        );
    } else {
        tryr!(
            sqlx::query("DELETE FROM settings WHERE key='egress_proxy'")
                .execute(&st.db)
                .await
        );
    }
    st.egress.replace(policy);
    st.set_router_table((*st.router_table()).clone());
    saved("/admin/settings#egress", "Global egress proxy saved.")
}

async fn save_provider_egress(
    _: Admin,
    State(st): State<AppState>,
    Path(name): Path<String>,
    Form(f): Form<EgressForm>,
) -> Response {
    let _guard = st.settings_write.lock().await;
    let old: Option<Option<String>> = tryr!(
        sqlx::query_scalar("SELECT proxy_url FROM providers WHERE name=?")
            .bind(&name)
            .fetch_optional(&st.db)
            .await
    );
    let Some(old) = old else {
        return form_error("Provider not found.");
    };
    let proxy = match f.mode.as_str() {
        "global" | "none" => None,
        "custom" if f.proxy_url.is_empty() => {
            tryr!(old.map(|v| st.secret_key.decrypt(&v)).transpose())
        }
        "custom" => Some(f.proxy_url),
        _ => return form_error("Choose Use global, None or Custom proxy."),
    };
    if f.mode == "custom" && proxy.is_none() {
        return form_error("Enter a proxy URL.");
    }
    if let Some(value) = &proxy
        && let Err(e) = crate::egress::ProxyUrl::parse(value)
    {
        return form_error(e);
    }
    let policy = match crate::egress::Policy::prepare(
        &st.db,
        &st.secret_key,
        st.settings().upstream_timeout_secs,
        None,
        Some((&name, &f.mode, proxy.clone())),
    )
    .await
    {
        Ok(policy) => policy,
        Err(e) => return form_error(e),
    };
    let secret = tryr!(proxy.map(|v| st.secret_key.encrypt(&v)).transpose());
    tryr!(
        sqlx::query("UPDATE providers SET proxy_mode=?,proxy_url=? WHERE name=?")
            .bind(&f.mode)
            .bind(secret)
            .bind(&name)
            .execute(&st.db)
            .await
    );
    st.egress.replace(policy);
    st.set_router_table((*st.router_table()).clone());
    saved("/admin/providers", "Provider egress proxy saved.")
}

async fn test_provider(_: Admin, State(st): State<AppState>, Path(name): Path<String>) -> Response {
    let table = st.router_table();
    let Some(p) = table.providers.iter().find(|p| p.name == name) else {
        return form_error("Provider not found.");
    };
    if matches!(p.kind, ProviderKind::Bedrock | ProviderKind::Cohere) {
        return form_error(
            "This API format has no model-list connection test. Send a model request instead.",
        );
    }
    let url = crate::provider::upstream_url(&p.base_url, p.kind.models_path());
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
    let Ok(http) = st.snapshot_http(p).await else {
        return form_error("Provider egress policy unavailable.");
    };
    let req = http.get(url).timeout(std::time::Duration::from_secs(15));
    let req = p.key_header.apply(req, key);
    let req = if p.kind == ProviderKind::Anthropic {
        req.header("anthropic-version", "2023-06-01")
    } else {
        req
    };
    let (class, text) = match req.send().await {
        Err(e) if st.egress.connect_error(&p.name, &e) => (
            "bad",
            "Cannot connect to the egress proxy. Check its address, credentials and network."
                .to_owned(),
        ),
        Err(_) => (
            "bad",
            "Cannot reach the Provider. Check the base URL and the network.".to_owned(),
        ),
        Ok(r) if r.status().as_u16() == 407 && st.egress.label(Some(&p.name)).is_some() => {
            metrics::counter!("tokomotive_proxy_connect_errors_total", "provider" => p.name.clone()).increment(1);
            (
                "bad",
                "The egress proxy refused authentication. Check its credentials.".to_owned(),
            )
        }
        Ok(r) => test_message(r.status().as_u16()),
    };
    let via = st
        .egress
        .label(Some(&p.name))
        .map(|label| {
            format!(
                " via proxy {}",
                label.split_once("://").expect("proxy scheme").1
            )
        })
        .unwrap_or_else(|| " via direct connection".into());
    Html(format!(
        "<p><span class=\"tag {class}\">{}</span> {}{}</p>",
        match class {
            "good" => "OK",
            "warn" => "Warning",
            _ => "Failed",
        },
        esc(&text),
        esc(&via)
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
    if tryr!(crate::admin::configured(&st.db).await) {
        return setup_done();
    }
    render(SetupPage {
        base: Base::anonymous(),
        mode: st.settings().mode.clone(),
    })
}

#[derive(Deserialize)]
struct SetupForm {
    #[serde(default)]
    username: String,
    password: String,
    /// Optional, so that scripted Setup with one field keeps working.
    password_confirm: Option<String>,
    mode: Option<String>,
}

async fn setup(State(st): State<AppState>, Form(f): Form<SetupForm>) -> Response {
    let _guard = st.settings_write.lock().await;
    if tryr!(crate::admin::configured(&st.db).await) {
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
    let username = tryform!(crate::admin::username(&f.username));
    let mut tx = tryr!(db::begin_write(&st.db, "setup").await);
    let result=tryr!(
        sqlx::query("INSERT INTO admin_accounts(username,password_hash,role) SELECT ?,?,'owner' WHERE NOT EXISTS(SELECT 1 FROM admin_accounts)")
            .bind(username)
            .bind(&hash)
            .execute(&mut *tx)
            .await
    );
    if result.rows_affected() == 0 {
        return setup_done();
    }
    if username.eq_ignore_ascii_case("admin") {
        tryr!(
            sqlx::query("INSERT INTO settings(key,value) VALUES('admin_password_hash',?)")
                .bind(&hash)
                .execute(&mut *tx)
                .await
        );
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
    let account = tryr!(crate::admin::by_username(&st.db, username).await).expect("setup account");
    let session = tryr!(start_session(&st, &account).await);
    let mut r = saved(
        "/admin",
        "Setup is complete. Follow the steps below to connect your first client.",
    );
    r.headers_mut()
        .append(header::SET_COOKIE, tryr!(HeaderValue::from_str(&session)));
    r.extensions_mut().insert(account);
    r
}

#[cfg(test)]
mod key_mask_tests {
    use super::last4;

    /// A short key must never be shown in full: the mask shows a tail only
    /// when at least 8 characters stay hidden.
    #[test]
    fn short_keys_are_fully_masked() {
        assert_eq!(last4("abc"), "****");
        assert_eq!(last4("abcd"), "****");
        assert_eq!(last4("abcdefghijk"), "****");
        assert_eq!(last4("sk-abcdefghij1234"), "****1234");
    }
}
