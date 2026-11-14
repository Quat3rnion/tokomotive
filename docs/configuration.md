# Configuration reference

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

- All rates are USD per million tokens.
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
| `upstream_timeout_secs`  | integer | `600`                          | Time limit in seconds for one request to a Provider. The limit includes the time to read the response body. The connect time limit is 15 seconds and is fixed. |
| `metrics_token`          | string | none                            | The bearer token for `/metrics`, in the file. It seeds once. Later changes belong in **Settings**. |
| `metrics_token_env`      | string | `"TOKOMOTIVE_METRICS_TOKEN"` | Name of the variable that holds the bearer token. The variable has priority over `metrics_token`.                                                       |
| `reserve_output_tokens`  | integer | `4096` | Fallback output tokens for a Reservation when the request sets no maximum and the Model has no mapped output limit. Must be non-negative. Does not cap a models.dev output limit. |
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

If no password hash exists, dashboard pages redirect to `/setup`. Setup accepts
a password of at least 8 characters and refuses all later setup requests.
The Proxy stores passwords as salted Argon2 hashes. Use **Settings** to change the
password. Enter the current password and a new password of at least 8 characters.
This ends all sessions. Existing sessions otherwise survive restarts.

`TOKOMOTIVE_ADMIN_PASSWORD`, when non-empty, replaces the stored hash at
each start. Remove that variable after recovery if you want UI password changes
to remain after a restart. Legacy `admin_password` or `admin_password_env` values
seed a hash only when none exists. The default environment variable remains the
recovery override.

Complete Setup through a trusted local connection before you expose the service.
Use TLS at the reverse proxy for remote access. Preserve the public Host and
Origin headers. All dashboard POST routes refuse cross-origin browser forms.

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

For each request, the Proxy starts at the next key in turn. After a 429, a 5xx or
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
rate in USD per million tokens. Each key is optional and its default is `0`.

| Key              | Token Class         |
| ---------------- | ------------------- |
| `input`          | Input (not cached)  |
| `output`         | Output              |
| `cache_read`     | Cache read          |
| `cache_write_5m` | Cache write, 5 minutes |
| `cache_write_1h` | Cache write, 1 hour |
| `image_input`    | Image input         |
| `image_output`   | Image output        |

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
| `budget_monthly` | boolean          | `false` | If `true`, the Budget starts again on the first day of each month (UTC). If `false`, it is for all time. |
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
| Strategy     | `priority`, `round_robin` or `weighted`. See below.                                                  |
| Deployments  | Rows that you add and remove. Each row has a Provider (select), an upstream Model and a weight. The weight is a positive integer. Only the `weighted` strategy uses it. |
| Fallbacks    | A comma list of Route names or catalog Model names.                                                  |

The strategies:

| Strategy      | Order of the Deployments                                                                          |
| ------------- | ------------------------------------------------------------------------------------------------- |
| `priority`    | The order of the rows.                                                                            |
| `round_robin` | The order of the rows, but the first choice moves one row for each request.                       |
| `weighted`    | One Deployment is the first choice, selected at random by weight. The others follow in descending weight order. |

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

The Proxy keeps one Circuit Breaker for each pair of Provider and upstream model.
A connection error, a 429, a 5xx or a subscription credential failure counts as
a failure. Any other response resets
the count. When the count reaches `failure_threshold`, the Circuit Breaker opens.
After `cooldown_secs`, one probe request tests the Deployment. A good result
closes the Circuit Breaker. A failed probe opens it again. If every candidate is
open, the Proxy tries them anyway. The Proxy keeps this state in memory, so it
resets at a restart.

All Provider, Route and router changes apply without a restart. The TOML loader
and the dashboard share the same validation.

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
