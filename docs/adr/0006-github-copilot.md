# GitHub Copilot sign-in

Status: accepted by the owner on 2026-11-13.

## Decision

Add `auth = "github_copilot"` as an always-available Subscription Provider.
Use the opencode device flow, not the copilot-api token adapter. This is an
unofficial integration. The client ID and private routes can change without
notice. Third-party use can break GitHub's terms of service or cause account
restriction. The owner accepted these risks. A public API-key Provider is the
alternative. No live account check establishes permission to use this client.

## Protocol

Send JSON to `https://github.com/login/device/code`, then poll
`https://github.com/login/oauth/access_token`. Use client ID
`Ov23li8tweQw6odWQebz` and scope `read:user`. Handle OAuth errors in successful
HTTP responses. Read `https://api.github.com/copilot_internal/user` with the
OAuth bearer and `X-GitHub-Api-Version: 2025-04-01`. Refuse an explicit
`chat_enabled: false` response. Discovery failure also refuses sign-in; this
is stricter than opencode's fallback on discovery failure.

Read `endpoints.api`, or use `https://api.githubcopilot.com` when it is absent.
Accept only HTTPS destinations under `githubcopilot.com`. The Operator can
save an encrypted GitHub Enterprise domain. Its OAuth endpoints use that
domain; discovery uses `api.<domain>` and the fallback is
`copilot-api.<domain>`. Enterprise discovery can select that fallback host or
its subdomains. Other hosts, credentials, custom ports and URL queries are
refused. Changing the domain disconnects the Provider.

The pinned opencode source uses the OAuth bearer directly. It does not request
`copilot_internal/v2/token`. The bearer has no reported expiry and is kept
until rejection or Disconnect. It has no rotating refresh grant. A rejection
needs a new sign-in. The alternative copilot-api source uses client ID
`Iv1.b507a08c87ecfe98`, a short-lived token from `/copilot_internal/v2/token`,
and VS Code identity headers. Tokomotive does not use that alternative.

Only canonical model POSTs can carry the bearer. OpenAI-format requests use
`/chat/completions` or `/responses`. Anthropic-format requests use
`/v1/messages`. The Provider's API format selects the protocol. Existing
Translation rules apply. Non-model paths cannot carry the bearer.

Tokomotive sends its own `User-Agent: Tokomotive/0.1.0`,
`Openai-Intent: conversation-edits`, `X-GitHub-Api-Version: 2026-08-01`,
`x-initiator`, `X-Interaction-Type: conversation-agent`, and a new
`X-Interaction-Id` for each attempt. The final protocol body selects a user or
agent initiator and `Copilot-Vision-Request: true` when it contains images.
It does not claim to be VS Code or opencode. The pinned
opencode source sends no `Editor-Version` or `Copilot-Integration-Id`; those
headers belong to the alternative adapter. Client identity, session-token,
editor and cookie headers cannot override the protected headers. Copilot Auto
session tokens and live model discovery are not implemented.

Credentials use the existing encrypted SQLite store. Provider Access needs an
explicit grant. Cost is 0; Price uses the Rate Cards. No schema change is needed.

## Sources

Verified against sst/opencode commit
`83960f8a56b2f2b8ad0b751f23baaf9d0eb65ad9`,
`packages/core/src/plugin/provider/github-copilot.ts`: device/token requests,
user discovery, enterprise URLs, entitlement and inference headers.

Compared with ericc-ch/copilot-api commit
`0ea08febdd7e3e055b03dd298bf57e669500b5c1`, `src/lib/api-config.ts` and
`src/services/github/`: alternative client ID and short-lived token exchange.
See the pinned links in [the Provider roster](../provider-roster.md).

Mock tests verify sign-in, discovery, path limits, protected headers, encrypted
storage, explicit access and accounting. Live entitlement, Enterprise routing,
model support and future service-policy changes remain unverified.
