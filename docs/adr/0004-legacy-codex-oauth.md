# Legacy Codex OAuth

Status: accepted by the owner on 2026-11-13.

## Decision

Add `auth = "codex"`, shown as **ChatGPT via Codex (legacy)**. It uses the
Codex CLI client `app_EMoamEEZ73f0CkXaXp7hrann` for model requests. This narrows
ADR 0003: Codex credentials are no longer limited to optional plan usage when
the Operator selects this new authentication type. Existing `chatgpt` Providers
keep their separate token-sharing credentials and public API route. Token
sharing remains the recommended ChatGPT sign-in.

The model route is private and undocumented. It can change without notice.
Use of the CLI client and private route in a third-party Proxy can break
OpenAI's terms of service and can cause account restriction. The owner accepted
these risks. A public API-key Provider is the alternative.

## Protocol

The fixed model destination is
`POST https://chatgpt.com/backend-api/codex/responses`. Only canonical client
`POST /v1/responses`, `/v1/chat/completions` and `/v1/messages` can use it. The
Proxy selects the destination internally and does not forward client query
parameters. Other paths cannot carry the Codex bearer token. Provider Access
needs an explicit grant. Cost is zero; Price, Token Classes and the Ledger's
Subscription flag keep the rules from ADR 0003.

The request uses a bearer token, `chatgpt-account-id` from the ID token's
`chatgpt_account_id` claim under `https://api.openai.com/auth`, `originator=codex_cli_rs`,
`session-id`, `thread-id`, `x-client-request-id`, `version`, and a Codex-style
`User-Agent`. Session IDs are new for each upstream attempt. The version is
Tokomotive's build version, not a claim to be an installed Codex release.
The response format is SSE. Requests set `store:false`, `stream:true` and
include `reasoning.encrypted_content`.

Chat requests map messages, system/developer instructions, images, function
tools, calls and results, output limits, temperature and stream preferences.
Responses map text deltas, function-argument deltas, completion usage and
errors back to Chat. Messages clients chain Messages -> Chat -> Responses;
replies use the reverse chain. This reuses the existing Messages tool and
usage rules. JSON clients receive the final Response collected from SSE.
An incomplete Response maps to an output-limit stop. A failed or truncated
stream reports an error. Cached input is subtracted from ordinary input and
recorded in the cache-read Token Class.

This adapter is limited to Codex Providers. It is not a general Responses-only
Provider setting. Only function tools are translated. The existing Messages
adapter's limits still apply. Unmapped Chat fields are not forwarded. Model
and parameter support depends on the private backend; local mocks cannot
prove live account entitlement or support for temperature and output limits.

## Sign-in

Paste sign-in uses `/oauth/authorize` and `/oauth/token` on
`https://auth.openai.com`, PKCE S256, the CLI client above, and the scopes
`openid profile email offline_access api.connectors.read api.connectors.invoke`.
Its authorize parameters include `id_token_add_organizations=true`,
`codex_cli_simplified_flow=true` and `originator=codex_cli_rs`. The redirect is
`http://127.0.0.1:1455/auth/callback`. Refresh uses `/oauth/token`.

Device Code Sign-in is the default for both a Codex Provider and the optional
Codex usage login of a token-sharing Provider. These use the same Codex flow:

1. POST JSON `{client_id}` to `/api/accounts/deviceauth/usercode`.
2. Show only `user_code` and `https://auth.openai.com/codex/device`.
3. Poll `/api/accounts/deviceauth/token` with `device_auth_id` and `user_code`.
4. Exchange the returned authorization code and PKCE verifier at `/oauth/token`
   with redirect `https://auth.openai.com/deviceauth/callback`.

Upstream treats HTTP 403 and 404 as pending and limits the wait to 15 minutes.
The Proxy also handles explicit pending, slow-down, denial and expiry errors.
It respects the interval, adds five seconds on slow-down and stops after
expiry, denial, replacement or Disconnect. Status polls are browser heartbeats;
without one for 15 seconds, server polling stops. An in-flight issuer request
can take up to 30 more seconds. Attempts stay in memory, with a 15-minute TTL;
terminal status expires one minute after that TTL. Cleanup runs
when another attempt starts or status is read. Restart loses attempts.

All sign-in and status endpoints require Admin and same-origin POSTs. Device
credentials and token responses never enter HTML or logs. Saved tokens use
the existing encrypted storage and single-flight, fail-closed refresh.

No device flow is advertised for the user-owned ChatGPT token-sharing client:
OpenAI's discovery document lists only authorization-code and refresh grants
and has no device authorization endpoint. Codex's general RFC 8628 gateway
helper requires a separately configured endpoint; it does not establish
support for token sharing. Keep paste sign-in for `chatgpt`. Anthropic's
published Claude Code sign-in describes browser consent and pasted codes, not
a device grant for the Claude Code client. No supported device endpoint was
found for this flow. Keep `code#state` for `claude`; do not guess an endpoint.

## Sources

Verified on 2026-11-13 against openai/codex commit
`52ff3d1a353953ccc08a39bdfc66a5fda9b08d0e`:

- [CLI client ID](https://github.com/openai/codex/blob/52ff3d1a353953ccc08a39bdfc66a5fda9b08d0e/codex-rs/login/src/auth/manager.rs),
  [PKCE authorize and token exchange](https://github.com/openai/codex/blob/52ff3d1a353953ccc08a39bdfc66a5fda9b08d0e/codex-rs/login/src/server.rs).
- [Codex device protocol, URLs and 15-minute wait](https://github.com/openai/codex/blob/52ff3d1a353953ccc08a39bdfc66a5fda9b08d0e/codex-rs/login/src/device_code_auth.rs).
- [Backend base URL and version header](https://github.com/openai/codex/blob/52ff3d1a353953ccc08a39bdfc66a5fda9b08d0e/codex-rs/model-provider-info/src/lib.rs),
  [bearer/account headers](https://github.com/openai/codex/blob/52ff3d1a353953ccc08a39bdfc66a5fda9b08d0e/codex-rs/model-provider/src/auth.rs),
  [originator and user agent](https://github.com/openai/codex/blob/52ff3d1a353953ccc08a39bdfc66a5fda9b08d0e/codex-rs/login/src/auth/default_client.rs).
- [SSE route and request ID](https://github.com/openai/codex/blob/52ff3d1a353953ccc08a39bdfc66a5fda9b08d0e/codex-rs/codex-api/src/endpoint/responses.rs),
  [session headers](https://github.com/openai/codex/blob/52ff3d1a353953ccc08a39bdfc66a5fda9b08d0e/codex-rs/codex-api/src/requests/headers.rs),
  [store, stream and include fields](https://github.com/openai/codex/blob/52ff3d1a353953ccc08a39bdfc66a5fda9b08d0e/codex-rs/core/src/client.rs).
- [General gateway device support](https://github.com/openai/codex/blob/52ff3d1a353953ccc08a39bdfc66a5fda9b08d0e/codex-rs/login/src/oauth_session_login.rs).

Other first-party sources:
[OpenAI discovery](https://auth.openai.com/.well-known/openid-configuration),
[Claude Code authentication](https://code.claude.com/docs/en/authentication).
