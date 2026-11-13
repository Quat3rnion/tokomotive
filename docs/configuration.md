# Configuration reference

This document lists every setting of llm-accounting. The terms are defined in
[GLOSSARY.md](../GLOSSARY.md).

The Proxy reads one TOML file at start. Environment variables can replace some
settings. See [usage.md](usage.md) for the Operator guide.

## How the Proxy finds the file

The Proxy reads the path of the file in this order:

1. The `--config PATH` flag (short form `-c PATH`).
2. The environment variable `LLM_ACCOUNTING_CONFIG`.
3. The file `config.toml` in the working directory.

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
| `LLM_ACCOUNTING_LISTEN`         | Replaces `server.listen`.                                                                            |
| `LLM_ACCOUNTING_DATABASE`       | Replaces `server.database`.                                                                          |
| `LLM_ACCOUNTING_ADMIN_PASSWORD` | The dashboard password. This is the default value of `server.admin_password_env`.                    |
| `LLM_ACCOUNTING_METRICS_TOKEN`  | The bearer token for `/metrics`. This is the default value of `server.metrics_token_env`.            |
| `RUST_LOG`                      | Log filter, for example `info` or `llm_accounting=debug`. The default is `info`.                     |
| Names in `*_env` keys           | Each `api_key_env`, `api_keys_env`, `admin_password_env` and `metrics_token_env` key names a variable. |

`LLM_ACCOUNTING_LISTEN` and `LLM_ACCOUNTING_DATABASE` replace the file value
when they are set, even if they are empty.

A `*_env` key holds the name of a variable, not the secret. The Proxy ignores a
variable that is not set or is empty.

## `[server]`

All keys are optional. The whole section is optional.

| Key                      | Type   | Default                         | Meaning                                                                                                                                                  |
| ------------------------ | ------ | ------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `listen`                 | string | `"127.0.0.1:8787"`              | Address and port that the Proxy listens on. `LLM_ACCOUNTING_LISTEN` replaces it.                                                                         |
| `database`               | string | `"sqlite://llm-accounting.db"`  | URL of the SQLite database. The Proxy creates the file if it is not there. The Proxy runs the migrations at start. `LLM_ACCOUNTING_DATABASE` replaces it. |
| `admin_password`         | string | none                            | The dashboard password, in the file. Do not use it in a repository that you commit.                                                                      |
| `admin_password_env`     | string | `"LLM_ACCOUNTING_ADMIN_PASSWORD"` | Name of the variable that holds the dashboard password. The variable has priority over `admin_password`.                                               |
| `default_markup_percent` | float  | `0`                             | Markup for Models that have no Price Card. The file `config.example.toml` sets it to `20`.                                                               |
| `upstream_timeout_secs`  | integer | `600`                          | Time limit in seconds for one request to a Provider. The limit includes the time to read the response body. The connect time limit is 15 seconds and is fixed. |
| `metrics_token`          | string | none                            | The bearer token for `/metrics`, in the file.                                                                                                            |
| `metrics_token_env`      | string | `"LLM_ACCOUNTING_METRICS_TOKEN"` | Name of the variable that holds the bearer token. The variable has priority over `metrics_token`.                                                       |
| `reserve_output_tokens`  | integer | `4096`                         | Output tokens that a Reservation assumes when the request sets no maximum. A negative value counts as 0.                                                 |
| `session_days`           | integer | `7`                            | Number of days that a dashboard login stays valid. A value below 1 counts as 1.                                                                          |

### Dashboard password

The Proxy uses the password from the first of these sources that is not empty:

1. The variable that `admin_password_env` names.
2. The key `admin_password`.

If no source gives a password, the Proxy writes a warning at start. The dashboard
is then off. Each `/admin` page answers HTTP 503 with the text "The dashboard is
off. Set the admin password in the configuration." The `/v1` routes, `/healthz`
and `/metrics` still work.

### Metrics token

The Proxy uses the token from the variable that `metrics_token_env` names, then
from `metrics_token`. If no token is set, `/metrics` needs no authentication.

## `[[providers]]`

Each item defines one Provider. The file can have many items.

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
| `keys`           | array of strings | `[]`    | Virtual Keys to create at start. For development only. See below.                                    |

A key in `keys` is stored with the label `config`. The key is in plain text in the
file. Do not use this setting for production Users. Create Virtual Keys on the
**Keys** page of the dashboard instead.

## Seeding

The file seeds the database at each start. Seeding adds only items that are not in
the database. It never changes an item that is in the database.

| Item                          | Rule                                                                                             |
| ----------------------------- | ------------------------------------------------------------------------------------------------ |
| `server.default_markup_percent` | Set only if the database has no value yet. Later changes are made in the dashboard.            |
| Team                          | Added if no Team has the same name.                                                              |
| Model                         | Added with its Cost Card and Price Card if no Model has the same name. If the Model exists, the file is ignored for its Provider, aliases and cards. |
| Team override                 | Added if the Model has no card for this Team. If the Model is not in the database, the Proxy writes a warning. |
| User                          | Added if no User has the same name. If the User exists, the Team and the Budget are not changed. |
| Virtual Key in `keys`         | Added for the User if the key is not in the database. This also applies to existing Users.       |

Changes that you make in the dashboard stay after a restart. To change a seeded
item, use the dashboard. These settings are not seeded and always come from the
file or the environment at start: `listen`, `database`, the passwords, the
tokens, `upstream_timeout_secs`, `reserve_output_tokens`, `session_days` and the
Providers with their Provider Keys.

The dashboard cannot add or change Providers. To add a Provider, edit the file and
restart the Proxy. The dashboard shows Provider names but not Provider Keys.

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
| `LLM_ACCOUNTING_CONFIG` | `/etc/llm-accounting/config.toml`                                                     |
| `LLM_ACCOUNTING_LISTEN` | `0.0.0.0:8787`                                                                        |
| `LLM_ACCOUNTING_DATABASE` | `sqlite:///data/llm-accounting.db`                                                  |

Build and run:

```sh
docker build -t llm-accounting .
docker run -p 8787:8787 -v llma-data:/data \
  -v $PWD/config.toml:/etc/llm-accounting/config.toml:ro \
  -e OPENAI_API_KEY -e ANTHROPIC_API_KEY -e LLM_ACCOUNTING_ADMIN_PASSWORD \
  llm-accounting
```

Rules for Docker:

- Mount your own file at `/etc/llm-accounting/config.toml`. Without a mount, the
  container uses the example file.
- Keep the database on the volume `/data`. Without a volume, the Ledger Entries
  are lost when the container is removed.
- Pass the secrets with `-e NAME`. Do not write them in the file.
- To publish the port on the loopback interface only, use
  `-p 127.0.0.1:8787:8787`.
- Run one container for each database. See the Limits in [README.md](../README.md).
