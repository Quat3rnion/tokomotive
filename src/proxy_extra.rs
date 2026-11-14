//! Small adapters for endpoints outside text generation.
use super::*;

pub(super) const COUNT_BODY_LIMIT: usize = 1024 * 1024;

pub(super) fn native_count(provider: &Provider) -> bool {
    provider.kind == ProviderKind::Anthropic
        && matches!(
            provider.auth,
            crate::provider::ProviderAuth::ApiKey | crate::provider::ProviderAuth::Claude
        )
}

pub(super) fn estimate_count(ctx: Ctx, body: &Value) -> Response {
    // Include tools and system text, but not routing and generation settings.
    let payload = json!({"messages":body.get("messages"),"system":body.get("system"),"tools":body.get("tools")});
    let tokens = payload.to_string().len().div_ceil(4);
    ctx.finish(Outcome {
        estimated: true,
        ..Outcome::failed(200)
    });
    (
        [("x-tokomotive-estimate", "true")],
        axum::Json(json!({"input_tokens":tokens})),
    )
        .into_response()
}
