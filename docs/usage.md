# Operator guide

## Guardrails

Open **Guardrails**. Add a rule set, then attach it to a Route, Team or
Virtual Key. The Proxy uses the union of these attachments. A block has
priority over redaction. Redaction has priority over logging. The rules
also apply in Simple Mode.

Rules have a name, kind, action and direction. Use `request`, `response` or
`both` for the direction. The actions are `block`, `redact` and `log`.
Redaction uses `[REDACTED:rule-name]`. A regex uses the Rust `regex` syntax.
Keyword lists can use whole-word matching and case-sensitive matching.
The built-in detectors are `email`, `phone`, `credit_card`, `iban` and
`secret`. See [configuration](configuration.md#guardrails) for their limits.

Example rule array:

```json
[
  {"name":"EMAIL","kind":{"type":"detector","detector":"email"},"action":"redact","direction":"both"},
  {"name":"restricted","kind":{"type":"keywords","words":["restricted"],"whole_word":true,"case_sensitive":false},"action":"block","direction":"request"}
]
```

Request rules inspect text in Chat Completions messages, Responses input,
Anthropic system and message blocks, tool results, and embeddings input.
They do not inspect images, audio or binary data. Redaction changes the
upstream request body. Opt-in body capture stores this changed request.
Request checks run before the response cache lookup. Cached responses still
pass through response rules, even if a rule set changed after the cache write.

A blocked request returns HTTP 400. The JSON error has the stable code
`guardrail_blocked` and the rule name, not the matched text. Anthropic errors
also have the top-level `type: error`. The Ledger flag stores only rule names.
The counter `llm_guardrail_matches_total` has `rule` and `action` labels.

Non-streaming response rules inspect the assembled JSON text. Streaming
rules inspect assembled text across SSE events. The Proxy keeps 8 KiB of
text between windows. A match that needs more than 8 KiB across released
windows can be missed. Use non-streaming requests when this limit is not
acceptable. Buffering adds latency. A block during a stream terminates it
with an error event in the client API format. Earlier released text cannot
be recalled. A stopped stream can still have upstream usage and Cost.

Use **Test a rule** to test a saved set on pasted text. It shows the rule
names, byte offsets and changed text. It does not store the text or results
and does not call external hooks.

## Egress proxies

Open **Settings > Global egress proxy** to set the default outbound proxy.
On a Provider card, open **Egress proxy** and select one connection:

- **Use global** uses the default from Settings. This is the initial setting.
- **None** connects directly, even when a global proxy is set.
- **Custom proxy** uses the URL saved for that Provider.

Only an Owner can change an egress proxy. A proxy decides where the Proxy
sends Provider credentials. Admins and Viewers see the setting but cannot
change it.

Enter a URL such as `http://user:password@proxy.example:8080` or
`socks5h://user:password@proxy.example:1080`. The port is required. Do not add
a trailing slash, path, query or fragment. Percent-encode reserved characters
in the user name and password. A blank field keeps the saved URL. Select
**None** to remove it. The Master Key encrypts the complete URL. The page
shows only `scheme://host:port`, without the user name or password.

The Provider setting applies to inference, retries, model discovery, file and
batch requests, sign-in, device sign-in, token refresh and plan usage checks.
It also applies to Vertex service-account grants and Code Assist discovery.
models.dev Sync and alert webhooks use only the global default. Trace export
has its own client and does not use these proxy settings.

Select **Test connection** on an API-key Provider card. The result identifies
the connection as `via proxy host:port` or a direct connection. Proxy connection
and authentication failures have fixed messages. Upstream replies show the
status, not the response text. Existing API-format restrictions still apply.
Monitor `tokomotive_proxy_connect_errors_total{provider="NAME"}` for proxy
connection errors on inference, sign-in, refresh and connection tests.

Use only a trusted proxy. An egress proxy can reach private network services.
It can read unencrypted HTTP traffic and its Provider credentials. HTTPS
upstream certificate checks remain enabled. Redirects are not followed.
Changing a proxy URL is a trusted configuration operation, not a way to grant
Provider Access to a Virtual Key. Process proxy environment variables do not
override these explicit connection settings.

## Request diagnostics

Use **Ledger** as the request log. Filter by User, Provider, Model, UTC dates,
status, attempt error class, Provider Key number or retries. Provider Key
numbers start at 1 in the dashboard. Filters apply to each page. Open an
entry's **Details**, then **Attempt timeline** to see the ordered attempts.
Each attempt shows its Route, Deployment, Provider, key last 4, status,
error class, skip reason, latency and time to first token. Old entries have
no attempt records. Diagnostics keep at most 100 records per request and no
more than `max_attempts`. Actual attempts take priority over skip records.

To export traces, open **Settings > Traces and body capture**. Enter the full
OTLP/HTTP endpoint, such as `http://collector:4318/v1/traces`. Set the service
name and sample ratio, then select **Export traces**. Export is off by
default. Headers use one `Header: value` per line. Saved headers are encrypted
and masked. An empty field keeps them; select **Clear saved headers** to
remove them. Export uses JSON, not protobuf. It does not follow redirects.
One request span contains the GenAI model and token attributes, User, Team ID
and Virtual Key name. Each recorded attempt has a child span. Bodies and
credentials are not exported.

Body capture is separate from trace export. It is off by default. Enable it
on a **Route** or in a **Virtual Key** edit panel. Either setting enables
capture for that request. The Ledger timeline shows the captured bodies to
Admins only. Non-streaming requests capture the upstream response body.
Streaming requests capture text events, up to the same size cap. Secrets in
known token patterns and credential fields are redacted before encryption.
Request headers are not captured. Redaction does not remove all personal
data. Enable capture only when you need it and protect database backups.

The default cap is 64 KiB per body. A clipped body is marked **truncated**.
The default retention is 7 days. The background purge removes up to 500 old
captures each minute. A large backlog can take more than one pass. The Master
Key is required to read captures. Keep it with your database backup.

Export uses a 128-request queue and batches up to 16 requests. A full queue
drops traces without delaying requests. Each export has a 5-second timeout.
Use `/metrics` to monitor `tokomotive_trace_export_queue_drops_total`,
`tokomotive_trace_export_failures_total` and `tokomotive_captured_bodies_total`.
The capture counter counts two bodies for each captured request.

## Gemini CLI (Code Assist)

Select the Gemini CLI preset. Save the Provider. Select **Sign in**, allow
access on Google, then paste the code from the Code Assist page. Device
sign-in is not available. The Proxy loads the account tier and project. For a
new account it starts onboarding and waits for a bounded operation.

This is an unofficial subscription. Use can break the service terms. See
[ADR 0009](adr/0009-gemini-cli.md). Give each Virtual Key explicit Provider
Access. Cost is 0. Price follows the Price Card. Use canonical Chat Completions
or Messages POST requests; the Proxy translates JSON and SSE to Gemini.

This guide is for the Operator. It shows how to connect clients, how the Proxy
routes requests, how to use the dashboard, how the Proxy calculates amounts, and
how to monitor the Proxy.

The terms are defined in [GLOSSARY.md](../GLOSSARY.md). For the settings, see
[configuration.md](configuration.md). For the first installation, see
[INSTALL.md](../INSTALL.md).

In the examples, the Proxy runs at `http://127.0.0.1:8787` and the Virtual Key is
in the variable `KEY`:

```sh
export PROXY=http://127.0.0.1:8787
export KEY=sk-toko-...
```

## models.dev model data and pricing

The first start loads a bundled models.dev snapshot. It works without a network
connection. Daily sync uses `https://models.dev/api.json` by default. In
**Settings > models.dev Sync**, change the URL, turn daily sync off, or select
**Refresh model data**. A failed check keeps old data and shows an error.

Each Model matches a models.dev Provider from its Provider preset or API URL.
API URLs can differ by a trailing slash or `/v1`. Existing Providers also match
a hand-verified preset by its API URL. The Model matches by exact ID,
then `canonical_model_id`, then the longest `<id>-` prefix, as in catalog lookup.
The Model page shows the matched limits, capabilities, dates, status and Provider
documentation. It shows **Not in models.dev** when there is no match. Use the
Provider and Model override fields to correct a match. Blank fields restore
automatic matching. A saved override applies at the next sync.

Sync updates Cost Cards automatically. A price change creates a Card Version
with the data date and ETag in its source. An unchanged price creates no version.
Select **Pinned: models.dev sync does not change this card** to keep manual
rates. The Operator can still edit a pinned card. Subscription Providers keep
Cost zero. Old Ledger Entries do not change.

Cache write uses the 5-minute Token Class. The 1-hour rate uses
`cache_write_1h` when available. Otherwise it is twice the input rate for the
Anthropic format, and the cache write rate for other formats. Reasoning uses
the output Token Class. Context tiers and `context_over_200k` become Context
Tiers. Audio prices have no matching Token Class and are not imported.

A change of more than 50% shows **Price changed a lot. Check it.** on the
Overview. Open the old and new values and follow the Model link. Check plan
Models with care: upstream prices can be zero or retail rates that do not match
your contract. Pin the Cost Card when the data does not match your invoice.

The Add Provider preset list includes Providers with an API URL from models.dev.
They are marked **from models.dev**. Hand-verified presets take priority. For
new data presets, the API format is OpenAI unless the package names Anthropic.
The default key header is bearer. Check the Provider documentation before use.

For a request without `max_tokens` or `max_output_tokens`, the Reservation uses
the mapped output limit. **Reserved output tokens** is only the fallback when
the Model is unmapped or has no output limit. It does not cap a larger model limit.

The bundled data is from [models.dev](https://models.dev), fetched on 2026-11-13.
See [ADR 0005](adr/0005-models-dev-pricing.md) for the pricing decision and risks.

## Exact response cache

On **Routes**, set **Cache** to **On** and select its lifetime in seconds.
The default is Off. Select the scope with care:

- **Virtual Key** keeps each Virtual Key separate, including keys of the same User.
- **Team** shares responses only with keys whose Users belong to the same Team.
  A User with no Team bypasses this scope.
- **Global** shares responses with all permitted clients of this Route.

The cache supports Chat Completions, Messages, Responses and Embeddings.
Tool requests can use it. A request with `n > 1` bypasses it.
Only identical normalized bodies on the same endpoint and resolved Route can match.
The SHA-256 key includes the Route's Models, source settings and scope.
JSON object keys are sorted at every level. Array order stays unchanged.
The Proxy removes only these top-level fields before hashing:
`stream`, `stream_options`, `user`, `metadata`. Nested fields stay unchanged.
The endpoint includes its query string. The stream flag and
`stream_options.include_usage` select a separate response
format. API version and beta headers also separate entries.

A complete streamed miss can populate the cache. The Proxy stores the final
translated SSE bytes, not a JSON reconstruction. A hit replays those bytes in
the client's format, including a usage event when requested. JSON and SSE
do not share entries. A JSON request cannot hit an entry made by a stream.
Different SSE usage options do not share entries either.

Use `Cache-Control: no-cache` to skip lookup but permit a new stored response.
Use `Cache-Control: no-store` or `x-tokomotive-cache: bypass` to skip lookup and
storage. Successful responses have `x-tokomotive-cache: hit`, `miss` or `bypass`.
Errors, incomplete Responses, aborted or partial streams, and oversized entries
are not stored. Expired entries cannot produce a hit.

On **Settings**, set the entry limit, total byte limit and entry byte limit.
The default entry limit is 10,000. The default total is 100 MiB. The default
entry size is 1 MiB. Byte limits include encryption and metadata overhead.
The SQLite bodies use AES-256-GCM with the Master Key. Back up this key with
the database. Oldest entries are removed first when a limit is exceeded.
An expiry and limit check runs once a minute and after each store.
Saved limit changes also remove entries that exceed the new limits.

Each hit writes its own Ledger Entry with `cache_hit=true`, zero Cost and the
configured fixed **Hit Price**. The default Price is zero microdollars.
The Ledger retains the stored token counts and marks them as a cache hit.
These tokens did not run upstream. Hits count toward RPM, but do not add TPM
tokens. Existing TPM limits still apply at admission. In Full Mode, all
Budgets must cover the hit Price. Model and Provider permissions still apply.
Simple Mode keeps its usual exemption from Budgets and Rate Limits.

The Overview shows **Exact cache hit rate**: hits divided by all recorded requests in
the selected range. The `/metrics` endpoint exports `llm_cache_hits_total`,
`llm_cache_misses_total`, `llm_cache_hit_bytes_total` and
`llm_cache_stored_bytes_total`. Bypasses are not misses. Ledger-based metrics
have a `cache_hit` label so that you can separate stored usage from upstream usage.

## Dashboard accounts and audit log

Open `/setup` on first use. Choose the first Owner's username and password.
After an upgrade from the single-password dashboard, log in as `admin` with
the same password. An empty username also means `admin`.

In **Settings > Admin accounts**, an Owner can add accounts and set their roles:

- Owner: all configuration, admin accounts, metrics credentials, environment
  key references and audit retention.
- Admin: configuration and audit records, without admin-account management or
  metrics-credential, environment-reference or audit-retention changes. An
  Admin cannot change the address, API format or authentication header of a
  Provider with environment references. Pasted Provider Keys remain available.
- Viewer: read-only dashboard access. Configuration forms are hidden and
  configuration changes return HTTP 403. Logout and own-password changes
  remain available.

Keep at least one Owner. A role change, password change or account deletion
ends that account's sessions. To change your own password, enter the current
password in **Settings > Change password**.

Login permits 20 attempts per socket IP in a 5-minute window, with at most
5 attempts per username in that window. Changing the username does not reset
the IP budget. Wait for the window to end if the IP budget is full. A different
IP can still log in. Forwarded IP headers do not change the socket IP. The
bounded maps keep at most 4096 IPs and 16 usernames per IP. When a map is full,
the server removes the least recently used IP or the oldest username bucket.

Dashboard bodies have a 2 MiB limit and a 15-second read deadline. Body reads
do not lock other dashboard changes. The server checks the session and role
after it gets the mutation lock. These limits do not cut `/v1` streams.

Open **Audit log** to see dashboard changes. Filter by account, action, target
or UTC time, then use the page links for older entries. The compact table shows
25 entries by default. It scrolls inside its container on small screens.
Expand **Before and after** to see the public fields. Before/after fields
exclude secrets and URLs. An interrupted operation shows an unknown outcome.
Only an Owner can set retention in **Settings > Audit log**. The default is
365 days, with an hourly purge in batches of at most 5000 entries. The purge
task yields between batches. Only Owners and Admins can read the log. Dashboard OIDC sign-in
is not available.

## Connect a client

Set the base URL of the client to the Proxy. Use a Virtual Key as the API key.
The Proxy takes the key from `Authorization: Bearer <key>` or from
`x-api-key: <key>`.

The Proxy uses the Provider Keys of the Provider. The client never sends a
Provider Key.

### OpenAI SDK

```sh
export OPENAI_BASE_URL=$PROXY/v1
export OPENAI_API_KEY=$KEY
```

Or in Python:

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:8787/v1", api_key="sk-toko-...")
```

### Anthropic SDK

The Anthropic SDK adds `/v1/messages` to the base URL. Do not add `/v1`.

```sh
export ANTHROPIC_BASE_URL=$PROXY
export ANTHROPIC_API_KEY=$KEY
```

Or in Python:

```python
from anthropic import Anthropic

client = Anthropic(base_url="http://127.0.0.1:8787", api_key="sk-toko-...")
```

### Endpoints that the Proxy accounts

`POST /v1/messages/count_tokens` uses the same Model routing, Model permission,
Provider Access and request Rate Limits as Messages. The body limit is 1 MiB.
The Proxy forwards counts to Anthropic API-key and Claude subscription Providers.
For other deployments, including Vertex and Bedrock, it returns an estimate:
the UTF-8 byte length of the JSON messages, system and tools, divided by four
and rounded up. The response has `x-tokomotive-estimate: true`. This method
does not tokenize images or use the Model's tokenizer. Do not use it for billing.
The Proxy writes a zero-Cost, zero-Price Ledger Entry for audit and metrics.
Counts do not consume the token Rate Limit. A zero Reservation applies to a
User with a Budget; counting cannot add inference charges.

`POST /v1/rerank` accepts a Cohere-style JSON body with `model`, `query` and
non-empty `documents`. The body limit is 1 MiB. Streaming is not supported.
A `cohere` Provider uses `/v2/rerank`; an OpenAI-compatible Provider uses
`/v1/rerank`. You can use the latter for Jina or Voyage. Provider-specific
options pass through. The Proxy does not convert optional options or responses.
Model routing, Model permissions, Provider Access, Rate Limits and Budgets apply.

The Proxy records Cohere `meta.billed_units.search_units` as Service units.
For Jina and Voyage, `usage.total_tokens` (or `prompt_tokens`) is Input.
Set the Model's `service_units` Cost/Price rate in dollars per million search
units, or set its Input rate for a token-based Provider. For example, $0.002 per
search unit is `service_units=2000` in a seed card. Blank rates are zero.
Service units have their own Ledger and `llm_service_units_total` counts; they
do not consume TPM. Card Versions, Markup and Team overrides apply as for tokens.
Amounts are frozen on the Ledger Entry under ADR 0002. If usage is absent, the
Proxy records an Estimated Entry with one search unit per request byte. This is
a conservative estimate, not a Provider bill. Budget Reservations allow one
search unit and one input token per request byte across all candidate Models.

`POST /v1/audio/transcriptions` and `/v1/audio/translations` accept multipart
uploads for OpenAI-compatible Providers. The multipart `model` field uses normal
Model routing, including Model rewrite. Each request needs one non-empty `file`.
The total multipart limit is 25 MiB by default. Set **Audio upload limit (MiB)**
in Settings to a value from 1 to 63. The seed setting is
`server.audio_upload_limit_mib`. Text fields have a 64 KiB limit, and each request
can have at most 32 fields. Streaming transcription is not supported.

One Service unit is one audio second, rounded up per request. The Proxy uses
`usage.seconds` first, then the `duration` in a JSON response. Select the upstream
`verbose_json` response format to get duration where supported. If neither value
is present (for example a text or SRT response), the Proxy estimates one second
per 16,000 file bytes, rounded up, and marks the Ledger Entry as Estimated.
This assumes 128 kbit/s compressed audio; it is not exact for all formats.
Set `service_units` on the Model's Cost/Price card in dollars per million audio
seconds. For example, $0.006 per minute is `service_units=100`.
Budget Reservations allow one audio second per multipart request byte. This is
deliberately conservative and can reject a large upload before the actual Budget
is exhausted. Requests consume RPM, but audio seconds do not consume TPM.

`POST /v1/audio/speech` accepts a JSON body up to 1 MiB. It streams the binary
upstream body without conversion, up to 64 MiB. One Service unit is one Unicode
input character. Set `service_units` on the Model card in dollars per million
characters. Reservations use the input character count. The Proxy records the
full input count even if the client disconnects or the stream fails after the
upstream accepts the request. Errors before a successful stream have no character
charge. Use a separate Model for each unit type; one Model card has one Service
unit rate. Audio requests use the same Model permissions, Provider Access,
Rate Limits, Budgets, Ledger and metrics as other accounted endpoints.

#### Files and batches

The OpenAI file endpoints are `POST /v1/files`, `GET /v1/files`,
`GET /v1/files/{id}`, `DELETE /v1/files/{id}` and
`GET /v1/files/{id}/content`. Batch endpoints are `POST /v1/batches`,
`GET /v1/batches`, `GET /v1/batches/{id}` and
`POST /v1/batches/{id}/cancel`. Each object belongs to one Virtual Key.
Other Virtual Keys get 404, even when they belong to the same User.
Lists use the local ownership table, not the upstream list. Set `limit` from
1 to 100 (default 20) and `after` to the last ID of a previous page. Results
use descending creation time, then descending ID. File lists also accept `purpose`.

Set `x-tokomotive-provider` on each upload to the name of an `openai`
API-key Provider. Provider Access applies. The Proxy selects the first key
in that Provider's Key Pool. It stores the Provider, base URL, key index and
key fingerprint with the object. All later calls use that same credential,
even if the pool order changes. There is no key retry, Route selection,
Translation or Model rewrite. If the credential is removed or the base URL
changes, restore it to access the object. A batch uses the Provider and
credential of its input file; it does not need the header again.

Upload one multipart `purpose` field and one `file` field. The Proxy buffers
the multipart body to check batch Model permissions before it sends data
upstream. The default limit is 16 MiB for the whole request. Set
`server.file_upload_limit_mib` in the seed file, or **File upload limit (MiB)**
in Settings, to a whole number from 1 to 63. File content downloads stream
without whole-file buffering. Uploads and file operations have no inference
Ledger Entry or money charge. Full Mode request Rate Limits apply to file
and batch API calls. Batch create counts as one request, not one per input line.

For `purpose=batch`, the file must contain JSONL. Each file can have at most
10,000 non-empty lines, with at most 1 MiB per line. Each line needs a unique
`custom_id`, `method=POST`, `url` and `body.model`. Use catalog Model names
from the selected Provider. The Virtual Key Model allowlist is checked at
upload and again at batch create. Supported batch endpoints are
`/v1/chat/completions`, `/v1/responses` and `/v1/embeddings`. Every line must
match the create request's `endpoint`. Set `completion_window` to `24h`.
Chat generation lines need `max_tokens` or `max_completion_tokens`.
Responses lines need `max_output_tokens`. Set exactly one output limit,
from 1 to 1,000,000. Function tools are supported; paid built-in tools are not.
Streaming, nested background requests, multiple output choices, stored context
and external or non-text inputs are not supported.

Full Mode batch create needs at least one enabled User, Team or Virtual Key
Budget and a Cost Card for every Model. The Proxy reserves the total estimated
Price. Each line allows one input token per JSONL byte and the explicit maximum
output tokens. It uses the highest applicable class rate across all Context
Tiers, applies the batch multiplier, then rounds the Reservation up per line.
This is deliberately conservative. The Reservation is stored before create
is sent upstream. It survives a restart and counts against all applicable
Budgets until accounting succeeds, including after a Budget period changes.
Synchronous requests also see these stored Reservations.

Cost and explicit Price Cards have an optional `batch_multiplier_bp` field.
It is a whole number of basis points from 0 to 65535. For example,
`cost={input=1.0,output=2.0,batch_multiplier_bp=5000}` uses 50% of normal Cost
rates for batches. A blank multiplier uses normal rates. A Markup Price
inherits the Cost Card multiplier; an explicit Price Card sets its own.
The card form has the same optional field. A card change makes a new Card
Version. The batch stores the Cost and Price Card Versions and resolved rates
at create. Later card edits do not change its amounts (ADR 0002). The Proxy
rounds money once per line after the multiplier is applied.

On GET, cancel, and a background check every five minutes, the Proxy checks
for `completed`, `failed`, `expired` or `cancelled`. Each background check
handles at most 20 unaccounted batches, oldest check first. Checks do not
overlap. The Proxy reads the output and error JSONL files with a combined
64 MiB limit, a 1 MiB line limit and at most 10,000 result lines. It writes
one Ledger Entry per submitted line, using the reported usage for successful
lines and zero usage for requests that did not run. Entries use endpoint
`batch`. A database transaction commits the entries and releases the
Reservation once. Repeated GETs and background checks cannot charge twice.
Reported completed tokens consume TPM when accounting commits in Full Mode.
Output and error files have the same Virtual Key ownership as the batch.
Files required by an unaccounted batch cannot be deleted.

An unavailable or invalid output keeps the Reservation and is checked again.
A timeout, 5xx response or process failure during create can leave an uncertain
upstream result. The Proxy does not retry create and keeps its durable
Reservation. The Operator must inspect that upstream account before manual
recovery. Do not remove such a Reservation without checking for charges.
Simple Mode requires no Budget, applies no Rate Limits, and writes zero money
amounts. Model permissions and ownership still apply. The create-time mode
also applies when the batch completes. Tests use a local mock OpenAI service;
live OpenAI account access is not verified.

| Path ends with                             | Native Provider kind | Usage that the Proxy records                  |
| ------------------------------------------ | ------------- | --------------------------------------------- |
| `/chat/completions`                        | `openai`      | Input, output, cache read                     |
| `/responses`                               | `openai`      | Input, output, cache read                     |
| `/embeddings`                              | `openai`      | Input                                         |
| `/images/generations`, `/images/edits`     | `openai`      | Input, cache read, image input, image output  |
| `/messages`                                | `anthropic`   | Input, output, cache read, cache write 5 minutes, cache write 1 hour |
| `/v1/rerank` (exact path)                   | `cohere` or `openai` | Search units or input tokens |
| `/v1/audio/transcriptions`, `/v1/audio/translations` (exact paths) | `openai` | Audio seconds |
| `/v1/audio/speech` (exact path)             | `openai` | Input characters |
| `/v1/batches` and owned batch objects      | `openai` | Per-line input, output and cache read at completion |

Except for the owned file and batch endpoints, the Proxy forwards paths under `/v1/` to the Provider, for all HTTP
methods. The Proxy does not read usage for other paths. It records them as
Ledger Entries with the endpoint `other`. If the Proxy cannot find a Model for
the request, the Ledger Entry is an Unpriced Entry.

The Proxy reads the Model name from the JSON body of the request, or from the
`model` field of a multipart audio request. Other non-JSON requests have no Model name.
The Proxy then records an Unpriced Entry unless the response body names the
Model. This applies, for example, to multipart requests to `/v1/images/edits`.

### Native AWS Bedrock

Add the **AWS Bedrock (native, SigV4)** preset in Providers. Enter the region
and AWS credentials, then add a Model or Route with its Bedrock Model ID or
inference profile ARN. Use the normal Chat Completions or Messages client path.
Both JSON responses and streaming responses support text and function tools.
Images must use base64 data URLs. Model discovery and the connection test are
not available for native Bedrock. Request options outside the mapped text,
image, tool and inference fields are not forwarded.

The Proxy records `usage.inputTokens` as Input and `outputTokens` as Output.
It records `cacheReadInputTokens` as Cache Read and `cacheWriteInputTokens` as
Cache Write. These input classes are separate; the Proxy does not subtract cache
tokens from `inputTokens`. The optional `cacheDetails` splits writes by TTL.
Writes without a TTL breakdown use the 5-minute class. See the
[AWS cache guide](https://docs.aws.amazon.com/bedrock/latest/userguide/prompt-caching.html#prompt-caching-converse)
and [TokenUsage reference](https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_TokenUsage.html).
Client Chat usage includes all input classes in `prompt_tokens`; Messages
usage keeps the three input classes separate.

SigV4 regression tests use the AWS-published
[get-vanilla, post-vanilla and post-x-www-form-urlencoded vectors](https://github.com/awslabs/aws-c-auth/tree/main/tests/aws-signing-test-suite/v4).
Tests use mock upstreams only. Live AWS account access is not verified.

### Routing and Translation

For **Cohere (v2 Chat)**, add the preset and a Provider Key, then add a Model
or Route with a Cohere Model ID. Both Chat Completions and Messages clients
can use JSON and streaming responses, including function tools. The Proxy
maps `content-delta`, `tool-call-*` and `message-end` events to client SSE.
An incomplete stream or an upstream ERROR/TIMEOUT returns an error, not a
successful completion.

For accounting, a complete `usage.billed_units` input/output report has
priority. Cohere defines these as the billed tokens. `usage.tokens` reports
raw model tokens and can be larger. The Proxy uses it only if the billed
report is absent or incomplete. It never adds both reports or subtracts
`cached_tokens` from the billed units. The same selected values are returned
in client usage. See [Chat usage](https://docs.cohere.com/v2/reference/chat#types)
and [stream usage](https://docs.cohere.com/v2/reference/chat-stream).
Live Cohere account access is not verified; tests use a local mock.

An explicit Route selects its Deployments before catalog lookup. The Proxy
replaces the requested Model with the upstream model name of the Deployment
(Model rewrite). The strategy of the Route sets the order of the Deployments. See
"Routes and router settings" in [configuration.md](configuration.md#routes-and-router-settings).
The Proxy tries the Key Pool of each Deployment before the next Deployment, then
the Fallbacks. Connection errors, HTTP 429 and HTTP 5xx can cause a retry before
the response starts. Other upstream errors do not cause a retry. The setting
`max_attempts` limits the total number of upstream attempts.

Each Deployment has a Circuit Breaker. After `failure_threshold` failures in a
row, the Proxy skips the Deployment for `cooldown_secs`. Then one probe request
tests it. If every candidate is open, the Proxy tries them anyway.

Without an explicit Route, `provider/model` selects that Provider and removes
the prefix, unless the full name matches a catalog Model. Otherwise, the Proxy
chooses the Provider in this order:

1. The Provider of the Model that matches the Model name.
2. The first Provider with the kind that the endpoint needs (`openai` for chat,
   responses, embeddings and images; `anthropic` for messages).
3. For other paths: the first `anthropic` Provider if the request has an
   `anthropic-version` header. If not, the first `openai` Provider.
4. The first Provider in the runtime table (ordered by name).

Chat Completions clients can use `anthropic` Deployments. Messages clients can
use `openai` Deployments. The Proxy translates the request, response, errors
and SSE events. It keeps query parameters and uses the upstream Provider Key.
It adds `anthropic-version` for Anthropic if necessary and removes
`anthropic-*` headers for OpenAI. Other accounted endpoints need their native
Provider kind. Translation does not change the Route strategy or Deployment
order. A Route with both Provider kinds can serve both client formats.

Translation supports text, images and function tools, with and without
streaming. It has these limits:

- It skips Deployments that cannot translate `n` greater than 1 or JSON-schema
  response formats. If no Deployment can send the request, it returns HTTP 400.
  If an upstream call already failed, it keeps the last upstream outcome and
  records a Ledger Entry.
- It drops other unsupported options, cache-control hints, thinking blocks and
  signatures.
- It translates choice 0 only.
- It buffers Chat tool calls until the stream ends when it converts them to
  Messages. Parallel tool calls become serial content blocks.
- Chat `reasoning_effort` (`low`, `medium` or `high`) sets a thinking budget of
  1,024, 4,096 or 16,384 tokens. An explicit output limit must be larger than the
  budget.

A Chat request can set both `max_tokens` and `max_completion_tokens` only if
their non-null values are equal. Different values return HTTP 400. Budget
Reservations cover the prepared output limits of all candidate Deployments,
including translated thinking budgets and defaults.

#### Gemini native Translation

A `gemini` Provider accepts Chat Completions and Messages clients. The Proxy
converts text, base64 image data URLs, system instructions, function tools,
tool results and output limits to the native Gemini format. It maps temperature,
top-p, top-k, stop sequences, JSON output formats and tool choice. Messages
requests use the existing Chat translation as an intermediate format.
JSON responses and SSE streams return in the client's format.

The upstream calls use `POST /v1beta/models/{model}:generateContent` and
`:streamGenerateContent?alt=sse`. Only canonical client paths
`/v1/chat/completions` and `/v1/messages` can use this adapter. Responses,
embeddings, arbitrary paths and remote image URLs are not supported.
Translation returns one candidate and rejects `n` other than 1.
Other unsupported client options are not forwarded. Thought text is not
returned as assistant text. Function calls can carry the `thought_signature`
extension in Chat tool calls or Messages tool-use blocks. A client must retain
this extension in its next request if the model requires signed tool history.

The Proxy reads the cumulative `usageMetadata` report. It subtracts
`cachedContentTokenCount` from `promptTokenCount` for input and records that
cached count as cache read. Output is `candidatesTokenCount` plus
`thoughtsTokenCount`. It does not add `totalTokenCount` again. The translated
client usage follows the same counts. SSE parsing accepts split UTF-8 and
CRLF boundaries, limits each event to 4 MiB and reports truncated generations
as stream errors. Budget Reservations use `generationConfig.maxOutputTokens`.

These fields and endpoints are defined in the
[Gemini API reference](https://ai.google.dev/api) and
[generateContent reference](https://ai.google.dev/api/generate-content).
The adapter tests use mock upstreams. Live account and model support are not
verified by these tests.

The Ledger Entry uses the client's endpoint and the upstream Model and usage.
Cost and Price use upstream Token Classes, before Translation. The Proxy
continues to read upstream streams after a client disconnect to obtain final
usage.

### Session affinity

Use Session affinity when a Route has several inference instances of the same
model. Add each instance as a separate Provider and Deployment. On **Routes**,
set **Session affinity** to **On**. It is off by default. A Route with only one
Deployment with no Key Pool ignores it. The setting keeps conversation turns on the account
that can reuse the prompt in its KV cache. It does not enable or control that
cache on the upstream server.

The Proxy reads the client request before Translation. It takes the first
non-empty string in this fixed order:

1. The `x-session-id` header.
2. The OpenAI body field `prompt_cache_key`.
3. The Anthropic body field `metadata.user_id`.
4. The OpenAI body field `user`.

The order is not configurable. Invalid header text, non-string body values and
empty strings are skipped. Explicit keys keep their whitespace. Use a unique
key per conversation. A value such as `user` or `metadata.user_id` that is the
same for all conversations keeps those conversations together.

Without an explicit key, the Proxy hashes the model name, initial system or
developer messages (or `system` and `instructions`), and the first user
message. This works for Chat Completions, Responses and Messages clients.
Responses also accepts a string `input` as the first user message. Adjacent
text parts are joined without a separator; whitespace is not changed. Images
use their URL or a hash of inline image data. Later conversation turns do not
change the key. Without a first user message or string input, the Proxy uses
the Route strategy instead.

The key is a SHA-256 hash scoped to the Route and the Virtual Key's User.
Different Users never share a pin. Different conversations with the same
initial prompt under one User can share a pin. Send `x-session-id` to separate
them. Raw keys and prompt prefixes are not stored in the affinity table,
Ledger or metrics.

For a new session with the Quota strategy, remaining quota orders the compatible
Deployments. Other strategies use weighted rendezvous hashing to order the compatible
Deployments that Provider Access permits. Deployment identity is the Provider
name plus the upstream model name. A pin also records the account. Weights apply even if the Route strategy
is Priority or Round robin. A live pin keeps its Deployment when weights
change. For sessions without a pin, adding a Deployment moves only the
sessions that it wins. Removing a Deployment moves only its sessions.

If the pinned Deployment's circuit is open, the Proxy tries the next
Deployment in rendezvous order. It also retries connection errors, HTTP 429
and 5xx before it sends a response to the client. Fallbacks, Provider Key
pools, the attempt limit and half-open probes work as before. The existing
all-circuits-open recovery rule still applies. A transient failure does not
replace a live account pin. After the circuit recovers, the session returns to
that account. An account at the quota exhaustion threshold (default 98% used),
an active rate limit, or invalid credentials permits failover and a new pin.
Removed Deployments and revoked Provider Access also permit a new pin.
For a new session, a successful response creates the pin, including a Fallback.

In **Settings > Router**, set the session affinity lifetime in seconds
(`session_affinity_ttl_secs`, default 1800) and capacity
(`session_affinity_capacity`, default 100000). Both must be positive. The
lifetime runs from the last use. When the table is full, a new pin removes
the least recently used pin. The Routes page shows active pins per Route.
Disabling affinity, removing the Route, or reducing it to one Deployment with no Key Pool
clears its pins on settings reload.

The metric `llm_session_affinity_total{route,result}` counts successful
selections. Its results are `hit`, `new` and `repinned`. Ledger Details shows
**Session: pinned**, **new** or **re-pinned**, without a session key.

Limits:

- The table belongs to one Proxy process. Separate Proxy processes do not
  share pins. A restart loses all pins, including failover choices.
- A TTL expiry or LRU eviction loses the pin. The next request uses current
  weights and membership. The upstream KV cache can expire separately.
- The automatic prefix hash changes if the client rewrites early messages,
  instructions, image URLs or the model name. Use an explicit session key
  when the client changes these fields between turns.
- Affinity is a routing choice, not an isolation boundary for the upstream
  cache. Provider Access, model permission, Budgets and Rate Limits still
  apply to every request.

### List models

`GET /v1/models` and `GET /v1/models/{id}` need a Virtual Key.

```sh
curl $PROXY/v1/models -H "Authorization: Bearer $KEY"
```

The list has the catalog Models (`owned_by` is the Provider) and the Routes
(`owned_by` is `router`). If the key has an allowed-Models list, the Proxy shows
only the permitted names. A request with the header `anthropic-version` gets the
Anthropic format. `provider/model` names are not listed, but they work. An
unknown `{id}` gets HTTP 404. These requests do not count against Rate Limits and
do not make Ledger Entries.

Lists use Model names in ascending order. A Route replaces a catalog Model
with the same name. Provider Access also limits the names in the list.

For the Anthropic format, send `anthropic-version` and these query parameters:

| Parameter | Function |
| --- | --- |
| `limit` | Rows per page. Default 50. The server limits the value to 1 through 200. |
| `after_id` | Names after this ID. Use the `last_id` of the previous response. |
| `before_id` | Names before this ID. Use the `first_id` to go back. |

Do not send both cursor parameters. The response has `data`, `has_more`,
`first_id` and `last_id`. Empty pages have null IDs. With `before_id`,
`has_more` refers to earlier names. IDs keep the same order in both directions.

```sh
curl "$PROXY/v1/models?limit=50&after_id=gpt-4o" \
  -H "x-api-key: $KEY" -H "anthropic-version: 2023-06-01"
```

The OpenAI format has `object: "list"` and `data`, without cursor fields.
The server returns at most 1,000 permitted names and writes a warning to the
log if more names are available. OpenAI list requests ignore the paging
parameters. Use the Anthropic format to get names after this cap.
`GET /v1/models/{id}` can still get one permitted name beyond the cap.

### Examples with curl

Chat Completions:

```sh
curl $PROXY/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H "content-type: application/json" \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"Hello"}]}'
```

Chat Completions with streaming:

```sh
curl -N $PROXY/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H "content-type: application/json" \
  -d '{"model":"gpt-4o","stream":true,"messages":[{"role":"user","content":"Hello"}]}'
```

For streamed Chat Completions, the Proxy adds `stream_options.include_usage`
when the Provider's **Send stream_options.include_usage** setting is on.
If the Proxy added the option, it removes the usage-only chunk from the stream
that you receive. With the setting off, client options stay unchanged and
natural final usage chunks pass through.
For an Anthropic Deployment, Translation sends a Chat Completions usage chunk
only if your request set `stream_options.include_usage` to `true`.

Responses API:

```sh
curl $PROXY/v1/responses \
  -H "Authorization: Bearer $KEY" -H "content-type: application/json" \
  -d '{"model":"gpt-4o","input":"Hello"}'
```

Anthropic Messages (the Proxy adds `anthropic-version: 2023-06-01` if you send
no such header):

```sh
curl $PROXY/v1/messages \
  -H "x-api-key: $KEY" -H "anthropic-version: 2023-06-01" \
  -H "content-type: application/json" \
  -d '{"model":"claude-sonnet-5-5","max_tokens":256,"messages":[{"role":"user","content":"Hello"}]}'
```

Embeddings:

```sh
curl $PROXY/v1/embeddings \
  -H "Authorization: Bearer $KEY" -H "content-type: application/json" \
  -d '{"model":"text-embedding-3-small","input":"Hello"}'
```

Image generation:

```sh
curl $PROXY/v1/images/generations \
  -H "Authorization: Bearer $KEY" -H "content-type: application/json" \
  -d '{"model":"gpt-image-2","prompt":"A red bicycle"}'
```

The Model names in these examples are samples. Use the names of the Models and
Routes in your dashboard, or a `provider/model` name.

## HTTP endpoints

| Path                                    | Method   | Function                                                          |
| --------------------------------------- | -------- | ----------------------------------------------------------------- |
| `/`                                     | GET      | Redirects to `/admin`.                                            |
| `/healthz`                              | GET      | Returns `ok`. Needs no authentication.                            |
| `/metrics`                              | GET      | Prometheus metrics. Needs a token if one is set.                  |
| `/v1/models`, `/v1/models/{id}`         | GET      | Lists the permitted Models and Routes. Needs a Virtual Key.       |
| `/v1/*`                                 | any      | The Proxy. Needs a Virtual Key.                                   |
| `/admin`                                | GET      | Overview page.                                                    |
| `/admin/usage`                          | GET      | Paged usage groups with column sorting. `model` filters by name.  |
| `/admin/search/{kind}`                  | GET      | At most 20 choices for a picker. See "User, Team, Provider and Route fields". |
| `/admin/search/models`                  | GET      | Model picker results. See "Model fields".                         |
| `/admin/login`                          | GET, POST | Login page and login form (field `password`).                    |
| `/admin/logout`                         | POST     | Ends the session.                                                 |
| `/admin/users`                          | GET, POST | Users page. POST adds a User.                                    |
| `/admin/users/{id}`                     | POST     | Changes the Team, the Budget and the status of a User.            |
| `/admin/teams`                          | GET, POST | Teams page. POST adds a Team.                                    |
| `/admin/teams/{id}`                     | POST     | Changes the Team name, Budget and period.                        |
| `/admin/keys`                           | GET, POST | Keys page. POST creates a Virtual Key.                           |
| `/admin/keys/{id}`                      | POST     | Changes Provider Access, limits, Budget, period and expiry.       |
| `/admin/keys/{id}/limits`               | POST     | Changes the Rate Limits of a Virtual Key.                        |
| `/admin/keys/{id}/revoke`               | POST     | Revokes a Virtual Key.                                            |
| `/admin/models`                         | GET, POST | Models page. POST adds a Model.                                  |
| `/admin/models/{id}`                    | GET, POST | Page of one Model. POST changes the Provider and the aliases.    |
| `/admin/models/{id}/cost`               | POST     | Saves a new Cost Card version.                                    |
| `/admin/models/{id}/price`              | POST     | Saves a new Price Card version.                                   |
| `/admin/models/{id}/team-price`         | POST     | Sets or removes a Team override.                                  |
| `/setup` | GET, POST | Sets the first admin password and the mode; refuses later setup requests. |
| `/admin/providers` | GET, POST | Lists or creates Providers. |
| `/admin/settings/alerts` | POST | Adds or changes an encrypted webhook and shared thresholds. |
| `/admin/settings/alerts/{id}/test` | POST | Queues a test event for an enabled webhook. |
| `/admin/settings/alerts/{id}/delete` | POST | Deletes a webhook. |
| `/admin/providers/{name}` | POST | Changes the name, kind and base URL of a Provider. |
| `/admin/providers/{name}/delete` | POST | Deletes a Provider. |
| `/admin/providers/{name}/keys` | POST | Adds a Provider Key or an `env:NAME` reference. |
| `/admin/providers/{name}/keys/{index}/delete` | POST | Removes one key. |
| `/admin/providers/{name}/test` | POST | Tests the Provider. |
| `/admin/routes` | GET, POST | Lists or creates Routes. |
| `/admin/routes/{name}` | POST | Changes a Route. |
| `/admin/routes/{name}/delete` | POST | Deletes a Route. |
| `/admin/settings` | GET, POST | Edits server and router settings, the mode and the metrics token. |
| `/admin/settings/password` | POST | Changes the password and ends all sessions. |
| `/admin/settings/mode` | POST | Sets the mode. The field `mode` is `full` or `simple`. |
| `/admin/settings/markup` | POST | Saves the default Markup from the Models page. |
| `/admin/ledger`                         | GET      | Ledger page. See "Ledger" for the query parameters. |
| `/admin/reprice`                        | POST     | Prices the Unpriced Entries.                                      |
| `/admin/assets/htmx.min.js`             | GET      | The script file of the dashboard.                                 |

All `/admin` paths need a valid session, except the login paths, the logout
path and the script file.

## Dashboard

Open `http://127.0.0.1:8787/admin`. With no password, Setup asks you to set one
of at least 8 characters. Then log in. See "Dashboard password" in
[configuration.md](configuration.md).

A login stays valid for `server.session_days` days (default 7) and survives a
restart. Select **Log out** to end the session. All dashboard times are UTC.

### Pages and name search

List pages show 50 rows by default. The Ledger shows 100. Use **Next** and
**Previous** below a list. The links keep the current filters. You can save
the address to open the same page later. `limit` sets the page size; the
server limits it to 1 through 200. The page shows **Showing N**, not a total
that needs a separate count query.

Users, Teams, Keys, Models, Routes and Providers have **Name search**. Search
matches part of a name. On Keys, it matches the label or the User name.
The `q` parameter keeps the search text. For example:
`/admin/users?q=maria&limit=25`.

Users, Teams, Keys and Models use fixed row IDs for paging. Their `after` and
`before` parameters are cursors, not page numbers. The Ledger uses `before`
for older IDs and `after` for newer IDs. New Ledger Entries do not move the
older pages. Routes and Providers use `page=1`, `page=2` and so on. These are
small configuration lists; an edit can change their name order.

### User, Team, Provider and Route fields

Each field that names a User, Team, Provider, Route or models.dev entry is a
searchable list. You do not type the name:

1. Click the field, or move the focus to it. The list opens and shows the
   first 20 choices in name order. A note shows how many more choices a
   search finds.
2. Optional: type a part of the name to filter the list. Names that start
   with the text come first.
3. Click a choice, or use Down and Up and then press Enter.

Each choice shows more data:

| Choice     | Data                                                    |
| ---------- | ------------------------------------------------------- |
| User       | The Team and the number of active Virtual Keys. A disabled User has a **disabled** tag. |
| Team       | The number of Users.                                    |
| Provider   | The API format and the sign-in type. A Subscription Provider has a **subscription** tag. |
| Route      | The strategy and the number of Deployments.             |
| models.dev | The name, the API host or the context window.           |

The field accepts only a name from the list. If you type a name that is not
in the list, the form shows "Choose a ... from the list." Escape closes the
list. Tab leaves the field.

| Field                                    | Values  |
| ---------------------------------------- | ------- |
| Keys: **User**                           | one     |
| Keys: **Allowed Providers** (more than 30 Providers) | several |
| Ledger: **User** and **Provider** filters | one    |
| Users: **Team** (new and edit)           | one     |
| Models: **Provider** of a new Model      | one     |
| Model: **Provider**, Team override **Team** | one  |
| Model: **models.dev Provider override** and **models.dev Model override** | one |
| Routes: Deployment **Provider**          | one     |

The models.dev Model list shows only the models of the models.dev Provider
override. If that field is empty, the list shows all models, and a choice
also sets the models.dev Provider override.

Provider Access uses checkboxes, with a **subscription** tag for Subscription
Providers, when there are 30 or fewer Providers. With more Providers,
**Allowed Providers** shows each chosen Provider as a chip. Select the X
button on a chip to remove it.

Without JavaScript, a list of 30 or fewer Users, Teams or Providers is a
select. A larger list is a text input: enter the exact name, or
comma-separated names for Allowed Providers. A User or Team name that does
not match shows an error; it does not silently select another item.

The search endpoints need an Operator login. They return at most 20 results:

- `/admin/search/users?q=`
- `/admin/search/teams?q=`
- `/admin/search/providers?q=`
- `/admin/search/routes?q=`
- `/admin/search/models-dev-providers?q=`
- `/admin/search/models-dev-models?q=&provider=`

An empty `q` returns the first results in name order. The `X-Total-Count`
response header gives the number of all matches. The response is HTML
`<li role="option">` items for `Accept: text/html`. Other requests get a JSON
array of objects with `name`, `detail`, and for Users and Teams also `id`.
Model fields use the model picker. See "Model fields".

### Model fields

All model fields use a searchable list (a combobox). Type a part of a name.
After a short pause, the list shows at most 20 results. Each result shows its
source, and if known, the context window and the Cost Card input and output
rates. The sources are:

- **route**: a Route name.
- **model**: a Model name or alias from the Models page.
- **provider**: a model from the live model list of the Provider
  (`GET {base}/v1/models`, or `{base}/models` when that path is not found).
- **models.dev**: a model from the models.dev catalog, if that source is
  installed.

You can always use a name that is not in the list. Type the name, then
select **Use 'name'** (the last result), or press Enter in a multi-value
field. The server checks the name as before.

Keyboard: Down and Up move through the results. Home and End go to the first
and last result. Enter selects the result. Escape closes the list. Tab leaves
the field.

| Field                               | Values   | Results from                                  |
| ----------------------------------- | -------- | --------------------------------------------- |
| Routes: Deployment **Upstream model** | one    | The Provider of the row: its Models and its live list. |
| Routes: **Fallbacks**               | ordered  | Routes and Models.                            |
| Keys: **Allowed models**            | several  | Models, Routes and `provider/model`.          |
| Ledger and Usage: **Model** filter  | one      | Models and Routes.                            |
| Models: **Name** of a new Model     | one      | The chosen Provider: its live list.           |
| Models: **Aliases**                 | several  | Typed names only.                             |

A field for several values shows each value as a chip. Select the X button
on a chip to remove the value. Fallbacks are in order: the Proxy tries them
from first to last. Use the Up and Down buttons on a chip to move it. When you
change the Provider of a Deployment row, the list shows the models of the new
Provider. A new Deployment row from **Add a Deployment** has the same list.

Without JavaScript, each model field is a plain text input. In a field for
several values, enter comma-separated names. The server accepts both forms:
one field per chip and comma-separated text.

The live model list is fetched only for API-key Providers, with the first
Provider Key. Subscription Providers are never queried. The Proxy keeps a
list for 10 minutes. It waits at most 3 seconds for a Provider. If a Provider
does not answer or refuses the key, its live results are not shown and the
other results stay. The Proxy then waits 1 minute before it asks that
Provider again.

`GET /admin/search/models` needs an Operator login. Parameters:

| Parameter  | Meaning                                                                |
| ---------- | ---------------------------------------------------------------------- |
| `q`        | The search text. Exact names come first, then names that start with it. |
| `provider` | Only results for this Provider. Routes and aliases are not included.    |
| `src`      | Comma-separated sources, for example `route,model`. Empty means all.    |
| `limit`    | The number of results. The default is 20 and the maximum is 50.        |

The response is HTML `<li role="option">` items. With
`Accept: application/json`, it is a JSON array of objects with `id`,
`label`, `provider`, `source`, `context` and `price`.

Revoked keys stay collapsed by default and have a separate pager. A link to
another revoked-key page opens that section. Their
cursors are `revoked_after` and `revoked_before`. Team member counts link to
the paged Users list with `team=ID`. Team override counts link to the paged
Models list with the same filter. The Keys filter accepts `user=NAME` or a
valid User ID.

On a Model page, Card history shows 20 rows, newest first. Team overrides
show 20 rows with `overrides_after` and `overrides_before` cursors. These
two pagers are independent. Provider Key Pools and Route Deployments show
50 rows each. Their page parameters are `pool_page` and `deployments_page`.
The links also set `pool_provider` or `deployments_route` to identify the
configuration array. They use offset pages because these arrays are editable. A
Route save changes only the Deployments on the current page; it keeps the
Deployments on other pages.

### Providers, Routes and Settings

The header has **Providers**, **Routes** and **Settings**. These pages apply
changes without a restart. The settings are in
[configuration.md](configuration.md).

**Providers**:

1. To add a Provider, use **Add a Provider** at the top of the page. See
   [Add a Provider](#add-a-provider).
2. To add a Provider Key, enter the key in **New Provider Key or env:NAME** and
   select **Add key**. The Proxy encrypts the key. The page shows only its last
   4 characters. Enter `env:NAME` to store a reference to an environment
   variable instead. The page shows the reference with the tag **set** or
   **not set**. It never shows characters of the value. The Proxy skips a
   variable that is not set.
3. To remove a Provider Key, select **Delete key** in its row.
4. To check an API-key Provider, select **Test connection**. The result shows
   in the Provider section. It tells you whether the Provider accepted the
   first Provider Key.
5. To connect a subscription, select **Sign in** in the Provider section and
   follow the steps in the panel. Then paste the result and select
   **Connect**. The panel stays in the Provider section. **Disconnect** shows
   only when the Provider is connected.
6. To remove a Provider, select **Delete provider**. A Route or a Model that
   uses the Provider blocks this.

The dashboard asks for confirmation before it deletes or disconnects.

Only an Owner can add or remove `env:NAME` references. A normal example is
`env:OPENAI_API_KEY`. Names that start with `TOKOMOTIVE_`, `AWS_`, `SSH_` or
`CI_`, or end with `_PASSWORD` or `_PRIVATE_KEY`, are reserved. Common system
secrets such as `DATABASE_URL` and `GH_TOKEN` are also reserved. See the full
list in [Provider configuration](configuration.md#providers). These checks
apply at save and load time, including old database data and seed files.
Never use a service password or a Master Key as a Provider Key.

#### Add a Provider

The **Add a Provider** panel is at the top of **Providers**. When there are no
Providers, the panel is open. When there are Providers, select the
**Add a Provider** button to open it.

1. Find the Provider in the **Provider** field. Type a name, an alias or an
   API host. For example, `glm` finds Z.ai, `kimi` finds Moonshot Kimi and
   Kimi Code, and `ollama` finds **Local server**. Press Down or click the
   field to see the popular choices. Use Up, Down and Enter to select a
   result. The list shows at most 20 results.
2. Each result shows the name, the sign-in type (for example **API key**,
   **Subscription sign-in**, **Device code** or **Self-hosted**), the source
   and the API format. **Verified** entries come from the
   [roster](provider-roster.md). Entries **from models.dev** are not
   verified. **Custom** entries accept any compatible API.
3. The form shows only the fields for that Provider. The name is filled with
   a name that is not in use. Change it if necessary.
4. Read the summary line, for example
   `OpenAI format · Bearer key · api.deepseek.com`. It shows what the
   Provider uses.
5. Select **Add provider**, or **Add and sign in** for a sign-in type.

The fields for each type:

- **API key**: the base URL and the API key. A verified base URL is
  read-only. Select **Edit** to change it, for example for a proxy. **Where to
  get a key** opens the Provider documentation. **Advanced** has the API
  format, the key header and `stream_options.include_usage`.
- **Subscriptions** (ChatGPT subscription, ChatGPT via Codex, Claude Pro/Max,
  GitHub Copilot, Qwen Code, Kimi Code and Gemini CLI): only the name. GitHub
  Copilot and Kimi Code also have the API format. Read the risk note and its
  decision record before you continue. **Add and sign in** adds the Provider
  and shows its sign-in panel. A device-code sign-in shows the code at once.
- **AWS Bedrock (native, SigV4)**: the AWS region and the AWS credentials.
- **Google Vertex AI**: the project ID and the region. The user sign-in needs
  your OAuth client ID and secret. The service-account type needs the JSON
  key. Paste it, or choose the file.
- **Azure Entra ID**: the Azure resource URL, the tenant and the client ID.
- **Hugging Face OAuth**: the client ID or the CIMD address.
- **Local server**: the base URL, for example `http://localhost:11434/v1`
  (Ollama), `http://localhost:1234/v1` (LM Studio) or
  `http://localhost:8000/v1` (vLLM).
- **Custom OpenAI-compatible** and **Custom Anthropic-compatible**: all
  fields.

Select **Change provider** to go back to the search. The name that you typed
stays. After the Provider is added, its card opens and gets the focus. The card
shows the next steps. For an API-key Provider, select **Test connection**.
Then select **add a Model**. The Models page opens with the new Provider
selected.

To open the form directly, use `/admin/providers?add=ID` or
`/admin/providers?preset=ID`, for example `/admin/providers?add=deepseek`.
Without JavaScript, the page shows a list of all Providers in groups. Select
one and select **Continue**.

Scripts can still send the old form fields to `POST /admin/providers`:
`name`, `auth`, `kind`, `base_url`, `api_key`, `key_header`,
`include_usage` and the AWS and Vertex fields. The guided form also sends
`then` (`card` or `signin`) and, for OAuth types, `client_id`,
`client_secret`, `tenant` or `service_account`.
`GET /admin/search/provider-types?q=` returns the search results as HTML
options, or as JSON for `Accept: application/json`.

**Routes**:

1. To add a Route, enter a name, select a strategy and enter Fallbacks,
   comma-separated. Each Fallback must be the name of another Route or of a
   Model. The Proxy refuses an unknown name and shows the error next to the
   form.
2. Select **Add a Deployment** for each Deployment. Each row has a Provider, an
   upstream model and a weight. The weight shows only for the Weighted
   strategy. Select **Up** or **Down** to change the order, and **Remove** to
   remove a row.
3. Select **Add route** or **Save**. Select **Delete route** to remove it.

**Settings** shows the Proxy base URL for clients and the mode. It has the
server and router values, the metrics token and the password. To change the
mode, select **Switch to Simple Mode** or **Switch to Full Mode** and confirm.
**Save settings** does not change the mode. In Full Mode, Settings shows the
Default Markup. Change it on the **Models** page. To change the password, enter
the current password and a new password of at least 8 characters. All sessions
end. Back up `secret.key` with the database. See "Master Key and backups" in
[configuration.md](configuration.md#master-key-and-backups).

### Simple mode

Select **Simple Mode** on `/setup`, or select **Switch to Simple Mode** in
**Settings**, to use the Proxy as a router without prices or limits. The
default is Full Mode. The dashboard asks for confirmation before it changes the
mode. A change applies to new requests without a restart. In Simple Mode, the
header shows a **Simple Mode** tag that links to **Settings**.

- Virtual Keys, User and Team usage, model allowlists and server-side Provider
  Keys stay active. Routing, Fallbacks, Circuit Breakers, Translation and
  `/v1/models` work in both modes.
- Simple Mode skips Budget checks, Reservations and Rate Limits. These checks
  cannot cause HTTP 402 or 429. A Provider can still return these statuses.
- The dashboard shows requests, all seven Token Classes, latency, TTFT, error
  rate, attempts and retried requests per Model, Provider, User and Team.
  Attempts include key retries and Fallbacks. It hides money, cards, Markup,
  Budgets, Rate Limits and the unpriced and estimated flags.
- Add Models without cards, or use `provider/model` names. No price setup is
  required. Prometheus token and latency metrics stay active.
- The Ledger still stores all usage and request details. If a card exists, it
  also stores the amounts. Otherwise the entry stays unpriced. Full Mode can
  price these entries with **Price unpriced entries** after you add cards.
- Saved cards and limits stay in the database. Simple Mode does not add to the
  Rate Limit window. When you select **Full**, the saved checks apply again;
  Budget checks include the amounts recorded in Simple Mode.

The financial fields and procedures below apply to Full Mode.

### Overview

The page shows the totals for a time range.

- Select the range with the links **24 hours**, **7 days** and **30 days**. The
  address `/admin?range=7d` selects a range directly. Valid values are `24h`,
  `7d` and `30d`. The default is `24h`.
- The page refreshes itself every 15 seconds.
- A notice shows the number of Unpriced Entries and Estimated Entries. This
  number counts all entries, not only the entries in the range. Each count is a
  link to the Ledger with the matching filter.
- The cards show Requests (with errors and requests each hour), Tokens, Price
  (what Users owe), Cost (what you owe), Profit (with the margin) and Latency
  (with time to first token and output tokens each second).
- The charts show Money (Cost, Price and Profit) and Traffic (requests and
  tokens).
- The tables **By model**, **By provider**, **By user** and **By team** show requests, errors,
  tokens for each Token Class, Price, Cost, Profit, margin, average latency, average
  time to first token and output tokens each second.

Each table shows the top 10 groups by requests. **Show all** opens a paged
view, for example `/admin/usage?group=model&range=24h`. Valid groups are
`model`, `provider`, `user` and `team`. Select a column heading to sort, or
use **Sort by** and **Order** in the filter form. These controls also work
on a narrow screen and without JavaScript.
The `sort` parameter accepts `key`, `requests`, `errors`, `input`, `output`,
`latency`, `price_micros`, `cost_micros` and `profit`. `direction` is `asc`
or `desc`. The defaults are `requests` and `desc`. Names break equal ranks.
The view uses `page` and `limit`. Its offset can change as new usage changes
the ranks. Simple Mode hides money columns and does not sort by money.

The margin is Profit divided by Price. The page shows `-` when the Price is 0.

The table **By model** uses the name of the matched Model. A dated name such as
`gpt-4o-2024-08-06` counts as `gpt-4o`.

### Users and Budgets

The **Users** page lists the Users. Each row has these fields:

- **Team**: the Team of the User, or `(no team)`.
- **Budget (USD)**: the limit on the total Price for the User. Leave it empty
  for no Budget.
- **Budget period (UTC)**: none (lifetime), daily, weekly (Monday), or monthly.
  The default is none. The page shows spend / Budget and the next reset time.
- **disabled**: if selected, the Proxy rejects all Virtual Keys of the User with
  HTTP 401.
- **Spend this period** and **Used**: the Price so far in the current period and
  its share of the Budget.
- **Keys**: the number of Virtual Keys that are not revoked.
- **RPM** and **TPM**: requests and completed tokens per minute across all keys
  of the User. Leave a field blank for unlimited use. Zero blocks requests.

Tasks:

- To add a User, fill in **Add user** (a name is required) and select **Add
  user**.
- To change a Team, a Budget or a status, edit the row and select **Save**.
- To change Rate Limits, edit **RPM** or **TPM** and select **Save**.
- The page has no function to delete a User. Select **disabled** to stop a User.

When a User reaches the Budget, the Proxy rejects new requests with HTTP 402.
See "Budgets and Reservations".

### Teams

The **Teams** page lists the Teams and the number of Users in each Team. To add a
Team, enter a name and select **Add team**. A Team gives a group of Users the
same Price Card overrides. You set the overrides on the page of each Model.

### Keys

The **Keys** page lists the Virtual Keys. It shows the User, the first characters
of the key, the label, the allowed Models, the creation time and the last use.
Set **RPM** and **TPM** when you create a key, or edit them in its row and select
**Save**. Both the key limits and the User limits apply. Leave a field blank for
unlimited use. Zero blocks requests. The window is the last 60 seconds. TPM
counts all Token Classes after a request finishes.

### Rate Limits

The Proxy checks Rate Limits after authentication and the allowed-Models check,
before Budget reservation. Both the Virtual Key and User limits must permit
the request. User limits apply across all keys of that User. Model-list and
model-detail requests do not count against Rate Limits.

An exceeded limit returns HTTP 429 with type `rate_limit_error`. The
`Retry-After` header gives the wait in seconds. The Proxy does not call a
Provider or write a Ledger Entry for this rejection. RPM counts admitted
client requests, not upstream retry attempts. TPM counts completed upstream
tokens across all Token Classes, including cache and image tokens. Requests
already in progress can cause TPM to exceed the limit before the next check.
Rate Limit state is local to one process and resets when the process restarts.

### Create and revoke Virtual Keys

To create a Virtual Key:

1. Select the User from the list. Click the field to see the Users.
2. Enter a label (optional).
3. Enter the allowed Models as a comma list, for example
   `gpt-4o, claude-sonnet-5-5`. Leave the field empty to allow all Models.
4. Select **Create key**.
5. Copy the key from the page. The Proxy stores only a hash of the key. You
   cannot see the key again.

A Virtual Key has the form `sk-toko-` followed by 48 hexadecimal characters.

Rules for allowed Models:

- A key with an allowed-Models list can use a requested Model if the list has the
  requested name or the name of the matched Model. If the list has `gpt-4o`, the
  key can use `gpt-4o-2024-08-06`.
- A request without a Model name is not allowed for such a key.
- The Proxy rejects a request that is not allowed with HTTP 403.

To revoke a key, select **Revoke** in its row and confirm. The Proxy rejects the
key with HTTP 401 from then on. You cannot undo a revoke. Create a new key
instead.

### Models

The **Models** page lists each Model with its Provider, aliases, Cost Card
rates, the number of Context Tiers, the Price and the number of Team overrides.
All rates are dollars per million tokens. A Model with no Cost Card shows the
tag **Unpriced: no Cost Card** and a link to add one. The Price column shows a
short tag: for example **+20% default** (the Default Markup), **+25%** (a Markup
for this Model) or **Explicit rates**.

Tasks on the list page:

- **Default Markup**: enter the Markup in percent for Models that have no Price
  Card and select **Save Default Markup**. The change applies to new requests
  only. This is the only place where you can change the Default Markup. The
  Settings page shows the value and links here.
- **Add a Model**: enter the name, the Provider and the aliases
  (comma-separated). In Full Mode, you can also enter the Cost Card rates and the
   Context Tiers. Then select **Add Model**. Select the Provider from the list.
   If there are no Providers, the page shows a
  link to the Providers page instead of the form.

Rules for the Cost Card when you add a Model:

- If you leave all rates and the Context Tiers blank, the Proxy does not add a
  Cost Card. The Model stays Unpriced, and its requests are Unpriced Entries
  until you add a Cost Card.
- If you enter a rate, the Proxy adds a Cost Card. A blank rate is 0.
- If the Input and Output rates are both 0, the Proxy refuses the form. Select
  **Input and Output are $0 on purpose** to save a $0 card, for example for a
  free local model. The same rule applies to the Cost Card and to explicit Price
  rates on the Model page.
- A name or an alias that another Model already uses is refused with a message
  next to the form. Your input stays in the form.

Select a Model name to open its page. The breadcrumb **Models** goes back to
the list. The page has these parts:

- **Model**: change the Provider and the aliases.
- **Cost Card**: change the rates and Context Tiers. Select **Save Cost Card**.
- **Price Card**: select a mode and select **Save Price Card**. The text above
  the form shows what Users pay now.
- **Team Price overrides**: set or remove a Price Card for one Team.
- **Card history**: all Card Versions of this Model.

Each save of a card makes a new Card Version. The old Card Versions stay. Old
Ledger Entries keep the amounts that they had.

Price Card modes:

| Mode                    | Result                                                           |
| ----------------------- | ---------------------------------------------------------------- |
| Default Markup          | The Model has no Price Card. The Default Markup applies.         |
| Markup for this Model   | Price Card = Cost Card + the Markup that you enter.              |
| Explicit rates          | The rates in the fields are the Price Card.                      |

The **Explicit rates** fields start empty, or with the saved explicit Price
Card. They never contain the Cost Card rates, so a save cannot set the Price
equal to the Cost by accident. To start from the Cost Card, select **Copy Cost
Card rates**, then change the rates before you save. If the Price is equal to or
lower than the Cost, the success message tells you.

When you type in the fields of a mode, the dashboard selects that mode. The
Markup field shows the resulting Input and Output Price.

Team overrides:

1. Select the Team from the list.
2. Select **Markup** and enter a percentage, or select **Explicit rates** and
   enter the rates. The fields start empty.
3. Select **Save Team override**.

An override replaces the Model Price Card for the Users of that Team. To remove
an override, select **Remove** in its row and confirm. A new save for the same
Team makes a new Card Version.

Context Tier line format. Enter one tier on each line in the **Context Tiers**
field. Separate the values with spaces:

```
above_input_tokens input output cache_read cache_write_5m cache_write_1h [image_input image_output]
```

Example, with the rates in dollars per million tokens:

```
200000 6 22.5 0.6 7.5 12
```

Rules for tiers:

- The first value is a whole number of input tokens. The tier applies when the
  input is more than this number.
- The input tokens include cache tokens and image input tokens.
- Each line has 6 or 8 numbers. A tier replaces all base rates, so repeat each
  rate that the tier needs.
- A rate is a number of dollars, 0 or more, with up to 6 decimal places. A
  leading `$` is allowed. An empty rate field counts as 0.

The **Card history** table shows the version number (`#id`), the card (**Cost
Card** or **Price Card**), what it applies to (**Model** or **Team:** and the
Team name), the rates or the Markup, and the time it was added (UTC).

### Ledger

The **Ledger** page shows the newest Ledger Entries, 100 on each page. Select
**Next** to see older entries. Use the tabs to filter them:

| Tab           | Shows                                              |
| ------------- | -------------------------------------------------- |
| **All**       | All entries.                                       |
| **Unpriced**  | Unpriced Entries.                                  |
| **Estimated** | Estimated Entries.                                 |
| **Errors**    | Entries with an HTTP status of 400 or more.        |

Simple Mode has only **All** and **Errors**.

Use the filter bar to show only the entries of one User, one Provider, one
Model, or a range of days (UTC). The filters are query parameters, so you can
save the address:

| Parameter  | Value                                                    |
| ---------- | -------------------------------------------------------- |
| `filter`   | `unpriced`, `estimated` or `errors`.                     |
| `user`     | The User name, or a valid User ID.                       |
| `provider` | The name of the Provider.                                |
| `model`    | The upstream or the requested Model name.                |
| `from`     | The first day, `YYYY-MM-DD`.                             |
| `to`       | The last day, `YYYY-MM-DD`. The day is included.         |
| `before`   | Show only entries with a smaller number (the next page). |
| `after`    | Show the previous page of entries with larger IDs.       |
| `limit`    | Rows per page. Default 100. Maximum 200.                 |

The columns are the time, the User, the Model and Provider, the status, the
flags, the input and output tokens, the Price, the Cost and the latency.
Select **Details** in a row to see the entry number, the endpoint, the tokens
for each Token Class, the time to first token, the Provider Key and the
attempts.

- **Provider Key** shows the last 4 characters of the key, or its `env:NAME`
  reference.
- The flags are **stream**, **unpriced**, **estimated** and **client aborted**.

An Estimated Entry has token counts that are partly estimated. The Proxy makes an
Estimated Entry when a successful response has no usage, or when a stream ends
before the Provider sends the final usage.

To price Unpriced Entries:

1. Add a Model with a Cost Card for the name in the entry (see "Models"). An
   alias also works.
2. Open **Ledger** and select **Unpriced**. The page lists the Models that
   have no Cost Card.
3. Select **Price Unpriced Entries**.

The page shows how many entries the Proxy priced. The Proxy prices only the
Unpriced Entries whose Model now has a Cost Card. It uses the current cards. It
does not change any other Ledger Entry.

## How the Proxy calculates amounts

### Find the Model

The Proxy finds the Model in this order:

1. The name is the same as the name of a Model.
2. The name is the same as an alias of a Model.
3. The name starts with `<name>-` for a Model. The longest `<name>` wins.

For a response, the Proxy uses the Model name in the response body (or in the
stream). If the response has none, it uses the name in the request. If no Model
matches, the Ledger Entry is an Unpriced Entry. An error response with no usage
is an error entry, not an Unpriced Entry.

### Normalize the usage

Each Provider reports usage in a different way. The Proxy converts the usage to
Token Classes. No token is in two Token Classes.

| Provider and endpoint        | Input                                    | Output                           | Cache read                         | Cache write                                                                                  | Image                                                             |
| ---------------------------- | ---------------------------------------- | -------------------------------- | ---------------------------------- | -------------------------------------------------------------------------------------------- | ----------------------------------------------------------------- |
| OpenAI, Chat Completions     | `prompt_tokens` minus cached tokens      | `completion_tokens`              | `prompt_tokens_details.cached_tokens` | Not used                                                                                  | Not used                                                          |
| OpenAI, Responses            | `input_tokens` minus cached tokens       | `output_tokens`                  | `input_tokens_details.cached_tokens`  | Not used                                                                                  | Not used                                                          |
| OpenAI, Embeddings           | `prompt_tokens`                          | None                             | None                               | Not used                                                                                     | Not used                                                          |
| OpenAI, Images               | `input_tokens_details.text_tokens` minus cached tokens (all `input_tokens` if there are no details) | None | `input_tokens_details.cached_tokens` | Not used | Image input: `image_tokens`. Image output: `output_tokens`. |
| Anthropic, Messages          | `input_tokens`                           | `output_tokens`                  | `cache_read_input_tokens`          | 5 minutes: `cache_creation.ephemeral_5m_input_tokens`. 1 hour: `cache_creation.ephemeral_1h_input_tokens`. If there is no `cache_creation` object, all `cache_creation_input_tokens` count as 5 minutes. | Not used |

For a stream, the Proxy reads the usage from the events:

- Chat Completions: the last chunk with `usage`.
- Responses: the event `response.completed`, `response.incomplete` or
  `response.failed`.
- Images: the event with `usage`.
- Anthropic: `message_start` gives the first values and `message_delta` gives the
  final values. The Proxy merges them.

If the client disconnects during a stream, the Proxy continues to read the stream
to get the full usage. See [ADR 0001](adr/0001-drain-aborted-streams.md). The
Ledger Entry has the flag **client aborted**.

### Select the rates

The Cost Card of the Model gives the Cost. The Price comes from the first item
that applies:

1. The Team override for the Model (if the User has a Team).
2. The Price Card of the Model.
3. The Cost Card with the default Markup.

If the card has Context Tiers, the Proxy selects the tier with the highest limit
that the input is above. The input is the sum of input, cache read, cache write
and image input tokens. If no tier applies, the base rates apply.

A Markup of *m* percent multiplies each rate by (100 + *m*) / 100. It applies
to the base rates and to the rates of each tier.

### Compute the amount

The amount is the sum of the tokens of each Token Class multiplied by the rate of
that class, divided by 1,000,000. The Proxy rounds once for each request, to the
nearest micro-dollar. Half values round away from zero.

The Proxy writes the Cost, the Price and the ids of the cards to the Ledger Entry.
The entry does not change when you change a card. See
[ADR 0002](adr/0002-freeze-amounts-on-ledger-entries.md). Unpriced Entries are the
one exception.

The dashboard shows money in dollars with 4 decimal places. The database holds
integer micro-dollars (1 micro-dollar is 0.000001 dollar).

### Example: OpenAI

The Model `gpt-4o` has this Cost Card, in USD per million tokens: input 2.50,
output 10.00, cache read 1.25. The Provider reports `prompt_tokens` 1000,
`completion_tokens` 100 and `cached_tokens` 400.

| Token Class | Tokens | Cost rate | Cost (micro-dollars) |
| ----------- | ------ | --------- | -------------------- |
| Input       | 600    | 2.50      | 1,500                |
| Output      | 100    | 10.00     | 1,000                |
| Cache read  | 400    | 1.25      | 500                  |

- Cost: 1,500 + 1,000 + 500 = 3,000 micro-dollars ($0.003).
- Price with the default Markup of 20 percent: the rates become 3.00, 12.00 and
  1.50. The Price is 1,800 + 1,200 + 600 = 3,600 micro-dollars.
- Price for a Team with an override of 50 percent: the rates become 3.75, 15.00
  and 1.875. The Price is 2,250 + 1,500 + 750 = 4,500 micro-dollars.

Embeddings: `text-embedding-3-small` has an input rate of 0.02. A response with
`prompt_tokens` 1000 gives a Cost of 1000 x 0.02 = 20 micro-dollars.

Images: `gpt-image-2` has these rates: input 5.00, cache read 1.25, image input
8.00 and image output 30.00. A response reports `input_tokens` 300 (100 text
tokens and 200 image tokens) and `output_tokens` 1000. The Cost is
100 x 5 + 200 x 8 + 1000 x 30 = 500 + 1,600 + 30,000 = 32,100 micro-dollars.
With a Markup of 20 percent, the Price is 38,520 micro-dollars.

### Example: Anthropic

A Model has these rates, in USD per million tokens: input 3.00, output 15.00,
cache read 0.30, cache write 5 minutes 3.75, cache write 1 hour 6.00. The Price
Card is a Markup of 100 percent. The test `anthropic_stream_cache_tiers` in
`tests/proxy.rs` uses these values.

The Provider reports `input_tokens` 50, `output_tokens` 77,
`cache_read_input_tokens` 2000, and a `cache_creation` object with 100 tokens for
5 minutes and 200 tokens for 1 hour.

| Token Class          | Tokens | Cost rate | Cost (micro-dollars) |
| -------------------- | ------ | --------- | -------------------- |
| Input                | 50     | 3.00      | 150                  |
| Output               | 77     | 15.00     | 1,155                |
| Cache read           | 2000   | 0.30      | 600                  |
| Cache write 5 minutes | 100   | 3.75      | 375                  |
| Cache write 1 hour   | 200    | 6.00      | 1,200                |

- Cost: 150 + 1,155 + 600 + 375 + 1,200 = 3,480 micro-dollars.
- Price with a Markup of 100 percent: 6,960 micro-dollars.

Example of a Context Tier. The Model `claude-haiku-5-5` in `config.example.toml`
has base rates of input 0.10 and output 0.50. Above 100,000 input tokens, the
rates are input 0.50 and output 2.50. A request with 150,000 input tokens and 1,000
output tokens is above the limit:

- Cost: 150,000 x 0.50 + 1,000 x 2.50 = 75,000 + 2,500 = 77,500 micro-dollars.
- Price with the default Markup of 20 percent: 93,000 micro-dollars.

## Budgets and Reservations

A Budget limits the total Price for a User, Team or Virtual Key. Set it on the
**Users**, **Teams** or **Virtual Keys** page. Leave the amount blank for no limit.
A request must fit every applicable Budget, including the Team of the User.

Select **none** for a lifetime Budget (the default), **daily**, **weekly** or
**monthly**. All boundaries use UTC. A daily period starts at 00:00. A weekly
period starts on Monday at 00:00. A monthly period starts on the first day of
the month at 00:00. The page shows spend this period / Budget and the reset time.
The Proxy sums the frozen Price amounts from indexed Ledger Entries in the
current period. A reset does not change or delete Ledger Entries. Team spend
uses the Team recorded on each Entry, not the current Team of an old User.

Before the Proxy sends a request to the Provider, it makes a Reservation for the
worst-case Price of the request:

- Input tokens: the size of the request body in bytes, divided by 4 and rounded
  up. All of them count as input tokens.
- Output tokens: the first value found of `max_tokens`, `max_completion_tokens`
  and `max_output_tokens` in the request. If none is set, the value
  `server.reserve_output_tokens`. For image generation, these count as image
  output tokens. For embeddings, there are no output tokens.
- The rates are the Price Card rates that apply to the User and the Model. If the
  Model has no card, the Reservation is 0.

The Proxy rejects the request with HTTP 402 in these cases:

- The Price so far in the period is equal to or more than the Budget.
- The Price so far, plus the open Reservations for the subject and period, plus the new
  Reservation, is more than the Budget.

The Proxy releases the Reservation after it writes the Ledger Entry.

Example: the User has a Budget of $0.01 (10,000 micro-dollars). The Model is
`gpt-4o` with the default Markup of 20 percent (input 3.00, output 12.00). The
request body has 400 bytes and `max_tokens` is 100. The Reservation is
100 x 3.00 + 100 x 12.00 = 300 + 1,200 = 1,500 micro-dollars. If the User has
spent 9,000 micro-dollars, then 9,000 + 1,500 is more than 10,000. The Proxy
rejects the request.

The Reservation is an estimate. The real Price of a request can be more, so a
Budget can be exceeded by a small amount. Reservations are in memory. Run one
Proxy process for each database.

### Usage alerts

On **Settings**, add a webhook URL and select **Generic JSON**, **Discord** or
**Slack**. Select **Enabled** to send events. Set the comma-separated Budget
thresholds, for example `50,80,100` (the default). Thresholds must be from 1 to
100 percent. All webhooks use the same thresholds. Each User, Team and Virtual
Key sends each threshold once per period, after committed spend reaches it.
The database keeps this record across restarts. A large request can cross more
than one threshold. Lifetime Budgets do not reset their alerts.

The Proxy also sends `model_price_change` events for models.dev price alerts.
The internal `alerts::emit` API can send Provider health events. Generic JSON
has `kind`, `subject`, `message` and `details` fields. Discord uses `content`;
Slack uses `text`. Budget details include the threshold, period, start, reset
time, spend and Budget in micro-dollars.

The URL is encrypted with the Master Key, like a Provider Key. The page shows
only its host. Leave the new URL blank to keep it. Use **Send test**, then
reload Settings to see the last delivery status. Disabled webhooks do not send
test events. Keep the Master Key backup with the database backup.

Delivery runs in the background. It does not delay or fail a client request.
Each attempt has a 10-second timeout. Connection failures, HTTP 429 and HTTP
5xx get at most two retries. Redirects are not followed. Other HTTP failures
are not retried. Failed events are not replayed after the retries or a restart.
Use trusted endpoints: the Operator's URL can access the Proxy host's network.
Simple Mode hides Budgets and alerts and does not send money alerts.

### Virtual Key expiry

Set **Expires at (UTC)** when you add or edit a Virtual Key. Leave it blank for
no expiry. The date-time field always uses UTC, not browser local time.
At or after that time, both API formats return HTTP 401 with the message
`Virtual Key expired`. The Keys page shows the expiry and an `expired` badge.
Expiry also applies in Simple Mode. It does not stop a request already admitted.

## Subscription quota and account health

Open **Providers** to see the last quota snapshot for each observed account.
Each window shows the used percent, the remaining count when available, the
reset time, and the observation time. Point to a reset time to see local time.
Without JavaScript, the tooltip shows UTC. A missing percent means that the
upstream did not report it. A reset time in the past does not refresh the data.

The Proxy reads OpenAI `x-ratelimit-*`, Anthropic `anthropic-ratelimit-*`, Claude
unified subscription headers, and Codex `x-codex-*` response headers. The
**Providers** page also reads Codex plan usage and Copilot `quota_snapshots`
with existing sign-ins. Plan requests use a 60-second cache. They are optional.
Saved plan snapshots affect quota scheduling. Claude has no separate plan reader here. Its
windows need response headers. The Proxy does not scrape account pages or
request new scopes. Private usage endpoints can change or be unavailable.

An upstream 401 or 403 sets `credentials_invalid`. A 429 sets `rate_limited`.
Its end time comes from `retry-after` or a reported reset header. If neither
is available, the end time is unknown. A successful inference clears a rate
limit, but does not clear invalid credentials.
The last failure reason and time remain visible after success. Reasons are
fixed text, not upstream response bodies.

Select **Quota** on a Route or in a Provider's **Key Pool strategy** to prefer
the account with the most remaining quota. For each account, the lowest
remaining ratio across its current windows is used. Equal ratios use the
earliest reset, then the saved order. Unknown quota ranks at 50% remaining.
Expired windows no longer rank or block an account. A Route ranks each
Deployment by its best available account; its Key Pool strategy selects the
key within that Provider. Subscription accounts use separate Providers and
Deployments. Priority, Round robin and Weighted keep their selection rules.

All strategies skip accounts at the exhaustion threshold. Set
`quota_exhausted_percent` in **Settings > Router** (default 98). The Proxy skips
a rate-limited account until its known reset. If several windows block it,
the latest blocking reset applies. Without a reset, the Circuit Breaker
cooldown applies. When every account is skipped, OpenAI and Anthropic clients
receive HTTP 429 with `Retry-After` in seconds to the earliest account reset.
If no reset is known, the configured cooldown is used.

Invalid credentials disable that account until a successful token refresh,
a new sign-in, a key change, or **Re-enable now**. A separate usage sign-in
does not re-enable inference. **Re-enable now** clears the account's quota
snapshot and the Provider's transient circuit cooldowns. The next request
tests access; it can disable the account again. This action requires an
Operator login and a same-origin form submission. Provider cards and Routes
show the skip reason and reset time. Transient 5xx and connection failures
still use the Circuit Breaker.

Health transitions emit `provider.rate_limited`,
`provider.credentials_invalid`, and `provider.recovered` webhook events.
Each account and target state can emit at most once per 10 minutes, including
repeated transitions during that time.
The card shows **Disabled** when the subscription sign-in is disconnected.
This is the current sign-in status, not a new upstream observation.

**Overview** shows at most 10 observed accounts. Each Provider card shows at
most 50. Stored snapshots have one row per account and window, not a history.
Provider Keys use a stable opaque ID, also shown beside the masked Key in
the Key Pool. A subscription uses `signin`; the
separate ChatGPT usage login uses `usage-signin`. No credential is shown.
Data remains until a new observation or Provider deletion.

The `/metrics` endpoint exports these persisted gauges:

- `tokomotive_provider_quota_used_ratio{provider,key,window}`: used percent
  divided by 100. It is absent when the upstream does not report usage.
- `tokomotive_provider_quota_reset_seconds{provider,key,window}`: reset time
  as Unix seconds, not a countdown. It is absent when the reset is unknown.
- `tokomotive_provider_health{provider,key,state}`: 1 for the last observed
  state and 0 for the other states. An unobserved account has no gauge.

The `key` label is an opaque Key ID or a sign-in slot, never the credential.
These gauges survive a Proxy restart. The counter
`tokomotive_account_skips_total{reason}` counts skipped account candidates by
`rate_limited` or `credentials_invalid`. It resets at a Proxy restart.
Protect this endpoint with the metrics
token when it is accessible outside a trusted network.

## Subscription Providers

Read [ADR 0003](adr/0003-subscription-providers.md) before you connect an
account. Also read [ADR 0004](adr/0004-legacy-codex-oauth.md) for legacy Codex
OAuth. Claude OAuth impersonates Claude Code and can cause account restriction.
Codex model requests and plan usage use private routes that can stop without
notice and can break the service terms.

### Sign in

1. Log in to `/admin`. Open **Providers**.
2. In **Add a Provider**, find **ChatGPT subscription** (recommended),
   **ChatGPT via Codex (legacy)** or **Claude Pro/Max**. Select
   **Add and sign in**. The page sets the API format and the base URL. The
   sign-in panel opens below the form.
3. For Codex, select **Sign in with a device code**. Copy the large user code.
   Open the verification page in the new tab. Sign in and enter that code.
   Keep the Tokomotive panel open. It connects automatically after approval.
   The panel shows the time left. A code lasts at most 15 minutes. If device
   sign-in is not enabled for the account, use **Paste the redirect URL instead**.
   For ChatGPT token sharing or Claude, select **Sign in**, then open the
   sign-in page in the new tab. These flows do not offer device sign-in.
4. Complete consent. For ChatGPT, copy the final
   `http://localhost:1455/auth/callback?...` URL from the address bar. A browser
   connection error at that address is expected. Tokomotive does not run a
   callback listener on your computer. Do not copy an earlier sign-in URL.
5. For Claude, copy the final `code#state`. Return to the Tokomotive tab. Paste
   the URL or `code#state` in the field and select **Connect**. Complete this
   step within 10 minutes. Start again if the attempt expires or fails.
6. Check **Connected**, the account, the plan, the expiry and the last error.
   Account or plan metadata can show **not available** if the issuer omits it.
7. Open **Keys**. Enter this Provider name in **Provider Access** for each Virtual
   Key that needs it. Select any API-key Providers that the same key must use.
8. Add Models or Routes that use this Provider. Send the usual client requests.
   Chat Completions and Messages Translation works with subscription auth too.

For optional ChatGPT plan usage, open **Codex plan usage (optional)** and select
**Sign in with a device code for usage** on the same Provider. Complete the
device steps above. The redirect-URL fallback remains available: paste the final
`http://127.0.0.1:1455/auth/callback?...` or `http://localhost:1455/...` URL and
select **Connect**. This separate login is used only for plan usage. The panel
shows 5-hour and weekly percentages and reset times, or **not available**.
Values are cached for up to 60 seconds. Reload **Providers** to update them.

A Subscription Provider accepts only model requests: `/v1/chat/completions`,
`/v1/responses`, `/v1/messages`, `/v1/embeddings` and the image endpoints. The
Proxy does not send the subscription token to any other path. A request for
another path through a Subscription Provider gets HTTP 400. The Proxy also
rejects every request path that has a `.` or `..` segment, also when it is
percent-encoded, with HTTP 400.

A Codex Provider accepts only `POST /v1/responses`, `POST /v1/chat/completions`
and `POST /v1/messages`. All three use the private Codex Responses route.
Chat requests map to Responses. Messages requests chain through Chat. JSON
clients get the final reply collected from upstream SSE. Streaming clients get
text and function-argument deltas, completion usage and errors in their own
format. Cached input is recorded as cache read. Only function tools are
translated. Unsupported Chat fields are omitted, and the existing Messages
Translation limits still apply. Models, temperature and output-limit support
depend on the private backend. Use a native Responses request for options that
have no Chat mapping. This adapter does not change ordinary OpenAI Providers.

Device polling stops after expiry, refusal or Disconnect. It also stops if
the sign-in panel stops sending status requests for 15 seconds, plus at most
one 30-second issuer request. Reloading or closing the panel can cancel the
attempt. Do not share the user code. The secret device credential is never
shown. Only connect an attempt that you started yourself.

Select **Disconnect** to remove the model-request tokens. **Disconnect usage
login** removes only the optional Codex tokens. API-key tests do not apply to
Subscription Providers. To change a Provider's authentication type, create a
new Provider.

### Provider Access

Create or edit Provider Access on **Keys**. With 30 or fewer Providers, use
the checkboxes. With more Providers, select the Providers from the list in
**Allowed Providers**. Each Provider shows as a chip. Without JavaScript,
enter comma-separated Provider names. The server accepts both forms:

- No checks, or a blank field, means all API-key Providers, but no Subscription Providers.
- A list of names permits only those Providers. Include API-key Providers
  too if the key needs them.
- Model permissions and Provider Access both apply. A Route's Fallback does
  not bypass Provider Access. The `provider/model` prefix cannot bypass it.
- A disallowed Deployment is skipped. An allowed Fallback can still serve
  the request. If no allowed Deployment remains, the Proxy returns HTTP 403,
  `permission_error`, with `this API key may not use provider X`.
- `/v1/models` hides Models and Routes that the key can reach only through
  disallowed Providers. Update Provider Access after a Provider rename.

Tokens are encrypted with the Master Key and are never shown in HTML or logs.
The Proxy refreshes them before expiry and retries once after an upstream 401.
A failed refresh disconnects the Provider and counts as a Circuit Breaker
failure. Routes can use their Fallbacks. Sign in again to restore the Provider.
Only one Proxy process may use a database, because refresh coordination is in
memory. Keep the database and Master Key together in backups.

Subscription requests keep the real token usage in the Ledger. The Provider
column identifies the account route. Cost is zero. Price still follows the
normal Price Cards, Team overrides and Markup, even when the Operator pays
for a subscription. Set rates if you want to charge Users. Simple Mode has
no extra financial controls.

## Provider presets

Open **Providers** and find the preset in **Add a Provider**. The preset
fills the name, API format, base URL, key header and stream-usage setting.
All fields stay editable. With JavaScript off, select the preset and select
**Continue**. The documentation link and key help show which key and
endpoint to use. A preset does not add a Model or a Cost Card.

Presets include OpenAI, Anthropic, DeepSeek, Moonshot Kimi, Z.ai, OpenRouter,
xAI, Mistral, DeepInfra, Nebius, Cerebras, Alibaba DashScope, Azure v1 and the
Hugging Face router. Coding Plan and verified China variants have separate
presets. Messages presets also include MiniMax and Volcengine Ark. Addresses
that are not verified in the [roster](provider-roster.md) are not presets.

For **Local server**, enter the server URL. Leave the Provider Key empty if the
server does not need one. Requests are Unpriced until you add a Model with a
Cost Card. A preset does not verify live model or endpoint support.

- **Key header** selects `Authorization: Bearer`, `x-api-key`, or Azure
  `api-key`. The Proxy removes client authentication headers before it adds
  the Provider Key. The default is Bearer for OpenAI and `x-api-key` for
  Anthropic. Some Messages presets use Bearer instead.
- **Send stream_options.include_usage** is on for generic OpenAI. DeepSeek
  and OpenRouter presets turn it off because final chunks report usage
  without it. Off does not remove an option that the client sends. The Proxy
  reads final usage with either setting.
- A base URL can include the API version, such as `/v1` or Z.ai `/v4`.
  Messages presets can include the full `/messages` endpoint. Do not add an
  extra version to a preset URL. For Azure, replace `<resource>` before you
  save. Azure v1 needs no `api-version` parameter.

The shared OpenAI reader accepts cached tokens in
`prompt_tokens_details.cached_tokens`, `input_tokens_details.cached_tokens`,
top-level `usage.cached_tokens` (Together), or `prompt_cache_hit_tokens`
(DeepSeek), in that order for the applicable API format. It subtracts cached
input once. Reasoning tokens in `completion_tokens_details.reasoning_tokens`
or `output_tokens_details.reasoning_tokens` stay inside output, not in an
extra Token Class. The reader also accepts a usage report inside a stream
choice (Kimi). Reports are snapshots; the reader does not add repeated reports.

Ledger **Details** shows **Cost** next to **Provider-reported cost** in Full
Mode. The reported amount comes from OpenRouter `usage.cost` in USD or xAI
`usage.cost_in_usd_ticks` (10,000,000,000 ticks per USD). The Proxy rounds it
to integer micro-dollars and stores it separately. A missing amount shows
**Not reported**. This amount is for comparison only. It never changes Cost
Cards, Price, Profit, Budgets, Reservations or accounting metrics.

## Key Pools

A Provider can have more than one Provider Key. The set of keys is the Key Pool.
Add or remove keys on **Providers**. Saved keys show only their last 4 characters.
You can enter `env:NAME` to store an environment variable reference. See "Key Pool" in
[configuration.md](configuration.md).

- The Proxy starts each request with the next key in turn.
- After a 429, a 5xx or a connection error, the Proxy tries the next key. This
  happens before it sends data to the client.
- If all keys fail, the client receives the last response of the Provider. If the
  last attempt was a connection error, the client receives HTTP 502.
- Each retry adds 1 to `llm_upstream_retries_total`.
- The Ledger shows the key number and the number of attempts.

## Prometheus metrics

The route `/metrics` returns metrics in the Prometheus text format. If a metrics
token is set, the request needs `Authorization: Bearer <token>`. Without a valid
token the Proxy answers HTTP 401.

The Proxy updates accounting metrics when it writes a Ledger Entry. It updates
`llm_rate_limited_total`, `llm_circuit_opens_total` and
`llm_upstream_fallbacks_total` when the event happens. The label `model`
is the Model name from the response (for example `gpt-4o-2024-08-06`). If the
response has none, it is the name from the request. The label `user` is the name
of the User.

| Metric                                | Type      | Labels                                       | Meaning                                       |
| ------------------------------------- | --------- | -------------------------------------------- | --------------------------------------------- |
| `llm_requests_total`                  | counter   | `provider`, `model`, `user`, `status`        | Requests, by HTTP status.                     |
| `llm_tokens_total`                    | counter   | `provider`, `model`, `user`, `class`         | Tokens. `class` is `input`, `output`, `cache_read`, `cache_write_5m`, `cache_write_1h`, `image_input` or `image_output`. |
| `llm_cost_micros_total`               | counter   | `provider`, `model`, `user`                  | Cost in micro-dollars.                        |
| `llm_price_micros_total`              | counter   | `provider`, `model`, `user`                  | Price in micro-dollars.                       |
| `llm_unpriced_requests_total`         | counter   | `provider`, `model`, `user`                  | Requests that are Unpriced Entries.           |
| `llm_estimated_requests_total`        | counter   | `provider`, `model`, `user`                  | Requests that are Estimated Entries.          |
| `llm_request_duration_seconds`        | histogram | `provider`, `model`, `user`                  | Time from request to the end of the response. |
| `llm_time_to_first_token_seconds`     | histogram | `provider`, `model`, `user`                  | Time to the first content of a stream.        |
| `llm_upstream_retries_total`          | counter   | `provider`                                   | Retries with the next key of the Key Pool.    |
| `llm_rate_limited_total`              | counter   | `scope`                                      | Rejected requests. `scope` is `key` or `user`. |
| `llm_circuit_opens_total`             | counter   | `provider`, `model`                          | Times that a Circuit Breaker opened. `model` is the upstream model name. |
| `llm_upstream_fallbacks_total`        | counter   | `from`, `to`                                 | Moves to the next Deployment after a failed one. The labels are the Route names of both Deployments. |

The Proxy omits `llm_tokens_total` series for a class with 0 tokens. The
histograms use these buckets, in seconds: 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30,
60, 120, 300. Profit is `llm_price_micros_total` minus `llm_cost_micros_total`.

Metrics start at 0 when the Proxy starts. The dashboard and the database keep the
history.

Scrape configuration for Prometheus. Put the token in a file, so that it is not in
the configuration:

```yaml
scrape_configs:
  - job_name: tokomotive
    metrics_path: /metrics
    authorization:
      type: Bearer
      credentials_file: /etc/prometheus/tokomotive.token
    static_configs:
      - targets: ["127.0.0.1:8787"]
```

The file `tokomotive.token` holds the value of `TOKOMOTIVE_METRICS_TOKEN`.

## Error responses

### Proxy routes (`/v1/...`)

The Proxy answers its own errors with this JSON body:

```json
{"error": {"type": "authentication_error", "message": "invalid API key"}}
```

For `/v1/messages`, the body also has the top-level field `"type": "error"`.

| HTTP status | `type`                 | `message`                                                       | Cause                                                          |
| ----------- | ---------------------- | --------------------------------------------------------------- | -------------------------------------------------------------- |
| 401         | `authentication_error` | `missing API key`                                               | No `Authorization: Bearer` header and no `x-api-key` header.   |
| 401         | `authentication_error` | `invalid API key`                                               | The key is unknown or revoked, or the User is disabled.        |
| 403         | `permission_error`     | `this API key may not use this model`                           | The key has an allowed-Models list and the Model is not in it. |
| 402         | `budget_exceeded`      | `a Virtual Key, User or Team budget does not cover this request` | The Budget check failed. See "Budgets and Reservations".       |
| 400         | `invalid_request_error` | Details of the unsupported request.                          | Translation could not convert the request, or the Chat output limits conflict. |
| 404         | `not_found_error`      | `model not found`                                               | `GET /v1/models/{id}` names a Model or Route that the key cannot see. |
| 429         | `rate_limit_error`     | The key or User request or token limit was exceeded.           | A Rate Limit check failed. The response has `Retry-After`.     |
| 502         | `api_error`            | `no provider is configured for this request`                    | The configuration has no Provider.                             |
| 502         | `api_error`            | `upstream request failed`                                       | The last attempt to connect to the Provider failed.            |
| 502         | `api_error`            | `upstream response failed`                                      | The Proxy could not read the response of the Provider.         |
| 500         | `api_error`            | `internal error`                                                | A database error while the Proxy checks the key or the Budget. See the server log. |

The Proxy does not write a Ledger Entry for its own 400, 401, 402, 403, 429 and
500 errors. It writes
a Ledger Entry with status 502 for the two 502 errors that come from the
upstream.

An error from the Provider (for example 400, 401, 429 or 500) keeps its status.
For a native request it keeps its body. For Translation, the Proxy converts
the error body to the client's format. The Ledger Entry has that status. A
401 from the Provider usually means a wrong Provider Key.

### Other routes

| Path            | Status | Cause                                                          |
| --------------- | ------ | -------------------------------------------------------------- |
| `/metrics`      | 401    | A metrics token is set and the request has no valid token. The response has `WWW-Authenticate: Bearer`. |
| `/admin/...`    | 303    | No valid session. The redirect goes to `/admin/login`.         |
| `/admin/...`    | 303    | No admin password is set. The redirect goes to `/setup`. |
| `/setup` | 403 | Setup is complete. Log in to change the password. |
| Dashboard POST | 403 | A browser form came from a different origin. |
| `/admin/login`  | 401    | Wrong password.                                                |
| `/admin/...`    | 400    | A form field is not valid, for example a dollar amount.        |
| `/admin/models/{id}` | 404 | No such Model.                                            |

## Official OAuth sign-in

These sign-ins use the Provider's documented OAuth flow. They do not use a
ChatGPT or Claude subscription. Cost follows the Cost Card. Price follows the
Price Card. A Hugging Face or Entra Provider needs explicit Provider Access on
each Virtual Key. An OpenRouter key follows the normal API-key access rules.

Client IDs, client secrets and tokens are encrypted with the Master Key. The
client settings form does not show saved values again. In **OAuth client settings**,
enter the client ID each time you save. An empty client secret keeps the saved
secret. Saving client settings disconnects that login. Sign in again after saving.
Back up the Master Key with the database.

### Sign in with OpenRouter

1. Add an API-key Provider with API format **OpenAI** and base URL
   `https://openrouter.ai/api`. You do not need to enter a Provider Key.
2. Select **Sign in with OpenRouter** on that Provider.
3. Open the sign-in page. Sign in to OpenRouter and create the key.
4. Copy the authorization code that OpenRouter shows. Paste the code into the
   sign-in panel and select **Connect** within 10 minutes.

This is the [headless PKCE flow](https://openrouter.ai/docs/guides/overview/auth/oauth).
It has no public callback. Do not paste an API key into the sign-in panel.
The Proxy exchanges the code with its private PKCE verifier. It adds the issued
key to the encrypted Key Pool as a normal Provider Key. The dashboard shows only
the last four characters. To remove it, select **Delete key**. There is no token
refresh for this flow.

### Sign in with Hugging Face

1. [Register an OAuth app](https://huggingface.co/docs/hub/oauth#creating-an-oauth-app).
   There is no shared public client ID. A public app needs no client secret.
   For the paste fallback, register `http://localhost/auth/callback` as a redirect
   URI. Allow the `inference-api` scope.
2. Add a Provider with authentication **Hugging Face OAuth**. Its base URL is
   `https://router.huggingface.co`.
3. Open **OAuth client settings** on the Provider. Enter your client ID. If the
   app has a secret, enter it. Select **Save OAuth client**.
4. Select **Sign in with a device code**. Open the verification page and enter
   the displayed user code. Keep the panel open until it connects.

If device sign-in is not available, select **Paste the redirect URL instead**.
Sign in, then copy the full local redirect address from the browser address bar.
The local page need not load. Paste the address into the panel and select
**Connect**. The Proxy uses PKCE and checks the state of the attempt.

You can also use [Client ID Metadata Documents (CIMD)](https://huggingface.co/docs/hub/oauth#automated-oauth-app-creation).
Enter `https://YOUR-PUBLIC-PROXY/.well-known/oauth-cimd` as the client ID on a
Provider with no saved client secret. The Proxy serves that public metadata
document, with its client ID, name, loopback redirect URIs and
`token_endpoint_auth_method: none`. Use the same public URL for all CIMD clients
on this Proxy. The URL must be reachable by Hugging Face over HTTPS. Select the
paste fallback for this option. Device-code PKCE with CIMD was not verified and
is not enabled for this flow.

The Proxy sends the bearer only to model paths on
`https://router.huggingface.co/v1`. It refreshes the token when necessary.
This is paid inference, not a zero-Cost subscription.

### Sign in with Azure Entra ID

1. Register your own public OAuth client in the tenant of your Azure resource.
   Enable public-client device sign-in. Give the account the Azure role that
   permits inference on that resource.
2. Add a Provider with authentication **Azure Entra ID**. Set its base URL to
   `https://RESOURCE.openai.azure.com/openai`, without `/v1`. Use your resource
   name, not the example name.
3. Open **OAuth client settings**. Enter the tenant ID or domain and your client
   ID. A public client needs no client secret.
4. Leave Scope empty to use
   `https://cognitiveservices.azure.com/.default offline_access`, or enter the
   scope that your resource needs. Select **Save OAuth client**.
5. Select **Sign in with a device code**. Open the Microsoft verification page,
   sign in and enter the displayed user code. Keep the panel open until it connects.

Microsoft documents the [device grant and offline access](https://learn.microsoft.com/en-us/entra/identity-platform/v2-oauth2-device-code)
and the [Cognitive Services scope for the Azure OpenAI v1 endpoint](https://learn.microsoft.com/en-us/azure/developer/ai/how-to/switching-endpoints).
Some Foundry documentation uses `https://ai.azure.com/.default` instead. Set the
scope to match your resource. Include `offline_access` to obtain a refresh token.
For the paste fallback, register `http://localhost/auth/callback` on your client.

The Proxy sends the token as `Authorization: Bearer`, not as an `api-key`
header. Requests use the configured Azure resource's `/openai/v1` model paths.
They have normal Cost and Price.

### Sign in with Google for Vertex

1. Create your own Google **Desktop app** OAuth client. Enable the Google Cloud
   services that your account needs. Configure the consent screen for that client.
2. Add a Provider with authentication **Google Vertex AI**. Set the project ID
   and region. Use `global` for the global endpoint.
3. Open **OAuth client settings**. Enter the client ID and secret. Select
   **Save OAuth client**.
4. Select **Sign in**. Open the Google page and allow the
   `https://www.googleapis.com/auth/cloud-platform` scope.
5. Copy the full address that starts with `http://localhost/auth/callback` from
   the browser address bar, even if that local page does not load. Paste the
   address into the panel and select **Connect** within 10 minutes.

This uses [Google's installed-app flow](https://developers.google.com/identity/protocols/oauth2/native-app)
with PKCE and offline access. It does not use Google's discontinued OOB flow.
The Proxy stores and refreshes the token. It can route Gemini and Anthropic
requests on Vertex. For an existing sign-in, save a project ID before the first
request. Each Virtual Key needs explicit Provider Access. Cost and Price follow
the cards. This is not a subscription.

Instead of user sign-in, open **Service-account JSON key** and paste the key
file. Select **Save service-account key**. This replaces user sign-in. The key
is encrypted and is not shown again. The Proxy signs an RS256 JWT and exchanges
it for a cloud-platform access token. It caches the token until 5 minutes before
expiry. Saving OAuth client settings removes the service-account key.

The Provider accepts canonical Chat Completions and Messages POST paths. Use
`claude-*` model names for Anthropic; other names select Gemini. A Deployment
can use `anthropic/MODEL` or `google/MODEL` to select a publisher explicitly.
These prefixes are not sent as part of the Vertex model ID. Use model IDs that
are available in the selected project and region. Live account support is not
verified; the tests use mock endpoints.

## Unofficial subscription sign-in

These flows use public client IDs and private Provider routes. They can stop
without notice. Third-party use can break the service terms and can cause
account restriction. The owner accepted this risk. Use an API-key Provider
instead if these terms do not permit your use.

Each Provider needs explicit Provider Access on each Virtual Key. Cost is 0;
Price uses the Rate Cards. Tokens stay encrypted in SQLite. Do not share the
device code. Keep the sign-in panel open until it connects. These three flows
have no redirect-paste fallback. Disconnect removes the login, not the Provider.

### Sign in with GitHub Copilot

1. Add a Provider with authentication **GitHub Copilot (unofficial)**.
2. For GitHub Enterprise, open **GitHub Enterprise (optional)**. Enter the
   deployment domain, such as `company.ghe.com`, and save it before sign-in.
   Leave it empty for github.com. Saving disconnects an existing login.
3. Select **Sign in with a device code**. Open the verification page, sign in
   to GitHub, and enter the displayed code.
4. Grant access. The Proxy checks Copilot access and reads the account's API
   endpoint. A rejected entitlement or untrusted endpoint prevents connection.
5. Use the OpenAI API format for Chat Completions and Responses models. Select
   Anthropic for Claude Messages models. Add Models and grant Provider Access.

The OAuth bearer is used directly, as in the pinned opencode source. No
short-lived Copilot token exchange is needed. A rejected token needs a new
sign-in. Tokomotive sends its own identity, not VS Code editor headers. Live
model discovery and Copilot Auto session tokens are not supported. See
[ADR 0006](adr/0006-github-copilot.md).

### Sign in with Qwen Code

1. Add a Provider with authentication **Qwen Code (unofficial)**.
2. Select **Sign in with a device code**. Open the verification page, sign in
   to Qwen, and enter the code.
3. Grant the displayed scope. Wait for the panel to connect.
4. Add Models and grant Provider Access. Use the OpenAI API format.

The Proxy uses PKCE and rotating refresh tokens. It reads `resource_url` from
the token response and accepts only the trusted HTTPS domains in
[ADR 0007](adr/0007-qwen-code.md). When no resource is supplied, the base is
`https://dashscope.aliyuncs.com/compatible-mode/v1`. Only Chat Completions model
requests can carry this bearer. Existing Messages Translation can use Chat.
Tokomotive sends the OAuth AuthType and its own DashScope UserAgent header.

### Sign in with Kimi Code

1. Add a Provider with authentication **Kimi Code (unofficial)**.
2. Select **Sign in with a device code**. Open the verification page, sign in
   to Kimi, and enter the code.
3. Wait for the panel to connect. Add Models and grant Provider Access.
4. Keep the default OpenAI format for OpenAI-like or Responses models. Select
   Anthropic for models that require Messages. The Proxy adds `?beta=true`
   for that endpoint. Protocol support is per model.

The fixed base is `https://api.kimi.com/coding/v1`. The global `.ai` region is
not selected by this flow. Tokomotive sends `X-Msh-*` identity headers with
platform `tokomotive`. Its random device ID is encrypted and stays the same
when you reconnect this Provider. Refresh tokens rotate. Live acceptance of
the Tokomotive platform is not yet verified. See [ADR 0008](adr/0008-kimi-code.md).
