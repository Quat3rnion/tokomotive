# Configuration reference

This document lists every setting of llm-accounting. The terms are defined in
[GLOSSARY.md](../GLOSSARY.md).

The database is the source of truth. Start the Proxy without a file, open
`/admin`, and set the first password on `/setup`. Use **Providers**, **Routes**
and **Settings** for live changes. See [usage.md](usage.md) for the Operator guide.

## Process options and optional seed file

The Proxy reads a seed file only when you give a path:

1. The `--config PATH` flag (short form `-c PATH`).
2. The environment variable `LLM_ACCOUNTING_CONFIG`.
There is no implicit `config.toml`. With no path, the Proxy uses defaults and
the existing database. `--listen ADDRESS` overrides `LLM_ACCOUNTING_LISTEN`.
`--database SQLITE_URL` overrides `LLM_ACCOUNTING_DATABASE`. Their defaults are
`127.0.0.1:8787` and `sqlite://llm-accounting.db`. Parent directories are created.
These two options cannot be changed in the UI. Legacy `server.listen` and
`server.database` seed-file fields are accepted but the binary ignores them.

The flag `--help` (short form `-h`) prints the usage text. The Proxy stops with an
error for any other flag.

The Proxy stops at start in these cases:

- The file cannot be read or is not valid TOML.
- The file has an unknown section at the top level.
- The file has an unknown key in `[server]`, in a `[[providers]]` item, in a
  `[[models]]` item, in a `[[teams]]` item or in a `[[users]]` item.
- A required key is missing.
- A Model uses a Provider that is not in the file.
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
| `LLM_ACCOUNTING_CONFIG`         | Path of the configuration file. The `--config` flag has priority.                                    |
| `LLM_ACCOUNTING_LISTEN`         | Listen address, unless `--listen` is set. |
| `LLM_ACCOUNTING_DATABASE`       | SQLite URL, unless `--database` is set. |
| `LLM_ACCOUNTING_ADMIN_PASSWORD` | Seeds or overrides the password hash at each start. |
| `LLM_ACCOUNTING_SECRET_KEY`     | Master Key: 32 bytes encoded as 64 hex characters or base64. |
| `LLM_ACCOUNTING_METRICS_TOKEN`  | Initial metrics token, used only before runtime settings exist. |
| `RUST_LOG`                      | Log filter, for example `info` or `llm_accounting=debug`. The default is `info`.                     |
| Names in `*_env` keys           | Each `api_key_env`, `api_keys_env`, `admin_password_env` and `metrics_token_env` key names a variable. |

An empty listen address or database URL is not valid.

A `*_env` key holds the name of a variable, not the secret. The Proxy ignores a
variable that is not set or is empty.

## `[server]`

All keys are optional. The whole section is optional. Except for the process
options and the password override, values seed once. Edit them in **Settings**.

| Key                      | Type   | Default                         | Meaning                                                                                                                                                  |
| ------------------------ | ------ | ------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `listen`                 | string | `"127.0.0.1:8787"` | Legacy field; use `--listen` or the environment. |
| `database`               | string | `"sqlite://llm-accounting.db"` | Legacy field; use `--database` or the environment. |
| `admin_password`         | string | none                            | The dashboard password, in the file. Do not use it in a repository that you commit.                                                                      |
| `admin_password_env`     | string | `"LLM_ACCOUNTING_ADMIN_PASSWORD"` | Name of the variable that holds the dashboard password. The variable has priority over `admin_password`.                                               |
| `default_markup_percent` | float  | `0`                             | Markup for Models that have no Price Card. The file `config.example.toml` sets it to `20`.                                                               |
| `upstream_timeout_secs`  | integer | `600`                          | Time limit in seconds for one request to a Provider. The limit includes the time to read the response body. The connect time limit is 15 seconds and is fixed. |
| `metrics_token`          | string | none                            | The bearer token for `/metrics`, in the file.                                                                                                            |
| `metrics_token_env`      | string | `"LLM_ACCOUNTING_METRICS_TOKEN"` | Name of the variable that holds the bearer token. The variable has priority over `metrics_token`.                                                       |
| `reserve_output_tokens`  | integer | `4096` | Output tokens that a Reservation assumes when the request sets no maximum. Must be non-negative. |
| `session_days`           | integer | `7` | Duration of new dashboard sessions, from 1 to 3650 days. |
| `mode`                   | string | `"full"` | `full` enables pricing and usage limits. `simple` enables routing and token metrics without Budget checks, Reservations or Rate Limits. |

### Simple mode

Select the mode on `/setup` or in **Settings**. The default is `full` for
compatibility. To seed Simple Mode on the first start:

```toml
[server]
mode = "simple"
```

Later file changes do not replace the saved mode. Use **Settings** for live
changes. Simple Mode hides financial data and controls, but keeps Virtual Keys,
model allowlists, routing and token metrics. Saved cards and limits stay in the
database. Existing cards still supply amounts for Ledger Entries. Entries with
no card can be priced after you select Full Mode. See "Simple mode" in
[usage.md](usage.md#simple-mode).

### Dashboard password

If no password hash exists, dashboard pages redirect to `/setup`. Setup accepts
a password of at least 8 characters and refuses all later setup requests.
Passwords are stored as salted Argon2 hashes. Use **Settings** to change the
password. Enter the current password and a new password of at least 8 characters.
This ends all sessions. Existing sessions otherwise survive restarts.

`LLM_ACCOUNTING_ADMIN_PASSWORD`, when non-empty, replaces the stored hash at
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

## Master Key and backups

Provider Keys and the metrics token use AES-256-GCM. Each saved value has a new
random 96-bit nonce. SQLite stores base64 of `nonce || ciphertext` (including the
authentication tag). The Master Key is not stored in SQLite.

Set `LLM_ACCOUNTING_SECRET_KEY` to 32 bytes in hex or base64. If it is absent,
the Proxy creates `secret.key` next to the database with Unix mode `0600`.
Back up this file with the database. If you use the environment variable, back
up that value instead. Protect both backups. A database backup without its
Master Key cannot recover the stored secrets. A wrong or lost key causes a clear
startup error when the Proxy cannot decrypt a value. Do not replace the key to
repair that error. Restore the original key.

Keep `secret.key` out of version control. It is not covered by this repository's
current ignore rules. Add the line `secret.key` to your local ignore rules before
you stage a whole working directory.

## `[[providers]]`

Each item defines one Provider. The file can have many items.

Use **Providers** to add, edit or delete Providers and to add or remove keys.
Names use letters, digits, dot, underscore or hyphen. Base URLs must use HTTP or
HTTPS and must not contain credentials, a query or a fragment. You cannot delete
a Provider used by a Route or Model. Update those references first. **Test**
calls `GET {base_url}/v1/models` with the first key and reports its status.

Saved keys show only their last 4 characters. A value `env:NAME` stores a
reference, not an encrypted key. The Proxy resolves the variable at start and
on configuration reload. Set the variable in the service environment; a UI edit
cannot change the environment of the running process.

| Key            | Type             | Default | Meaning                                                                                                          |
| -------------- | ---------------- | ------- | ---------------------------------------------------------------------------------------------------------------- |
| `name`         | string           | none    | Required. The name that `models.provider` uses.                                                                  |
| `kind`         | string           | none    | Required. `"openai"` or `"anthropic"`. The kind selects the usage format and the authentication headers.        |
| `base_url`     | string           | none    | Required. The URL of the Provider API, without a path such as `/v1`. The Proxy adds the path and query of the client request. |
| `api_key`      | string           | none    | One Provider Key, in the file. Do not use it in a repository that you commit.                                    |
| `api_key_env`  | string           | none    | Name of the variable that holds one Provider Key. The variable has priority over `api_key`.                      |
| `api_keys`     | array of strings | `[]`    | More Provider Keys, in the file.                                                                                 |
| `api_keys_env` | array of strings | `[]`    | Names of variables. Each variable holds one Provider Key.                                                        |

### Key Pool

The Key Pool of a Provider holds these keys, in this order:

1. The single key (`api_key_env`, else `api_key`).
2. The keys in `api_keys`.
3. The keys from the variables in `api_keys_env`.

The Proxy skips empty keys and removes duplicate keys.

For each request, the Proxy starts at the next key in turn. After a 429, a 5xx or
a connection error, it tries the next key. This happens before the Proxy sends
any data to the client. When the last key also fails, the Proxy returns that
result. The Ledger Entry records the number of the key and the number of
attempts.

If the Key Pool is empty, the Proxy writes a warning at start. The Proxy then
sends requests to the Provider with an empty key.

### Authentication headers

| Kind        | Header that the Proxy sends to the Provider                                                              |
| ----------- | -------------------------------------------------------------------------------------------------------- |
| `openai`    | `Authorization: Bearer <Provider Key>`                                                                   |
| `anthropic` | `x-api-key: <Provider Key>`. The Proxy adds `anthropic-version: 2023-06-01` if the client sent no such header. |

The Proxy does not copy these client headers to the Provider: `Authorization`,
`x-api-key`, `Host`, `Content-Length`, `Accept-Encoding` and the hop-by-hop
headers. It copies all other headers.

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

Rate Limits must be non-negative integers. Omit a limit for unlimited use. Zero
blocks requests. Both the User and Virtual Key limits apply. TPM counts all
Token Classes after a request finishes. The window is the last 60 seconds.
Seeding does not change the limits of an existing User or Virtual Key.

A key in `keys` is stored with the label `config`. The key is in plain text in the
file. Do not use this setting for production Users. Create Virtual Keys on the
**Keys** page of the dashboard instead.

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
to edit seeded items. Router changes preserve Circuit Breaker and key rotation
state. Timeout changes build a new HTTP client for new upstream requests.
Requests already in progress can finish with the settings that they captured.

## Routes and router settings

Use **Routes** to create, edit or delete a Route. Choose `priority`, `round_robin`
or `weighted`. Add deployment rows with Provider, upstream Model and positive
weight. Remove rows with **Remove row**. Enter Fallback names as a comma list.
Unknown Fallback names are skipped at request time. Fallbacks do not expand
recursively. Without JavaScript, edit the deployment JSON array in the form.

The optional file uses `[[routes]]` with `name`, `strategy`, `deployments` and
`fallbacks`, and `[router]` with these settings:

| Setting | Default | Meaning |
| --- | --- | --- |
| `failure_threshold` | `3` | Positive number of failures before a circuit opens. |
| `cooldown_secs` | `30` | Seconds before a recovery probe. |
| `max_attempts` | `8` | Positive limit on upstream attempts for one request. |

Change these values on **Settings**. All Provider, Route and router changes apply
without a restart. The TOML loader and dashboard share validation.

## Docker

The `Dockerfile` makes an image that holds the binary and a copy of
`config.example.toml`.

| Item                   | Value                                                                                  |
| ---------------------- | -------------------------------------------------------------------------------------- |
| Base of the final image | `debian:bookworm-slim`                                                                |
| User                   | `llma` (uid 10001)                                                                     |
| Working directory      | `/data`                                                                                |
| Volume                 | `/data`                                                                                |
| Port                   | `8787`                                                                                 |
| Configuration file     | `/etc/llm-accounting/config.toml`                                                      |
| `LLM_ACCOUNTING_CONFIG` | Not set; the example seed file is optional. |
| `LLM_ACCOUNTING_LISTEN` | `0.0.0.0:8787`                                                                        |
| `LLM_ACCOUNTING_DATABASE` | `sqlite:///data/llm-accounting.db`                                                  |

Build and run:

```sh
docker build -t llm-accounting .
docker run -p 8787:8787 -v llma-data:/data \
  llm-accounting
```

Rules for Docker:

- Open `/setup` on the first start. To seed instead, mount your own file and add
  `--config /etc/llm-accounting/config.toml` after the image name. The image does
  not use the example file unless you ask for it.
- Keep the database on the volume `/data`. Without a volume, the Ledger Entries
  are lost when the container is removed.
- Pass the secrets with `-e NAME`. Do not write them in the file.
- Back up `/data/secret.key` and the database. Keep the volume across updates.
- To publish the port on the loopback interface only, use
  `-p 127.0.0.1:8787:8787`.
- Run one container for each database. See the Limits in [README.md](../README.md).
