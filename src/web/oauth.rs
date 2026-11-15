//! Operator-only OAuth forms. No response includes credential material.
use super::*;
use crate::oauth::Flow;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/.well-known/oauth-cimd", get(client_metadata))
        .route("/admin/providers/{name}/oauth/start", post(start))
        .route("/admin/providers/{name}/oauth/configure", post(configure))
        .route(
            "/admin/providers/{name}/oauth/service-account",
            post(service_account),
        )
        .route("/admin/providers/{name}/oauth/paste", post(paste))
        .route(
            "/admin/providers/{name}/oauth/device/status",
            post(device_status),
        )
        .route("/admin/providers/{name}/oauth/finish", post(finish))
        .route("/admin/providers/{name}/oauth/disconnect", post(disconnect))
        .route("/admin/providers/{name}/oauth/refresh", post(refresh))
        .route("/admin/providers/{name}/usage/start", post(usage_start))
        .route("/admin/providers/{name}/usage/paste", post(usage_paste))
        .route(
            "/admin/providers/{name}/usage/device/status",
            post(usage_device_status),
        )
        .route("/admin/providers/{name}/usage/finish", post(usage_finish))
        .route(
            "/admin/providers/{name}/usage/disconnect",
            post(usage_disconnect),
        )
}
async fn refresh(_: Admin, State(st): State<AppState>, Path(name): Path<String>) -> Response {
    let Some(flow) = flow(&st, &name) else {
        return bad("This Provider does not use OAuth.");
    };
    let result = st.oauth.refresh_now(&name, flow).await;
    st.oauth.emit_refresh_events(&st).await;
    match result {
        Ok(()) => saved("/admin/providers", "Sign-in refreshed."),
        Err(e) if e.is::<crate::oauth::RefreshRetrying>() => {
            form_error("Sign-in refresh is retrying. Wait for the next scheduled retry.")
        }
        Err(_) => form_error("Cannot refresh this credential. Check the sign-in status."),
    }
}
async fn client_metadata(State(st): State<AppState>) -> Response {
    match st.oauth.client_metadata().await {
        Ok(Some(value)) => axum::Json(value).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => internal(e),
    }
}
fn flow(st: &AppState, name: &str) -> Option<Flow> {
    st.router_table()
        .providers
        .iter()
        .find(|p| p.name == name)
        .and_then(|p| {
            Flow::from_auth(p.auth).or_else(|| {
                (p.auth == crate::provider::ProviderAuth::ApiKey
                    && matches!(
                        p.base_url.trim_end_matches('/'),
                        "https://openrouter.ai/api" | "https://openrouter.ai/api/v1"
                    ))
                .then_some(Flow::OpenRouter)
            })
        })
}
async fn configure(
    _: Admin,
    State(st): State<AppState>,
    Path(name): Path<String>,
    Form(options): Form<crate::oauth::ClientOptions>,
) -> Response {
    let Some(flow) = flow(&st, &name) else {
        return bad("This Provider does not use OAuth.");
    };
    if st.oauth.configure(&name, flow, options).await.is_err() {
        return form_error(
            "Check the client ID, secret and tenant settings. For CIMD, use the public HTTPS metadata URL with no saved secret.",
        );
    }
    saved(
        "/admin/providers",
        "OAuth client saved. Sign in to connect.",
    )
}
#[derive(Deserialize)]
struct ServiceAccountForm {
    service_account: String,
}
async fn service_account(
    _: Admin,
    State(st): State<AppState>,
    Path(name): Path<String>,
    Form(form): Form<ServiceAccountForm>,
) -> Response {
    if flow(&st, &name) != Some(Flow::GoogleVertex) {
        return bad("This Provider does not use Vertex credentials.");
    }
    if st
        .oauth
        .configure_service_account(&name, &form.service_account)
        .await
        .is_err()
    {
        return form_error("Check the service-account JSON key and its Google Cloud permissions.");
    }
    crate::quota::emit_health_events(&st).await;
    saved(
        "/admin/providers",
        "Service-account key saved and connected.",
    )
}
#[derive(Template)]
#[template(path = "signin.html")]
struct AttemptPage {
    name: String,
    path: &'static str,
    url: String,
    /// "ChatGPT subscription", "Claude Pro/Max" or "Codex plan usage".
    label: &'static str,
    /// Claude shows a code; the other flows redirect to a local address.
    claude: bool,
    callback: &'static str,
    instructions: &'static str,
    key: bool,
}
async fn begin(st: AppState, name: String, flow: Flow, path: &'static str) -> Response {
    match st.oauth.start(&name, flow).await {
        Ok(a) => render(AttemptPage {
            name: name.clone(),
            path,
            url: a.authorize_url,
            label: match flow {
                Flow::Chatgpt => "ChatGPT subscription",
                Flow::Claude => "Claude Pro/Max",
                Flow::Codex if path == "usage" => "Codex plan usage",
                Flow::Codex => "ChatGPT via Codex",
                Flow::HuggingFace => "Hugging Face",
                Flow::Entra => "Azure Entra ID",
                Flow::GoogleVertex => "Google Vertex AI",
                Flow::OpenRouter => "OpenRouter",
                Flow::GeminiCli => "Gemini CLI (Code Assist)",
                _ => "OAuth",
            },
            claude: flow == Flow::Claude,
            callback: flow.redirect(),
            instructions: st
                .oauth
                .instructions(&name, flow)
                .await
                .unwrap_or("Allow access and copy the redirect URL."),
            key: matches!(flow, Flow::OpenRouter | Flow::GeminiCli),
        }),
        Err(_) => bad("Cannot start sign-in for this Provider. Reload the page and try again."),
    }
}
async fn start(_: Admin, State(st): State<AppState>, Path(name): Path<String>) -> Response {
    start_panel(st, name).await
}
/// The sign-in panel of a Provider: a device code or the sign-in link.
pub(super) async fn start_panel(st: AppState, name: String) -> Response {
    let Some(flow) = flow(&st, &name) else {
        return bad("This Provider does not use OAuth.");
    };
    if flow.device() {
        begin_device(st, name, flow, "oauth").await
    } else {
        begin(st, name, flow, "oauth").await
    }
}
async fn usage_start(_: Admin, State(st): State<AppState>, Path(name): Path<String>) -> Response {
    begin_device(st, name, Flow::Codex, "usage").await
}
async fn paste(_: Admin, State(st): State<AppState>, Path(name): Path<String>) -> Response {
    let Some(flow) = flow(&st, &name) else {
        return bad("This Provider does not use OAuth.");
    };
    begin(st, name, flow, "oauth").await
}
async fn usage_paste(_: Admin, State(st): State<AppState>, Path(name): Path<String>) -> Response {
    begin(st, name, Flow::Codex, "usage").await
}
#[derive(Template)]
#[template(path = "signin_device.html")]
struct DevicePage {
    name: String,
    path: &'static str,
    device: crate::oauth::DeviceStatus,
    paste: bool,
}
async fn begin_device(st: AppState, name: String, flow: Flow, path: &'static str) -> Response {
    match st.oauth.start_device(&name, flow).await {
        Ok(device) => {
            let state = st.clone();
            let provider = name.clone();
            let attempt = device.attempt.clone();
            st.spawn_tracked(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    let polled = state.oauth.poll_device(&provider, flow, &attempt).await;
                    crate::quota::emit_health_events(&state).await;
                    if !matches!(polled, Ok(true)) {
                        break;
                    }
                }
            });
            render(DevicePage {
                name,
                path,
                device,
                paste: flow.paste_supported(),
            })
        }
        Err(_) => render(DeviceUnavailable {
            name,
            path,
            paste: flow.paste_supported(),
        }),
    }
}
#[derive(Template)]
#[template(path = "signin_device_unavailable.html")]
struct DeviceUnavailable {
    name: String,
    path: &'static str,
    paste: bool,
}
#[derive(Deserialize)]
struct DeviceQuery {
    attempt: String,
}
async fn device_progress(
    st: AppState,
    name: String,
    flow: Flow,
    path: &'static str,
    attempt: String,
) -> Response {
    match st.oauth.device_status(&name, flow, &attempt).await {
        Ok(device) if device.state == "connected" => {
            crate::quota::emit_health_events(&st).await;
            saved("/admin/providers", &format!("Provider {name} connected."))
        }
        Ok(device) => render(DevicePage {
            name,
            path,
            device,
            paste: flow.paste_supported(),
        }),
        Err(_) => render(DeviceUnavailable {
            name,
            path,
            paste: flow.paste_supported(),
        }),
    }
}
async fn device_status(
    _: Admin,
    State(st): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<DeviceQuery>,
) -> Response {
    let Some(flow) = flow(&st, &name) else {
        return bad("This Provider does not use OAuth.");
    };
    device_progress(st, name, flow, "oauth", q.attempt).await
}
async fn usage_device_status(
    _: Admin,
    State(st): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<DeviceQuery>,
) -> Response {
    device_progress(st, name, Flow::Codex, "usage", q.attempt).await
}
#[derive(Deserialize)]
struct CallbackForm {
    callback: String,
}
async fn exchange(st: AppState, name: String, flow: Flow, callback: String) -> Response {
    let _settings = if flow == Flow::OpenRouter {
        Some(st.settings_write.lock().await)
    } else {
        None
    };
    match st.oauth.finish(&name, flow, &callback).await {
        Ok(()) => {
            crate::quota::emit_health_events(&st).await;
            st.oauth.emit_refresh_events(&st).await;
            if flow == Flow::OpenRouter {
                tryr!(st.reload_settings().await);
            }
            saved("/admin/providers", &format!("Provider {name} connected."))
        }
        // Never echo callback input or an issuer body. Log the fixed manager
        // message and show the Operator what to do.
        Err(e) => {
            tracing::warn!(error = %e, provider = %name, "OAuth sign-in failed");
            bad(oauth_message(&e.to_string()))
        }
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
    saved(
        "/admin/providers",
        &format!("Provider {name} disconnected."),
    )
}
async fn usage_disconnect(
    _: Admin,
    State(st): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    tryr!(st.oauth.disconnect(&name, Flow::Codex).await);
    saved(
        "/admin/providers",
        &format!("Usage login for {name} disconnected."),
    )
}

/// A plain next step for a failed sign-in. The manager messages are fixed
/// strings; unknown ones get a generic step.
fn oauth_message(e: &str) -> &'static str {
    let has = |p: &str| e.contains(p);
    if has("state mismatch") || has("expired") || has("attempt") {
        "This code is from an old or different sign-in attempt. Click Sign in again and paste the new result."
    } else if has("callback") || has("Duplicate") {
        "This is not the expected sign-in result. Copy the full address from the browser after sign-in (or the code for Claude) and paste it again."
    } else if has("consent rejected") {
        "Sign-in was cancelled or refused. Click Sign in again and allow access."
    } else if has("scope") {
        "The account did not grant the required access. Click Sign in again and allow all requested access."
    } else if has("does not match this Provider") {
        "This Provider uses a different sign-in. Reload the page and use its own Sign in button."
    } else {
        "Sign-in failed. Click Sign in again. If it fails again, see the server log."
    }
}
