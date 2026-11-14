//! Operator controls for model data, independent of manual card forms.
use super::*;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/settings/models-dev", post(save_sync))
        .route("/admin/settings/models-dev/refresh", post(refresh))
        .route("/admin/models/{id}/models-dev", post(save_mapping))
}

#[derive(Deserialize)]
struct SyncForm {
    url: String,
    enabled: Option<String>,
}
async fn save_sync(_: Admin, State(st): State<AppState>, Form(f): Form<SyncForm>) -> Response {
    let _guard = st.settings_write.lock().await;
    let url = match reqwest::Url::parse(f.url.trim()) {
        Ok(url)
            if matches!(url.scheme(), "http" | "https")
                && url.username().is_empty()
                && url.password().is_none() =>
        {
            url
        }
        _ => return bad("Use an HTTP or HTTPS model data URL without credentials."),
    };
    let mut tx = tryr!(crate::db::begin_write(&st.db, "models_dev_settings").await);
    // ETags are scoped to one URL. Do not send an old server's ETag to another.
    tryr!(sqlx::query("DELETE FROM settings WHERE key='models_dev_etag' AND (SELECT value FROM settings WHERE key='models_dev_url')!=?").bind(url.as_str()).execute(&mut *tx).await);
    for (key, value) in [
        ("models_dev_url", url.as_str()),
        (
            "models_dev_enabled",
            if f.enabled.is_some() { "true" } else { "false" },
        ),
    ] {
        tryr!(
            sqlx::query("UPDATE settings SET value=? WHERE key=?")
                .bind(value)
                .bind(key)
                .execute(&mut *tx)
                .await
        );
    }
    tryr!(tx.commit().await);
    saved("/admin/settings#models-dev", "Model data settings saved.")
}

async fn refresh(_: Admin, State(st): State<AppState>) -> Response {
    match crate::models_dev::refresh(&st).await {
        Ok(()) => saved(
            "/admin/settings#models-dev",
            "Model data refreshed. Unpinned Cost Cards are up to date.",
        ),
        Err(e) => form_error(format!(
            "Model data refresh failed. Old data is kept. {e:#}"
        )),
    }
}

#[derive(Deserialize)]
struct MappingForm {
    #[serde(default)]
    models_dev_provider: String,
    #[serde(default)]
    models_dev_model: String,
    cost_pinned: Option<String>,
}

async fn save_mapping(
    _: Admin,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<MappingForm>,
) -> Response {
    let _guard = st.settings_write.lock().await;
    let provider = f.models_dev_provider.trim();
    let model = f.models_dev_model.trim();
    if !model.is_empty() && provider.is_empty() {
        return bad("Set a models.dev Provider for a model override.");
    }
    if !provider.is_empty() {
        let exists: bool = tryr!(
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM models_dev_providers WHERE id=?)")
                .bind(provider)
                .fetch_one(&st.db)
                .await
        );
        if !exists {
            return bad("This Provider is not in models.dev.");
        }
    }
    if !model.is_empty() {
        let exists: bool = tryr!(
            sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM models_dev_models WHERE provider=? AND id=?)"
            )
            .bind(provider)
            .bind(model)
            .fetch_one(&st.db)
            .await
        );
        if !exists {
            return bad("This Model is not in the selected models.dev Provider.");
        }
    }
    let result = tryr!(
        sqlx::query(
            "UPDATE models SET models_dev_provider=?,models_dev_model=?,cost_pinned=? WHERE id=?"
        )
        .bind((!provider.is_empty()).then_some(provider))
        .bind((!model.is_empty()).then_some(model))
        .bind(f.cost_pinned.is_some())
        .bind(id)
        .execute(&st.db)
        .await
    );
    if result.rows_affected() == 0 {
        return bad("Model not found.");
    }
    saved(
        &format!("/admin/models/{id}#models-dev"),
        "Model data mapping and pin saved. The next sync applies mapped prices to unpinned cards.",
    )
}

#[derive(Default)]
pub(super) struct DetailView {
    pub provider_override: String,
    pub model_override: String,
    pub pinned: bool,
    pub mapped: Option<crate::models_dev::Details>,
    pub rows: Vec<(String, String)>,
}

pub(super) async fn detail_view(st: &AppState, id: i64) -> anyhow::Result<DetailView> {
    use sqlx::Row;
    let row = sqlx::query(
        "SELECT models_dev_provider,models_dev_model,cost_pinned FROM models WHERE id=?",
    )
    .bind(id)
    .fetch_one(&st.db)
    .await?;
    let mapped = crate::models_dev::details(&st.db, id).await?;
    let mut rows = Vec::new();
    if let Some(d) = &mapped {
        let data = &d.data;
        let limit = |n: Option<i64>| n.map(int).unwrap_or_else(|| "Not listed".into());
        let yes = |b| if b { "Yes" } else { "No" }.to_owned();
        let text = |s: &str| {
            if s.is_empty() {
                "Not listed".into()
            } else {
                s.to_owned()
            }
        };
        rows = vec![
            ("Context limit".into(), limit(data.limit.context)),
            ("Input limit".into(), limit(data.limit.input)),
            ("Output limit".into(), limit(data.limit.output)),
            (
                "Input modalities".into(),
                data.modalities["input"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_else(|| "Not listed".into()),
            ),
            (
                "Output modalities".into(),
                data.modalities["output"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_else(|| "Not listed".into()),
            ),
            ("Reasoning".into(), yes(data.reasoning)),
            ("Tool calls".into(), yes(data.tool_call)),
            ("Structured output".into(), yes(data.structured_output)),
            ("Knowledge cutoff".into(), text(&data.knowledge)),
            ("Release date".into(), text(&data.release_date)),
            ("Status".into(), text(&data.status)),
        ];
    }
    Ok(DetailView {
        provider_override: row
            .get::<Option<String>, _>("models_dev_provider")
            .unwrap_or_default(),
        model_override: row
            .get::<Option<String>, _>("models_dev_model")
            .unwrap_or_default(),
        pinned: row.get("cost_pinned"),
        mapped,
        rows,
    })
}

pub struct AlertView {
    pub model_id: i64,
    pub name: String,
    pub old: Vec<(String, String)>,
    pub new: Vec<(String, String)>,
}
pub(super) async fn alerts(st: &AppState) -> anyhow::Result<Vec<AlertView>> {
    use sqlx::Row;
    let mut alerts = Vec::new();
    for row in sqlx::query("SELECT a.model_id,m.name,a.old_spec,a.new_spec FROM models_dev_alerts a JOIN models m ON m.id=a.model_id ORDER BY a.id DESC LIMIT 20").fetch_all(&st.db).await? {
        let describe=|spec:&str| -> anyhow::Result<Vec<(String,String)>> {
            let card:CardRates=serde_json::from_str(spec)?;
            let mut out=vec![("Base".into(),serde_json::to_string(&card.base)?)];
            out.extend(card.tiers.iter().map(|t|(format!("Above {} input tokens",int(t.above_input_tokens)),serde_json::to_string(&t.rates).unwrap_or_default())));
            // Display exact dollars, all Token Classes, and all Context Tiers.
            for (_,rates) in &mut out {
                let r:crate::pricing::Rates=serde_json::from_str(rates)?;
                *rates=format!("Input ${}; Output ${}; Cache read ${}; Cache write 5m ${}; Cache write 1h ${}; Image input ${}; Image output ${}",crate::pricing::fmt_rate(r.input),crate::pricing::fmt_rate(r.output),crate::pricing::fmt_rate(r.cache_read),crate::pricing::fmt_rate(r.cache_write_5m),crate::pricing::fmt_rate(r.cache_write_1h),crate::pricing::fmt_rate(r.image_input),crate::pricing::fmt_rate(r.image_output));
            }
            Ok(out)
        };
        alerts.push(AlertView{model_id:row.get("model_id"),name:row.get("name"),old:describe(row.get("old_spec"))?,new:describe(row.get("new_spec"))?});
    }
    Ok(alerts)
}
