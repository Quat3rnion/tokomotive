# Subscription Providers

Status: accepted by the owner on 2026-11-13.

The owner wants ChatGPT token sharing, an optional Codex login for plan usage,
and Claude Pro/Max OAuth. All three are always available. No feature flag is
required. The implementation ports the flows from an earlier private project of the owner.

ChatGPT token sharing uses `auth.openai.com`, the scope
`chatgpt.tokens.use.direct`, and the resource `https://api.openai.com/v1`.
The first consent registers a client that the user owns. Model requests use
the normal public OpenAI APIs. They do not use a private ChatGPT backend.

The optional Codex CLI login uses the public client ID
`app_EMoamEEZ73f0CkXaXp7hrann`. Its tokens are separate from the model-request
tokens. They are used only for `GET https://chatgpt.com/backend-api/wham/usage`.
That route is private and undocumented. It can change or stop without notice.
Failure shows plan usage as **not available**. It does not stop model requests.
The owner accepted this risk.

Claude OAuth uses the public Claude Code client ID, PKCE, a pasted `code#state`,
JSON token exchange and one-shot rotating refresh tokens. Requests use an
OAuth bearer token, the required betas, the `claude-cli` user agent, the Claude
Code identity system block and its billing marker. Anthropic's published
[policy](https://code.claude.com/docs/en/legal-and-compliance) says that
third-party products must not collect, store or intermediate Claude.ai
credentials. This Provider impersonates Claude Code and can break Anthropic's
terms of service. It can cause account restriction. The owner accepted this
risk on 2026-11-13. An API-key Provider remains the alternative.

The Provider kind remains the wire format: `openai` or `anthropic`. A separate
`auth` field selects `api_key`, `chatgpt` or `claude`. Subscription credentials
are encrypted in SQLite with the existing Master Key. Sign-in attempts stay
in memory for 10 minutes. Refresh is single-flight for each credential. Before
refresh, the Proxy saves a disconnected state. This prevents replay of a
refresh token after a crash or an uncertain response. It saves the new tokens
before it uses them. A failed refresh needs a new sign-in.

Subscription Providers require explicit Provider Access for each Virtual Key.
An absent or empty list allows all API-key Providers, but no Subscription
Providers. A non-empty list allows only the listed Providers. This rule also
applies to Fallbacks, provider prefixes and model discovery.

Ledger Entries keep the real token usage and the Provider name. Subscription
Cost is zero, including after later pricing of an Unpriced Entry. Price still
uses the normal cards. Simple Mode adds no financial controls.
