//! The Operator dashboard: server-rendered HTML with askama and htmx.
// Form helpers return a ready error page as the Err value.
#![allow(clippy::result_large_err)]

mod chart;
mod views;

use askama::Template;
use axum::Router;
use axum::extract::{FromRequest, FromRequestParts, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use base64::Engine;
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config::percent_to_bp;
use crate::db::{self, LedgerFilter};
use crate::pricing::{CardRates, ContextTier, PriceSpec, Rates, parse_usd};
use crate::ratelimit::Limits;
use crate::state::AppState;
use views::*;

const COOKIE: &str = "tokomotive_session";
const FLASH: &str = "tokomotive_flash";

macro_rules! tryr {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return internal(e.into()),
        }
    };
}

mod oauth;
mod settings;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin", get(overview))
        .route("/admin/login", get(login_page).post(login))
        .route("/admin/logout", post(logout))
        .route("/admin/assets/htmx.min.js", get(htmx))
        .route("/admin/assets/favicon.svg", get(favicon))
        .route("/favicon.ico", get(favicon))
        .route("/admin/users", get(users).post(create_user))
        .route("/admin/users/{id}", post(update_user))
        .route("/admin/teams", get(teams).post(create_team))
        .route("/admin/teams/{id}", post(rename_team))
        .route("/admin/teams/{id}/delete", post(delete_team))
        .route("/admin/keys", get(keys).post(create_key))
        .route("/admin/keys/{id}", post(update_key))
        .route("/admin/keys/{id}/limits", post(update_key_limits))
        .route("/admin/keys/{id}/providers", post(update_key_providers))
        .route("/admin/keys/{id}/revoke", post(revoke_key))
        .route("/admin/models", get(models).post(create_model))
        .route("/admin/models/{id}", get(model_detail).post(update_model))
        .route("/admin/models/{id}/cost", post(set_cost))
        .route("/admin/models/{id}/price", post(set_price))
        .route("/admin/models/{id}/team-price", post(set_team_price))
        .route("/admin/settings/markup", post(set_settings))
        .route("/admin/ledger", get(ledger))
        .route("/admin/reprice", post(reprice))
        .merge(settings::router())
        .merge(oauth::router())
        .fallback(not_found)
        .layer(axum::middleware::from_fn(same_origin))
        .layer(axum::middleware::from_fn(htmx_redirect))
        .layer(axum::middleware::from_fn(context))
}

// ---------------------------------------------------------------- context

/// Facts about the current request that every page needs. The `context`
/// layer sets them, so handlers keep calling `Base::new(&st)` only.
#[derive(Clone, Default)]
pub(crate) struct RequestCtx {
    /// The one-time success message from the flash cookie.
    pub flash: Option<String>,
    /// The Proxy base URL as the browser sees it, for example
    /// `https://llm.example.com/v1`.
    pub proxy_url: String,
    /// True when the request carries a session cookie. Only used to hide the
    /// navigation on error pages for visitors who are not logged in.
    pub session: bool,
}

tokio::task_local! {
    static CTX: RequestCtx;
}

pub(crate) fn ctx() -> RequestCtx {
    CTX.try_with(Clone::clone).unwrap_or_default()
}

async fn context(req: Request, next: Next) -> Response {
    let h = req.headers();
    // Only a full page shows the flash. htmx requests leave it for the
    // page that HX-Redirect loads next.
    let page = req.method() == axum::http::Method::GET && !h.contains_key("hx-request");
    let flash = cookie(h, FLASH)
        .filter(|_| page)
        .and_then(|v| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(v)
                .ok()
        })
        .and_then(|v| String::from_utf8(v).ok())
        .filter(|v| !v.is_empty());
    let shown = flash.is_some();
    let ctx = RequestCtx {
        flash,
        proxy_url: proxy_url(h),
        session: cookie(h, COOKIE).is_some(),
    };
    let mut resp = CTX.scope(ctx, next.run(req)).await;
    if shown && resp.status() == StatusCode::OK {
        resp.headers_mut().append(
            header::SET_COOKIE,
            HeaderValue::from_static(
                "tokomotive_flash=; HttpOnly; SameSite=Strict; Path=/admin; Max-Age=0",
            ),
        );
    }
    resp
}

/// The Proxy base URL from `X-Forwarded-Proto`, `X-Forwarded-Host` and
/// `Host`. It is only displayed, so a forged header changes only the text.
fn proxy_url(h: &HeaderMap) -> String {
    let first = |name: &str| {
        h.get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let scheme = match first("x-forwarded-proto") {
        Some("https") => "https",
        _ => "http",
    };
    let host = first("x-forwarded-host")
        .or_else(|| first("host"))
        .filter(|v| {
            v.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b".-:[]_".contains(&c))
        })
        .unwrap_or("localhost");
    format!("{scheme}://{host}/v1")
}

// ------------------------------------------------------- errors and flash

/// A message for the Operator. Handlers return it through [`form_error`],
/// [`page_error`] or [`internal`]. The `htmx_redirect` layer renders it: as
/// a small fragment for the form's `.form-error` slot for htmx requests, or
/// as the full error page for plain requests.
#[derive(Clone)]
struct UserError {
    heading: Option<String>,
    message: String,
    back: Option<(String, String)>,
}

fn user_error(status: StatusCode, message: impl Into<String>) -> Response {
    let mut r = status.into_response();
    r.extensions_mut().insert(UserError {
        heading: None,
        message: message.into(),
        back: None,
    });
    r
}

/// The error for a form that the Operator can correct: status 400, shown
/// next to the submitted form. Write a full sentence that says what to do.
pub(crate) fn form_error(e: impl std::fmt::Display) -> Response {
    user_error(StatusCode::BAD_REQUEST, e.to_string())
}

fn bad(msg: impl Into<String>) -> Response {
    form_error(msg.into())
}

fn page_error(status: StatusCode, msg: &str) -> Response {
    user_error(status, msg)
}

/// An error page with its own heading and a link to the next step.
fn page_error_with(status: StatusCode, heading: &str, msg: &str, back: (&str, &str)) -> Response {
    let mut r = status.into_response();
    r.extensions_mut().insert(UserError {
        heading: Some(heading.into()),
        message: msg.into(),
        back: Some((back.0.into(), back.1.into())),
    });
    r
}

fn internal(e: anyhow::Error) -> Response {
    tracing::error!(error = %e, "dashboard error");
    user_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Something went wrong on the server. Try again. If it happens again, see the server log.",
    )
}

/// A unique-constraint violation from SQLite.
fn is_unique_violation(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<sqlx::Error>()
            .and_then(|e| e.as_database_error())
            .is_some_and(|d| d.is_unique_violation())
    })
}

/// Redirect after a successful save, with a one-time success message.
pub(crate) fn saved(path: &str, message: &str) -> Response {
    let value = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(message);
    (
        [(
            header::SET_COOKIE,
            format!("{FLASH}={value}; HttpOnly; SameSite=Strict; Path=/admin; Max-Age=60"),
        )],
        Redirect::to(path),
    )
        .into_response()
}

/// `axum::Form` with a plain-language rejection. Every dashboard handler
/// uses it; the internal parser message goes to the log only.
pub(crate) struct Form<T>(pub T);

impl<T, S> FromRequest<S> for Form<T>
where
    axum::Form<T>: FromRequest<S, Rejection = axum::extract::rejection::FormRejection>,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, st: &S) -> Result<Self, Self::Rejection> {
        match axum::Form::<T>::from_request(req, st).await {
            Ok(axum::Form(v)) => Ok(Form(v)),
            Err(e) => {
                tracing::warn!(error = %e, "dashboard form rejected");
                Err(user_error(
                    e.status(),
                    "The form is incomplete. Reload the page and try again.",
                ))
            }
        }
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The same-origin path of the Referer, for the link back from an error page.
fn referer_path(h: &HeaderMap) -> Option<String> {
    let r = h.get(header::REFERER)?.to_str().ok()?;
    let uri: axum::http::Uri = r.parse().ok()?;
    let host = h.get(header::HOST)?.to_str().ok()?;
    if uri.authority().map(|a| a.as_str()) != Some(host) {
        return None;
    }
    let pq = uri.path_and_query()?.as_str();
    pq.starts_with("/admin").then(|| pq.to_owned())
}

fn render_user_error(
    status: StatusCode,
    err: UserError,
    htmx: bool,
    referer: Option<String>,
) -> Response {
    if htmx {
        return (
            status,
            [
                ("hx-retarget", "find .form-error"),
                ("hx-reswap", "innerHTML"),
            ],
            Html(format!("<p>{}</p>", esc(&err.message))),
        )
            .into_response();
    }
    let base = Base::anonymous();
    let heading = err.heading.unwrap_or_else(|| {
        match status {
            StatusCode::NOT_FOUND => "Page not found",
            StatusCode::FORBIDDEN => "Not allowed",
            s if s.is_server_error() => "Something went wrong",
            _ => "The change was not saved",
        }
        .into()
    });
    let (back_href, back_label) = err.back.unwrap_or_else(|| match referer {
        Some(r) if status.is_client_error() && status != StatusCode::NOT_FOUND => {
            (r, "Go back to the form".into())
        }
        _ if base.logged_in => ("/admin".into(), "Go to the Overview".into()),
        _ => ("/admin/login".into(), "Log in".into()),
    });
    let mut r = render(ErrorPage {
        base,
        heading,
        message: err.message,
        back_href,
        back_label,
    });
    *r.status_mut() = status;
    r
}

/// Shapes dashboard responses for htmx and plain requests:
/// - an htmx request follows a 303 inside the XHR and would swap the target
///   page into the form. Answer a redirect with 200 and `HX-Redirect`
///   instead, so that the browser navigates (cookies are kept);
/// - a [`UserError`] becomes a fragment for the form's `.form-error` slot
///   (htmx) or the full error page (plain).
async fn htmx_redirect(req: Request, next: Next) -> Response {
    let htmx = req.headers().contains_key("hx-request");
    let referer = referer_path(req.headers());
    let mut resp = next.run(req).await;
    if let Some(err) = resp.extensions_mut().remove::<UserError>() {
        return render_user_error(resp.status(), err, htmx, referer);
    }
    if htmx
        && resp.status().is_redirection()
        && let Some(to) = resp.headers_mut().remove(header::LOCATION)
    {
        let mut out = StatusCode::OK.into_response();
        // A redirect to `#fragment` on the same page changes only the hash,
        // so the browser would not load the page. Ask htmx to reload it.
        if to.to_str().is_ok_and(|v| v.contains('#')) {
            out.headers_mut()
                .insert("hx-refresh", HeaderValue::from_static("true"));
        }
        out.headers_mut().insert("hx-redirect", to);
        for value in resp.headers().get_all(header::SET_COOKIE) {
            out.headers_mut().append(header::SET_COOKIE, value.clone());
        }
        resp = out;
    }
    resp
}

async fn same_origin(req: Request, next: Next) -> Response {
    if req.method() == axum::http::Method::POST {
        let h = req.headers();
        let cross_site = h
            .get("sec-fetch-site")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v != "same-origin" && v != "none");
        let wrong_origin = h.get(header::ORIGIN).is_some_and(|v| {
            let origin = v
                .to_str()
                .ok()
                .and_then(|v| v.parse::<axum::http::Uri>().ok());
            !origin.is_some_and(|u| {
                matches!(u.scheme_str(), Some("http" | "https"))
                    && u.authority().map(|a| a.as_str())
                        == h.get(header::HOST).and_then(|v| v.to_str().ok())
            })
        });
        if cross_site || wrong_origin {
            return page_error(
                StatusCode::FORBIDDEN,
                "Cross-origin form submission refused. Open the dashboard at its own address and try again.",
            );
        }
    }
    next.run(req).await
}

async fn not_found() -> Response {
    page_error(
        StatusCode::NOT_FOUND,
        "No dashboard page has this address. Check the address, or use the navigation.",
    )
}

async fn favicon() -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("../../assets/favicon.svg"),
    )
        .into_response()
}

// -------------------------------------------------------------------- auth

/// Extractor: present only for a logged-in Operator.
pub struct Admin;

impl FromRequestParts<AppState> for Admin {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        if crate::settings::value(&st.db, "admin_password_hash")
            .await
            .map_err(internal)?
            .is_none()
        {
            return Err(Redirect::to("/setup").into_response());
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
    cookie(h, COOKIE)
}

fn cookie(h: &HeaderMap, name: &str) -> Option<String> {
    h.get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, v)| *k == name && !v.is_empty())
        .map(|(_, v)| v.to_owned())
}

pub fn ct_eq(a: &str, b: &str) -> bool {
    let (x, y) = (Sha256::digest(a.as_bytes()), Sha256::digest(b.as_bytes()));
    x.iter()
        .zip(y.iter())
        .fold(0u8, |acc, (p, q)| acc | (p ^ q))
        == 0
}

async fn login_page(State(st): State<AppState>) -> Response {
    if tryr!(crate::settings::value(&st.db, "admin_password_hash").await).is_none() {
        return Redirect::to("/setup").into_response();
    }
    render(LoginPage {
        base: Base::anonymous(),
        error: None,
    })
}

#[derive(Deserialize)]
struct LoginForm {
    password: String,
}

async fn login(State(st): State<AppState>, Form(f): Form<LoginForm>) -> Response {
    let Some(pw) = tryr!(crate::settings::value(&st.db, "admin_password_hash").await) else {
        return Redirect::to("/setup").into_response();
    };
    if !crate::settings::verify_password(&f.password, &pw) {
        let mut r = render(LoginPage {
            base: Base::anonymous(),
            error: Some("Incorrect password. Try again.".into()),
        });
        *r.status_mut() = StatusCode::UNAUTHORIZED;
        return r;
    }
    let cookie = tryr!(start_session(&st, &pw).await);
    ([(header::SET_COOKIE, cookie)], Redirect::to("/admin")).into_response()
}

/// Creates a session for the password hash `pw` and returns its Set-Cookie
/// value. Login and Setup use it.
async fn start_session(st: &AppState, pw: &str) -> anyhow::Result<String> {
    let mut b = [0u8; 32];
    rand::rng().fill_bytes(&mut b);
    let token = hex::encode(b);
    db::create_session_for_password(&st.db, &token, st.settings().session_days, Some(pw)).await?;
    let max_age = st.settings().session_days * 86_400;
    Ok(format!(
        "{COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/admin; Max-Age={max_age}"
    ))
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

/// A Budget in USD: blank for no Budget, else 0 or more.
fn opt_budget(s: &str) -> Result<Option<i64>, Response> {
    match opt_usd(s)? {
        Some(v) if v < 0 => Err(bad("A Budget must be 0 or more, or blank for no Budget.")),
        v => Ok(v),
    }
}

fn opt_usd(s: &str) -> Result<Option<i64>, Response> {
    if s.trim().is_empty() {
        return Ok(None);
    }
    parse_usd(s).map(Some).ok_or_else(|| {
        bad(format!(
            "{s} is not a dollar amount. Use digits and a dot, for example 12.50."
        ))
    })
}

fn percent(s: &str) -> Result<i64, Response> {
    s.trim().parse::<f64>().map(percent_to_bp).map_err(|_| {
        bad(format!(
            "Enter the percentage as a number, for example 20. \"{s}\" is not a number."
        ))
    })
}

fn csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(str::to_owned)
        .collect()
}

#[derive(Deserialize, Default)]
struct LimitsForm {
    #[serde(default)]
    rpm_limit: String,
    #[serde(default)]
    tpm_limit: String,
}

impl LimitsForm {
    fn parse(&self) -> Result<Limits, Response> {
        let limit = |s: &str| {
            let s = s.trim();
            if s.is_empty() {
                return Ok(None);
            }
            s.parse::<i64>()
                .ok()
                .filter(|&n| n >= 0)
                .map(Some)
                .ok_or_else(|| {
                    bad("Rate Limits must be whole numbers of 0 or more, or blank for unlimited.")
                })
        };
        Ok(Limits {
            rpm: limit(&self.rpm_limit)?,
            tpm: limit(&self.tpm_limit)?,
        })
    }
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
    let by_provider = tryr!(db::by_provider(&st.db, since).await);
    let by_user = tryr!(db::by_user(&st.db, since).await);
    let by_team = tryr!(db::by_team(&st.db, since).await);
    let (unpriced, estimated) = tryr!(db::count_flags_since(&st.db, since).await);
    let (users, keys, requests): (bool, bool, bool) = tryr!(
        sqlx::query_as(
            "SELECT EXISTS(SELECT 1 FROM users), \
             EXISTS(SELECT 1 FROM virtual_keys WHERE revoked_at IS NULL), \
             EXISTS(SELECT 1 FROM ledger)"
        )
        .fetch_one(&st.db)
        .await
    );
    let table = st.router_table();
    let setup = SetupState {
        providers: !table.providers.is_empty(),
        models_or_routes: !table.routes.is_empty() || !st.catalog().models.is_empty(),
        users,
        keys,
        requests,
    };
    render(OverviewPage::build(
        Base::new(&st),
        range,
        OverviewData {
            totals: &totals,
            series: &series,
            by_model: &by_model,
            by_provider: &by_provider,
            by_user: &by_user,
            by_team: &by_team,
            unpriced,
            estimated,
            setup,
        },
    ))
}

// ------------------------------------------------------------------ users

async fn users(_: Admin, State(st): State<AppState>) -> Response {
    let users = tryr!(db::list_users(&st.db).await);
    let teams = tryr!(db::list_teams(&st.db).await);
    render(UsersPage::build(Base::new(&st), &users, &teams))
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
    #[serde(flatten)]
    limits: LimitsForm,
}

fn team_id(s: &str) -> Option<i64> {
    s.parse().ok()
}

async fn create_user(_: Admin, State(st): State<AppState>, Form(f): Form<UserForm>) -> Response {
    let name = f.name.trim();
    if name.is_empty() {
        return bad("A User needs a name.");
    }
    let budget = match opt_budget(&f.budget_usd) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let limits = match f.limits.parse() {
        Ok(l) => l,
        Err(r) => return r,
    };
    if let Err(e) = db::create_user_with_limits(
        &st.db,
        name,
        team_id(&f.team_id),
        budget,
        f.monthly.is_some(),
        limits,
    )
    .await
    {
        if is_unique_violation(&e) {
            return bad(format!("A User named {name} already exists."));
        }
        return internal(e);
    }
    saved("/admin/users", &format!("User {name} added."))
}

async fn update_user(
    _: Admin,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<UserForm>,
) -> Response {
    let simple = st.settings().mode == "simple";
    // Simple Mode hides Budgets and Rate Limits; keep the stored values.
    let money = if simple {
        None
    } else {
        let budget = match opt_budget(&f.budget_usd) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let limits = match f.limits.parse() {
            Ok(l) => l,
            Err(r) => return r,
        };
        Some((budget, limits))
    };
    let mut tx = tryr!(st.db.begin().await);
    // A blank name keeps the current name.
    let name = f.name.trim();
    if !name.is_empty()
        && let Err(e) = sqlx::query("UPDATE users SET name=? WHERE id=?")
            .bind(name)
            .bind(id)
            .execute(&mut *tx)
            .await
    {
        let e = anyhow::Error::from(e);
        if is_unique_violation(&e) {
            return bad(format!("A User named {name} already exists."));
        }
        return internal(e);
    }
    tryr!(
        sqlx::query("UPDATE users SET team_id=?, disabled=? WHERE id=?")
            .bind(team_id(&f.team_id))
            .bind(f.disabled.is_some())
            .bind(id)
            .execute(&mut *tx)
            .await
    );
    if let Some((budget, limits)) = money {
        tryr!(
            sqlx::query(
                "UPDATE users SET budget_micros=?, budget_monthly=?, rpm_limit=?, tpm_limit=? WHERE id=?"
            )
            .bind(budget)
            .bind(f.monthly.is_some())
            .bind(limits.rpm)
            .bind(limits.tpm)
            .bind(id)
            .execute(&mut *tx)
            .await
        );
    }
    tryr!(tx.commit().await);
    let label = if name.is_empty() {
        "User saved.".to_owned()
    } else {
        format!("User {name} saved.")
    };
    saved(&format!("/admin/users#u{id}"), &label)
}

// ------------------------------------------------------------------ teams

async fn teams(_: Admin, State(st): State<AppState>) -> Response {
    let teams = tryr!(db::list_teams(&st.db).await);
    let users = tryr!(db::list_users(&st.db).await);
    render(TeamsPage::build(
        Base::new(&st),
        &teams,
        &users,
        &st.catalog(),
    ))
}

#[derive(Deserialize)]
struct NameForm {
    #[serde(default)]
    name: String,
}

async fn create_team(_: Admin, State(st): State<AppState>, Form(f): Form<NameForm>) -> Response {
    let name = f.name.trim();
    if name.is_empty() {
        return bad("A Team needs a name.");
    }
    if let Err(e) = db::create_team(&st.db, name).await {
        if is_unique_violation(&e) {
            return bad(format!("A Team named {name} already exists."));
        }
        return internal(e);
    }
    saved("/admin/teams", &format!("Team {name} added."))
}

async fn rename_team(
    _: Admin,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<NameForm>,
) -> Response {
    let name = f.name.trim();
    if name.is_empty() {
        return bad("A Team needs a name.");
    }
    if let Err(e) = sqlx::query("UPDATE teams SET name=? WHERE id=?")
        .bind(name)
        .bind(id)
        .execute(&st.db)
        .await
    {
        let e = anyhow::Error::from(e);
        if is_unique_violation(&e) {
            return bad(format!("A Team named {name} already exists."));
        }
        return internal(e);
    }
    // Overrides show the Team name from the catalog.
    tryr!(st.reload_catalog().await);
    saved(
        &format!("/admin/teams#t{id}"),
        &format!("Team {name} saved."),
    )
}

async fn delete_team(_: Admin, State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    let (users, used): (i64, bool) = tryr!(
        sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM users WHERE team_id=?1), \
             EXISTS(SELECT 1 FROM cards WHERE team_id=?1) OR EXISTS(SELECT 1 FROM ledger WHERE team_id=?1)"
        )
        .bind(id)
        .fetch_one(&st.db)
        .await
    );
    if users > 0 {
        return bad("This Team has Users. Move its Users to another Team first.");
    }
    if used {
        return bad(
            "This Team has price overrides or Ledger Entries, so it stays for the records. Rename it instead.",
        );
    }
    let name: Option<String> = tryr!(
        sqlx::query_scalar("DELETE FROM teams WHERE id=? RETURNING name")
            .bind(id)
            .fetch_optional(&st.db)
            .await
    );
    match name {
        Some(name) => saved("/admin/teams", &format!("Team {name} deleted.")),
        None => bad("Team not found. Reload the page."),
    }
}

// ------------------------------------------------------------------- keys

async fn keys_page(st: &AppState, new_key: Option<String>, filter: Option<i64>) -> Response {
    let keys = tryr!(db::list_keys(&st.db).await);
    let users = tryr!(db::list_users(&st.db).await);
    let table = st.router_table();
    let providers: Vec<(String, bool)> = table
        .providers
        .iter()
        .map(|p| {
            (
                p.name.clone(),
                p.auth != crate::provider::ProviderAuth::ApiKey,
            )
        })
        .collect();
    let mut model_names: Vec<String> = table
        .routes
        .iter()
        .map(|r| r.name.clone())
        .chain(st.catalog().models.iter().map(|m| m.name.clone()))
        .collect();
    model_names.sort();
    model_names.dedup();
    render(KeysPage::build(
        Base::new(st),
        KeysData {
            keys: &keys,
            users: &users,
            new_key,
            providers: &providers,
            model_names,
            filter,
        },
    ))
}

#[derive(Deserialize, Default)]
struct KeysQ {
    user: Option<String>,
}

async fn keys(_: Admin, State(st): State<AppState>, Query(q): Query<KeysQ>) -> Response {
    let filter = q.user.as_deref().and_then(|u| u.parse().ok());
    keys_page(&st, None, filter).await
}

/// Checks Allowed models: each name must be a Route, a catalog Model (or
/// alias), or `provider/model` with a known Provider.
fn validate_allowed_models(st: &AppState, names: &[String]) -> Result<(), Response> {
    let table = st.router_table();
    let catalog = st.catalog();
    for name in names {
        let known = table.routes.iter().any(|r| &r.name == name)
            || catalog.resolve(name).is_some()
            || name
                .split_once('/')
                .is_some_and(|(p, m)| !m.is_empty() && table.providers.iter().any(|x| x.name == p));
        if !known {
            return Err(bad(format!(
                "Unknown model: {name}. Use a Model name, a Route name, or provider/model."
            )));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct KeyForm {
    user_id: i64,
    #[serde(default)]
    label: String,
    #[serde(default)]
    allowed_models: String,
    #[serde(flatten)]
    limits: LimitsForm,
}

async fn create_key(
    _: Admin,
    State(st): State<AppState>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let access: Vec<String> = pairs
        .iter()
        .filter(|(k, _)| k == "allowed_providers")
        .map(|(_, v)| v.clone())
        .collect();
    if let Err(r) = validate_provider_access(&st, &access) {
        return r;
    }
    let fields: std::collections::HashMap<_, _> = pairs.into_iter().collect();
    let user_id = match fields.get("user_id").and_then(|s| s.parse().ok()) {
        Some(id) => id,
        None => return bad("Choose a User. Add a User first if the list is empty."),
    };
    let f = KeyForm {
        user_id,
        label: fields.get("label").cloned().unwrap_or_default(),
        allowed_models: fields.get("allowed_models").cloned().unwrap_or_default(),
        limits: LimitsForm {
            rpm_limit: fields.get("rpm_limit").cloned().unwrap_or_default(),
            tpm_limit: fields.get("tpm_limit").cloned().unwrap_or_default(),
        },
    };
    let limits = match f.limits.parse() {
        Ok(l) => l,
        Err(r) => return r,
    };
    let allowed = csv(&f.allowed_models);
    if let Err(r) = validate_allowed_models(&st, &allowed) {
        return r;
    }
    let allowed = if allowed.is_empty() {
        None
    } else {
        Some(allowed)
    };
    let (id, key) =
        tryr!(db::create_key_with_limits(&st.db, f.user_id, f.label.trim(), allowed, limits).await);
    tryr!(db::update_key_providers(&st.db, id, &access).await);
    keys_page(&st, Some(key), None).await
}

fn validate_provider_access(st: &AppState, access: &[String]) -> Result<(), Response> {
    if access
        .iter()
        .any(|name| !st.router_table().providers.iter().any(|p| &p.name == name))
    {
        return Err(bad(
            "Unknown Provider in Provider Access. Reload the page and try again.",
        ));
    }
    Ok(())
}
/// The edit form of one key: Provider Access and, in Full Mode, Rate Limits.
async fn update_key(
    _: Admin,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let access: Vec<String> = pairs
        .iter()
        .filter(|(k, _)| k == "allowed_providers")
        .map(|(_, v)| v.clone())
        .collect();
    if let Err(r) = validate_provider_access(&st, &access) {
        return r;
    }
    let field = |name: &str| {
        pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let limits = LimitsForm {
        rpm_limit: field("rpm_limit"),
        tpm_limit: field("tpm_limit"),
    };
    // Simple Mode hides Rate Limits; keep the stored values.
    if st.settings().mode != "simple" {
        let limits = match limits.parse() {
            Ok(l) => l,
            Err(r) => return r,
        };
        tryr!(db::update_key_limits(&st.db, id, limits).await);
    }
    tryr!(db::update_key_providers(&st.db, id, &access).await);
    saved(&format!("/admin/keys#k{id}"), "Virtual Key saved.")
}

async fn update_key_providers(
    _: Admin,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let access: Vec<String> = pairs
        .into_iter()
        .filter(|(k, _)| k == "allowed_providers")
        .map(|(_, v)| v)
        .collect();
    if let Err(r) = validate_provider_access(&st, &access) {
        return r;
    }
    tryr!(db::update_key_providers(&st.db, id, &access).await);
    saved("/admin/keys", "Provider Access saved.")
}

async fn update_key_limits(
    _: Admin,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<LimitsForm>,
) -> Response {
    let limits = match f.parse() {
        Ok(l) => l,
        Err(r) => return r,
    };
    tryr!(db::update_key_limits(&st.db, id, limits).await);
    saved("/admin/keys", "Rate Limits saved.")
}

async fn revoke_key(_: Admin, State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    tryr!(db::revoke_key(&st.db, id).await);
    saved("/admin/keys", "Virtual Key revoked.")
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
    render(ModelsPage::build(Base::new(&st), &cat, providers))
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
    /// The check box "Input and Output are $0 on purpose".
    #[serde(default)]
    allow_zero: String,
}

/// What a set of rates is for. It selects the error messages.
#[derive(Clone, Copy)]
enum CardUse {
    NewModel,
    Cost,
    Price,
}

const RATE_LABELS: [&str; 7] = [
    "Input",
    "Output",
    "Cache read",
    "Cache write 5m",
    "Cache write 1h",
    "Image input",
    "Image output",
];

/// One rate in dollars per million tokens. Blank is 0; negative is refused.
fn rate(label: &str, s: &str) -> Result<i64, Response> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(0);
    }
    match parse_usd(t) {
        Some(v) if v >= 0 => Ok(v),
        Some(_) => Err(bad(format!("The {label} rate must be 0 or more."))),
        None => Err(bad(format!(
            "The {label} rate \"{t}\" is not a dollar amount. Use digits and a dot, for example 2.50."
        ))),
    }
}

fn rates_of(v: [i64; 7]) -> Rates {
    Rates {
        input: v[0],
        output: v[1],
        cache_read: v[2],
        cache_write_5m: v[3],
        cache_write_1h: v[4],
        image_input: v[5],
        image_output: v[6],
    }
}

impl RatesForm {
    fn fields(&self) -> [&str; 7] {
        [
            &self.input,
            &self.output,
            &self.cache_read,
            &self.cache_write_5m,
            &self.cache_write_1h,
            &self.image_input,
            &self.image_output,
        ]
    }

    /// The card in the form. All fields blank gives `None`: the caller
    /// decides what no card means, and never saves a $0 card for it. A card
    /// with Input and Output both 0 needs the `allow_zero` check box.
    fn card(&self, card: CardUse) -> Result<Option<CardRates>, Response> {
        if self.fields().iter().all(|s| s.trim().is_empty()) && self.tiers.trim().is_empty() {
            return Ok(None);
        }
        let mut v = [0; 7];
        for (i, s) in self.fields().iter().enumerate() {
            v[i] = rate(RATE_LABELS[i], s)?;
        }
        let base = rates_of(v);
        let tiers = parse_tiers(&self.tiers)?;
        if base.input == 0 && base.output == 0 && self.allow_zero.is_empty() {
            return Err(bad(match card {
                CardUse::NewModel => {
                    "The Input and Output rates are 0, so the requests of this Model would have no Cost and would not be flagged as Unpriced. Select \"Input and Output are $0 on purpose\" to save a $0 Cost Card, or leave all rates blank to keep the Model Unpriced."
                }
                CardUse::Cost => {
                    "The Input and Output rates are 0, so the requests of this Model would have no Cost. Select \"Input and Output are $0 on purpose\" to save a $0 Cost Card."
                }
                CardUse::Price => {
                    "The Input and Output rates are 0, so Users would pay nothing for this Model. Select \"Input and Output are $0 on purpose\" to save a $0 Price."
                }
            }));
        }
        Ok(Some(CardRates { base, tiers }))
    }
}

/// One tier per line: `above_input_tokens input output cache_read
/// cache_write_5m cache_write_1h [image_input image_output]`.
fn parse_tiers(s: &str) -> Result<Vec<ContextTier>, Response> {
    const HELP: &str = "Write 6 or 8 numbers: the input tokens above which the tier applies, then the Input, Output, Cache read, Cache write 5m and Cache write 1h rates, and optionally the Image input and Image output rates.";
    let mut out = vec![];
    for (n, line) in s.lines().map(str::trim).enumerate() {
        let n = n + 1;
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() != 6 && parts.len() != 8 {
            return Err(bad(format!(
                "Context Tier line {n} has {} numbers. {HELP}",
                parts.len()
            )));
        }
        let above: i64 = parts[0].parse().ok().filter(|v| *v >= 0).ok_or_else(|| {
            bad(format!(
                "Context Tier line {n} must start with a whole number of input tokens, for example 200000. \"{}\" is not one.",
                parts[0]
            ))
        })?;
        let mut v = [0; 7];
        for (i, p) in parts[1..].iter().enumerate() {
            v[i] = rate(&format!("Context Tier line {n} {}", RATE_LABELS[i]), p)?;
        }
        out.push(ContextTier {
            above_input_tokens: above,
            rates: rates_of(v),
        });
    }
    Ok(out)
}

/// A note for the success message when Input or Output is 0 but not both.
fn zero_note(r: &Rates) -> &'static str {
    match (r.input == 0, r.output == 0) {
        (true, false) => " The Input rate is 0, so input tokens are free.",
        (false, true) => " The Output rate is 0, so output tokens are free.",
        _ => "",
    }
}

/// Model names and aliases must not overlap, so that each model string
/// resolves to one Model. `own` is the Model that is being changed.
fn check_names(
    st: &AppState,
    own: Option<i64>,
    name: &str,
    aliases: &[String],
) -> Result<(), Response> {
    let cat = st.catalog();
    let others: Vec<_> = cat.models.iter().filter(|m| Some(m.id) != own).collect();
    if let Some(m) = others.iter().find(|m| m.aliases.iter().any(|a| a == name)) {
        return Err(bad(format!(
            "{name} is already an alias of Model {}. Choose another name, or remove the alias from that Model first.",
            m.name
        )));
    }
    for a in aliases {
        if a == name {
            return Err(bad(format!(
                "The alias {a} is the same as the Model name. Remove it from the aliases."
            )));
        }
        if let Some(m) = others
            .iter()
            .find(|m| &m.name == a || m.aliases.contains(a))
        {
            return Err(bad(format!(
                "The alias {a} is already used by Model {}. Choose another alias.",
                m.name
            )));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct NewModelForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
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
    let _guard = st.settings_write.lock().await;
    let name = f.name.trim();
    if name.is_empty() {
        return bad("Enter a name for the Model, for example gpt-4o.");
    }
    if f.provider.is_empty() {
        return bad("Add a Provider first. A Model needs a Provider.");
    }
    if !st
        .router_table()
        .providers
        .iter()
        .any(|p| p.name == f.provider)
    {
        return bad("Unknown Provider. Reload the page and choose a Provider from the list.");
    }
    let aliases = csv(&f.aliases);
    if let Err(r) = check_names(&st, None, name, &aliases) {
        return r;
    }
    let simple = st.settings().mode == "simple";
    // Simple Mode has no cards. In Full Mode, blank rates give no Cost Card:
    // the Model stays Unpriced and its requests are flagged.
    let rates = if simple {
        None
    } else {
        match f.rates.card(CardUse::NewModel) {
            Ok(r) => r,
            Err(r) => return r,
        }
    };
    let id = match db::create_model(&st.db, name, &f.provider, &aliases).await {
        Ok(id) => id,
        Err(e) if is_unique_violation(&e) => {
            return bad(format!(
                "A Model named {name} already exists. Choose another name, or open that Model to change it."
            ));
        }
        Err(e) => return internal(e),
    };
    if let Some(rates) = &rates {
        tryr!(db::add_cost_card(&st.db, id, rates).await);
    }
    tryr!(st.reload_catalog().await);
    let message = match &rates {
        Some(r) => format!("Model {name} added.{}", zero_note(&r.base)),
        None if !simple => format!(
            "Model {name} added with no Cost Card. Its requests are Unpriced until you add a Cost Card."
        ),
        None => format!("Model {name} added."),
    };
    saved(&format!("/admin/models/{id}"), &message)
}

async fn model_detail(_: Admin, State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    let cat = st.catalog();
    let Some(m) = cat.models.iter().find(|m| m.id == id) else {
        return page_error_with(
            StatusCode::NOT_FOUND,
            "Model not found",
            "No Model has this ID. It was deleted, or the address is wrong.",
            ("/admin/models", "Go to Models"),
        );
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
        Base::new(&st),
        m,
        cat.default_markup_bp,
        &history,
        &teams,
        providers,
    ))
}

#[derive(Deserialize)]
struct ModelForm {
    #[serde(default)]
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
    let _guard = st.settings_write.lock().await;
    if !st
        .router_table()
        .providers
        .iter()
        .any(|p| p.name == f.provider)
    {
        return bad("Unknown Provider. Reload the page and choose a Provider from the list.");
    }
    let Some(name) = st
        .catalog()
        .models
        .iter()
        .find(|m| m.id == id)
        .map(|m| m.name.clone())
    else {
        return bad("This Model does not exist any more. Go to Models and reload the page.");
    };
    let aliases = csv(&f.aliases);
    if let Err(r) = check_names(&st, Some(id), &name, &aliases) {
        return r;
    }
    tryr!(db::update_model(&st.db, id, &f.provider, &aliases).await);
    tryr!(st.reload_catalog().await);
    saved(
        &format!("/admin/models/{id}"),
        &format!("Model {name} saved."),
    )
}

async fn set_cost(
    _: Admin,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<RatesForm>,
) -> Response {
    let rates = match f.card(CardUse::Cost) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return bad(
                "Enter the rates. A Cost Card needs at least the Input and Output rates, in $ per million tokens.",
            );
        }
        Err(r) => return r,
    };
    tryr!(db::add_cost_card(&st.db, id, &rates).await);
    tryr!(st.reload_catalog().await);
    saved(
        &format!("/admin/models/{id}#cost"),
        &format!(
            "Cost Card saved as a new Card Version.{}",
            zero_note(&rates.base)
        ),
    )
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
            "markup" => {
                let t = self.markup_percent.trim();
                if t.is_empty() {
                    return Err(bad(
                        "Enter the Markup in percent, for example 25. Or choose another option.",
                    ));
                }
                let bp = percent(t)?;
                if bp < -10_000 {
                    return Err(bad(
                        "A Markup below -100% gives a negative Price. Enter -100 or more.",
                    ));
                }
                Ok(Some(PriceSpec::MarkupBp(bp)))
            }
            "rates" => match self.rates.card(CardUse::Price)? {
                Some(r) => Ok(Some(PriceSpec::Rates(r))),
                None => Err(bad(
                    "Enter the Price rates: at least Input and Output, in $ per million tokens. Or choose another option.",
                )),
            },
            _ => Err(bad("Choose how to set the Price.")),
        }
    }
}

/// A note for the success message when explicit Price rates are not more
/// than the Cost Card rates.
fn margin_note(st: &AppState, id: i64, spec: Option<&PriceSpec>) -> &'static str {
    let Some(PriceSpec::Rates(p)) = spec else {
        return "";
    };
    let cat = st.catalog();
    let Some((_, c)) = cat
        .models
        .iter()
        .find(|m| m.id == id)
        .and_then(|m| m.cost.as_ref())
    else {
        return "";
    };
    let (p, c) = (&p.base, &c.base);
    if p.input < c.input || p.output < c.output {
        " The Price is lower than the Cost for Input or Output."
    } else if p.input == c.input && p.output == c.output {
        " The Price is the same as the Cost, so there is no margin."
    } else {
        ""
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
    match &spec {
        Some(s) => tryr!(db::add_price_card(&st.db, id, None, s).await),
        None => tryr!(db::add_null_price_card(&st.db, id, None).await),
    };
    tryr!(st.reload_catalog().await);
    saved(
        &format!("/admin/models/{id}#price"),
        &format!(
            "Price Card saved as a new Card Version.{}",
            margin_note(&st, id, spec.as_ref())
        ),
    )
}

async fn set_team_price(
    _: Admin,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<PriceForm>,
) -> Response {
    let Some(team) = team_id(&f.team_id) else {
        return bad("Choose a Team.");
    };
    let spec = match f.spec() {
        Ok(s) => s,
        Err(r) => return r,
    };
    let message = match &spec {
        Some(s) => {
            tryr!(db::add_price_card(&st.db, id, Some(team), s).await);
            "Team override saved."
        }
        None => {
            tryr!(db::add_null_price_card(&st.db, id, Some(team)).await);
            "Team override removed. The Team pays the Model Price."
        }
    };
    tryr!(st.reload_catalog().await);
    saved(
        &format!("/admin/models/{id}#team-prices"),
        &format!("{message}{}", margin_note(&st, id, spec.as_ref())),
    )
}

#[derive(Deserialize)]
struct SettingsForm {
    #[serde(default)]
    default_markup_percent: String,
}

/// The one editor of the Default Markup (on the Models page).
async fn set_settings(
    _: Admin,
    State(st): State<AppState>,
    Form(f): Form<SettingsForm>,
) -> Response {
    let _guard = st.settings_write.lock().await;
    let value = f.default_markup_percent.trim();
    if value.is_empty() {
        return bad("Enter the Default Markup in percent, for example 20.");
    }
    let mut cfg = tryr!(crate::settings::load(&st.db, &st.secret_key).await).0;
    cfg.server.default_markup_percent = match value.parse() {
        Ok(v) => v,
        Err(_) => return bad("Enter the Default Markup as a number, for example 20."),
    };
    if let Err(e) = cfg.validate() {
        return bad(e.to_string());
    }
    let bp = match percent(value) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if bp < -10_000 {
        return bad("A Markup below -100% gives a negative Price. Enter -100 or more.");
    }
    tryr!(db::set_default_markup_bp(&st.db, bp).await);
    tryr!(st.reload_catalog().await);
    saved("/admin/models#markup", "Default Markup saved.")
}

// ----------------------------------------------------------------- ledger

#[derive(Deserialize, Default)]
struct LedgerQ {
    filter: Option<String>,
}

async fn ledger(_: Admin, State(st): State<AppState>, Query(q): Query<LedgerQ>) -> Response {
    let base = Base::new(&st);
    let (filter, name) = match q
        .filter
        .as_deref()
        .filter(|f| !base.simple || *f == "errors")
    {
        Some("unpriced") => (LedgerFilter::Unpriced, "unpriced"),
        Some("estimated") => (LedgerFilter::Estimated, "estimated"),
        Some("errors") => (LedgerFilter::Errors, "errors"),
        _ => (LedgerFilter::All, "all"),
    };
    let rows = tryr!(db::recent_ledger(&st.db, filter, 200).await);
    render(LedgerPage::build(base, &rows, name, None))
}

async fn reprice(_: Admin, State(st): State<AppState>) -> Response {
    if st.settings().mode == "simple" {
        return Redirect::to("/admin/ledger").into_response();
    }
    let n = tryr!(db::reprice_unpriced(&st.db, &st.catalog()).await);
    let rows = tryr!(db::recent_ledger(&st.db, LedgerFilter::Unpriced, 200).await);
    render(LedgerPage::build(
        Base::new(&st),
        &rows,
        "unpriced",
        Some(format!("Priced {n} entries.")),
    ))
}
