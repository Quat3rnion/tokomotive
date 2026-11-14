# Kimi Code sign-in

Status: accepted by the owner on 2026-11-13.

## Decision

Add `auth = "kimi_code"` as an always-available Subscription Provider. Use
device sign-in and the published Kimi Code client ID. This is an unofficial
integration. Private routes and client registration can change without notice.
Third-party use can break the service terms or cause account restriction. The
owner accepted these risks. An API-key Provider is the alternative. Source
inspection and local mocks do not establish live permission.

## Protocol

Send form requests to `https://auth.kimi.com/api/oauth/device_authorization`
and `https://auth.kimi.com/api/oauth/token`. Use client ID
`17e5f671-d194-4dfb-9706-5516cb48c098`. No scope is added. The refresh token
rotates. The existing single-flight engine saves a disconnected state before
refresh and saves the new pair before use. A failed refresh needs a new sign-in.

The fixed inference base is `https://api.kimi.com/coding/v1`. The pinned
`region.ts` also lists global hosts `auth.kimi.ai` and `api.kimi.ai/coding/v1`.
This implementation uses the default `.com` region only. It does not follow
arbitrary region or resource URLs from a client request.

The managed model list reports protocols per model. `kimi` is OpenAI-like,
`anthropic` uses Messages, and `openai_responses` uses Responses. The coding
base does not require Anthropic for all models. Default to the OpenAI API
format. Keep the Provider's API format setting available so the Operator can
select Anthropic for models that need it. The source selects the beta Messages
API for Anthropic models. Tokomotive sets `?beta=true` on that upstream path
and does not let a client query change it. Only canonical model POSTs can send
the bearer: Chat Completions, Responses, or Messages, with the existing
Translation rules. A model can still reject a protocol it does not support.

Send `X-Msh-Platform: tokomotive`, the Tokomotive version, a fixed device name
`Tokomotive server`, the server architecture and OS name, and `X-Msh-Device-Id`.
Generate one random UUID for each Provider, encrypt it with the credentials,
and keep it across reconnects and restarts. The User-Agent is
`Tokomotive/0.1.0`. The pinned identity factory explicitly requires each host
to state its own platform. No source evidence requires Tokomotive to impersonate
`kimi_code_cli`. Unknown-platform acceptance has not been checked live. Client
identity and cookie headers cannot override these values.

Credentials use the existing encrypted SQLite store. Provider Access needs an
explicit grant. Cost is 0; Price uses the Rate Cards. The encrypted record
already has a stable host ID, so no schema migration is needed.

## Sources

Verified against MoonshotAI/kimi-code commit
`f082f04a093a3e71a2263c5461d52507dc31f83a`, under `packages/oauth/src/`:

- `constants.ts` and `oauth.ts`: endpoints, client and rotating refresh;
- `identity.ts`: host-owned platform, identity headers and stable UUID;
- `managed-kimi-code.ts`: managed model protocols;
- `region.ts` and `managed-usage.ts`: default and global coding bases.

See the pinned links in [the Provider roster](../provider-roster.md).
Mocks verify polling, rotation, encrypted storage, stable identity headers,
path limits, access and accounting. Live consent, platform acceptance,
entitlement, region routing and per-model protocol support remain unverified.
