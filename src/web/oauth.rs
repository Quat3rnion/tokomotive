//! Operator-only OAuth forms. No response includes credential material.
use super::*;
use crate::oauth::Flow;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/providers/{name}/oauth/start", post(start))
        .route("/admin/providers/{name}/oauth/finish", post(finish))
        .route("/admin/providers/{name}/oauth/disconnect", post(disconnect))
        .route("/admin/providers/{name}/usage/start", post(usage_start))
        .route("/admin/providers/{name}/usage/finish", post(usage_finish))
        .route(
            "/admin/providers/{name}/usage/disconnect",
            post(usage_disconnect),
        )
}
fn flow(st: &AppState, name: &str) -> Option<Flow> {
    st.router_table()
        .providers
        .iter()
        .find(|p| p.name == name)
        .and_then(|p| Flow::from_auth(p.auth))
}
#[derive(Template)]
#[template(
    source = "<section class=\"panel\"><h2>Sign in</h2><p>Open the link in a new tab. Complete sign-in. Paste the final redirect URL (ChatGPT or Codex) or code#state (Claude). The attempt expires in 10 minutes.</p><a href=\"{{ url }}\" target=\"_blank\" rel=\"noopener noreferrer\">Open sign-in page</a><form method=\"post\" action=\"/admin/providers/{{ name }}/{{ path }}/finish\"><label>Redirect URL or code#state <input name=\"callback\" type=\"password\" required autocomplete=\"off\" style=\"width:400px\"></label><button>Connect</button></form></section>",
    ext = "html"
)]
struct AttemptPage {
    name: String,
    path: &'static str,
    url: String,
}
async fn begin(st: AppState, name: String, flow: Flow, path: &'static str) -> Response {
    match st.oauth.start(&name, flow).await {
        Ok(a) => render(AttemptPage {
            name,
            path,
            url: a.authorize_url,
        }),
        Err(_) => bad("Cannot start sign-in for this Provider."),
    }
}
async fn start(_: Admin, State(st): State<AppState>, Path(name): Path<String>) -> Response {
    let Some(flow) = flow(&st, &name) else {
        return bad("This Provider does not use OAuth.");
    };
    begin(st, name, flow, "oauth").await
}
async fn usage_start(_: Admin, State(st): State<AppState>, Path(name): Path<String>) -> Response {
    begin(st, name, Flow::Codex, "usage").await
}
#[derive(Deserialize)]
struct CallbackForm {
    callback: String,
}
async fn exchange(st: AppState, name: String, flow: Flow, callback: String) -> Response {
    match st.oauth.finish(&name, flow, &callback).await {
        Ok(()) => Redirect::to("/admin/providers").into_response(),
        // Manager errors have fixed messages. Never echo callback input or an issuer body.
        Err(e) => bad(e.to_string()),
    }
}
async fn finish(
    _: Admin,
    State(st): State<AppState>,
    Path(name): Path<String>,
    Form(f): Form<CallbackForm>,
) -> Response {
    let Some(flow) = flow(&st, &name) else {
        return bad("This Provider does not use OAuth.");
    };
    exchange(st, name, flow, f.callback).await
}
async fn usage_finish(
    _: Admin,
    State(st): State<AppState>,
    Path(name): Path<String>,
    Form(f): Form<CallbackForm>,
) -> Response {
    exchange(st, name, Flow::Codex, f.callback).await
}
async fn disconnect(_: Admin, State(st): State<AppState>, Path(name): Path<String>) -> Response {
    let Some(flow) = flow(&st, &name) else {
        return bad("This Provider does not use OAuth.");
    };
    tryr!(st.oauth.disconnect(&name, flow).await);
    Redirect::to("/admin/providers").into_response()
}
async fn usage_disconnect(
    _: Admin,
    State(st): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    tryr!(st.oauth.disconnect(&name, Flow::Codex).await);
    Redirect::to("/admin/providers").into_response()
}
