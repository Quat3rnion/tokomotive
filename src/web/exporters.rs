//! Exporter forms never return stored credentials to the browser.
use super::*;
use crate::exporters::Exporter;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/settings/exporters", post(save))
        .route("/admin/settings/exporters/{id}/delete", post(delete))
        .route("/admin/settings/exporters/{id}/test", post(test))
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct ExporterForm {
    id: Option<i64>,
    name: String,
    kind: String,
    site: String,
    endpoint: String,
    repo_id: String,
    api_key: String,
    spans: String,
    metrics: String,
    enabled: String,
    sample_rate: Option<f64>,
    include_bodies: String,
    tag_allowlist: String,
    batch_size: Option<i64>,
    interval_ms: Option<i64>,
    allow_private: String,
}
async fn save(
    Admin(account): Admin,
    State(st): State<AppState>,
    Form(f): Form<ExporterForm>,
) -> Response {
    let _guard = st.settings_write.lock().await;
    if f.name.trim().is_empty() || f.name.len() > 100 || f.name.chars().any(char::is_control) {
        return form_error("Use a name with 1 to 100 bytes and no control characters.");
    }
    if !matches!(f.kind.as_str(), "datadog" | "maxim") {
        return form_error("Select a listed exporter type.");
    }
    if f.api_key.len() > 65536 || f.api_key.chars().any(char::is_control) {
        return form_error("Use an API key with at most 65536 bytes and no control characters.");
    }
    if f.id.is_none()
        && tryr!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM exporters")
                .fetch_one(&st.db)
                .await
        ) >= 16
    {
        return form_error("At most 16 exporters can be stored. Delete an unused exporter first.");
    }
    let old: Option<Exporter> = tryr!(
        sqlx::query_as("SELECT * FROM exporters WHERE id=?")
            .bind(f.id)
            .fetch_optional(&st.db)
            .await
    );
    if f.id.is_some() && old.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if account.role != "owner"
        && (crate::secrets::is_reference(f.api_key.trim())
            || old.as_ref().is_some_and(|e| {
                e.reference_backed()
                    && (!f.api_key.is_empty()
                        || e.kind != f.kind
                        || e.site != f.site
                        || e.endpoint != f.endpoint.trim().trim_end_matches('/')
                        || e.repo_id != f.repo_id.trim()
                        || e.allow_private != !f.allow_private.is_empty()
                        || e.include_bodies != !f.include_bodies.is_empty())
            }))
    {
        return page_error(
            StatusCode::FORBIDDEN,
            "Only an Owner can change key references or their destination and body permissions.",
        );
    }
    let api_key = if f.api_key.is_empty() {
        match &old {
            Some(e) => e.api_key.clone(),
            None => return form_error("Set an API key or a key reference."),
        }
    } else {
        match crate::settings::encode_provider_key(&st.secret_key, &f.api_key) {
            Ok(key) => key,
            Err(_) => return form_error("Use a valid key or key reference."),
        }
    };
    let e = Exporter {
        id: f.id.unwrap_or(0),
        name: f.name.trim().into(),
        kind: f.kind,
        site: f.site,
        endpoint: f.endpoint.trim().trim_end_matches('/').into(),
        repo_id: f.repo_id.trim().into(),
        api_key,
        spans: !f.spans.is_empty(),
        metrics: !f.metrics.is_empty(),
        enabled: !f.enabled.is_empty(),
        sample_rate: f.sample_rate.unwrap_or(1.0),
        include_bodies: !f.include_bodies.is_empty(),
        tag_allowlist: f
            .tag_allowlist
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(","),
        batch_size: f.batch_size.unwrap_or(16),
        interval_ms: f.interval_ms.unwrap_or(1000),
        allow_private: !f.allow_private.is_empty(),
    };
    if let Err(error) = e.validate() {
        return form_error(error);
    }
    let url = match e.base_url().and_then(|s| Ok(reqwest::Url::parse(&s)?)) {
        Ok(url) => url,
        Err(_) => return form_error("Select a listed site and a valid endpoint."),
    };
    if crate::secrets::adapters::webhook_client(&url, e.allow_private, &st.egress)
        .await
        .is_err()
    {
        return form_error("URL denied. Use HTTPS; private HTTP needs explicit permission.");
    }
    let result = sqlx::query("INSERT INTO exporters(id,name,kind,site,endpoint,repo_id,api_key,spans,metrics,enabled,sample_rate,include_bodies,tag_allowlist,batch_size,interval_ms,allow_private) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET name=excluded.name,kind=excluded.kind,site=excluded.site,endpoint=excluded.endpoint,repo_id=excluded.repo_id,api_key=excluded.api_key,spans=excluded.spans,metrics=excluded.metrics,enabled=excluded.enabled,sample_rate=excluded.sample_rate,include_bodies=excluded.include_bodies,tag_allowlist=excluded.tag_allowlist,batch_size=excluded.batch_size,interval_ms=excluded.interval_ms,allow_private=excluded.allow_private")
        .bind(f.id).bind(&e.name).bind(&e.kind).bind(&e.site).bind(&e.endpoint).bind(&e.repo_id).bind(&e.api_key)
        .bind(e.spans).bind(e.metrics).bind(e.enabled).bind(e.sample_rate).bind(e.include_bodies).bind(&e.tag_allowlist)
        .bind(e.batch_size).bind(e.interval_ms).bind(e.allow_private).execute(&st.db).await;
    if result.is_err() {
        return form_error("Cannot save exporter. Use a unique name and valid limits.");
    }
    tryr!(st.exporters.reload(&st.db).await);
    saved("/admin/settings#exporters", "Exporter saved.")
}
async fn delete(_: Admin, State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    let _guard = st.settings_write.lock().await;
    tryr!(
        sqlx::query("DELETE FROM exporters WHERE id=?")
            .bind(id)
            .execute(&st.db)
            .await
    );
    tryr!(st.exporters.reload(&st.db).await);
    saved("/admin/settings#exporters", "Exporter deleted.")
}
async fn test(_: Admin, State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    // Never accept destination or credential fields from an unsaved form.
    let exporter: Exporter = match tryr!(
        sqlx::query_as("SELECT * FROM exporters WHERE id=?")
            .bind(id)
            .fetch_optional(&st.db)
            .await
    ) {
        Some(e) => e,
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    axum::Json(crate::exporters::test(&st, &exporter).await).into_response()
}
