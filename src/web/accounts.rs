//! Central role enforcement and account forms.
use super::*;
use std::collections::HashMap;
use std::sync::Mutex;

pub(crate) struct DashboardState {
    pub login: Mutex<LoginLimiter>,
    pub dummy_hash: String,
    pub mutations: tokio::sync::Mutex<()>,
}
impl DashboardState {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            login: Mutex::new(LoginLimiter::default()),
            dummy_hash: crate::settings::hash_password(&hex::encode(rand::random::<[u8; 32]>()))?,
            mutations: tokio::sync::Mutex::new(()),
        })
    }
}
#[derive(Default)]
pub(crate) struct LoginLimiter(HashMap<String, (i64, u8)>);
impl LoginLimiter {
    pub fn allow(&mut self, ip: &str, username: &str, now: i64) -> bool {
        self.0.retain(|_, (start, _)| now - *start < 300);
        let key = db::hash_key(&format!("{ip}:{}", username.to_ascii_lowercase()));
        if !self.0.contains_key(&key) && self.0.len() >= 1024 {
            return false;
        }
        let entry = self.0.entry(key).or_insert((now, 0));
        if entry.1 >= 5 {
            return false;
        }
        entry.1 += 1;
        true
    }
}

/// These are the only authenticated POST routes available to a Viewer.
pub(crate) const SELF_SERVICE: &[&str] = &["/admin/logout", "/admin/settings/password"];

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/settings/accounts", post(create_account))
        .route("/admin/settings/accounts/{id}", post(update_account))
        .route("/admin/settings/accounts/{id}/delete", post(delete_account))
        .route("/admin/audit", get(super::audit::page))
        .route("/admin/settings/audit", post(super::audit::retention))
}

pub(super) async fn authorize(
    State(st): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path().to_owned();
    if req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .is_none()
    {
        return next.run(req).await;
    }
    if !path.starts_with("/admin") && path != "/setup" {
        return next.run(req).await;
    }
    if path == "/setup"
        || path == "/admin/login"
        || path.starts_with("/admin/assets/")
        || path == "/favicon.ico"
        || path == "/.well-known/oauth-cimd"
    {
        if req.method() == axum::http::Method::POST
            && matches!(path.as_str(), "/setup" | "/admin/login")
        {
            if path == "/setup" && tryr!(crate::admin::configured(&st.db).await) {
                return next.run(req).await;
            }
            let _guard = if path == "/setup" {
                Some(st.dashboard.mutations.lock().await)
            } else {
                None
            };
            return super::audit::run(&st, req, next, None).await;
        }
        return next.run(req).await;
    }
    let mutation = !matches!(
        *req.method(),
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    );
    // Authenticate again only after earlier account changes have committed.
    let _guard = if mutation {
        Some(st.dashboard.mutations.lock().await)
    } else {
        None
    };
    if !tryr!(crate::admin::configured(&st.db).await) {
        return Redirect::to("/setup").into_response();
    }
    let account = match session_cookie(req.headers()) {
        Some(token) => tryr!(crate::admin::session(&st.db, &token).await),
        None => None,
    };
    let Some(account) = account else {
        return Redirect::to("/admin/login").into_response();
    };
    if account.role == "viewer"
        && ((mutation && !SELF_SERVICE.contains(&path.as_str())) || path == "/admin/audit")
    {
        return page_error(StatusCode::FORBIDDEN, "This account has read-only access.");
    }
    if path.starts_with("/admin/settings/accounts") && account.role != "owner" {
        return page_error(
            StatusCode::FORBIDDEN,
            "Only an Owner can manage admin accounts.",
        );
    }
    // Metrics credentials are Master-Key-sensitive. Ordinary server settings
    // remain available to Admin accounts, including old scripted forms.
    if mutation && path == "/admin/settings" && account.role != "owner" {
        let (parts, body) = req.into_parts();
        let bytes = match axum::body::to_bytes(body, 2 * 1024 * 1024).await {
            Ok(b) => b,
            Err(_) => return bad("The form is too large."),
        };
        let fields =
            serde_urlencoded::from_bytes::<Vec<(String, String)>>(&bytes).unwrap_or_default();
        if fields
            .iter()
            .any(|(k, v)| (k == "metrics_token" && !v.is_empty()) || k == "clear_metrics_token")
        {
            return page_error(
                StatusCode::FORBIDDEN,
                "Only an Owner can change the metrics token.",
            );
        }
        req = Request::from_parts(parts, axum::body::Body::from(bytes));
    }
    let mut context = ctx();
    context.role = account.role.clone();
    req.extensions_mut().insert(account.clone());
    if mutation {
        CTX.scope(context, super::audit::run(&st, req, next, Some(account)))
            .await
    } else {
        CTX.scope(context, next.run(req)).await
    }
}

#[derive(sqlx::FromRow)]
pub(super) struct AccountView {
    pub id: i64,
    pub username: String,
    pub role: String,
}
pub(super) async fn list(st: &AppState) -> anyhow::Result<Vec<AccountView>> {
    Ok(
        sqlx::query_as("SELECT id,username,role FROM admin_accounts ORDER BY username")
            .fetch_all(&st.db)
            .await?,
    )
}

#[derive(Deserialize)]
struct AccountForm {
    username: String,
    role: String,
    #[serde(default)]
    password: String,
}
fn valid_role(value: &str) -> bool {
    matches!(value, "owner" | "admin" | "viewer")
}
async fn create_account(
    _: Admin,
    State(st): State<AppState>,
    Form(f): Form<AccountForm>,
) -> Response {
    let username = tryform!(crate::admin::username(&f.username));
    if !valid_role(&f.role) {
        return bad("Choose Owner, Admin or Viewer.");
    }
    if f.password.chars().count() < 8 {
        return bad("Use at least 8 characters for the password.");
    }
    let hash = tryr!(crate::settings::hash_password(&f.password));
    match sqlx::query("INSERT INTO admin_accounts(username,password_hash,role) VALUES(?,?,?)")
        .bind(username)
        .bind(hash)
        .bind(f.role)
        .execute(&st.db)
        .await
    {
        Ok(_) => saved("/admin/settings#accounts", "Admin account added."),
        Err(e) => {
            let e: anyhow::Error = e.into();
            if is_unique_violation(&e) {
                bad("This username is already in use.")
            } else {
                internal(e)
            }
        }
    }
}
async fn update_account(
    _: Admin,
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<AccountForm>,
) -> Response {
    let username = tryform!(crate::admin::username(&f.username));
    if !valid_role(&f.role) {
        return bad("Choose Owner, Admin or Viewer.");
    }
    if !f.password.is_empty() && f.password.chars().count() < 8 {
        return bad("Use at least 8 characters for the password.");
    }
    let hash = tryr!(
        (!f.password.is_empty())
            .then(|| crate::settings::hash_password(&f.password))
            .transpose()
    );
    let result = sqlx::query("UPDATE admin_accounts SET username=?,role=?,password_hash=COALESCE(?,password_hash) WHERE id=?")
        .bind(username).bind(f.role).bind(hash).bind(id).execute(&st.db).await;
    account_result(result, "Admin account saved.")
}
async fn delete_account(_: Admin, State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    let result = sqlx::query("DELETE FROM admin_accounts WHERE id=?")
        .bind(id)
        .execute(&st.db)
        .await;
    account_result(result, "Admin account deleted.")
}
fn account_result(
    result: Result<sqlx::sqlite::SqliteQueryResult, sqlx::Error>,
    message: &str,
) -> Response {
    match result {
        Ok(r) if r.rows_affected() > 0 => saved("/admin/settings#accounts", message),
        Ok(_) => bad("Admin account not found. Reload the page."),
        Err(e) if e.to_string().contains("Keep at least one Owner") => {
            bad("Keep at least one Owner account.")
        }
        Err(e) => {
            let e: anyhow::Error = e.into();
            if is_unique_violation(&e) {
                bad("This username is already in use.")
            } else {
                internal(e)
            }
        }
    }
}

/// Remove mutation forms on the server, including fragments, for Viewers.
/// Authorization above is independent of this presentation-only pass.
pub(super) fn visible_forms(html: String, role: &str) -> String {
    if role != "viewer" {
        return html;
    }
    let mut out = String::with_capacity(html.len());
    let mut rest = html.as_str();
    while let Some(start) = rest.find("<form") {
        out.push_str(&rest[..start]);
        let form = &rest[start..];
        let Some(end) = form.find("</form>").map(|n| n + 7) else {
            out.push_str(form);
            return out;
        };
        let tag = form.split('>').next().unwrap_or_default();
        let read = tag.contains("method=\"get\"")
            || SELF_SERVICE
                .iter()
                .any(|p| tag.contains(&format!("action=\"{p}\"")));
        if read {
            out.push_str(&form[..end]);
        }
        rest = &form[end..];
    }
    out.push_str(rest);
    out
}
