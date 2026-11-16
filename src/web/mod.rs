//! The Operator dashboard: server-rendered HTML with askama and htmx.
// Form helpers return a ready error page as the Err value.
#![allow(clippy::result_large_err)]

mod chart;
mod views;

use askama::Template;
use axum::Router;
use axum::extract::{FromRequest, FromRequestParts, OriginalUri, Path, Query, Request, State};
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
use crate::pagination::{PageQuery, Pager};
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
macro_rules! tryform {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return form_error(e),
        }
    };
}

pub(crate) mod accounts;
mod audit;
mod guardrails;
mod live;
mod mcp;
mod models_dev;
mod oauth;
mod oidc;
mod pickers;
mod quota;
mod route_rules;
mod secrets;
mod settings;

pub fn router(st: AppState) -> Router<AppState> {
    Router::new()
        .route("/admin", get(overview))
        .route("/admin/login", get(login_page).post(login))
        .route("/admin/logout", post(logout))
        .route("/admin/assets/htmx.min.js", get(htmx))
        .route(
            "/admin/assets/secret-store-form.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../../assets/secret-store-form.js"),
                )
            }),
        )
        .route(
            "/admin/assets/secret-picker.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../../assets/secret-picker.js"),
                )
            }),
        )
        .route("/admin/assets/favicon.svg", get(favicon))
        .route("/admin/assets/ibm-plex-sans-latin.woff2", get(font_latin))
        .route(
            "/admin/assets/ibm-plex-sans-latin-ext.woff2",
            get(font_latin_ext),
        )
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
        .route("/admin/ledger/live", get(live::watch))
        .route("/admin/usage", get(usage))
        .route("/admin/search/models", get(search_models))
        .route("/admin/search/{kind}", get(pickers::search))
        .route("/admin/reprice", post(reprice))
        .merge(settings::router())
        .merge(secrets::router())
        .merge(guardrails::router())
        .merge(route_rules::router())
        .merge(models_dev::router())
        .merge(mcp::router())
        .merge(oauth::router())
        .merge(accounts::router())
        .merge(oidc::router())
        .fallback(not_found)
        .layer(axum::middleware::from_fn_with_state(
            st,
            accounts::authorize,
        ))
        .layer(axum::middleware::from_fn(accounts::read_body))
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
    pub role: String,
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
        role: String::new(),
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
    /// The `name` of the form field that the message is about.
    field: Option<String>,
}

fn user_error(status: StatusCode, message: impl Into<String>) -> Response {
    let mut r = status.into_response();
    r.extensions_mut().insert(UserError {
        heading: None,
        message: message.into(),
        back: None,
        field: None,
    });
    r
}

/// The error for a form that the Operator can correct: status 400, shown
/// next to the submitted form. Write a full sentence that says what to do.
pub(crate) fn form_error(e: impl std::fmt::Display) -> Response {
    user_error(StatusCode::BAD_REQUEST, e.to_string())
}

/// A [`form_error`] about one field: the dashboard JS marks the field
/// with the `name` `field` and shows the message next to it.
pub(crate) fn field_error(field: &str, e: impl std::fmt::Display) -> Response {
    let mut r = form_error(e);
    if let Some(err) = r.extensions_mut().get_mut::<UserError>() {
        err.field = Some(field.into());
    }
    r
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
        field: None,
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
///
/// The model picker sends one field per chip plus the text field, so the
/// repeated values of [`MULTI_FIELDS`] are joined with ", " into one value
/// before parsing. A form with one comma-separated value is unchanged.
pub(crate) struct Form<T>(pub T);

/// Comma-separated list fields that also arrive as repeated fields.
pub(crate) const MULTI_FIELDS: &[&str] = &["aliases", "allowed_models", "fallbacks"];

/// Join the repeated values of [`MULTI_FIELDS`] into the first one, in order.
pub(crate) fn join_multi(pairs: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::with_capacity(pairs.len());
    for (k, v) in pairs {
        if MULTI_FIELDS.contains(&k.as_str())
            && let Some((_, first)) = out.iter_mut().find(|(n, _)| *n == k)
        {
            let v = v.trim();
            if !v.is_empty() {
                if !first.trim().is_empty() {
                    first.push_str(", ");
                }
                first.push_str(v);
            }
            continue;
        }
        out.push((k, v));
    }
    out
}

impl<T, S> FromRequest<S> for Form<T>
where
    T: serde::de::DeserializeOwned,
    axum::Form<T>: FromRequest<S, Rejection = axum::extract::rejection::FormRejection>,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, st: &S) -> Result<Self, Self::Rejection> {
        let incomplete = |status: StatusCode| {
            user_error(
                status,
                "The form is incomplete. Reload the page and try again.",
            )
        };
        let form_body = req
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/x-www-form-urlencoded"));
        if req.method() == axum::http::Method::GET || !form_body {
            return match axum::Form::<T>::from_request(req, st).await {
                Ok(axum::Form(v)) => Ok(Form(v)),
                Err(e) => {
                    tracing::warn!(error = %e, "dashboard form rejected");
                    Err(incomplete(e.status()))
                }
            };
        }
        let body = match axum::body::Bytes::from_request(req, st).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "dashboard form body rejected");
                return Err(incomplete(e.status()));
            }
        };
        let parsed = serde_urlencoded::from_bytes::<Vec<(String, String)>>(&body)
            .map_err(|e| e.to_string())
            .and_then(|pairs| {
                serde_urlencoded::to_string(join_multi(pairs)).map_err(|e| e.to_string())
            })
            .and_then(|text| serde_urlencoded::from_str::<T>(&text).map_err(|e| e.to_string()));
        match parsed {
            Ok(v) => Ok(Form(v)),
            Err(e) => {
                tracing::warn!(error = %e, "dashboard form rejected");
                Err(incomplete(StatusCode::UNPROCESSABLE_ENTITY))
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
            Html(match &err.field {
                Some(f) => format!("<p data-field=\"{}\">{}</p>", esc(f), esc(&err.message)),
                None => format!("<p>{}</p>", esc(&err.message)),
            }),
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
    // The page that sent the htmx request, without its #fragment.
    let current = req
        .headers()
        .get("hx-current-url")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<axum::http::Uri>().ok())
        .and_then(|u| u.path_and_query().map(|pq| pq.as_str().to_owned()))
        .or_else(|| referer.clone());
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
        // Not for another page: the reload would load the old address.
        if to.to_str().is_ok_and(|v| {
            v.split_once('#')
                .is_some_and(|(page, _)| current.as_deref().is_none_or(|c| c == page))
        }) {
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
    if !matches!(
        *req.method(),
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    ) || req.uri().path() == "/admin/ledger/live"
    {
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

/// IBM Plex Sans Variable (SIL OFL 1.1, assets/fonts/OFL.txt). The files do
/// not change between releases, so browsers may keep them for a year.
fn font(bytes: &'static [u8]) -> Response {
    (
        [
            (header::CONTENT_TYPE, "font/woff2"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        bytes,
    )
        .into_response()
}

async fn font_latin() -> Response {
    font(include_bytes!(
        "../../assets/fonts/ibm-plex-sans-latin.woff2"
    ))
}

async fn font_latin_ext() -> Response {
    font(include_bytes!(
        "../../assets/fonts/ibm-plex-sans-latin-ext.woff2"
    ))
}

// -------------------------------------------------------------------- auth

/// Extractor: present only for a logged-in Operator.
pub struct Admin(pub(crate) crate::admin::Account);

impl FromRequestParts<AppState> for Admin {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        if !crate::admin::configured(&st.db).await.map_err(internal)? {
            return Err(Redirect::to("/setup").into_response());
        }
        if let Some(account) = parts.extensions.get::<crate::admin::Account>() {
            Ok(Admin(account.clone()))
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
    if !tryr!(crate::admin::configured(&st.db).await) {
        return Redirect::to("/setup").into_response();
    }
    render(LoginPage {
        base: Base::anonymous(),
        error: None,
        sso: tryr!(oidc::label(&st).await),
    })
}

#[derive(Deserialize)]
struct LoginForm {
    #[serde(default)]
    username: String,
    password: String,
}

async fn login(
    State(st): State<AppState>,
    peer: Result<
        axum::extract::ConnectInfo<std::net::SocketAddr>,
        axum::extract::rejection::ExtensionRejection,
    >,
    Form(f): Form<LoginForm>,
) -> Response {
    if !tryr!(crate::admin::configured(&st.db).await) {
        return Redirect::to("/setup").into_response();
    }
    let username = f.username.trim();
    let username = if username.is_empty() {
        "admin"
    } else {
        username
    };
    let ip = peer
        .ok()
        .map(|p| p.0.ip().to_string())
        .unwrap_or_else(|| "local".into());
    if !st
        .dashboard
        .login
        .lock()
        .expect("login limiter")
        .allow(&ip, username, db::now_s())
    {
        let mut response = page_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many login attempts. Wait 5 minutes and try again.",
        );
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("300"));
        return response;
    }
    let account = tryr!(crate::admin::by_username(&st.db, username).await);
    // Verify a valid dummy hash for unknown usernames to avoid a cheap enumeration oracle.
    let hash = account
        .as_ref()
        .map(|a| a.password_hash.as_str())
        .unwrap_or(&st.dashboard.dummy_hash);
    let local: bool = if let Some(a) = &account {
        tryr!(
            sqlx::query_scalar("SELECT local_login FROM admin_accounts WHERE id=?")
                .bind(a.id)
                .fetch_one(&st.db)
                .await
        )
    } else {
        false
    };
    if !crate::settings::verify_password(&f.password, hash) || !local {
        let mut r = render(LoginPage {
            base: Base::anonymous(),
            error: Some("Incorrect username or password. Try again.".into()),
            sso: tryr!(oidc::label(&st).await),
        });
        *r.status_mut() = StatusCode::UNAUTHORIZED;
        return r;
    }
    let account = account.expect("verified account");
    let cookie = tryr!(start_session(&st, &account).await);
    let mut response = ([(header::SET_COOKIE, cookie)], Redirect::to("/admin")).into_response();
    response.extensions_mut().insert(account);
    response
}

/// Creates a session for the account's verified hash and version, then returns
/// its Set-Cookie value. Login and Setup use it.
async fn start_session(st: &AppState, account: &crate::admin::Account) -> anyhow::Result<String> {
    let mut b = [0u8; 32];
    rand::rng().fill_bytes(&mut b);
    let token = hex::encode(b);
    crate::admin::create_session(&st.db, &token, st.settings().session_days, account).await?;
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
        Ok(s) => Html(accounts::visible_forms(s, &ctx().role)).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "template render failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "template error").into_response()
        }
    }
}

fn period_field(value: Option<&str>, monthly: bool) -> anyhow::Result<String> {
    let value = value
        .filter(|v| !v.is_empty())
        .unwrap_or(if monthly { "monthly" } else { "none" });
    crate::budgets::Period::parse(value)?;
    Ok(value.to_owned())
}
#[derive(Default, Deserialize)]
struct BudgetWindowForm {
    budget_window: Option<String>,
    budget_days: Option<String>,
}
impl BudgetWindowForm {
    fn parse(&self) -> anyhow::Result<(&str, i64)> {
        let kind = self
            .budget_window
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("calendar");
        let days = self
            .budget_days
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("30")
            .parse::<i64>()
            .map_err(|_| anyhow::anyhow!("Enter rolling days from 1 to 366."))?;
        crate::budgets::Window::parse(kind, days)?;
        Ok((kind, days))
    }
}
fn parse_expiry(value: &str) -> anyhow::Result<Option<i64>> {
    if value.trim().is_empty() {
        return Ok(None);
    }
    let date = chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S"))
        .map_err(|_| anyhow::anyhow!("Enter the expiry date and time in UTC."))?;
    Ok(Some(date.and_utc().timestamp()))
}
async fn set_budget(
    st: &AppState,
    table: &str,
    id: i64,
    budget: Option<i64>,
    period: &str,
    window: (&str, i64),
) -> anyhow::Result<()> {
    sqlx::query(&format!(
        "UPDATE {table} SET budget_micros=?, budget_period=?, budget_window=?, budget_days=? WHERE id=?"
    ))
    .bind(budget)
    .bind(period)
    .bind(window.0)
    .bind(window.1)
    .bind(id)
    .execute(&st.db)
    .await?;
    Ok(())
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
    let cache_hits = tryr!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM ledger WHERE cache_hit=1 AND semantic_cache_hit=0 AND ts_ms>=?"
        )
        .bind(since.saturating_mul(1000))
        .fetch_one(&st.db)
        .await
    );
    let semantic_cache_hits = tryr!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM ledger WHERE semantic_cache_hit=1 AND ts_ms>=?"
        )
        .bind(since.saturating_mul(1000))
        .fetch_one(&st.db)
        .await
    );
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
    let mut page = OverviewPage::build(
        Base::new(&st),
        range,
        OverviewData {
            cache_hits,
            semantic_cache_hits,
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
    );
    let (attempts, retried): (i64, i64) = tryr!(
        sqlx::query_as(
            "SELECT IFNULL(SUM(attempts),0),IFNULL(SUM(attempts>1),0) FROM ledger WHERE ts_ms>=?"
        )
        .bind(since * 1000)
        .fetch_one(&st.db)
        .await
    );
    page.model_price_alerts = tryr!(models_dev::alerts(&st).await);
    // Overview: only accounts that report quota or need attention. The
    // Providers page keeps the full list.
    page.quota = tryr!(quota::views(&st, None, 200).await)
        .into_iter()
        .filter(|a| !a.windows.is_empty() || a.skipped || a.state != "ok")
        .take(10)
        .collect();
    for (label, value) in &mut page.more {
        if *label == "Attempts" {
            *value = int(attempts);
        }
        if *label == "Retried requests" {
            *value = int(retried);
        }
    }
    render(page)
}

#[derive(Deserialize, Default)]
struct UsageQ {
    #[serde(flatten)]
    page: PageQuery,
    group: Option<String>,
    range: Option<String>,
    sort: Option<String>,
    direction: Option<String>,
    /// Only requests for this upstream or requested model name.
    model: Option<String>,
}

#[derive(Template)]
#[template(path = "usage.html")]
struct UsagePage {
    base: Base,
    pager: Pager,
    rows: Vec<GroupView>,
    group: String,
    range: String,
    sort: String,
    direction: &'static str,
    limit: i64,
    model: String,
    sort_options: Vec<(&'static str, &'static str)>,
    headers: Vec<(&'static str, String)>,
    money_headers: Vec<(&'static str, String)>,
}

async fn usage(
    _: Admin,
    State(st): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Query(q): Query<UsageQ>,
) -> Response {
    let base = Base::new(&st);
    let group = q.group.as_deref().unwrap_or("model");
    let range = q.range.as_deref().unwrap_or("24h");
    let mut columns = vec![
        ("Name", "key"),
        ("Requests", "requests"),
        ("Errors", "errors"),
        ("Input", "input"),
        ("Output", "output"),
        ("Latency", "latency"),
    ];
    if !base.simple {
        columns.extend([
            ("Price", "price_micros"),
            ("Cost", "cost_micros"),
            ("Profit", "profit"),
        ]);
    }
    let model = q
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_owned);
    let requested_sort = q.sort.as_deref().unwrap_or("requests");
    let sort = columns
        .iter()
        .find_map(|(_, column)| (*column == requested_sort).then_some(*column))
        .unwrap_or("requests");
    let mut rows = tryr!(
        db::usage_page(
            &st.db,
            Range::parse(Some(range)).since(),
            group,
            sort,
            q.direction.as_deref() == Some("asc"),
            &q.page,
            model.as_deref(),
        )
        .await
    );
    let pager = Pager::offset(&uri.to_string(), &q.page, rows.len());
    rows.truncate(q.page.limit() as usize);
    let header = |(label, column): (&'static str, &str)| {
        let direction = if sort == column && q.direction.as_deref() != Some("asc") {
            "asc"
        } else {
            "desc"
        };
        (
            label,
            crate::pagination::link(
                &uri.to_string(),
                &[
                    ("page", None),
                    ("sort", Some(column.into())),
                    ("direction", Some(direction.into())),
                ],
            ),
        )
    };
    render(UsagePage {
        base,
        pager,
        rows: rows.iter().map(GroupView::new).collect(),
        group: group.into(),
        range: range.into(),
        sort: sort.into(),
        direction: if q.direction.as_deref() == Some("asc") {
            "asc"
        } else {
            "desc"
        },
        limit: q.page.limit(),
        model: model.unwrap_or_default(),
        headers: columns.iter().take(6).copied().map(header).collect(),
        money_headers: columns.iter().skip(6).copied().map(header).collect(),
        sort_options: columns,
    })
}

// ------------------------------------------------------------------ users

#[derive(Default, Deserialize)]
struct ListQ {
    #[serde(flatten)]
    page: PageQuery,
    team: Option<i64>,
    user: Option<String>,
    revoked_after: Option<i64>,
    revoked_before: Option<i64>,
    overrides_after: Option<i64>,
    overrides_before: Option<i64>,
    /// Models: the Provider that "Add a Model" preselects.
    provider: Option<String>,
}

#[derive(Deserialize, Default)]
struct ModelSearchQ {
    #[serde(default)]
    q: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    limit: String,
    /// Comma-separated source names, for example `route,model`.
    #[serde(default)]
    src: String,
}

/// One option of the model picker list.
struct ModelOption {
    id: String,
    source: &'static str,
    tag: &'static str,
    meta: String,
}

#[derive(Template)]
#[template(path = "model_options.html")]
struct ModelOptions {
    hits: Vec<ModelOption>,
    note: String,
}

/// "128K" or "1M" tokens.
fn context_text(n: u64) -> String {
    if n >= 1_000_000 {
        let tenths = (n + 50_000) / 100_000;
        if tenths.is_multiple_of(10) {
            format!("{}M", tenths / 10)
        } else {
            format!("{}.{}M", tenths / 10, tenths % 10)
        }
    } else if n >= 1000 {
        format!("{}K", (n + 500) / 1000)
    } else {
        n.to_string()
    }
}

/// The model picker search: `<li role="option">` items for the picker and
/// htmx, or JSON for `Accept: application/json`.
async fn search_models(
    _: Admin,
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ModelSearchQ>,
) -> Response {
    use crate::model_search::{DEFAULT_LIMIT, SearchCtx};
    let text: String = q.q.trim().chars().take(200).collect();
    let provider = Some(q.provider.trim().to_owned()).filter(|p| !p.is_empty());
    let scoped = provider.is_some();
    let only = Some(csv(&q.src)).filter(|v| !v.is_empty());
    let limit = q.limit.trim().parse().unwrap_or(DEFAULT_LIMIT);
    let ctx = SearchCtx {
        provider,
        state: st.clone(),
    };
    let hits = st
        .model_sources
        .search(&ctx, &text, limit, only.as_deref())
        .await;
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !headers.contains_key("hx-request") && !accept.contains("text/html") {
        return axum::Json(hits).into_response();
    }
    let hits = hits
        .into_iter()
        .map(|h| {
            let mut meta = Vec::new();
            if h.label != h.id {
                meta.push(h.label.clone());
            }
            if !scoped
                && let Some(p) = &h.provider
                && !h.id.starts_with(&format!("{p}/"))
            {
                meta.push(p.clone());
            }
            if let Some(c) = h.context {
                meta.push(format!("{} context", context_text(c)));
            }
            if let Some(p) = &h.price {
                meta.push(p.clone());
            }
            ModelOption {
                tag: if h.source == "route" { "info" } else { "" },
                source: h.source,
                id: h.id,
                meta: meta.join(", "),
            }
        })
        .collect();
    let note = st.model_sources.connection_note(ctx.provider.as_deref());
    let mut r = render(ModelOptions { hits, note });
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r.headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Accept, HX-Request"));
    r
}

async fn chosen_id(
    st: &AppState,
    kind: &str,
    value: &str,
    optional: bool,
    exact_name: bool,
) -> Result<Option<i64>, Response> {
    if optional && value.trim().is_empty() {
        return Ok(None);
    }
    let id = db::resolve_name(&st.db, kind, value, !exact_name)
        .await
        .map_err(internal)?;
    id.map(Some).ok_or_else(|| {
        bad(format!(
            "No {} matches this name. Enter an exact name from the {} page.",
            if kind == "teams" { "Team" } else { "User" },
            if kind == "teams" { "Teams" } else { "Users" }
        ))
    })
}

async fn users(
    _: Admin,
    State(st): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Query(q): Query<ListQ>,
) -> Response {
    let users = tryr!(db::users_page(&st.db, &q.page, q.team).await);
    let mut page = UsersPage::build(Base::new(&st), &users.items);
    for user in &mut page.users {
        user.budget_view =
            tryr!(views::BudgetView::load(&st, crate::budgets::Subject::User(user.id)).await);
        user.used = user.budget_view.used.clone();
        user.over = user.budget_view.over;
    }
    page.team_choices = tryr!(db::small_choices(&st.db, "teams").await);
    page.pager = Pager::keyset(&uri.to_string(), &q.page, &users);
    render(page)
}

#[derive(Deserialize)]
struct UserForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    team_id: String,
    team_name: Option<String>,
    #[serde(default)]
    budget_usd: String,
    monthly: Option<String>,
    budget_period: Option<String>,
    #[serde(flatten)]
    window: BudgetWindowForm,
    disabled: Option<String>,
    #[serde(flatten)]
    limits: LimitsForm,
}

async fn create_user(_: Admin, State(st): State<AppState>, Form(f): Form<UserForm>) -> Response {
    let window = tryform!(f.window.parse());
    let period = tryform!(period_field(
        f.budget_period.as_deref(),
        f.monthly.is_some()
    ));
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
    let team = match chosen_id(
        &st,
        "teams",
        f.team_name.as_deref().unwrap_or(&f.team_id),
        true,
        f.team_name.is_some(),
    )
    .await
    {
        Ok(id) => id,
        Err(r) => return r,
    };
    let id =
        match db::create_user_with_limits(&st.db, name, team, budget, period == "monthly", limits)
            .await
        {
            Ok(id) => id,
            Err(e) => {
                if is_unique_violation(&e) {
                    return bad(format!("A User named {name} already exists."));
                }
                return internal(e);
            }
        };
    tryr!(set_budget(&st, "users", id, budget, &period, window).await);
    saved("/admin/users", &format!("User {name} added."))
}

async fn update_user(
    _: Admin,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<UserForm>,
) -> Response {
    let simple = st.settings().mode == "simple";
    let window = tryform!(f.window.parse());
    let period = tryform!(period_field(
        f.budget_period.as_deref(),
        f.monthly.is_some()
    ));
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
    let team = match chosen_id(
        &st,
        "teams",
        f.team_name.as_deref().unwrap_or(&f.team_id),
        true,
        f.team_name.is_some(),
    )
    .await
    {
        Ok(id) => id,
        Err(r) => return r,
    };
    let mut tx = tryr!(db::begin_write(&st.db, "user_settings").await);
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
            .bind(team)
            .bind(f.disabled.is_some())
            .bind(id)
            .execute(&mut *tx)
            .await
    );
    if let Some((budget, limits)) = money {
        tryr!(
            sqlx::query(
                "UPDATE users SET budget_micros=?, budget_monthly=?, budget_period=?, budget_window=?, budget_days=?, rpm_limit=?, tpm_limit=? WHERE id=?"
            )
            .bind(budget)
            .bind(period=="monthly")
            .bind(&period)
            .bind(window.0)
            .bind(window.1)
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

async fn teams(
    _: Admin,
    State(st): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Query(q): Query<PageQuery>,
) -> Response {
    let teams = tryr!(db::teams_page(&st.db, &q).await);
    let mut page = TeamsPage::build(Base::new(&st), &teams.items, &st.catalog());
    for team in &mut page.teams {
        team.mcp_access = tryr!(crate::mcp::data::access(&st.db, "teams", team.id).await);
        team.budget_view =
            tryr!(views::BudgetView::load(&st, crate::budgets::Subject::Team(team.id)).await);
    }
    page.pager = Pager::keyset(&uri.to_string(), &q, &teams);
    render(page)
}

#[derive(Deserialize)]
struct NameForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    budget_usd: String,
    budget_period: Option<String>,
    #[serde(flatten)]
    window: BudgetWindowForm,
}

async fn create_team(_: Admin, State(st): State<AppState>, Form(f): Form<NameForm>) -> Response {
    let window = tryform!(f.window.parse());
    let period = tryform!(period_field(f.budget_period.as_deref(), false));
    let budget = match opt_budget(&f.budget_usd) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let name = f.name.trim();
    if name.is_empty() {
        return bad("A Team needs a name.");
    }
    let id = match db::create_team(&st.db, name).await {
        Ok(id) => id,
        Err(e) => {
            if is_unique_violation(&e) {
                return bad(format!("A Team named {name} already exists."));
            }
            return internal(e);
        }
    };
    if st.settings().mode != "simple" {
        tryr!(set_budget(&st, "teams", id, budget, &period, window).await);
    }
    saved("/admin/teams", &format!("Team {name} added."))
}

async fn rename_team(
    _: Admin,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<NameForm>,
) -> Response {
    let window = tryform!(f.window.parse());
    let period = tryform!(period_field(f.budget_period.as_deref(), false));
    let budget = match opt_budget(&f.budget_usd) {
        Ok(b) => b,
        Err(r) => return r,
    };
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
    if st.settings().mode != "simple" {
        tryr!(set_budget(&st, "teams", id, budget, &period, window).await);
    }
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

async fn keys_page(st: &AppState, new_key: Option<String>, q: ListQ, uri: &str) -> Response {
    let filter = match q.user.as_deref() {
        Some(v) if !v.is_empty() => match chosen_id(st, "users", v, false, false).await {
            Ok(id) => id,
            Err(r) => return r,
        },
        _ => None,
    };
    let active = tryr!(db::keys_page(&st.db, &q.page, filter, false).await);
    let revoked_q = PageQuery {
        after: q.revoked_after,
        before: q.revoked_before,
        ..q.page.clone()
    };
    let revoked = tryr!(db::keys_page(&st.db, &revoked_q, filter, true).await);
    let pager = Pager::keyset(uri, &q.page, &active);
    let mut revoked_pager = Pager::keyset(uri, &revoked_q, &revoked);
    revoked_pager.next = revoked.next.map(|id| {
        crate::pagination::link(
            uri,
            &[
                ("revoked_before", None),
                ("revoked_after", Some(id.to_string())),
            ],
        )
    });
    revoked_pager.prev = revoked.prev.map(|id| {
        crate::pagination::link(
            uri,
            &[
                ("revoked_after", None),
                ("revoked_before", Some(id.to_string())),
            ],
        )
    });
    let mut keys = active.items;
    keys.extend(revoked.items);
    let mut page = KeysPage::build(
        Base::new(st),
        KeysData {
            keys: &keys,
            new_key,
        },
    );
    page.pager = pager;
    for key in page.keys.iter_mut().chain(page.revoked.iter_mut()) {
        key.mcp_access = tryr!(crate::mcp::data::access(&st.db, "keys", key.id).await);
        key.budget_view =
            tryr!(views::BudgetView::load(st, crate::budgets::Subject::Key(key.id)).await);
        let expires: Option<i64> = tryr!(
            sqlx::query_scalar("SELECT expires_at FROM virtual_keys WHERE id=?")
                .bind(key.id)
                .fetch_one(&st.db)
                .await
        );
        key.expired = expires.is_some_and(|t| t <= db::now_s());
        key.expires = expires
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
            .map(|t| t.format("%Y-%m-%dT%H:%M").to_string())
            .unwrap_or_default();
    }
    page.provider_choices = tryr!(db::small_choices(&st.db, "providers").await);
    page.user_choices = tryr!(db::small_choices(&st.db, "users").await);
    page.revoked_pager = revoked_pager;
    page.pager.label = "Active key pages".into();
    page.revoked_pager.label = "Revoked key pages".into();
    page.revoked_open = q.revoked_after.is_some() || q.revoked_before.is_some();
    page.filter_user = match filter {
        Some(id) => tryr!(
            sqlx::query_scalar("SELECT name FROM users WHERE id=?")
                .bind(id)
                .fetch_optional(&st.db)
                .await
        ),
        None => None,
    };
    page.has_users = tryr!(
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users)")
            .fetch_one(&st.db)
            .await
    );
    render(page)
}

async fn keys(
    _: Admin,
    State(st): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Query(q): Query<ListQ>,
) -> Response {
    keys_page(&st, None, q, &uri.to_string()).await
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
        .flat_map(|(_, v)| csv(v))
        .collect();
    if let Err(r) = validate_provider_access(&st, &access) {
        return r;
    }
    let fields: std::collections::HashMap<_, _> = pairs.into_iter().collect();
    let window_form = BudgetWindowForm {
        budget_window: fields.get("budget_window").cloned(),
        budget_days: fields.get("budget_days").cloned(),
    };
    let window = tryform!(window_form.parse());
    let period = tryform!(period_field(
        fields.get("budget_period").map(String::as_str),
        false
    ));
    let budget = match opt_budget(
        fields
            .get("budget_usd")
            .map(String::as_str)
            .unwrap_or_default(),
    ) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let expires = tryform!(parse_expiry(
        fields
            .get("expires_at")
            .map(String::as_str)
            .unwrap_or_default()
    ));
    let user_id = match chosen_id(
        &st,
        "users",
        fields
            .get("user_name")
            .or_else(|| fields.get("user_id"))
            .map(String::as_str)
            .unwrap_or_default(),
        false,
        fields.contains_key("user_name"),
    )
    .await
    {
        Ok(Some(id)) => id,
        Ok(None) => unreachable!(),
        Err(r) => return r,
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
    tryr!(
        sqlx::query("UPDATE virtual_keys SET expires_at=? WHERE id=?")
            .bind(expires)
            .bind(id)
            .execute(&st.db)
            .await
    );
    if st.settings().mode != "simple" {
        tryr!(set_budget(&st, "virtual_keys", id, budget, &period, window).await);
    }
    keys_page(&st, Some(key), ListQ::default(), "/admin/keys").await
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
        .flat_map(|(_, v)| csv(v))
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
    let expires = tryform!(parse_expiry(&field("expires_at")));
    let window_form = BudgetWindowForm {
        budget_window: Some(field("budget_window")),
        budget_days: Some(field("budget_days")),
    };
    let window = tryform!(window_form.parse());
    let period = tryform!(period_field(Some(&field("budget_period")), false));
    let budget = match opt_budget(&field("budget_usd")) {
        Ok(b) => b,
        Err(r) => return r,
    };
    // Simple Mode hides Rate Limits; keep the stored values.
    if st.settings().mode != "simple" {
        let limits = match limits.parse() {
            Ok(l) => l,
            Err(r) => return r,
        };
        tryr!(db::update_key_limits(&st.db, id, limits).await);
        tryr!(set_budget(&st, "virtual_keys", id, budget, &period, window).await);
    }
    tryr!(
        sqlx::query("UPDATE virtual_keys SET expires_at=? WHERE id=?")
            .bind(expires)
            .bind(id)
            .execute(&st.db)
            .await
    );
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
        .flat_map(|(_, v)| csv(&v))
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

async fn models(
    _: Admin,
    State(st): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Query(q): Query<ListQ>,
) -> Response {
    let ids = tryr!(db::model_ids_page(&st.db, &q.page, q.team).await);
    let cat = st.catalog();
    let wanted: std::collections::HashSet<_> = ids.items.iter().copied().collect();
    let mut selected: Vec<_> = cat
        .models
        .iter()
        .filter(|m| wanted.contains(&m.id))
        .collect();
    selected.sort_by_key(|m| m.id);
    let providers = st
        .router_table()
        .providers
        .iter()
        .take(1)
        .map(|p| p.name.clone())
        .collect();
    let mut page = ModelsPage::build(Base::new(&st), &selected, cat.default_markup_bp, providers);
    page.provider_choices = tryr!(db::small_choices(&st.db, "providers").await);
    if let Some(p) = q.provider.as_deref().filter(|p| {
        st.router_table()
            .providers
            .iter()
            .any(|known| known.name == *p)
    }) {
        page.new_provider = p.into();
    }
    page.pager = Pager::keyset(&uri.to_string(), &q.page, &ids);
    render(page)
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
    service_units: String,
    #[serde(default)]
    batch_multiplier_bp: String,
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

const RATE_LABELS: [&str; 8] = [
    "Input",
    "Output",
    "Cache read",
    "Cache write 5m",
    "Cache write 1h",
    "Image input",
    "Image output",
    "Service units",
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

fn rates_of(v: [i64; 8]) -> Rates {
    Rates {
        input: v[0],
        output: v[1],
        cache_read: v[2],
        cache_write_5m: v[3],
        cache_write_1h: v[4],
        image_input: v[5],
        image_output: v[6],
        service_units: v[7],
    }
}

impl RatesForm {
    fn fields(&self) -> [&str; 8] {
        [
            &self.input,
            &self.output,
            &self.cache_read,
            &self.cache_write_5m,
            &self.cache_write_1h,
            &self.image_input,
            &self.image_output,
            &self.service_units,
        ]
    }

    /// The card in the form. All fields blank gives `None`: the caller
    /// decides what no card means, and never saves a $0 card for it. A card
    /// with Input and Output both 0 needs the `allow_zero` check box.
    fn card(&self, card: CardUse) -> Result<Option<CardRates>, Response> {
        if self.fields().iter().all(|s| s.trim().is_empty()) && self.tiers.trim().is_empty() {
            return Ok(None);
        }
        let mut v = [0; 8];
        for (i, s) in self.fields().iter().enumerate() {
            v[i] = rate(RATE_LABELS[i], s)?;
        }
        let base = rates_of(v);
        let tiers = parse_tiers(&self.tiers)?;
        if base.input == 0
            && base.output == 0
            && base.service_units == 0
            && self.allow_zero.is_empty()
        {
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
        let batch_multiplier_bp = if self.batch_multiplier_bp.trim().is_empty() {
            None
        } else {
            Some(
                self.batch_multiplier_bp
                    .trim()
                    .parse::<u16>()
                    .map_err(|_| {
                        bad("Batch multiplier must be whole basis points from 0 to 65535.")
                    })?,
            )
        };
        Ok(Some(CardRates {
            base,
            tiers,
            batch_multiplier_bp,
        }))
    }
}

/// One tier per line: `above_input_tokens input output cache_read
/// cache_write_5m cache_write_1h [image_input image_output]`.
fn parse_tiers(s: &str) -> Result<Vec<ContextTier>, Response> {
    const HELP: &str = "Write 6, 8 or 9 numbers: the input tokens above which the tier applies, then the Input, Output, Cache read, Cache write 5m and Cache write 1h rates, optionally Image input and Image output, then Service units.";
    let mut out = vec![];
    for (n, line) in s.lines().map(str::trim).enumerate() {
        let n = n + 1;
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() != 6 && parts.len() != 8 && parts.len() != 9 {
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
        let mut v = [0; 8];
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

async fn model_detail(
    _: Admin,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    OriginalUri(uri): OriginalUri,
    Query(mut q): Query<ListQ>,
) -> Response {
    let cat = st.catalog();
    let Some(m) = cat.models.iter().find(|m| m.id == id) else {
        return page_error_with(
            StatusCode::NOT_FOUND,
            "Model not found",
            "No Model has this ID. It was deleted, or the address is wrong.",
            ("/admin/models", "Go to Models"),
        );
    };
    q.page.limit = Some(20);
    let history = tryr!(db::history_page(&st.db, id, &q.page).await);
    let override_q = PageQuery {
        after: q.overrides_after,
        before: q.overrides_before,
        limit: Some(20),
        ..Default::default()
    };
    let teams = tryr!(db::override_teams_page(&st.db, id, &override_q).await);
    let mut page = ModelPage::build(
        Base::new(&st),
        m,
        cat.default_markup_bp,
        &history.items,
        &teams.items,
    );
    page.pager = Pager::keyset(&uri.to_string(), &q.page, &history);
    page.team_choices = tryr!(db::small_choices(&st.db, "teams").await);
    page.provider_choices = tryr!(db::small_choices(&st.db, "providers").await);
    page.pager.label = "Card history pages".into();
    page.models_dev = tryr!(models_dev::detail_view(&st, id).await);
    page.override_pager = Pager::keyset(&uri.to_string(), &override_q, &teams);
    page.override_pager.label = "Team override pages".into();
    page.override_pager.next = teams.next.map(|id| {
        crate::pagination::link(
            &uri.to_string(),
            &[
                ("overrides_before", None),
                ("overrides_after", Some(id.to_string())),
            ],
        )
    });
    page.override_pager.prev = teams.prev.map(|id| {
        crate::pagination::link(
            &uri.to_string(),
            &[
                ("overrides_after", None),
                ("overrides_before", Some(id.to_string())),
            ],
        )
    });
    render(page)
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
    team_name: Option<String>,
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
    let team = match chosen_id(
        &st,
        "teams",
        f.team_name.as_deref().unwrap_or(&f.team_id),
        false,
        f.team_name.is_some(),
    )
    .await
    {
        Ok(Some(id)) => id,
        Ok(None) => unreachable!(),
        Err(r) => return r,
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
    cursor: Option<String>,
    rule: Option<String>,
    status: Option<String>,
    error_class: Option<String>,
    provider_key: Option<String>,
    has_retries: Option<String>,
    entry: Option<i64>,
    filter: Option<String>,
    user: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    from: Option<String>,
    to: Option<String>,
    before: Option<String>,
    after: Option<i64>,
    limit: Option<i64>,
}

/// Ledger Entries per page.
const LEDGER_PAGE: i64 = 100;

fn some_text(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

impl LedgerQ {
    /// The Ledger address with these filters, another tab and a page.
    fn link(&self, filter: &str, before: Option<i64>) -> String {
        let mut url = reqwest::Url::parse("http://x/admin/ledger").expect("static URL");
        {
            let mut q = url.query_pairs_mut();
            if filter != "all" {
                q.append_pair("filter", filter);
            }
            for (k, v) in [
                ("user", &self.user),
                ("rule", &self.rule),
                ("status", &self.status),
                ("error_class", &self.error_class),
                ("provider_key", &self.provider_key),
                ("has_retries", &self.has_retries),
                ("provider", &self.provider),
                ("model", &self.model),
                ("from", &self.from),
                ("to", &self.to),
            ] {
                if let Some(v) = some_text(v) {
                    q.append_pair(k, v);
                }
            }
            if let Some(limit) = self.limit {
                q.append_pair("limit", &limit.clamp(1, 200).to_string());
            }
            if let Some(b) = before {
                q.append_pair("before", &b.to_string());
            }
        }
        match url.query() {
            Some(q) if !q.is_empty() => format!("/admin/ledger?{q}"),
            _ => "/admin/ledger".into(),
        }
    }
}

/// A `YYYY-MM-DD` day as Unix milliseconds at 00:00 UTC.
fn day_ms(s: &str) -> Option<i64> {
    let d = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
    Some(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis())
}

async fn ledger_query(
    st: &AppState,
    q: &LedgerQ,
    simple: bool,
) -> Result<(db::LedgerQuery, &'static str, Option<String>), Response> {
    let (filter, name) = match some_text(&q.filter) {
        None | Some("all") => (LedgerFilter::All, "all"),
        Some("errors") => (LedgerFilter::Errors, "errors"),
        Some("unpriced") if !simple => (LedgerFilter::Unpriced, "unpriced"),
        Some("estimated") if !simple => (LedgerFilter::Estimated, "estimated"),
        // Simple Mode has no money filters; an unknown filter shows all.
        Some(_) => return Err(Redirect::to(&q.link("all", None)).into_response()),
    };
    let mut error = None;
    let mut date = |v: &Option<String>, next_day: bool| {
        let v = some_text(v)?;
        match day_ms(v) {
            Some(ms) => Some(if next_day { ms + 86_400_000 } else { ms }),
            None => {
                error =
                    Some("Use the date format YYYY-MM-DD. The Ledger ignores this date.".into());
                None
            }
        }
    };
    let (from_ms, to_ms) = (date(&q.from, false), date(&q.to, true));
    let user_id = match chosen_id(
        st,
        "users",
        some_text(&q.user).unwrap_or_default(),
        true,
        false,
    )
    .await
    {
        Ok(id) => id,
        Err(_) => {
            error = Some("No User matches this name. Enter an exact User name.".into());
            Some(-1)
        }
    };
    let limit = q.limit.unwrap_or(LEDGER_PAGE).clamp(1, 200);
    let query = db::LedgerQuery {
        rule: some_text(&q.rule).map(str::to_owned),
        status: some_text(&q.status).and_then(|s| s.parse().ok()),
        error_class: some_text(&q.error_class).map(str::to_owned),
        provider_key: some_text(&q.provider_key)
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|i| *i > 0)
            .map(|i| i - 1),
        has_retries: some_text(&q.has_retries).map(|s| s == "true"),
        filter,
        user_id,
        provider: some_text(&q.provider).map(str::to_owned),
        model: some_text(&q.model).map(str::to_owned),
        from_ms,
        to_ms,
        before_id: some_text(&q.before).and_then(|b| b.parse().ok()),
        after_id: q.after,
        limit: limit + 1,
    };
    Ok((query, name, error))
}

async fn ledger(
    _: Admin,
    State(st): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Query(q): Query<LedgerQ>,
) -> Response {
    let base = Base::new(&st);
    let (query, name, error) = match ledger_query(&st, &q, base.simple).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let filter = query.filter;
    let user_id = query.user_id;
    let limit = query.limit - 1;
    let live_cursor = st.ledger_live.cursor();
    let mut rows = tryr!(db::ledger_window(&st.db, &query).await);
    let more = rows.len() as i64 > limit;
    rows.truncate(limit as usize);
    if q.after.is_some() {
        rows.reverse();
    }
    let next = if q.after.is_some() || more {
        rows.last().map(|r| {
            crate::pagination::link(
                &uri.to_string(),
                &[("after", None), ("before", Some(r.id.to_string()))],
            )
        })
    } else {
        None
    };
    let prev = if q.after.is_some() && more || q.after.is_none() && query.before_id.is_some() {
        rows.first().map(|r| {
            crate::pagination::link(
                &uri.to_string(),
                &[("before", None), ("after", Some(r.id.to_string()))],
            )
        })
    } else {
        None
    };
    let pager = Pager {
        label: "Ledger pages".into(),
        next,
        prev,
        shown: rows.len(),
        ..Default::default()
    };
    let labels = tryr!(settings::key_labels(&st, &rows).await);
    let key_label = |provider: &str, index: i64| labels.get(&(provider.to_owned(), index)).cloned();
    let unpriced_models = if filter == LedgerFilter::Unpriced {
        tryr!(db::unpriced_models(&st.db).await)
    } else {
        vec![]
    };
    let filters = LedgerFilterView {
        rule: q.rule.clone().unwrap_or_default(),
        status: q.status.clone().unwrap_or_default(),
        error_class: q.error_class.clone().unwrap_or_default(),
        provider_key: q.provider_key.clone().unwrap_or_default(),
        has_retries: q.has_retries.clone().unwrap_or_default(),
        provider_choices: tryr!(db::small_choices(&st.db, "providers").await),
        user_choices: tryr!(db::small_choices(&st.db, "users").await),
        user: q.user.clone().unwrap_or_default(),
        provider: query.provider.clone().unwrap_or_default(),
        model: query.model.clone().unwrap_or_default(),
        from: some_text(&q.from).unwrap_or_default().to_owned(),
        to: some_text(&q.to).unwrap_or_default().to_owned(),
        active: user_id.is_some()
            || query.rule.is_some()
            || query.status.is_some()
            || query.error_class.is_some()
            || query.provider_key.is_some()
            || query.has_retries.is_some()
            || query.provider.is_some()
            || query.model.is_some()
            || some_text(&q.from).is_some()
            || some_text(&q.to).is_some(),
    };
    render(LedgerPage {
        live_cursor,
        detail_id: q.entry,
        attempts: if let Some(id) = q.entry {
            tryr!(crate::observability::attempts(&st.db, id).await)
        } else {
            vec![]
        },
        // Captured bodies can hold prompts and personal data: Owners and
        // Admins only. A Viewer never causes a decryption.
        bodies: if let Some(id) = q.entry.filter(|_| ctx().role != "viewer") {
            tryr!(crate::observability::bodies(&st.db, &st.secret_key, id).await)
        } else {
            None
        },
        pager,
        rows: rows
            .iter()
            .map(|r| LedgerView::build(r, &key_label))
            .collect(),
        filter: name,
        filters,
        tab_all: q.link("all", None),
        tab_unpriced: q.link("unpriced", None),
        tab_estimated: q.link("estimated", None),
        tab_errors: q.link("errors", None),
        error,
        page_size: limit,
        unpriced_models,
        base,
    })
}

async fn reprice(_: Admin, State(st): State<AppState>) -> Response {
    if st.settings().mode == "simple" {
        return Redirect::to("/admin/ledger").into_response();
    }
    let n = tryr!(db::reprice_unpriced(&st.db, &st.catalog()).await);
    let message = match n {
        0 => "No entries were priced. The Models below still have no Cost Card.".to_owned(),
        1 => "Priced 1 entry.".to_owned(),
        n => format!("Priced {n} entries."),
    };
    saved("/admin/ledger?filter=unpriced", &message)
}
