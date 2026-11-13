# Install and configure llm-accounting (instructions for AI coding agents)

This file tells an AI coding agent how to install, configure, start and verify
the Proxy without a human. Run the steps in order. Each step has a command and an
expected result. Stop at the first step that does not give the expected result.
Then go to "Common failures".

The terms are defined in [GLOSSARY.md](GLOSSARY.md).

The installation is UI-first. You start the Proxy with no configuration file.
You complete Setup, then you add Providers, Models, Users and Virtual Keys in the
dashboard. Steps 4 and 5 show the dashboard forms as `curl` commands, so that you
can do the work without a browser.

## Rules for agents

1. Do not use port 3000 or port 4095. The default port of the Proxy is 8787. To
   use another port, set `LLM_ACCOUNTING_LISTEN` (for example
   `127.0.0.1:8790`).
2. Give Provider Keys to the Proxy as `env:NAME` references or on **Providers**.
   Never write a Provider Key in a file that you commit. For an optional seed
   file, use `api_key_env` and `api_keys_env` rather than secrets.
3. Set the dashboard password through Setup. Set the metrics token on
   **Settings**. Keep secrets and the Master Key outside the repository.
4. Use the `keys` setting of a User in a seed file only for local tests. The key
   is in plain text in the file.
5. Do not commit `config.toml`, `*.db`, `*.db-shm` or `*.db-wal`. The file
   `.gitignore` already lists them. It does not list `secret.key`. Do not commit
   that file.
6. Do not change `GLOSSARY.md` or the files in `docs/adr/`.

## 1. Prerequisites

Use one of these two ways.

### Way A: build from source

| Need                                   | Check command        | Expected                                        |
| -------------------------------------- | -------------------- | ----------------------------------------------- |
| Rust 1.88 or newer (edition 2024)      | `rustc --version`    | `rustc 1.88.0` or a higher version             |
| Cargo                                  | `cargo --version`    | Any version that comes with that Rust          |
| A C compiler (the build compiles SQLite) | `cc --version`     | Version text, no error                          |
| curl                                   | `curl --version`     | Version text                                    |
| sqlite3 (to read the database)         | `sqlite3 --version`  | Version text                                    |

The code uses `let` chains. These need Rust 1.88 or newer. If `rustc` is older,
run `rustup update stable`.

The build needs network access to download the crates. The Proxy needs network
access to the Provider at run time.

### Way B: Docker Compose

| Need           | Check command            | Expected      |
| -------------- | ------------------------ | ------------- |
| Docker         | `docker --version`       | Version text  |
| Docker Compose | `docker compose version` | Version text  |
| curl           | `curl --version`         | Version text  |

For Way B, go to "Docker Compose" at the end of this file. Then continue with
step 4. In step 4 and later, `$PROXY` is the address of the published port.

## 2. Build

Run in the root directory of the repository:

```sh
cargo build --release --locked
```

Expected: the last output line starts with ``Finished `release` profile``. The
binary is `target/release/llm-accounting`.

## 3. Start the Proxy

Set the Provider Keys in the environment of the Proxy. The Proxy reads an
`env:NAME` reference at start, so set the variables before you start it.

```sh
export OPENAI_API_KEY='<your Provider Key>'
export ANTHROPIC_API_KEY='<your Provider Key>'
```

Set only the variables for the Providers that you use. The next command starts
the Proxy with no configuration file:

```sh
NO_COLOR=1 nohup ./target/release/llm-accounting --listen 127.0.0.1:8787 \
  --database sqlite://data/llm-accounting.db > llm-accounting.log 2>&1 &
echo $! > llm-accounting.pid
sleep 2
cat llm-accounting.log
```

Expected: a log line that ends with
`llm-accounting started listen=127.0.0.1:8787`. The directory `data/` now holds
`llm-accounting.db` and `secret.key`. The variable `NO_COLOR=1` removes color
codes from the log.

To stop the Proxy:

```sh
kill "$(cat llm-accounting.pid)"
```

Only the listen address and the database path stay outside the dashboard. The
flags `--listen` and `--database` override `LLM_ACCOUNTING_LISTEN` and
`LLM_ACCOUNTING_DATABASE`. The defaults are `127.0.0.1:8787` and
`sqlite://llm-accounting.db`.

### Environment variables

| Variable                        | Needed | Function                                                              |
| ------------------------------- | ------ | --------------------------------------------------------------------- |
| Names in `env:NAME` references  | If used | Each variable holds one Provider Key, for example `OPENAI_API_KEY`.  |
| `LLM_ACCOUNTING_SECRET_KEY`     | No     | Master Key: 32 bytes as 64 hex characters or as base64. Otherwise the Proxy makes `secret.key`. |
| `LLM_ACCOUNTING_ADMIN_PASSWORD` | No     | Recovery override. Sets the dashboard password at each start. Use Setup instead. |
| `LLM_ACCOUNTING_METRICS_TOKEN`  | No     | Initial metrics token, used only on the first start. Later changes belong in **Settings**. |
| `LLM_ACCOUNTING_LISTEN`         | No     | Listen address, unless `--listen` is set.                             |
| `LLM_ACCOUNTING_DATABASE`       | No     | SQLite URL, unless `--database` is set.                               |
| `LLM_ACCOUNTING_CONFIG`         | No     | Path of the optional seed file.                                       |
| `RUST_LOG`                      | No     | Log filter. The default is `info`.                                    |

### Master Key

The Proxy encrypts the Provider Keys and the metrics token with the Master Key.
Use one of these two sources:

- The default. The Proxy makes `secret.key` next to the database at the first
  start, with Unix mode `0600`. Back up this file with the database.
- The variable `LLM_ACCOUNTING_SECRET_KEY`. Set it before the first start. Back up
  its value. An empty value stops the start, so do not set it to an empty string.

A lost Master Key makes the stored secrets unrecoverable. If the startup log says
`cannot decrypt stored secret`, restore the original key. Do not make a new key.

Back up now:

```sh
cp -p data/secret.key "$HOME/llm-accounting-secret.key.backup"
```

Keep the copy outside the repository and away from the database backup.

## 4. Complete Setup

Check that the Proxy runs and wants Setup:

```sh
export PROXY=http://127.0.0.1:8787
curl -s $PROXY/healthz
curl -si $PROXY/admin | head -3
```

Expected: `ok`, then a `303` redirect with `location: /setup` on the first start.

Setup sets the dashboard password and the mode. Choose the mode now:

| Mode   | Choose it when                                                     |
| ------ | ------------------------------------------------------------------ |
| Full   | You need Cost, Price, Profit, Budgets and Rate Limits. This is the default. |
| Simple | You need only routing and token metrics. No cards or limits are needed. |

You can change the mode later on **Settings**. See "Simple mode" in
[docs/usage.md](docs/usage.md#simple-mode).

In a browser, open `http://127.0.0.1:8787/setup` through a trusted local
connection. Without a browser, run:

```sh
ADMIN_PW="$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')"
echo "$ADMIN_PW"
curl -si -X POST --data-urlencode "password=$ADMIN_PW" -d "mode=full" $PROXY/setup | head -3
```

Use `mode=simple` for Simple Mode. Keep the printed password. Expected: a `303`
redirect with `location: /admin/login`. Setup works once. A second request gets
HTTP 403.

Log in. Store the session cookie in a file. Browser POSTs must come from the
same origin. Preserve the public Host and Origin headers at a reverse proxy.

```sh
curl -si -c cookies.txt --data-urlencode "password=$ADMIN_PW" $PROXY/admin/login | head -3
```

Expected output (the cookie value is different each time):

```
HTTP/1.1 303 See Other
location: /admin
set-cookie: llma_session=<64 hex characters>; HttpOnly; SameSite=Strict; Path=/admin; Max-Age=604800
```

A wrong password gives `HTTP/1.1 401 Unauthorized`. Delete `cookies.txt` when you
finish. It holds a valid session.

## 5. Configure the Proxy

Every item below is a dashboard page. The `curl` commands post the same forms.
All changes apply at once, without a restart.

### 5.1 Provider

On **Providers**, add a name, a kind (`openai` or `anthropic`), a base URL and a
key. The value `env:NAME` stores a reference to a variable. A plain key is stored
encrypted, and the page shows only its last 4 characters.

```sh
curl -s -o /dev/null -w '%{http_code}\n' -b cookies.txt \
  -d "name=openai&kind=openai&base_url=https://api.openai.com" \
  --data-urlencode "api_key=env:OPENAI_API_KEY" $PROXY/admin/providers
```

Expected: `303`. Select **Test** on the page, or run the next command:

```sh
curl -s -b cookies.txt -X POST $PROXY/admin/providers/openai/test
```

Expected: `<p>Upstream status: 200</p>`. A status 401 means the Provider Key is
wrong. The Proxy and the Provider answer, so the Provider step is correct.

### 5.2 Model (optional in Simple Mode)

In Full Mode, add a Model with its Cost Card on **Models**. The rates are USD per
million tokens. Check them against the Provider price list.

```sh
curl -s -o /dev/null -w '%{http_code}\n' -b cookies.txt \
  -d "name=gpt-4o&provider=openai&input=2.50&output=10.00&cache_read=1.25" \
  $PROXY/admin/models
```

Expected: `303`. Without a Model, a request with the name `openai/gpt-4o` still
works. The Proxy records it as an Unpriced Entry.

A new installation has a default Markup of 0 percent. To set 20 percent for Models
that have no Price Card, run:

```sh
curl -s -o /dev/null -w '%{http_code}\n' -b cookies.txt \
  -d "default_markup_percent=20" $PROXY/admin/settings/markup
```

Expected: `303`.

### 5.3 Route (optional)

On **Routes**, add a Route. Enter the name that clients send, select a strategy
(`priority`, `round_robin` or `weighted`) and add Deployment rows. Each row has a
Provider, an upstream Model and a weight. Enter Fallback names as a comma list.
Use the dashboard for this step.

### 5.4 User and Virtual Key

```sh
curl -s -o /dev/null -w '%{http_code}\n' -b cookies.txt -d "name=agent" $PROXY/admin/users
USER_ID=$(sqlite3 data/llm-accounting.db "select id from users where name='agent'")
KEY=$(curl -s -b cookies.txt -d "user_id=$USER_ID&label=agent-key&allowed_models=" $PROXY/admin/keys \
  | grep -o 'sk-llma-[0-9a-f]\{48\}')
echo "$KEY"
```

Expected: `303`, then one line with `sk-llma-` and 48 hexadecimal characters. The page
shows the key once. Save it at once. The Proxy keeps only a hash.

Form fields of `POST /admin/keys`:

| Field            | Meaning                                                              |
| ---------------- | -------------------------------------------------------------------- |
| `user_id`        | Required. The number of the User (`users.id`).                       |
| `label`          | Optional text.                                                       |
| `allowed_models` | Comma list of Model and Route names. Empty means all.                |
| `rpm_limit`, `tpm_limit` | Optional Rate Limits (Full Mode). Empty means unlimited.     |

Form fields of `POST /admin/users`: `name` (required), `team_id` (empty for no
Team) and `budget_usd` (empty for no Budget). Add `monthly=on` for a monthly
Budget.

### 5.5 Other settings

Use **Settings** for the upstream timeout, the Circuit Breaker values, the maximum
attempts, the Reservation tokens, the session duration, the default Markup, the
mode, the password and the metrics token. See
[docs/configuration.md](docs/configuration.md).

## 6. Verify

Run all commands in the same shell. The variables `PROXY` and `KEY` come from
steps 4 and 5.

### 6.1 Health

```sh
curl -s $PROXY/healthz
```

Expected output: `ok`

### 6.2 Model list

```sh
curl -s $PROXY/v1/models -H "Authorization: Bearer $KEY"
```

Expected output: a JSON list. With the Model from step 5.2, it is
`{"data":[{"created":0,"id":"gpt-4o","object":"model","owned_by":"openai"}],"object":"list"}`.
The list has the catalog Models and the Routes that the key can use. A Route has
`"owned_by":"router"`. The list is empty when you added no Model and no Route.
Add the header `anthropic-version: 2023-06-01` to get the Anthropic format.

### 6.3 Request with a Virtual Key

```sh
curl -s $PROXY/v1/chat/completions \
  -H "Authorization: Bearer $KEY" \
  -H "content-type: application/json" \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"Say ok."}]}'
```

Expected output: the JSON response of the Provider.

If you have no valid Provider Key, the Provider answers with an error, for
example HTTP 401. The check is still good: the Proxy accepted the Virtual Key and
wrote a Ledger Entry. The Ledger Entry in step 6.4 then has the status 401.

A request with a wrong Virtual Key gives the Proxy's own error:

```sh
curl -s $PROXY/v1/chat/completions -H "Authorization: Bearer wrong" -d '{}'
```

Expected output:

```json
{"error":{"message":"invalid API key","type":"authentication_error"}}
```

### 6.4 Ledger Entry

The Proxy writes the Ledger Entry in the background. Wait 1 second, then read it:

```sh
sleep 1
sqlite3 data/llm-accounting.db \
  "select id, model, status, input, output, cache_read, cost_micros, price_micros, unpriced, estimated from ledger order by id desc limit 1;"
```

The columns are: `id`, `model`, `status`, `input`, `output`, `cache_read`,
`cost_micros`, `price_micros`, `unpriced`, `estimated`. Expected output for a
successful request in Full Mode. The token counts depend on the response. This
example has 600 input, 100 output and 400 cache read tokens, and a default Markup
of 20 percent:

```
1|gpt-4o|200|600|100|400|3000|3600|0|0
```

Check these points:

- `status` is the HTTP status of the Provider response.
- `unpriced` is `0` when the Model has a Cost Card. It is `1` when it has none.
- `price_micros` is `cost_micros` plus the Markup.
- Money is in micro-dollars (1,000,000 micro-dollars is 1 USD).
- In Simple Mode without cards, the amounts are `0` and `unpriced` is `1`.

### 6.5 Metrics

```sh
curl -s $PROXY/metrics | grep llm_requests_total
```

Expected: one line that starts with
`llm_requests_total{provider="openai",model="gpt-4o` and ends with `} 1`. If you
set a metrics token on **Settings**, add
`-H "Authorization: Bearer <token>"`.

## 7. Optional seed file

Use `--config PATH` or `LLM_ACCOUNTING_CONFIG` only if you need a seed file. There
is no implicit `config.toml`. Providers, Routes, router settings and server
settings seed once, on the first start, before runtime settings exist. Later file
edits do not replace dashboard changes. Models, Teams and Users keep an additive
seed behavior. See "Seeding" in [docs/configuration.md](docs/configuration.md).

The file `config.example.toml` has a full example. The next command makes a
minimal file with one Provider, one Model and one local test User:

```sh
cat > config.toml <<'EOF'
[[providers]]
name = "openai"
kind = "openai"
base_url = "https://api.openai.com"
api_key_env = "OPENAI_API_KEY"

[[models]]
name = "gpt-4o"
provider = "openai"
cost = { input = 2.50, output = 10.00, cache_read = 1.25 }

[[users]]
name = "agent"
# Local test only: this makes a Virtual Key at start.
keys = ["sk-dev-agent-change-me"]
EOF
NO_COLOR=1 ./target/release/llm-accounting --config config.toml
```

Check the rates against the current price list of the Provider before you use the
Proxy for real work. Complete step 4 to set the password. Then use the key
`sk-dev-agent-change-me` in step 6. Do not use this key in production.

## 8. Run the tests

```sh
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
```

Expected: each `test result:` line says `ok`, and `cargo clippy` prints no
warnings. The tests start a mock Provider and a Proxy on random free ports and use
a temporary database. They do not use port 3000 or port 4095.

## Common failures

| Symptom                                                                                                  | Cause                                                | Fix                                                                                           |
| -------------------------------------------------------------------------------------------------------- | ---------------------------------------------------- | --------------------------------------------------------------------------------------------- |
| `Error: bind 127.0.0.1:8787` and `Address already in use (os error 98)`                                  | Another process uses the port.                       | Find it with `ss -ltnp \| grep 8787`. Or set `LLM_ACCOUNTING_LISTEN=127.0.0.1:8790` and start again. Do not use 3000 or 4095. |
| `unknown argument`                                                                                       | The Proxy accepts only `--config`, `--listen`, `--database` and `--help`. | Remove the flag. Use the dashboard for other settings.                |
| `Error: read config.toml` and `No such file or directory`                                                | The seed file path is wrong.                         | Correct the path of `--config` or `LLM_ACCOUNTING_CONFIG`.                                    |
| `Error: parse config.toml` and `unknown field`                                                           | A key has a wrong name.                              | Correct the key. See [docs/configuration.md](docs/configuration.md).                          |
| `missing field` `kind` (or another key)                                                                  | A required key is missing.                           | Add the key.                                                                                  |
| `model X uses unknown provider Y`                                                                        | `provider` of a Model has no `[[providers]]` item.   | Make the names the same.                                                                      |
| `user X uses unknown team Y`                                                                             | `team` of a User has no `[[teams]]` item.            | Add the Team or correct the name.                                                             |
| Startup says `LLM_ACCOUNTING_SECRET_KEY must contain 32 bytes (hex or base64)`.                          | The variable is empty or has a wrong length.         | Set 64 hex characters or 32 bytes in base64. Or remove the variable.                          |
| Startup says `cannot decrypt stored secret`.                                                             | The Master Key changed or was lost.                  | Restore the original `secret.key` or `LLM_ACCOUNTING_SECRET_KEY`. Do not make a new key.      |
| `/admin` redirects to `/setup`.                                                                          | No password hash exists.                             | Complete step 4.                                                                              |
| `/setup` gives HTTP 403.                                                                                 | Setup is complete.                                   | Log in. Change the password on **Settings**.                                                  |
| A dashboard POST gives HTTP 403 `Cross-origin form submission refused.`                                  | The `Origin` header does not match the `Host` header. | Do not send `Origin`, or make it equal to the public address. Preserve both at a reverse proxy. |
| Log warning `no API key is set for this provider`                                                        | An `env:NAME` reference points to a variable that is not set or is empty. | Set the variable in the environment of the Proxy. Restart. Requests go upstream with an empty key until you do. |
| A request gives HTTP 401 and the body is from the Provider (not `authentication_error` from the Proxy). The Ledger Entry has `status` 401. | The Provider Key is wrong, expired or empty. | Add a valid key on **Providers** and remove the old key. The change is live. The Proxy does not retry a 401. |
| A request gives `{"error":{"type":"authentication_error", ...}}`                                         | The Virtual Key is missing, unknown or revoked, or the User is disabled. | Use a valid Virtual Key. Check the **Keys** and **Users** pages.                |
| HTTP 403 `this API key may not use this model`                                                           | The Virtual Key has an allowed-Models list.          | Add the Model or Route to the list, or use another key.                                       |
| HTTP 402 `budget_exceeded`                                                                               | The Budget of the User is used or reserved.          | Raise the Budget on the **Users** page.                                                       |
| HTTP 429 `rate_limit_error`                                                                              | A Rate Limit of the key or the User is reached.      | Wait for the seconds in `Retry-After`. Or raise the limit.                                    |
| HTTP 400 `invalid_request_error` with a Chat Completions request to an Anthropic Provider (or the reverse) | Translation does not support the request, for example `n` greater than 1. | Change the request. See "Routing and Translation" in [docs/usage.md](docs/usage.md#routing-and-translation). |
| HTTP 502 `no provider is configured for this request`                                                    | No Provider can serve the request.                   | Add a Provider on **Providers**. Check that its kind fits the endpoint, or that a Route has Deployments. |
| HTTP 502 `upstream request failed`                                                                       | The Proxy cannot connect to the Provider.            | Check the base URL and the network.                                                           |
| `unpriced` is `1` in the Ledger Entry. The dashboard shows an **unpriced** notice.                       | The Model name in the request is not a configured Model, alias or `<name>-` prefix. | Add the Model on the **Models** page. Then select **Price unpriced entries** on the **Ledger** page (**Unpriced** tab). |
| A change in `config.toml` has no effect.                                                                 | A seed file seeds once. It never changes existing items. | Change the item in the dashboard. See "Seeding" in [docs/configuration.md](docs/configuration.md). |
| In Docker: the Proxy stops at start with a database error                                                | `/data` is not writable for uid 10001.               | Use a named volume (`-v llma-data:/data`) or give uid 10001 write access to the host directory. |

To find unpriced entries in the database:

```sh
sqlite3 data/llm-accounting.db "select id, requested_model, model from ledger where unpriced = 1;"
```

## Docker Compose

The file `compose.yaml` builds the image from the `Dockerfile` and starts the
Proxy. Run in the root directory of the repository:

```sh
export OPENAI_API_KEY='<your Provider Key>'
docker compose up -d --build
```

Expected: the container starts. After about 30 seconds, `docker compose ps` shows
the status `healthy`. Compose passes `OPENAI_API_KEY` and `ANTHROPIC_API_KEY` to
the container, so a Provider Key can use `env:OPENAI_API_KEY`. Compose also reads
a `.env` file in the same directory.

Notes:

- The port is published on `127.0.0.1:8787` only. Remove `127.0.0.1:` in
  `compose.yaml` to accept other hosts. Use TLS at a reverse proxy.
- The volume `llma-data` holds `/data`: the database and `secret.key`. Keep the
  volume across updates.
- The line `LLM_ACCOUNTING_SECRET_KEY` in `compose.yaml` is a comment. An empty
  value stops the start. Remove the `#` only when you set a real value in the
  shell or in `.env`.
- To seed from a file, remove the `#` from the `volumes` line and the `command`
  line in `compose.yaml`, and put your file at `./config.toml`.

Back up the Master Key and the database:

```sh
docker compose cp llm-accounting:/data ./llma-data-backup
```

Keep the copy outside the repository. Now go to step 4 with
`PROXY=http://127.0.0.1:8787`. Step 5.4 and step 6.4 read the database with
`sqlite3`. Use `./llma-data-backup/llm-accounting.db` after a new
`docker compose cp`. Copy all `llm-accounting.db*` files, because the newest
writes can be in the `-wal` file.

To stop the container: `docker compose down`. This keeps the volume.

## Docker without Compose

Build the image:

```sh
docker build -t llm-accounting .
```

Expected: the build ends without an error.

The image holds an optional example seed file. It does not use that file by
default. Use Setup and the dashboard as described in steps 4 and 5.

Run the container. Replace the port if 8787 is in use. Do not use 3000 or 4095.

```sh
docker run -d --name llm-accounting -p 127.0.0.1:8787:8787 \
  -e OPENAI_API_KEY \
  -v llma-data:/data \
  llm-accounting
```

The container sets `LLM_ACCOUNTING_LISTEN=0.0.0.0:8787`
and `LLM_ACCOUNTING_DATABASE=sqlite:///data/llm-accounting.db`. The database is on
the volume `llma-data`. Keep and back up `/data/secret.key` with the database.
To seed instead, mount your own file and add `--config PATH` after the image
name. Do not use the example development keys in production.

Verify with the commands in step 6. To read the Ledger, copy the database files
from the volume to the current directory:

```sh
docker run --rm -v llma-data:/data -v "$PWD:/out" debian:bookworm-slim \
  sh -c 'cp /data/llm-accounting.db* /out/'
```

Then run the `sqlite3` command of step 6.4 on `llm-accounting.db`. Copy all
`llm-accounting.db*` files, because the newest writes can be in the `-wal` file.

To stop and remove the container: `docker rm -f llm-accounting`.
