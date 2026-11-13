//! The Operator dashboard: server-rendered HTML with askama and htmx.
// Form helpers return a ready error page as the Err value.
#![allow(clippy::result_large_err)]

mod chart;
mod views;

use askama::Template;
use axum::Router;
use axum::extract::{Form, FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config::percent_to_bp;
use crate::db::{self, LedgerFilter};
use crate::pricing::{CardRates, ContextTier, PriceSpec, Rates, parse_usd};
use crate::state::AppState;
use views::*;

const COOKIE: &str = "llma_session";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin", get(overview))
        .route("/admin/login", get(login_page).post(login))
        .route("/admin/logout", post(logout))
        .route("/admin/assets/htmx.min.js", get(htmx))
        .route("/admin/users", get(users).post(create_user))
        .route("/admin/users/{id}", post(update_user))
        .route("/admin/teams", get(teams).post(create_team))
        .route("/admin/keys", get(keys).post(create_key))
        .route("/admin/keys/{id}/revoke", post(revoke_key))
        .route("/admin/models", get(models).post(create_model))
        .route("/admin/models/{id}", get(model_detail).post(update_model))
        .route("/admin/models/{id}/cost", post(set_cost))
        .route("/admin/models/{id}/price", post(set_price))
        .route("/admin/models/{id}/team-price", post(set_team_price))
        .route("/admin/settings", post(set_settings))
        .route("/admin/ledger", get(ledger))
        .route("/admin/reprice", post(reprice))
}

// -------------------------------------------------------------------- auth

/// Extractor: present only for a logged-in Operator.
pub struct Admin;

impl FromRequestParts<AppState> for Admin {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        if st.admin_password.is_none() {
            return Err(page_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "The dashboard is off. Set the admin password in the configuration.",
            ));
        }
        let ok = match session_cookie(&parts.headers) {
            Some(s) => db::session_valid(&st.db, &s).await.unwrap_or_else(|e| {
                tracing::error!(error = %e, "session lookup failed");
                false
            }),
            None => false,
        };
        if ok {
            Ok(Admin)
        } else {
            Err(Redirect::to("/admin/login").into_response())
        }
    }
}

fn session_cookie(h: &HeaderMap) -> Option<String> {
    h.get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE)
        .map(|(_, v)| v.to_owned())
}

pub fn ct_eq(a: &str, b: &str) -> bool {
    let (x, y) = (Sha256::digest(a.as_bytes()), Sha256::digest(b.as_bytes()));
    x.iter()
        .zip(y.iter())
        .fold(0u8, |acc, (p, q)| acc | (p ^ q))
        == 0
}

async fn login_page() -> Response {
    render(LoginPage { error: None })
}

#[derive(Deserialize)]
struct LoginForm {
    password: String,
}

async fn login(State(st): State<AppState>, Form(f): Form<LoginForm>) -> Response {
    let Some(pw) = &st.admin_password else {
        return page_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The dashboard is off. Set the admin password in the configuration.",
        );
    };
    if !ct_eq(pw, &f.password) {
        let mut r = render(LoginPage {
            error: Some("Incorrect password.".into()),
        });
        *r.status_mut() = StatusCode::UNAUTHORIZED;
        return r;
    }
    let mut b = [0u8; 32];
    rand::rng().fill_bytes(&mut b);
    let token = hex::encode(b);
    if let Err(e) = db::create_session(&st.db, &token, st.session_days).await {
        return internal(e);
    }
    let max_age = st.session_days * 86_400;
    (
        [(
            header::SET_COOKIE,
            format!("{COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/admin; Max-Age={max_age}"),
        )],
        Redirect::to("/admin"),
    )
        .into_response()
}

async fn logout(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(s) = session_cookie(&headers)
        && let Err(e) = db::delete_session(&st.db, &s).await
    {
        return internal(e);
    }
    (
        [(
            header::SET_COOKIE,
            format!("{COOKIE}=; HttpOnly; SameSite=Strict; Path=/admin; Max-Age=0"),
        )],
        Redirect::to("/admin/login"),
    )
        .into_response()
}

async fn htmx() -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("../../assets/htmx.min.js"),
    )
        .into_response()
}

// ----------------------------------------------------------------- helpers

fn render<T: Template>(t: T) -> Response {
    match t.render() {
        Ok(s) => Html(s).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "template render failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "template error").into_response()
        }
    }
}

fn page_error(status: StatusCode, msg: &str) -> Response {
    let mut r = render(ErrorPage {
        message: msg.to_owned(),
    });
    *r.status_mut() = status;
    r
}

fn bad(msg: impl Into<String>) -> Response {
    page_error(StatusCode::BAD_REQUEST, &msg.into())
}

fn internal(e: anyhow::Error) -> Response {
    tracing::error!(error = %e, "dashboard error");
    page_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Internal error. See the server log.",
    )
}

macro_rules! tryr {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return internal(e.into()),
        }
    };
}

fn opt_usd(s: &str) -> Result<Option<i64>, Response> {
    if s.trim().is_empty() {
        return Ok(None);
    }
    parse_usd(s)
        .map(Some)
        .ok_or_else(|| bad(format!("Not a valid dollar amount: {s}")))
}

fn usd(s: &str) -> Result<i64, Response> {
    parse_usd(s).ok_or_else(|| bad(format!("Not a valid dollar amount: {s}")))
}

fn percent(s: &str) -> Result<i64, Response> {
    s.trim()
        .parse::<f64>()
        .map(percent_to_bp)
        .map_err(|_| bad(format!("Not a valid percentage: {s}")))
}

fn csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(str::to_owned)
        .collect()
}

// --------------------------------------------------------------- overview

#[derive(Deserialize, Default)]
struct RangeQ {
    range: Option<String>,
}

async fn overview(_: Admin, State(st): State<AppState>, Query(q): Query<RangeQ>) -> Response {
    let range = Range::parse(q.range.as_deref());
    let since = range.since();
    let totals = tryr!(db::totals_since(&st.db, since).await);
    let series = tryr!(db::series(&st.db, since, range.bucket()).await);
    let by_model = tryr!(db::by_model(&st.db, since).await);
    let by_user = tryr!(db::by_user(&st.db, since).await);
    let by_team = tryr!(db::by_team(&st.db, since).await);
    let (unpriced, estimated) = tryr!(db::count_flags(&st.db).await);
    render(OverviewPage::build(
        range, &totals, &series, &by_model, &by_user, &by_team, unpriced, estimated,
    ))
}

// ------------------------------------------------------------------ users

async fn users(_: Admin, State(st): State<AppState>) -> Response {
    let users = tryr!(db::list_users(&st.db).await);
    let teams = tryr!(db::list_teams(&st.db).await);
    render(UsersPage::build(&users, &teams))
}

#[derive(Deserialize)]
struct UserForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    team_id: String,
    #[serde(default)]
    budget_usd: String,
    monthly: Option<String>,
    disabled: Option<String>,
}

fn team_id(s: &str) -> Option<i64> {
    s.parse().ok()
}

async fn create_user(_: Admin, State(st): State<AppState>, Form(f): Form<UserForm>) -> Response {
    let name = f.name.trim();
    if name.is_empty() {
        return bad("A user needs a name.");
    }
    let budget = match opt_usd(&f.budget_usd) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if let Err(e) = db::create_user(
        &st.db,
        name,
        team_id(&f.team_id),
        budget,
        f.monthly.is_some(),
    )
    .await
    {
        return bad(format!("Cannot create the user: {e}"));
    }
    Redirect::to("/admin/users").into_response()
}

async fn update_user(
    _: Admin,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<UserForm>,
) -> Response {
    let budget = match opt_usd(&f.budget_usd) {
        Ok(b) => b,
        Err(r) => return r,
    };
    tryr!(
        db::update_user(
            &st.db,
            id,
            team_id(&f.team_id),
            budget,
            f.monthly.is_some(),
            f.disabled.is_some()
        )
        .await
    );
    Redirect::to("/admin/users").into_response()
}

// ------------------------------------------------------------------ teams

async fn teams(_: Admin, State(st): State<AppState>) -> Response {
    let teams = tryr!(db::list_teams(&st.db).await);
    render(TeamsPage { teams })
}

#[derive(Deserialize)]
struct NameForm {
    name: String,
}

async fn create_team(_: Admin, State(st): State<AppState>, Form(f): Form<NameForm>) -> Response {
    let name = f.name.trim();
    if name.is_empty() {
        return bad("A team needs a name.");
    }
    if let Err(e) = db::create_team(&st.db, name).await {
        return bad(format!("Cannot create the team: {e}"));
    }
    Redirect::to("/admin/teams").into_response()
}

// ------------------------------------------------------------------- keys

async fn keys_page(st: &AppState, new_key: Option<String>) -> Response {
    let keys = tryr!(db::list_keys(&st.db).await);
    let users = tryr!(db::list_users(&st.db).await);
    render(KeysPage::build(&keys, &users, new_key))
}

async fn keys(_: Admin, State(st): State<AppState>) -> Response {
    keys_page(&st, None).await
}

#[derive(Deserialize)]
struct KeyForm {
    user_id: i64,
    #[serde(default)]
    label: String,
    #[serde(default)]
    allowed_models: String,
}

async fn create_key(_: Admin, State(st): State<AppState>, Form(f): Form<KeyForm>) -> Response {
    let allowed = csv(&f.allowed_models);
    let allowed = if allowed.is_empty() {
        None
    } else {
        Some(allowed)
    };
    let (_, key) = tryr!(db::create_key(&st.db, f.user_id, f.label.trim(), allowed).await);
    keys_page(&st, Some(key)).await
}

async fn revoke_key(_: Admin, State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    tryr!(db::revoke_key(&st.db, id).await);
    Redirect::to("/admin/keys").into_response()
}

// ----------------------------------------------------------------- models

async fn models(_: Admin, State(st): State<AppState>) -> Response {
    let cat = st.catalog();
    let providers: Vec<String> = st
        .router_table()
        .providers
        .iter()
        .map(|p| p.name.clone())
        .collect();
    render(ModelsPage::build(&cat, providers))
}

/// Form fields for one set of rates plus Context Tiers.
#[derive(Deserialize, Default)]
struct RatesForm {
    #[serde(default)]
    input: String,
    #[serde(default)]
    output: String,
    #[serde(default)]
    cache_read: String,
    #[serde(default)]
    cache_write_5m: String,
    #[serde(default)]
    cache_write_1h: String,
    #[serde(default)]
    image_input: String,
    #[serde(default)]
    image_output: String,
    #[serde(default)]
    tiers: String,
}

impl RatesForm {
    fn parse(&self) -> Result<CardRates, Response> {
        Ok(CardRates {
            base: Rates {
                input: usd(&self.input)?,
                output: usd(&self.output)?,
                cache_read: usd(&self.cache_read)?,
                cache_write_5m: usd(&self.cache_write_5m)?,
                cache_write_1h: usd(&self.cache_write_1h)?,
                image_input: usd(&self.image_input)?,
                image_output: usd(&self.image_output)?,
            },
            tiers: parse_tiers(&self.tiers)?,
        })
    }
}

/// One tier per line: `above_input_tokens input output cache_read
/// cache_write_5m cache_write_1h [image_input image_output]`.
fn parse_tiers(s: &str) -> Result<Vec<ContextTier>, Response> {
    let mut out = vec![];
    for line in s.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        let above: i64 = parts[0]
            .parse()
            .map_err(|_| bad(format!("Tier line must start with a token count: {line}")))?;
        let r = |i: usize| parts.get(i).map(|p| usd(p)).unwrap_or(Ok(0));
        out.push(ContextTier {
            above_input_tokens: above,
            rates: Rates {
                input: r(1)?,
                output: r(2)?,
                cache_read: r(3)?,
                cache_write_5m: r(4)?,
                cache_write_1h: r(5)?,
                image_input: r(6)?,
                image_output: r(7)?,
            },
        });
    }
    Ok(out)
}

#[derive(Deserialize)]
struct NewModelForm {
    name: String,
    provider: String,
    #[serde(default)]
    aliases: String,
    #[serde(flatten)]
    rates: RatesForm,
}

async fn create_model(
    _: Admin,
    State(st): State<AppState>,
    Form(f): Form<NewModelForm>,
) -> Response {
    let name = f.name.trim();
    if name.is_empty() {
        return bad("A model needs a name.");
    }
    if !st
        .router_table()
        .providers
        .iter()
        .any(|p| p.name == f.provider)
    {
        return bad("Unknown provider.");
    }
    let rates = match f.rates.parse() {
        Ok(r) => r,
        Err(r) => return r,
    };
    let id = match db::create_model(&st.db, name, &f.provider, &csv(&f.aliases)).await {
        Ok(id) => id,
        Err(e) => return bad(format!("Cannot create the model: {e}")),
    };
    tryr!(db::add_cost_card(&st.db, id, &rates).await);
    tryr!(st.reload_catalog().await);
    Redirect::to(&format!("/admin/models/{id}")).into_response()
}

async fn model_detail(_: Admin, State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    let cat = st.catalog();
    let Some(m) = cat.models.iter().find(|m| m.id == id) else {
        return page_error(StatusCode::NOT_FOUND, "No such model.");
    };
    let history = tryr!(db::card_history(&st.db, id).await);
    let teams = tryr!(db::list_teams(&st.db).await);
    let providers: Vec<String> = st
        .router_table()
        .providers
        .iter()
        .map(|p| p.name.clone())
        .collect();
    render(ModelPage::build(
        m,
        cat.default_markup_bp,
        &history,
        &teams,
        providers,
    ))
}

#[derive(Deserialize)]
struct ModelForm {
    provider: String,
    #[serde(default)]
    aliases: String,
}

async fn update_model(
    _: Admin,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<ModelForm>,
) -> Response {
    if !st
        .router_table()
        .providers
        .iter()
        .any(|p| p.name == f.provider)
    {
        return bad("Unknown provider.");
    }
    tryr!(db::update_model(&st.db, id, &f.provider, &csv(&f.aliases)).await);
    tryr!(st.reload_catalog().await);
    Redirect::to(&format!("/admin/models/{id}")).into_response()
}

async fn set_cost(
    _: Admin,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<RatesForm>,
) -> Response {
    let rates = match f.parse() {
        Ok(r) => r,
        Err(r) => return r,
    };
    tryr!(db::add_cost_card(&st.db, id, &rates).await);
    tryr!(st.reload_catalog().await);
    Redirect::to(&format!("/admin/models/{id}")).into_response()
}

#[derive(Deserialize)]
struct PriceForm {
    /// "default", "markup" or "rates". For Team overrides, "default" removes it.
    mode: String,
    #[serde(default)]
    team_id: String,
    #[serde(default)]
    markup_percent: String,
    #[serde(flatten)]
    rates: RatesForm,
}

impl PriceForm {
    fn spec(&self) -> Result<Option<PriceSpec>, Response> {
        match self.mode.as_str() {
            "default" => Ok(None),
            "markup" => Ok(Some(PriceSpec::MarkupBp(percent(&self.markup_percent)?))),
            "rates" => Ok(Some(PriceSpec::Rates(self.rates.parse()?))),
            _ => Err(bad("Unknown price mode.")),
        }
    }
}

async fn set_price(
    _: Admin,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<PriceForm>,
) -> Response {
    let spec = match f.spec() {
        Ok(s) => s,
        Err(r) => return r,
    };
    match spec {
        Some(s) => tryr!(db::add_price_card(&st.db, id, None, &s).await),
        None => tryr!(db::add_null_price_card(&st.db, id, None).await),
    };
    tryr!(st.reload_catalog().await);
    Redirect::to(&format!("/admin/models/{id}")).into_response()
}

async fn set_team_price(
    _: Admin,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<PriceForm>,
) -> Response {
    let Some(team) = team_id(&f.team_id) else {
        return bad("Select a team.");
    };
    let spec = match f.spec() {
        Ok(s) => s,
        Err(r) => return r,
    };
    match spec {
        Some(s) => tryr!(db::add_price_card(&st.db, id, Some(team), &s).await),
        None => tryr!(db::add_null_price_card(&st.db, id, Some(team)).await),
    };
    tryr!(st.reload_catalog().await);
    Redirect::to(&format!("/admin/models/{id}")).into_response()
}

#[derive(Deserialize)]
struct SettingsForm {
    default_markup_percent: String,
}

async fn set_settings(
    _: Admin,
    State(st): State<AppState>,
    Form(f): Form<SettingsForm>,
) -> Response {
    let bp = match percent(&f.default_markup_percent) {
        Ok(b) => b,
        Err(r) => return r,
    };
    tryr!(db::set_default_markup_bp(&st.db, bp).await);
    tryr!(st.reload_catalog().await);
    Redirect::to("/admin/models").into_response()
}

// ----------------------------------------------------------------- ledger

#[derive(Deserialize, Default)]
struct LedgerQ {
    filter: Option<String>,
}

async fn ledger(_: Admin, State(st): State<AppState>, Query(q): Query<LedgerQ>) -> Response {
    let (filter, name) = match q.filter.as_deref() {
        Some("unpriced") => (LedgerFilter::Unpriced, "unpriced"),
        Some("estimated") => (LedgerFilter::Estimated, "estimated"),
        Some("errors") => (LedgerFilter::Errors, "errors"),
        _ => (LedgerFilter::All, "all"),
    };
    let rows = tryr!(db::recent_ledger(&st.db, filter, 200).await);
    render(LedgerPage::build(&rows, name, None))
}

async fn reprice(_: Admin, State(st): State<AppState>) -> Response {
    let n = tryr!(db::reprice_unpriced(&st.db, &st.catalog()).await);
    let rows = tryr!(db::recent_ledger(&st.db, LedgerFilter::Unpriced, 200).await);
    render(LedgerPage::build(
        &rows,
        "unpriced",
        Some(format!("Priced {n} entries.")),
    ))
}
