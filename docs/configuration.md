# Configuration reference

## OAuth refresh

Refresh settings are automatic. There is no seed-file setting. Run only one
Proxy process per database. Each credential has one in-process refresh lock.
Tokens, pending refresh state and retry times use the encrypted credential
record. A restart keeps them. Keep the Master Key with the database backup.

The background task checks credentials every 60 seconds. It refreshes at 80%
of the access lifetime or 10 minutes before expiry, whichever comes first.
The schedule has up to 30 seconds of early jitter. At most four background
refreshes run at once. Google user tokens and Entra tokens also have a 30-day
keep-alive limit. Normal short access lifetimes cause refresh much sooner.
Vertex service-account tokens use the same schedule, lock and retry policy.
An upstream 401 can also start one refresh. A 401 changes account health only
if its access token is still current. A late 401 for an older token cannot
disable a recovered account; the request retries with the current token.
**Refresh now** uses the same lock and does not bypass a retry delay.
This POST action requires Admin or Owner
and the same origin. A Viewer gets HTTP 403.

### Flow rules

The access lifetime below is the issuer's `expires_in`, not a promise that a
token cannot be revoked sooner. Except for Claude, the current fallback for
a missing access lifetime is one hour. A flow with no refresh grant and no
reported expiry has no local expiry. "Unknown" means that the cited source
does not establish a server guarantee. A local mock does not prove a live
reuse window.

| Flow | Refresh rotation and reuse | Access lifetime | Idle expiry |
| --- | --- | --- | --- |
| ChatGPT token sharing | Replacement required. Reuse grace unknown [1]. | Reported; fallback 1 hour. | Unknown. |
| Claude | Replacement required. Single-use; no verified grace window [1]. | Reported; required. | Unknown. |
| Codex, including optional plan usage | Save a replacement when supplied; otherwise keep the old token. Treat a lost response as possible rotation. Upstream reports already-used tokens; no verified grace window [2]. | Reported; fallback 1 hour. | Unknown. |
| Hugging Face | Save a replacement when supplied; otherwise keep the old token. Rotation and reuse guarantees are not documented in the cited guide [3]. | Reported; fallback 1 hour. | Unknown. |
| Entra | Replaces on use, but does not revoke the old token on use [4]. | Reported; normally 60 to 90 minutes [5]. | Normally 90 days for this public-client flow; tenant policy or revocation can shorten it [4]. |
| Google Vertex user OAuth | Reusable refresh token; keep it when the response omits a replacement [6]. | Reported; fallback 1 hour. | Six months without refresh use. Testing clients can expire after 7 days; session controls can require sign-in sooner [6]. |
| Google Vertex service account | No refresh token. Mint a new access token with a signed JWT [7]. | Reported; normally 1 hour. | No refresh-token idle expiry. Key and account policy still apply. |
| Gemini CLI / Code Assist | Google reusable refresh token; keep it when omitted [6, 8]. | Reported; fallback 1 hour. | Google six-month inactivity rule and session controls [6]. |
| Qwen Code | Tokomotive requires a replacement. The pinned client can retain an omitted replacement; no verified reuse grace window [9]. | Reported; fallback 1 hour. | Unknown. |
| Kimi Code | Replacement required. The pinned client retries transport errors, but does not establish a server grace window [10]. | Reported; fallback 1 hour. | Unknown. |
| GitHub Copilot | No refresh grant. Use the GitHub OAuth bearer directly; no short-lived Copilot exchange [11]. | No expiry reported in this flow. | GitHub can revoke a bearer unused for one year [12]. Sign in again; there is no refresh-token keep-alive. |
| OpenRouter | PKCE creates a normal API key. No refresh grant [13]. | API-key settings. | API-key settings. |

Sources, checked 2026-11-15:

1. [Subscription protocol and limits](adr/0003-subscription-providers.md).
   The ChatGPT sharing flow came from the owner's earlier project. This is
   not an issuer guarantee of refresh-token reuse.
2. [Codex refresh manager at the pinned commit](https://github.com/openai/codex/blob/52ff3d1a353953ccc08a39bdfc66a5fda9b08d0e/codex-rs/login/src/auth/manager.rs).
3. [Hugging Face OAuth](https://huggingface.co/docs/hub/oauth).
4. [Microsoft refresh-token lifetime and replacement](https://learn.microsoft.com/en-us/entra/identity-platform/refresh-tokens).
5. [Microsoft access-token lifetime](https://learn.microsoft.com/en-us/entra/identity-platform/access-tokens#token-lifetime).
6. [Google OAuth refresh and expiration](https://developers.google.com/identity/protocols/oauth2#expiration).
7. [Google service-account grants](https://developers.google.com/identity/protocols/oauth2/service-account).
8. [Pinned Gemini CLI protocol](adr/0009-gemini-cli.md).
9. [Pinned Qwen refresh client](https://github.com/QwenLM/qwen-code/blob/6f754be6548903e545e6ff0ed43a34927fe3bfb9/packages/core/src/qwen/qwenOAuth2.ts).
10. [Pinned Kimi refresh client](https://github.com/MoonshotAI/kimi-code/blob/f082f04a093a3e71a2263c5461d52507dc31f83a/packages/oauth/src/oauth.ts).
11. [Copilot direct-bearer decision and source](adr/0006-github-copilot.md).
12. [GitHub token expiration and revocation](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/token-expiration-and-revocation).
13. [OpenRouter PKCE](https://openrouter.ai/docs/guides/overview/auth/oauth).

### Refresh outcomes

- A token endpoint HTTP 400 or 401 with `invalid_grant`, `invalid_token` or
  `unauthorized_client` disconnects the credential. The page shows fixed
  text, not the issuer's response. A complete malformed or oversized successful
  response also disconnects a rotating flow. Sign in again.
- A connection failure before send, HTTP 429 or HTTP 5xx keeps the credential.
  A non-rotating flow also keeps it after a timeout, network reset or body
  transport failure after HTTP 2xx headers. The
  persisted retry delay doubles from 30 seconds to a maximum of 15 minutes,
  with jitter. The access token remains usable until its actual expiry.
  Requests that need a fresh token get HTTP 503 with
  `Sign-in refresh is retrying`, in the API error format. This local retry
  state records neither a success nor a failure in the Circuit Breaker.
  A Fallback can still serve the request.
- A rotating request with no complete response can have consumed the refresh
  token. This includes a timeout, reset or truncated body after HTTP 2xx headers.
  The Proxy keeps the old token and a **Refresh in doubt** marker. After the
  delay it makes one recovery attempt with that token. A crash during the
  first request also permits this one attempt. An explicit transient response
  does not consume the recovery attempt. A second ambiguous result, or a crash
  during recovery, stops further replay and shows
  `Refresh token state is unknown; sign in again`. The in-doubt state also
  stops after 24 hours or 20 attempts, whichever comes first. This limit
  includes the first ambiguous request and explicit transient recovery replies.
  An already-used
  rejection shows `Refresh token was already used; sign in again`.

The bounded recovery attempt is a recovery policy, not a claim that every
issuer permits reuse. No grace duration is assumed for the rotating flows.
The Provider card shows access expiry, last refresh, next refresh or retry,
and the last fixed-text refresh error. Refresh errors do not change the
**Connected** state unless the credential is disconnected.

`tokomotive_oauth_refresh_total{provider,flow,result}` counts `success`,
`transient`, `in_doubt` and `disconnected` outcomes.
`tokomotive_oauth_access_expiry_seconds{provider,flow}` reports remaining access
lifetime in seconds when credentials are checked. Logs contain only Provider,
flow and fixed result text. Alerts use `oauth_disconnected` and
`oauth_recovered`, once per transition. Recovery includes a successful retry
after a transient error. Delivery is best effort, as for other alert webhooks.

## Guardrails

Use **Guardrails** in the dashboard. Rule sets and their Route, Team and
Virtual Key attachments are stored in SQLite, not in the seed file.

| Limit | Value |
| --- | --- |
| Rule sets | 64 |
| Rules per set / per request union | 64 / 256 |
| Regex or keyword pattern | 8192 bytes |
| Compiled regex program | 1 MiB, with `RegexBuilder::size_limit` |
| Inspected text | 1 MiB by default; set 1 to 16777216 bytes per rule set |
| Matches per inspection | 1024 |
| Stream evaluation interval | At least 32 KiB of pending SSE data, or stream end |
| Retained stream text | 8 KiB between windows |
| SSE frame / pending data | 128 KiB / 256 KiB |

The smallest attached text limit applies. Exceeding an inspection, match
or stream buffer limit blocks the request or terminates the stream.
Invalid JSON response bodies and invalid SSE data also stop the response.
Regex uses the linear-time `regex` crate, without backtracking. Empty-text patterns
are not accepted. These checks bound work; they do not prove that every
possible PII or secret format is detected.

- `email`: a common ASCII email address with a dotted domain.
- `phone`: E.164-like text, a plus sign followed by 7 to 15 digits. The
  first digit cannot be zero. Spaces, brackets and local phone numbers
  are not detected. This detector does not check national numbering plans.
- `credit_card`: 13 to 19 digits, with optional spaces or hyphens, and a
  valid Luhn checksum. All-zero numbers are not accepted.
- `iban`: an uppercase IBAN with optional spaces and a valid modulo-97
  checksum. Country lengths are checked. Lowercase text is not detected.
- `secret`: OpenAI `sk-`, Anthropic `sk-ant-`, AWS `AKIA`, GitHub `ghp_`,
  Slack `xoxb-` and `xoxp-`, Google `AIza`, private-key PEM headers and
  JWT-shaped tokens. Short prefixes and other secret formats can be missed.

Each rule set can have an external moderation URL. Use an operator-controlled
HTTP or HTTPS URL without URL credentials. The Proxy sends a JSON object:

```json
{"text":"assembled text","direction":"request"}
```

The hook must return `{"action":"block","reason":"policy"}`. `redact`
replaces all inspected text; `log` leaves it unchanged. The reason is not
stored or returned to the client. The request and response body timeout is
2 seconds. The reply is limited to 8192 bytes. Redirects are not followed.
Streaming can call the hook once per window; windows can overlap.

The default is fail-open: a timeout, network error, bad status or invalid
reply does not block. Select **Block if the external hook fails** for
fail-closed behavior. The Ledger name for a hook action is
`rule-set-name:external`. A hook receives prompt or response text, which
can contain private data. Test-box text is never sent to the hook.

## Egress proxies

Set egress proxies in the dashboard. These settings use the database, not the
seed file. New installations have no global proxy. Each Provider initially
uses **Use global**. Select **None** to bypass the global proxy, or **Custom
proxy** to save a separate URL.

Accepted schemes are `http`, `https`, `socks5` and `socks5h`. The host and an
explicit port from 1 to 65535 are required. A URL can contain a user name and
password. It cannot contain whitespace, a path, query or fragment. The maximum
URL length is 4096 bytes. With SOCKS5, `socks5` resolves the upstream host
locally. `socks5h` asks the proxy to resolve it.

The complete URL is encrypted with the Master Key, including credentials.
Only `scheme://host:port` is shown. Blank password fields keep the saved URL.
Switching a Provider to **Use global** or **None** removes its custom URL.

The cache permits at most 64 distinct proxy URLs, plus a direct connection.
Providers that share a URL share clients. Separate client profiles preserve
the upstream timeout and 15-second connect timeout, the 30-second OAuth
timeout with automatic retries disabled, the 60-second models.dev timeout,
and the 10-second webhook timeout. Settings reload builds a new cache before
it replaces the old cache. Requests do not build clients. An invalid edit or
an edit above the cache limit is rejected before it is stored.

Each request keeps the Provider's client generation for all attempts and key
retries. A proxy edit affects new requests only. An unknown, renamed or deleted
Provider cannot use the global policy as a fallback. A reused Provider name
does not restore access for an old request. Outbound failures record a fixed
error class, not the request URL or the error chain.

Redirects and response body limits do not change. Provider overrides apply
to all outbound Provider operations. models.dev Sync and alert webhooks use
the global default only. Trace export is separate. The clients do not inherit
`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` or `NO_PROXY` from the process.

Use network policy to restrict proxy destinations. Private addresses are
allowed for Operator-managed proxies. A proxy URL must therefore be changed
only by a trusted Operator. See [Egress proxies](usage.md#egress-proxies).

## Traces and body capture

Use **Settings > Traces and body capture**. These settings are stored in the
database. They do not use the seed file.

| Setting | Default | Accepted value |
| --- | --- | --- |
| Export traces | Off | On or off |
| Collector endpoint | Empty | Full HTTP or HTTPS OTLP/HTTP JSON URL, including `/v1/traces` |
| Collector headers | Empty | One `Header: value` per line; encrypted and masked |
| Service name | `tokomotive` | At most 256 characters |
| Sample ratio | `1` | Number from 0 to 1 |
| Body size cap | `65536` | 1 to 1048576 bytes per body |
| Body retention | `7` | 1 to 3650 days |

The collector URL must not contain a user name or password. Export does not
follow redirects. Collector headers cannot override Host, Content-Length,
Content-Type or Transfer-Encoding. Headers have a 16 KiB total limit. The
queue holds 128 requests. Each batch holds at most 16 requests. The batch
window is 25 ms and the HTTP timeout is 5 seconds. A full queue drops traces.
Export does not block Proxy requests. Queued traces are best effort and can
be lost at shutdown.

Each Route and Virtual Key has a separate **Capture request and response
bodies** control. Both are off by default. Either can enable capture. Captures
are encrypted with the Master Key. They are not part of trace export. The
background retention job runs each minute and deletes at most 500 captures
per pass. Only Admins can read captures through the Ledger entry timeline.
See [Request diagnostics](usage.md#request-diagnostics) for the limits and
privacy controls.

## Google Vertex AI

```toml
[[providers]]
name = "vertex"
kind = "gemini"
auth = "google_vertex"
project_id = "my-project"
region = "us-central1"
base_url = "https://us-central1-aiplatform.googleapis.com"
```

Set `project_id` to a Google Cloud project ID: 6 to 30 lowercase letters,
digits or hyphens. Start with a letter. End with a letter or digit. The region
must be `global` or two lowercase components, such as `us-central1`. The second
component ends with a digit. The Proxy builds the host from the region only.
For `global`, use `https://aiplatform.googleapis.com`. The dashboard builds the
base URL when you save. An old sign-in can keep an empty project until you edit
it, but no request can use an empty project.

Do not add API keys. In the dashboard, configure an Operator-supplied Google
Desktop OAuth client, or save a service-account JSON key. The JSON key must
have type `service_account` and token URI `https://oauth2.googleapis.com/token`.
Credential fields are encrypted. JSON key fields cannot change the token host.
RS256 JWT assertions use cloud-platform scope, the fixed token URI as audience,
and a lifetime of 1 hour. Token refresh is single-flight. The HTTP clients do
not follow redirects. Responses use the bounded JSON reader and SSE limits.

The publisher is `anthropic` for `claude-*` names and `google` for other names.
A Deployment model can use an explicit `anthropic/` or `google/` prefix. Gemini
uses `generateContent` or `streamGenerateContent?alt=sse`. Anthropic uses
`rawPredict` or `streamRawPredict`, with `anthropic_version: vertex-2023-10-16`
and no `model` body field. Both use the project and location in the URL.
Usage follows the selected API format. Cost follows the Cost Card, not the
subscription rule. Each Virtual Key needs explicit Provider Access.

Use the bare model ID for Cost and Price Cards. Publisher prefixes select a
route, not a different billable Model. They are removed for card lookup, even
when the response omits its model name.

Sources: [Vertex locations](https://cloud.google.com/vertex-ai/generative-ai/docs/learn/locations),
[Claude on Vertex](https://cloud.google.com/vertex-ai/generative-ai/docs/partner-models/claude/use-claude),
and [service-account OAuth](https://developers.google.com/identity/protocols/oauth2/service-account).

## Gemini CLI subscription

Use `auth = "gemini_cli"`, `kind = "gemini"` and the fixed base URL
`https://cloudcode-pa.googleapis.com`. Do not add API keys. Use the dashboard
paste sign-in. The CLI client configuration is fixed. The published
installed-app secret is not an Operator secret and is not shown in HTML.
Tokens, the project and the tier are stored in encrypted credentials. See
[ADR 0009](adr/0009-gemini-cli.md) for the exact trusted hosts and source.

This document lists every setting of tokomotive. The terms are defined in
[GLOSSARY.md](../GLOSSARY.md).

The database is the source of truth. Start the Proxy without a file, open
`/admin`, and set the first password and the mode on `/setup`. Use **Providers**,
**Routes** and **Settings** for live changes. They apply without a restart. See
[usage.md](usage.md) for the Operator guide and [INSTALL.md](../INSTALL.md) for
the first installation.

SQLite uses WAL, up to eight connections, and a five-second busy timeout.
Ledger and account-health writes take the writer lock before they read data.
If that lock stays busy past the timeout, these writes wait and try again.
They do not discard a Ledger Entry because another writer holds the lock.

Ledger writes run in tracked background tasks. A response can reach the client
before its Ledger Entry is stored. `AppState::flush` waits for these tasks,
including stream drains after a client disconnect. Tests that read the Ledger
after requests must call it first.

## Process options and optional seed file

The Proxy reads a seed file only when you give a path:

1. The `--config PATH` flag (short form `-c PATH`).
2. The environment variable `TOKOMOTIVE_CONFIG`.

There is no implicit `config.toml`. With no path, the Proxy uses defaults and
the existing database. `--listen ADDRESS` overrides `TOKOMOTIVE_LISTEN`.
`--database SQLITE_URL` overrides `TOKOMOTIVE_DATABASE`. Their defaults are
`127.0.0.1:8787` and `sqlite://tokomotive.db`. Parent directories are created.
These two options cannot be changed in the UI. Legacy `server.listen` and
`server.database` seed-file fields are accepted but the binary ignores them.

The flag `--help` (short form `-h`) prints the usage text. The Proxy stops with an
error for any other flag.

The Proxy stops at start in these cases:

- The file cannot be read or is not valid TOML.
- The file has an unknown section at the top level.
- The file has an unknown key in `[server]`, in `[router]`, in a `[[providers]]`
  item, in a `[[routes]]` item, in a `[[models]]` item, in a `[[teams]]` item or
  in a `[[users]]` item.
- A required key is missing.
- A Model or a Route Deployment uses a Provider that is not in the file.
- Two Routes or two Providers have the same name, or a name has a character that
  is not a letter, a digit, a dot, an underscore or a hyphen.
- A Deployment has a weight of 0. A `[router]` `failure_threshold` or
  `max_attempts` is 0.
- A User uses a Team that is not in the file.
- A Price Card has both `markup_percent` and rates, or has neither.

The Proxy does not check the spelling of rate keys. This applies to the keys in
`cost`, in `price`, in `price_overrides` and in `tiers`. A wrong key such as
`inputt` is ignored and the rate stays 0. Check these keys with care.

## Units

- Rates are USD per million tokens or endpoint-defined Service units.
- All `*_percent` values are percentages. The Proxy keeps a percentage with a
  resolution of 0.01 percent.
- The Proxy stores money as integer micro-dollars (1 USD is 1,000,000).

## Environment variables

| Variable                        | Function                                                                                             |
| ------------------------------- | ---------------------------------------------------------------------------------------------------- |
| `TOKOMOTIVE_CONFIG`         | Path of the optional seed file. The `--config` flag has priority.                                    |
| `TOKOMOTIVE_LISTEN`         | Listen address, unless `--listen` is set. |
| `TOKOMOTIVE_DATABASE`       | SQLite URL, unless `--database` is set. |
| `TOKOMOTIVE_ADMIN_PASSWORD` | Recovery override. When not empty, it replaces the stored password hash at each start. Use Setup for the normal path. |
| `TOKOMOTIVE_SECRET_KEY`     | Master Key: 32 bytes encoded as 64 hex characters or base64. |
| `TOKOMOTIVE_METRICS_TOKEN`  | Initial metrics token, used only on the first start, before runtime settings exist. |
| `RUST_LOG`                      | Log filter, for example `info` or `tokomotive=debug`. The default is `info`.                     |
| Names in `env:NAME` references and in `*_env` keys | Each name is a variable that holds a Provider Key, a password or a token. |

An empty listen address or database URL is not valid.

A `*_env` key holds the name of a variable, not the secret. The Proxy ignores a
variable that is not set or is empty.

## `[server]`

All keys are optional. The whole section is optional. Values seed once, on the
first start. Edit them in **Settings** afterward.

| Key                      | Type   | Default                         | Meaning                                                                                                                                                  |
| ------------------------ | ------ | ------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `listen`                 | string | `"127.0.0.1:8787"` | Legacy field; use `--listen` or the environment. |
| `database`               | string | `"sqlite://tokomotive.db"` | Legacy field; use `--database` or the environment. |
| `admin_password`         | string | none                            | Legacy. A dashboard password in the file. It seeds the hash only when none exists. Use Setup instead. Do not use it in a repository that you commit. |
| `admin_password_env`     | string | `"TOKOMOTIVE_ADMIN_PASSWORD"` | Name of the variable that holds the dashboard password. The variable has priority over `admin_password`. |
| `default_markup_percent` | float  | `0`                             | Markup for Models that have no Price Card. The file `config.example.toml` sets it to `20`.                                                               |
| `upstream_timeout_secs`  | integer | `600`                          | Default request time limit in seconds. For non-streaming requests, this includes the response body. For streams, it limits the wait for response headers. Provider limits can override it. |
| `metrics_token`          | string | none                            | The bearer token for `/metrics`, in the file. It seeds once. Later changes belong in **Settings**. |
| `metrics_token_env`      | string | `"TOKOMOTIVE_METRICS_TOKEN"` | Name of the variable that holds the bearer token. The variable has priority over `metrics_token`.                                                       |
| `reserve_output_tokens`  | integer | `4096` | Fallback output tokens for a Reservation when the request sets no maximum and the Model has no mapped output limit. Must be non-negative. Does not cap a models.dev output limit. |
| `audio_upload_limit_mib` | integer | `25` | Total multipart audio request limit in MiB, from 1 to 63. Also available in Settings. |
| `session_days`           | integer | `7` | Duration of new dashboard sessions, from 1 to 3650 days. |
| `mode`                   | string | `"full"` | `full` enables pricing and usage limits. `simple` enables routing and token metrics without Budget checks, Reservations or Rate Limits. See "Simple mode". |

### models.dev settings

Use **Settings > models.dev Sync** for these live settings. They are not seed-file fields.

| Setting | Default | Function |
| --- | --- | --- |
| Model data URL | `https://models.dev/api.json` | HTTP or HTTPS JSON source. Redirects are not followed. The size limit is 16 MB. |
| Sync model data once a day | On | Check the saved URL once a day with `If-None-Match` when an ETag is available. |

**Refresh model data** checks the saved URL at once, also when daily sync is off.
Settings shows the last accepted check and the last error. A bad payload, a
negative price, or a loss of more than 20% of Models keeps the old data.
The first start loads the bundled snapshot without a network request.

When a request has no `max_tokens` or `max_output_tokens`, its Reservation uses
the mapped Model's `limit.output`. `reserve_output_tokens` is the fallback when
the Model is unmapped or has no output limit. A lower fallback does not cap the
model limit.

### Simple mode

Select the mode on `/setup` or in **Settings**. The default is `full`. To seed
Simple Mode on the first start:

```toml
[server]
mode = "simple"
```

Later file changes do not replace the saved mode. A change in **Settings** applies
to new requests without a restart. Saved cards and limits stay in the database.
See "Simple mode" in [usage.md](usage.md#simple-mode) for the behavior.

### Dashboard password

If no admin account exists, dashboard pages redirect to `/setup`. Setup creates
the first Owner with a username and a password of at least 8 characters. Later
Setup requests are refused. Passwords use salted Argon2 hashes.

An upgrade keeps the old password. It creates an Owner named `admin` and binds
existing valid sessions to that account. The login form now has a username.
An empty or absent username means `admin`, so old login scripts still work.
Usernames are not case-sensitive. They contain at most 64 letters, digits,
dots, underscores or hyphens.

Use **Settings > Admin accounts** to add, change or delete accounts. Only an
Owner can use these controls. At least one Owner must remain. An Admin can
change other configuration, but cannot manage accounts, change the metrics
token or set audit retention. Only an Owner can add or remove environment key
references. Only an Owner can change the address, API format or authentication
header of a Provider that has these references. An Admin can still add pasted
Provider Keys. A Viewer has read-only access. Viewers can still log out and change
their own password with the current password.

Use **Settings > Change password** for your own password. A password change,
role change or account deletion ends that account's sessions only. Other
accounts stay logged in. Sessions otherwise survive restarts.

`TOKOMOTIVE_ADMIN_PASSWORD`, when non-empty, creates or resets the Owner named
`admin` at each start. It ends that account's sessions. Remove the variable
after recovery to keep UI password changes after a restart. Legacy
`admin_password` or `admin_password_env` values seed an Owner only when no
account exists.

Complete Setup through a trusted local connection before you expose the service.
Use TLS at the reverse proxy for remote access. Preserve the public Host and
Origin headers. All dashboard POST routes refuse cross-origin browser forms.
Login permits 20 attempts per socket IP in a 5-minute window. Each username
permits at most 5 attempts in that IP window. A username change does not reset
the IP budget. After the window ends, the same IP can log in again. Forwarded
IP headers do not bypass these limits. Behind a reverse proxy, the socket IP
is the reverse proxy IP. The server stores at most 4096 IP budgets, with at
most 16 username buckets per IP. A full IP map removes the least recently used
IP. A full username map removes the oldest bucket. One full budget does not
block a different IP.

All dashboard request bodies, including Setup and login, have a 2 MiB size
limit and a 15-second read deadline. The server reads the body before it locks
dashboard mutations. It checks the session and role after it gets the lock.
These limits do not apply to `/v1` requests or streaming Proxy responses.

### Audit log

Owners and Admins can open **Audit log** from Settings or the navigation.
Filter by account, action, target type or ID, and UTC time. The page contains
25 entries by default, with at most 200 entries at a time. The compact table
shows UTC time, account, action, target and result. Expand **Before and after**
to see public fields. The table scrolls inside its container on small screens.
Each dashboard mutation records its account,
time, route, target, HTTP outcome and public before/after fields. Passwords,
Provider Keys, client secrets, tokens and URLs are not copied to the log.
Secret-only changes show metadata, not secret values. Authorized form errors
also have entries. Failed login attempts are not stored, to limit unauthenticated
log growth. An interrupted configuration operation has an unknown outcome.

Only an Owner can set retention in **Settings > Audit log**. The default is
365 days. The server removes old entries at start and once per hour. Each
DELETE removes at most 5000 entries. The task yields between batches. The log covers
dashboard operations, not direct database edits, environment recovery or
background model-data sync. Export audit records separately if you need
protection against database administrators. Dashboard OIDC sign-in is not
implemented.

### Metrics token

The initial token comes from `metrics_token_env`, then `metrics_token`. Later
changes on **Settings** apply at once. The token is encrypted in SQLite and is
never shown in HTML. Leave the field blank to keep it, or select **Remove token**.
If no token is set, `/metrics` needs no authentication.

## Native AWS Bedrock credentials

Select **AWS Bedrock (native, SigV4)** in Providers. Set the region, access key
ID, secret access key and optional session token. These credentials are dashboard
settings, not TOML seed fields. All credential fields use encrypted storage.
Saved fields are empty in HTML. An empty field keeps its saved value. Select
**Clear saved session token** to remove that token. Temporary credentials must
be replaced before they expire; automatic AWS credential refresh is not supported.

The region must match `[a-z]{2}(-[a-z]+){1,3}-[0-9]`, with a maximum of 40
characters. It sets `https://bedrock-runtime.REGION.amazonaws.com`. Other hosts,
ports, paths and URL schemes are refused. This adapter does not use a Key Pool
or a bearer key. SigV4 uses service `bedrock` and signs the exact request bytes.
The existing Bedrock OpenAI API-key route is separate and is not changed.

Use a Model ID or inference profile ARN for the upstream Model. The Proxy
URL-encodes the whole ID as one path segment. The API paths are
`/model/{modelId}/converse` and `/model/{modelId}/converse-stream`; see the
[Converse reference](https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_Converse.html)
and [ConverseStream reference](https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_ConverseStream.html).
Only Chat Completions and Messages POST requests use this adapter.

The binary stream reader checks both IEEE CRC32 values. A frame is at most
16 MiB and has at most 64 headers. A damaged or incomplete stream returns a
client error event and a failed Ledger Entry. HTTP 401 and 403 are not retried.
HTTP 429 and 5xx keep the normal retry rules.

## Cohere v2 Chat

Select **Cohere (v2 Chat)** in Providers and add a Cohere API key. The API
format is `cohere`. The preset base is `https://api.cohere.com`; the upstream
path is `/v2/chat`. A base that ends with `/v2` is also accepted. This adapter
always uses `Authorization: Bearer`. Keys use the existing encrypted Key Pool.
Only canonical Chat Completions and Messages POST requests are supported.
The upstream Model can be set by a Model or Route as with other Providers.

The adapter maps text, images, function tool calls and tool results. It maps
token limits, temperature, `top_p` to `p`, `top_k` to `k`, stop sequences and
JSON response formats. Tool choice supports auto, required and none; a named
tool choice is refused. Documents, citations and thinking content are not
mapped. Other unsupported request options are not forwarded. Model discovery
and the connection test are not available for this format. See the
[Chat reference](https://docs.cohere.com/v2/reference/chat) and
[streaming reference](https://docs.cohere.com/v2/reference/chat-stream).

## Database writer limits and Ledger recovery

Use **Settings > Server and router** to set these limits. Changes apply to new
writer acquisitions without a restart.

| Setting | Default | Range |
| --- | --- | --- |
| Database writer wait limit (`writer_timeout_secs`) | 120 seconds | 1 to 3600 seconds |
| Ledger spool limit (`ledger_spool_max_bytes`) | 67108864 bytes (64 MiB) | 1 to 1073741824 bytes |

Writer wait time includes admission, pool acquisition, SQLite busy waits and
retry delays. After 10 seconds, a warning names the waiting caller. At the hard
limit, acquisition returns an error. Waiting writers share one admission gate;
they do not each hold a pool connection. This limit does not cancel a transaction
that has already started.

Normal and blocked Proxy requests save a failed Ledger write in
`ledger-spool.jsonl`, next to the SQLite database. The file has Unix mode `0600`.
It contains usage, identities and frozen amounts, not request or response bodies.
Request diagnostics and body captures are not spooled. The file is not encrypted.
Each append ends with a newline and is synced to disk before
the Proxy releases its Reservation. The size limit includes both this file and
`ledger-spool.replay.jsonl`. If the spool is full or cannot be synced, the Proxy
logs an error and keeps the entry and its Reservation in memory while it retries.
Shutdown waits for these writes. Repair the storage problem before you stop the
process; an unsaved in-memory entry cannot survive a forced stop.

A synced entry's actual Price counts against its User, Team and Virtual Key
Budgets until replay commits it. Each Budget uses the entry's timestamp to select
the UTC period. Startup rebuilds these pending charges from both spool files.
Writer contention does not give back this Budget capacity.

The replay task checks the spool every 5 seconds. It renames the active file to
`ledger-spool.replay.jsonl` and removes that segment only after all entries commit.
New writes can append to a new active file during replay. Each entry keeps one
idempotency key. A repeated replay cannot add a second Ledger row or rollup charge.
Startup validates both files and drains both segments before it accepts requests.
A corrupt record, an incomplete record or a replay error stops startup. It does
not discard the segment. See [Ledger spool recovery](#ledger-spool-recovery).

Batch accounting uses its existing durable `batch_jobs` row instead of the spool.
A writer timeout leaves the job unaccounted and keeps its persisted Reservation.
The next batch poll retries it. The job state, Ledger Entries and batch entry
links commit together, so a later poll cannot charge the batch twice.

Retention tasks have a separate 100 ms writer acquisition limit. A busy task
returns and retries on its next scheduled tick. Body and MCP retention remove at
most 500 rows per tick; audit retention removes at most 5000. Cache maintenance
removes at most 500 expired or oversized rows and 500 excess rows per tick.
Large model imports and explicit repricing remain atomic transactions. Do not
schedule them together with other large storage operations.

Stop the Proxy for a consistent backup of the database, both spool files if
present, and the Master Key. Restore them together. Old spool segments are safe
to replay against the matching database backup because their keys are persistent.
Do not share one data directory between different databases. In-memory SQLite
databases have no durable spool and are for tests only.

### Ledger spool recovery

1. Stop the service. Keep a copy of the database, both spool files and the Master
   Key before you change any file.
2. Read the startup error to find the damaged spool file. Each JSON record must
   end with a newline. Do not delete the file, skip a record or start with an
   empty spool. These actions can lose charges.
3. Restore the damaged file from a matching backup. If no complete backup exists,
   recover the original record and its frozen amounts before you restart. Keep
   its `write_key` unchanged. A missing tail cannot be reconstructed from a
   partial JSON record alone.
4. Start the service again. Startup validates both files and commits every
   pending entry before it accepts traffic. Repeated keys do not charge twice.

## Master Key and backups

The Master Key protects the stored Provider Keys, subscription credentials and
the metrics token. They use
AES-256-GCM. Each saved value has a new random 96-bit nonce. SQLite stores base64
of `nonce || ciphertext` (including the authentication tag). The Master Key is
not stored in SQLite.

The Proxy finds the Master Key in this order:

1. The variable `TOKOMOTIVE_SECRET_KEY`. It holds 32 bytes as 64 hex
   characters or as base64. An empty value or a wrong length stops the start.
2. The file `secret.key` next to the database. It holds 32 bytes in base64. If
   the file does not exist, the Proxy makes it at the first start with Unix mode
   `0600`.

Back up this file with the database. If you use the environment variable, back
up that value instead. Protect both backups. A database backup without its
Master Key cannot recover the stored secrets. A wrong or lost key causes a clear
startup error when the Proxy cannot decrypt a value. Do not replace the key to
repair that error. Restore the original key.

References with `env:NAME` are not encrypted. They store only the variable name.

Keep `secret.key` out of version control. It is not covered by this repository's
current ignore rules. Add the line `secret.key` to your local ignore rules before
you stage a whole working directory.

## `[[providers]]`

Each item defines one Provider. The file can have many items.

Use **Providers** to add, edit or delete Providers and to add or remove keys.
Names use letters, digits, dot, underscore or hyphen. Base URLs must use HTTP or
HTTPS and must not contain credentials, a query or a fragment. You cannot delete
a Provider used by a Route or Model. Update those references first. **Test**
calls the API's `/models` endpoint with the first key and reports its HTTP status.
It has a time limit of 15 seconds.

Saved keys show only their last 4 characters. A value `env:NAME` stores a
reference, not an encrypted key. `NAME` has letters, digits and underscores. The
Proxy resolves the variable at start and on each reload. Set the variable in the
service environment. A dashboard edit cannot change the environment of the
running process.

Only an Owner can add or remove `env:NAME` references in the dashboard. An
Admin cannot change the address, API format or authentication header of a
Provider with an environment reference. Use a pasted key for Admin-managed
Providers. For example, an Owner can use `env:OPENAI_API_KEY`.

Provider references cannot use names that start with `TOKOMOTIVE_`, `AWS_`,
`SSH_` or `CI_`, or end with `_PASSWORD` or `_PRIVATE_KEY`. They also cannot
use `GOOGLE_APPLICATION_CREDENTIALS`, `GITHUB_TOKEN`, `GH_TOKEN`,
`GH_ENTERPRISE_TOKEN`, `GITLAB_TOKEN`, `GLAB_TOKEN`, `DATABASE_URL`,
`REDIS_URL`, `SECRET_KEY` or `PRIVATE_KEY`. These checks are not case-sensitive.
They protect service and common system secrets. They apply to `api_key_env`
and `api_keys_env` in seed files as well. A reserved reference in old database
data causes a load error before the value is read. Remove that reference with
a trusted database tool before you start the service again.

The table below lists the keys of the seed file. The dashboard has the fields
`name`, `kind`, `auth`, `base_url`, `key_header`, `include_usage` and the keys.
The Add Provider form also has a preset selector. Presets fill editable fields;
they are not a seed-file key. See [Provider presets](usage.md#provider-presets).

| Key            | Type             | Default | Meaning                                                                                                          |
| -------------- | ---------------- | ------- | ---------------------------------------------------------------------------------------------------------------- |
| `name`         | string           | none    | Required. The name that `models.provider` uses.                                                                  |
| `kind`         | string           | none    | Required. `"openai"`, `"anthropic"`, `"gemini"`, `"bedrock"` or `"cohere"`. The kind selects the wire format and usage parser. |
| `auth`         | string           | `"api_key"` | `"api_key"`, `"chatgpt"`, `"codex"` or `"claude"`. Subscription auth needs sign-in on Providers. |
| `base_url`     | string           | none    | Required. The API origin or versioned base URL. The Proxy does not repeat `/v1`, Z.ai `/v4`, or the version in DeepInfra `/v1/openai`. A full Messages endpoint is also accepted. Custom prefixes and client query parameters stay. |
| `key_header`   | string           | by kind | `"bearer"`, `"x-api-key"`, `"api-key"` or `"x-goog-api-key"`. Applies to API keys only. If omitted, OpenAI uses Bearer, Anthropic uses `x-api-key` and Gemini uses `x-goog-api-key`. |
| `include_usage` | boolean         | `true`  | Send `stream_options.include_usage` for streamed Chat Completions. Off does not change options from a native client request. The Proxy still reads final usage. Presets can select a different default. |
| `api_key`      | string           | none    | One Provider Key, in the file. The Proxy encrypts it when it seeds. Do not use it in a repository that you commit. |
| `api_key_env`  | string           | none    | Name of the variable that holds one Provider Key. A non-empty variable takes precedence over `api_key`.         |
| `api_keys`     | array of strings | `[]`    | More Provider Keys, in the file. The Proxy encrypts them when it seeds.                                          |
| `api_keys_env` | array of strings | `[]`    | Names of variables. Each variable holds one Provider Key.                                                        |

### Provider request limits

Set the defaults in **Settings > Server and router > Default Provider request
limits**. Set overrides in **Providers > Advanced request limits**. A blank
Provider field inherits the current default. Changes apply without a restart.
The optional seed file uses `[server.provider_defaults]` and
`[providers.limits]`. Both tables accept these fields:

| Field | Default when not set | Rule |
| --- | --- | --- |
| `request_timeout_ms` | `upstream_timeout_secs` times 1000 | Positive milliseconds. Includes the body for JSON responses. Limits response headers only for streams. |
| `connect_timeout_ms` | `15000` | Milliseconds to connect, including the TLS handshake. Zero disables this limit. |
| `stream_idle_timeout_ms` | `0` | Milliseconds with no upstream bytes. Zero disables this limit. Each received chunk resets the timer. |
| `max_concurrent` | `0` | In-flight requests for this Provider, across its keys and models. Zero is unlimited. |
| `max_wait_ms` | `0` | Maximum wait for a concurrency slot. Zero does not wait. |
| `max_retries` | Each key once | Retries after the first attempt on this Provider. Zero allows one attempt. The next key comes first; then the key pool repeats. |
| `backoff_base_ms` | `0` | Delay before the first retry. It doubles for each later retry. |
| `backoff_max_ms` | `30000` | Maximum retry delay. Zero disables the delay. |
| `backoff_jitter` | `false` | With `true`, choose a random delay from zero through the calculated delay. |

Timeouts, waits and backoff values cannot exceed 86400000 ms (one day).
The retry limit cannot exceed 10000. The concurrency limit cannot exceed 1000000.
The router's `max_attempts` remains the overall request cap, including retries
after an OAuth refresh. No retry starts after stream bytes reach the client.
When retries are not set, the existing one-pass key pool and OAuth refresh
retry rules remain in use.

Connect timeouts are client-level in reqwest. The Proxy caches a client for
each proxy URL and connect timeout within one settings generation. Other
timeout limits apply to each request. Streams have no total body time limit.

A concurrency rejection is `concurrency_limited`, not a Circuit Breaker
failure. The Proxy tries the next Deployment or Fallback. It returns 429 with
`Retry-After: 1` only when all candidates are concurrency-limited. A resized
limit does not cancel in-flight requests. New requests use the new limit.

Metrics use the `provider` label: `tokomotive_provider_inflight`,
`tokomotive_provider_limited_total` and `tokomotive_provider_timeouts_total`.
Limited counter kinds are `wait`, `rejected` and `wait_timeout`.
Timeout counter kinds are `request`, `connect` and `stream_idle`.
Attempt traces use `request_timeout`, `connect_timeout`,
`stream_idle_timeout` and `concurrency_limited`.

### Google Gemini native API

Select the **Google Gemini (native)** preset, or use this seed configuration:

```toml
[[providers]]
name = "gemini"
kind = "gemini"
base_url = "https://generativelanguage.googleapis.com"
api_key_env = "GEMINI_API_KEY"
```

The base can end with `/v1beta`. The default key header is `x-goog-api-key`.
This adapter uses API keys, not a Google sign-in. Chat Completions and Messages
clients use their normal paths. The Deployment model is a single model ID,
such as `gemini-2.5-flash`; the `models/` prefix is also accepted.
The Proxy builds the native model path. It does not forward client query
parameters to Gemini. **Test** and Provider model search use `/v1beta/models`.
The stream-usage switch does not apply to this native API.

See [Gemini native Translation](usage.md#gemini-native-translation) for limits
and token accounting. The [Google API reference](https://ai.google.dev/api)
defines the base URL, key header and request format.

### Key Pool

Key Pools apply only to `auth = "api_key"`. Subscription Providers have no
Provider Keys. Their tokens use separate encrypted storage.

The Key Pool of a Provider holds the keys in the order that **Providers** shows.
The Proxy skips empty keys, and `env:NAME` references that are not set, and
removes duplicate keys.

A seed file fills the Key Pool in this order:

1. The single key. With only `api_key_env`, the Proxy stores a variable reference.
   With both keys, it resolves the variable at seed time and uses `api_key` if
   the variable is unset or empty. It stores that resolved key encrypted.
   With only `api_key`, it stores that key encrypted.
2. The keys in `api_keys`, encrypted.
3. The variables in `api_keys_env`, as references.

By default, the Proxy starts at the next key in turn. Set
`key_strategy = "quota"` on a Provider, or select **Quota** in its Key Pool
settings, to use the most remaining quota first. The account's lowest remaining
ratio across its current windows applies. Equal ratios use the earliest reset,
then the saved key order. Unknown quota ranks at 50% remaining. The default
is `key_strategy = "round_robin"`. Subscription sign-ins use separate Providers
and a Route with `strategy = "quota"`.

After a 401, a 403, a 429, a 5xx or
a connection error, it tries the next key. This happens before the Proxy sends
any data to the client. When the last key also fails, the Proxy returns that
result. The Ledger Entry records the number of the key and the number of
attempts.

If the Key Pool is empty, the Proxy writes a warning at start. The Proxy then
sends requests to the Provider with an empty key.

### Authentication headers

| `key_header` | Header that the Proxy sends to the Provider |
| --- | --- |
| `bearer` | `Authorization: Bearer <Provider Key>` |
| `x-api-key` | `x-api-key: <Provider Key>` |
| `api-key` | `api-key: <Provider Key>` (Azure) |

For Anthropic format, the Proxy also adds `anthropic-version: 2023-06-01`
if the client sent no such header. The connection test uses the same key header.

Seed-file example for Azure v1:

```toml
[[providers]]
name = "azure"
kind = "openai"
base_url = "https://your-resource.openai.azure.com/openai/v1"
key_header = "api-key"
include_usage = true
api_key_env = "AZURE_OPENAI_API_KEY"
```

For DeepSeek or OpenRouter, use `key_header = "bearer"` and
`include_usage = false`. Their streams send final usage without that option.
The seed file is read only for an empty database. Change existing Providers
in the dashboard.

This table applies to `auth = "api_key"`. ChatGPT token sharing uses a bearer
token with the normal public OpenAI API. Claude OAuth uses a bearer token, the
required betas and the Claude Code user agent, identity and billing marker.

### Subscription auth

To seed a Subscription Provider:

```toml
[[providers]]
name = "my-chatgpt"
kind = "openai"
auth = "chatgpt"
base_url = "https://api.openai.com"

[[providers]]
name = "legacy-codex"
kind = "openai"
auth = "codex"
base_url = "https://chatgpt.com/backend-api/codex"

[[providers]]
name = "my-claude"
kind = "anthropic"
auth = "claude"
base_url = "https://api.anthropic.com"
```

These base URLs and kind/auth pairs are required. Subscription requests use
fixed issuer and resource URLs, not an Operator-supplied token destination.
Do not set `api_key`, `api_key_env`, `api_keys` or `api_keys_env` for these
Providers. Tokens cannot be seeded in TOML. Open **Providers** and complete
sign-in. A Provider's authentication type cannot be changed after creation.

The optional Codex usage login attaches to a token-sharing ChatGPT Provider.
Its credential is separate from that Provider's inference credential. The
new `codex` auth uses Codex credentials for model requests instead. It requires
the fixed base URL above. ChatGPT token sharing remains recommended. See
[Subscription Providers](usage.md#subscription-providers) for sign-in steps,
Provider Access and the usage panel, and
[ADR 0003](adr/0003-subscription-providers.md) and
[ADR 0004](adr/0004-legacy-codex-oauth.md) for the owner's risk decisions.

Paste attempts expire after 10 minutes; device attempts expire after at most
15 minutes. Both are lost at restart. Before a request, the
Proxy refreshes a token that expires within 5 minutes. It also retries once
after a 401. Refresh is single-flight for each
credential. A failed or uncertain refresh needs sign-in again. Refresh errors
do not expose token response bodies. Run one process for each database.

The Proxy does not copy these client headers to the Provider: `Authorization`,
`x-api-key`, `api-key`, `Host`, `Content-Length`, `Accept-Encoding` and the hop-by-hop
headers. It copies all other headers.
Codex requests also replace client identity, account, session, version and
user-agent headers and do not forward cookies or OpenAI organization/project
headers. Their model destination is fixed; client query parameters are omitted.

## `[[models]]`

Each item defines one Model, its Cost Card and an optional Price Card.

| Key        | Type             | Default | Meaning                                                                                           |
| ---------- | ---------------- | ------- | ------------------------------------------------------------------------------------------------- |
| `name`     | string           | none    | Required. The name of the Model. A dated name that starts with `<name>-` also matches.            |
| `provider` | string           | none    | Required. The `name` of a `[[providers]]` item.                                                   |
| `aliases`  | array of strings | `[]`    | Other names that match this Model. An alias must match the whole name.                            |
| `cost`     | table            | none    | Required. The Cost Card. See "Rates".                                                             |
| `price`    | table            | none    | The Price Card. See "Price Card". Without it, the default Markup applies.                         |

You can write `cost` and `price` as inline tables or as `[models.cost]` tables.

### Rates

A `cost` table, a `price` table and a Context Tier use these keys. Each key is a
rate in USD per million tokens or Service units. Each key is optional and its default is `0`.

| Key              | Token Class         |
| ---------------- | ------------------- |
| `input`          | Input (not cached)  |
| `output`         | Output              |
| `cache_read`     | Cache read          |
| `cache_write_5m` | Cache write, 5 minutes |
| `cache_write_1h` | Cache write, 1 hour |
| `image_input`    | Image input         |
| `image_output`   | Image output        |
| `service_units`  | Non-token Service units: search units, audio seconds or speech input characters, as defined by the endpoint |

### Context Tiers

A `cost` table or a `price` table can have a `tiers` array. Each item has the
rate keys and this key:

| Key                 | Type    | Meaning                                                              |
| ------------------- | ------- | -------------------------------------------------------------------- |
| `above_input_tokens` | integer | Required. The tier applies when the input tokens are more than this value. |

Rules for tiers:

- The input tokens include input, cache read, cache write and image input
  tokens.
- If more than one tier applies, the tier with the highest limit wins.
- A tier replaces all base rates. A rate that the tier does not set is 0.
  Repeat each rate that the tier needs.

Example, in `[models.cost]` form:

```toml
[[models]]
name = "claude-haiku-5-5"
provider = "anthropic"

[models.cost]
input = 0.10
output = 0.50

[[models.cost.tiers]]
above_input_tokens = 100000
input = 0.50
output = 2.50
```

### Price Card

A `price` table has exactly one of these forms:

| Form           | Keys                                                  | Result                                          |
| -------------- | ----------------------------------------------------- | ----------------------------------------------- |
| Markup         | `markup_percent` (float)                              | Price Card = Cost Card + this Markup.           |
| Explicit rates | The rate keys, and optionally `tiers`                 | The rates are the Price Card.                   |

A Price Card with both forms is an error. A Price Card with neither form is an
error. A Price Card with rates that are all 0 and no tiers counts as having no
rates.

The Price Card for a request comes from the first item that applies:

1. The Team override for the Model.
2. The Price Card of the Model.
3. The Cost Card with `server.default_markup_percent`.

## `[[teams]]`

| Key               | Type  | Default | Meaning                                              |
| ----------------- | ----- | ------- | ---------------------------------------------------- |
| `name`            | string | none   | Required. The name of the Team.                      |
| `price_overrides` | array | `[]`    | Price Cards that replace the Model Price Card for this Team. |

Each item in `price_overrides` has these keys:

| Key                      | Meaning                                                              |
| ------------------------ | -------------------------------------------------------------------- |
| `model`                  | Required. The name of a Model.                                       |
| `markup_percent` or rates | Required. Same rules as a Price Card. Use one form only.            |

Example:

```toml
[[teams]]
name = "acme"
price_overrides = [{ model = "gpt-4o", markup_percent = 50 }]
```

If `model` names a Model that is not in the database, the Proxy writes a warning
and skips the override.

## `[[users]]`

| Key              | Type             | Default | Meaning                                                                                              |
| ---------------- | ---------------- | ------- | ---------------------------------------------------------------------------------------------------- |
| `name`           | string           | none    | Required. The name of the User.                                                                      |
| `team`           | string           | none    | The `name` of a `[[teams]]` item. Without it, the User has no Team.                                  |
| `budget_usd`     | float            | none    | The Budget in USD. Without it, the User has no Budget.                                               |
| `budget_monthly` | boolean          | `false` | Legacy seed option. If `true`, selects a monthly Budget. If `false`, selects a lifetime Budget. Use the dashboard for other periods. |

### Budget periods, alerts and key expiry

Use the dashboard for these settings. No file change is needed. On **Users**,
**Teams** and **Virtual Keys**, set the Budget amount, period and window.
Calendar periods are none (lifetime, the default), daily, weekly (Monday),
monthly, quarterly and yearly. Calendar periods start at 00:00 UTC. Quarters
start on January 1, April 1, July 1 and October 1. Years start on January 1.
For a rolling window, set 1 to 366 days. The period does not apply, and there
is no reset time. Every applicable Budget must permit the Reservation.
Spend uses indexed Ledger ranges and pending spool entries in the selected
window. Open Reservations and Batch holds also count against admission.
Existing Budgets keep their calendar period when the database is upgraded.

On **Settings**, add webhook URLs, select Generic JSON, Discord or Slack, and
enable each webhook. The shared thresholds default to `50,80,100` percent.
The Proxy stores URLs encrypted with the Master Key. It shows only the host,
not the URL path or token. A blank URL in an edit keeps the saved secret.
**Send test** queues a test event. Reload the page for the last delivery status.
Delivery uses a 10-second timeout, no redirects and at most three attempts.
Only connection failures, HTTP 429 and HTTP 5xx are retried.

Calendar Budget events are sent once per subject, threshold and period.
A rolling threshold fires once. It becomes active again only when a Budget
check finds spend below the threshold minus 5 percentage points. For example,
an 80% alert becomes active again below 75%. This state survives restarts.
models.dev price alerts also send webhook events. Delivery failures do not
fail client requests. Failed events are not replayed after the retries or a
restart. Use trusted webhook endpoints with the required network access.

On **Virtual Keys**, set the optional expiry date and time in UTC. A blank
value means no expiry. Expired keys return HTTP 401 in both API formats.
Simple Mode hides money controls and money alerts but still applies key expiry.
| `rpm_limit`      | integer          | none    | Requests per minute across all Virtual Keys of the User. |
| `tpm_limit`      | integer          | none    | Completed tokens per minute across all Virtual Keys of the User. |
| `key_rpm_limit`  | integer          | none    | Requests per minute for each new Virtual Key in `keys`. |
| `key_tpm_limit`  | integer          | none    | Completed tokens per minute for each new Virtual Key in `keys`. |
| `keys`           | array of strings | `[]`    | Virtual Keys to create at start. For development only. See below.                                    |
| `key_allowed_providers` | array of strings | `[]` | Provider names allowed for each new Virtual Key in `keys`. Empty allows all API-key Providers, but no Subscription Providers. A non-empty list allows only these Providers. |

Rate Limits must be non-negative integers. Omit a limit for unlimited use. Zero
blocks requests. Both the User and Virtual Key limits apply. TPM counts all
Token Classes after a request finishes. The window is the last 60 seconds.
Seeding does not change the limits of an existing User or Virtual Key.

A key in `keys` is stored with the label `config`. The key is in plain text in the
file. Do not use this setting for production Users. Create Virtual Keys on the
**Keys** page of the dashboard instead.

For example, to allow a seeded key to use a subscription and an API-key
Fallback:

```toml
[[users]]
name = "development"
keys = ["development-key-only"]
key_allowed_providers = ["my-chatgpt", "openai-api"]
```

Use the Provider names saved in your database. This setting applies only when
a key is first created. It does not replace existing Provider Access. Edit
each key with the checkboxes on **Keys**. The database field `allowed_providers`
holds this list; `NULL` and `[]` both mean API-key Providers only. Provider Access
still applies to provider prefixes, Fallbacks and model discovery.

## Seeding

The optional file seeds the database. Providers, their keys, Routes, router and
server settings seed together only before runtime settings exist (first start).
Even an empty first start marks this step complete. A later file cannot replace
UI edits or restore a deleted Provider or Route. Models, Teams, Users and Virtual
Keys keep the additive rules below.

| Item                          | Rule                                                                                             |
| ----------------------------- | ------------------------------------------------------------------------------------------------ |
| `server.default_markup_percent` | Set only if the database has no value yet. Later changes are made in the dashboard.            |
| Team                          | Added if no Team has the same name.                                                              |
| Model                         | Added with its Cost Card and Price Card if no Model has the same name. If the Model exists, the file is ignored for its Provider, aliases and cards. |
| Team override                 | Added if the Model has no card for this Team. If the Model is not in the database, the Proxy writes a warning. |
| User                          | Added if no User has the same name. If the User exists, the Team and the Budget are not changed. |
| Virtual Key in `keys`         | Added for the User if the key is not in the database. This also applies to existing Users.       |

Changes that you make in the dashboard stay after a restart. Use the dashboard
to edit seeded items. The Proxy also seeds Provider Keys from the file: see
"Key Pool". Router changes preserve Circuit Breaker and key rotation
state. Timeout changes build a new HTTP client for new upstream requests.
Requests already in progress can finish with the settings that they captured.

## Routes and router settings

A Route has a name, a strategy, Deployments and Fallbacks. Use **Routes** to
create, edit or delete a Route. The page has one section for each Route.

| Field        | Meaning                                                                                              |
| ------------ | ---------------------------------------------------------------------------------------------------- |
| Name         | The model name that clients send. Use letters, digits, dot, underscore or hyphen. The name must be unique. |
| Strategy     | `priority`, `round_robin`, `weighted`, `quota` or `adaptive`. See below. |
| Deployments  | Rows that you add and remove. Each row has a Provider, an upstream Model and a weight. The positive integer weight applies to Weighted, Adaptive and weighted session selection. |
| Fallbacks    | A comma list of Route names or catalog Model names.                                                  |

The strategies:

| Strategy      | Order of the Deployments                                                                          |
| ------------- | ------------------------------------------------------------------------------------------------- |
| `priority`    | The order of the rows.                                                                            |
| `round_robin` | The order of the rows, but the first choice moves one row for each request.                       |
| `weighted`    | One Deployment is the first choice, selected at random by weight. The others follow in descending weight order. |
| `quota`       | Most remaining quota first, using each Deployment's best available account. Equal ratios use the earliest reset, then row order. Unknown quota ranks at 50% remaining. |
| `adaptive`    | Random selection by weight times success squared divided by latency. Uniform exploration keeps estimates fresh. See [Adaptive routing](usage.md#adaptive-routing). |

Rules for Fallbacks:

- The Proxy tries the Fallbacks after all Deployments of the Route fail.
- A Fallback does not expand recursively. A Fallback Route contributes its own
  Deployments only.
- The Proxy skips an unknown Fallback name at request time and writes a warning.
- Model permission for a Route includes its Fallbacks. Provider Access still
  applies to each Deployment.
- You cannot delete or rename a Route that another Route uses as a Fallback.
  Remove that reference first.
- You cannot delete a Provider that a Route uses.

The request-time behavior is in "Routing and Translation" in
[usage.md](usage.md#routing-and-translation).

The optional file uses `[[routes]]` with `name`, `strategy`, `deployments` and
`fallbacks`, and `[router]` with these settings. They also appear on
**Settings**:

| Setting             | Default | Meaning                                                                                  |
| ------------------- | ------- | ---------------------------------------------------------------------------------------- |
| `failure_threshold` | `3`     | Positive number of failures in a row before a Circuit Breaker opens.                     |
| `cooldown_secs`     | `30`    | Seconds that an open Circuit Breaker waits before it allows one probe request. 0 or more. |
| `max_attempts`      | `8`     | Positive limit on upstream attempts for one request. Each Provider Key try counts.       |
| `quota_exhausted_percent` | `98` | Skip accounts at this used percentage until reset. A finite number from 0 to 100. Applies to all strategies. |
| `adaptive_epsilon` | `0.05` | Uniform exploration probability, from 0 to 1. |
| `adaptive_half_life` | `10.0` | Positive, finite EWMA half-life in samples. |
| `adaptive_prior_decay_minutes` | `5.0` | Positive, finite idle interval before estimates decay toward the neutral prior. The same interval is the decay half-life. |

The Proxy keeps one Circuit Breaker for each pair of Provider and upstream model.
A connection error, a 429 without a known reset, a 5xx or a failed token request counts as
a failure. Any other response resets
the count. When the count reaches `failure_threshold`, the Circuit Breaker opens.
After `cooldown_secs`, one probe request tests the Deployment. A good result
closes the Circuit Breaker. A failed probe opens it again. If every candidate is
open, the Proxy tries them anyway, except when an Adaptive Route is in use.
Adaptive never bypasses an open circuit. The Proxy keeps this state in memory, so it
resets at a restart.

An account with a known rate-limit reset is skipped until that reset, not the
fixed cooldown. An exhausted window without a reset uses `cooldown_secs` from
the last quota observation. Invalid credentials (401/403) disable the account until
a successful token refresh, a new sign-in, a key change, or **Re-enable now**.
The health state and quota observations persist across restarts. If all
accounts are skipped, both API formats return 429 with `Retry-After` set to
the earliest reset in seconds. With no known reset, it uses the cooldown.
See [account health](usage.md#subscription-quota-and-account-health) for the
Operator controls, session behavior, webhook events and skip metrics.

All Provider, Route and router changes apply without a restart. The TOML loader
and the dashboard share the same validation.

## Exact response cache settings

Each Route has an optional `cache` object. The same fields appear on **Routes**:

```toml
[[routes]]
name = "cached-chat"
deployments = [{ provider = "openai", model = "gpt-4o" }]
cache = { enabled = true, ttl_secs = 300, scope = "key" }
```

| Route field | Default | Meaning |
| --- | --- | --- |
| `cache.enabled` | `false` | Enable exact response caching on this Route. |
| `cache.ttl_secs` | `300` | Lifetime in seconds. Zero disables caching. The UI requires 1 or more. |
| `cache.scope` | `"key"` | `key`, `team` or `global`. Team scope requires a Team. Scopes never share entries. |

Global limits are live **Settings**, not `[server]` seed fields. They are stored
in the runtime settings object's `cache` field:

| Runtime field | Default | Meaning |
| --- | --- | --- |
| `cache.max_entries` | `10000` | Maximum stored entries. Zero disables storage and hits. |
| `cache.max_bytes` | `104857600` | Maximum total encrypted body bytes. Zero disables storage and hits. |
| `cache.max_entry_bytes` | `1048576` | Maximum encrypted entry bytes, including metadata. |
| `cache.hit_price_micros` | `0` | Fixed Price per hit, in microdollars. Must be non-negative. Cost is always zero. |

The cache table is `response_cache`. Bodies use the existing Master Key.
Expiry checks apply at lookup. Every 60 seconds, a tracked maintenance task
removes up to 500 expired or oversized entries and up to 500 excess entries.
Saved limit changes also run one maintenance batch. A store repeats eviction
in batches of up to 500 rows in the same transaction until the entry count and
total encrypted bytes fit the limits. Only then does it commit. Eviction keeps
the newest entries, not the most recently used entries. These limits apply to
stored bodies, not to SQLite file overhead.
See [cache use and accounting](usage.md#exact-response-cache) for normalization,
client bypass headers, SSE variants and Rate Limits.

## Docker

The `Dockerfile` makes an image that holds the binary and a copy of
`config.example.toml`.

| Item                   | Value                                                                                  |
| ---------------------- | -------------------------------------------------------------------------------------- |
| Base of the final image | `debian:bookworm-slim`                                                                |
| User                   | `tokomotive` (uid 10001)                                                                     |
| Working directory      | `/data`                                                                                |
| Volume                 | `/data`                                                                                |
| Port                   | `8787`                                                                                 |
| Configuration file     | `/etc/tokomotive/config.toml`                                                      |
| `TOKOMOTIVE_CONFIG` | Not set; the example seed file is optional. |
| `TOKOMOTIVE_LISTEN` | `0.0.0.0:8787`                                                                        |
| `TOKOMOTIVE_DATABASE` | `sqlite:///data/tokomotive.db`                                                  |

Build and run:

```sh
docker build -t tokomotive .
docker run -p 8787:8787 -v tokomotive-data:/data \
  tokomotive
```

The file `compose.yaml` builds this image and runs it with these settings:

- It publishes `127.0.0.1:8787:8787`.
- It mounts the volume `tokomotive-data` at `/data`.
- It passes `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `RUST_LOG` and
  `TOKOMOTIVE_ADMIN_PASSWORD` from the shell or from a `.env` file.
- It has a health check on `/healthz`.
- It does not set `TOKOMOTIVE_SECRET_KEY`. The line is a comment, because an
  empty value stops the start. Without it, the Proxy makes `/data/secret.key`.

Start it with `docker compose up -d`. See [INSTALL.md](../INSTALL.md).

Rules for Docker:

- Open `/setup` on the first start. To seed instead, mount your own file and add
  `--config /etc/tokomotive/config.toml` after the image name. The image does
  not use the example file unless you ask for it.
- Keep the database on the volume `/data`. Without a volume, the Ledger Entries
  are lost when the container is removed.
- Pass the secrets with `-e NAME`, or with `env:NAME` references. Do not write
  them in a file.
- Back up `/data/secret.key` and the database. Keep the volume across updates.
- To publish the port on the loopback interface only, use
  `-p 127.0.0.1:8787:8787`.
- Run one container for each database. See the Limits in [README.md](../README.md).
