//! Owner-only secret-store administration. No resolved values enter templates.
use super::*;
use crate::secrets::{self, Store};
mod form;
use form::{StoreForm, StoreView};

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/settings/secrets", get(page).post(save))
        .route("/admin/settings/secrets/{name}/delete", post(delete))
        .route("/admin/secrets/stores", get(store_choices))
        .route("/admin/secrets/browse", get(browse))
        .route("/admin/secrets/fields", get(fields))
        .route("/admin/secrets/test", post(test))
}
#[derive(Template)]
#[template(path = "secret_stores.html")]
struct Page {
    base: Base,
    stores: Vec<StoreView>,
    new_store: StoreView,
}
async fn page(_: Admin, State(st): State<AppState>) -> Response {
    render(Page {
        base: Base::new(&st),
        stores: tryr!(
            tryr!(secrets::stores(&st.db).await)
                .into_iter()
                .map(|s| StoreView::from_store(s, &st.secret_key))
                .collect::<anyhow::Result<Vec<_>>>()
        ),
        new_store: StoreView::new(),
    })
}
async fn save(_: Admin, State(st): State<AppState>, Form(f): Form<StoreForm>) -> Response {
    if f.name.is_empty()
        || f.name.len() > 64
        || !f
            .name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        || !matches!(
            f.kind.as_str(),
            "vault" | "aws-sm" | "gcp-sm" | "bws" | "bw"
        )
    {
        return form_error(
            "Use a store name with 1 to 64 letters, digits, underscores or hyphens, and choose a type.",
        );
    }
    let _guard = st.settings_write.lock().await;
    let previous = tryr!(secrets::stores(&st.db).await);
    let old = previous.iter().find(|s| s.name == f.name);
    if old.is_none() && previous.len() >= 64 {
        return form_error("At most 64 secret stores are allowed.");
    }
    let store = tryform!(f.prepare(old, &st.secret_key));
    if !(1..=86400).contains(&store.refresh_secs) {
        return form_error("Refresh must be 1 to 86400 seconds.");
    }
    if let Err(e) = secrets::adapters::validate_address(&store, &st.egress).await {
        return form_error(e);
    }
    for option in ["identity_url", "token_url", "api_url"] {
        if let Some(address) = store.option(option) {
            let target = Store {
                address,
                ..store.clone()
            };
            if let Err(e) = secrets::adapters::validate_address(&target, &st.egress).await {
                return form_error(e);
            }
        }
    }
    let mut tx = tryr!(crate::db::begin_write(&st.db, "secret_store").await);
    if store.is_default {
        tryr!(
            sqlx::query("UPDATE secret_stores SET is_default=0 WHERE kind=?")
                .bind(&store.kind)
                .execute(&mut *tx)
                .await
        );
    }
    tryr!(sqlx::query("INSERT INTO secret_stores(name,kind,address,credentials,options,allow_private,is_default,refresh_secs) VALUES(?,?,?,?,?,?,?,?) ON CONFLICT(name) DO UPDATE SET kind=excluded.kind,address=excluded.address,credentials=excluded.credentials,options=excluded.options,allow_private=excluded.allow_private,is_default=excluded.is_default,refresh_secs=excluded.refresh_secs")
        .bind(&store.name).bind(&store.kind).bind(&store.address).bind(&store.credentials).bind(&store.options).bind(store.allow_private).bind(store.is_default).bind(store.refresh_secs).execute(&mut *tx).await);
    tryr!(tx.commit().await);
    tryr!(st.reload_settings().await);
    saved("/admin/settings/secrets", "Secret store saved.")
}
async fn delete(_: Admin, State(st): State<AppState>, Path(name): Path<String>) -> Response {
    let _guard = st.settings_write.lock().await;
    tryr!(
        sqlx::query("DELETE FROM secret_stores WHERE name=?")
            .bind(name)
            .execute(&st.db)
            .await
    );
    tryr!(st.reload_settings().await);
    saved("/admin/settings/secrets", "Secret store deleted.")
}

async fn store_choices(_: Admin, State(st): State<AppState>) -> Response {
    let stores = tryr!(secrets::stores(&st.db).await);
    axum::Json(
        stores
            .into_iter()
            .map(|s| serde_json::json!({"name":s.name,"kind":s.kind}))
            .collect::<Vec<_>>(),
    )
    .into_response()
}
#[derive(Deserialize)]
struct BrowseQuery {
    store: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    cursor: String,
    #[serde(default)]
    q: String,
}
async fn browse(_: Admin, State(st): State<AppState>, Query(q): Query<BrowseQuery>) -> Response {
    let stores = tryr!(secrets::stores(&st.db).await);
    let Some(store) = stores.iter().find(|s| s.name == q.store) else {
        return form_error("Choose a secret store.");
    };
    match secrets::browse(store, &st.secret_key, &st.egress, &q.path, &q.cursor, &q.q).await {
        Ok(page) => axum::Json(page).into_response(),
        Err(e) => form_error(e),
    }
}
#[derive(Deserialize)]
struct ReferenceForm {
    reference: String,
}
async fn fields(_: Admin, State(st): State<AppState>, Query(q): Query<ReferenceForm>) -> Response {
    let stores = tryr!(secrets::stores(&st.db).await);
    let r = tryform!(secrets::Reference::parse(&q.reference));
    let store = tryform!(r.select(&stores));
    match secrets::fields(store, &st.secret_key, &st.egress, &q.reference).await {
        Ok(fields) => axum::Json(fields).into_response(),
        Err(e) => form_error(e),
    }
}
async fn test(_: Admin, State(st): State<AppState>, Form(f): Form<ReferenceForm>) -> Response {
    let stores = tryr!(secrets::stores(&st.db).await);
    let r = tryform!(secrets::Reference::parse(&f.reference));
    let store = tryform!(r.select(&stores));
    match secrets::resolve(store, &st.secret_key, &st.egress, None, &f.reference).await {
        Ok(value) => {
            axum::Json(serde_json::json!({"masked":super::settings::last4(value.expose())}))
                .into_response()
        }
        Err(e) => form_error(e),
    }
}
