# Install and configure llm-accounting (instructions for AI coding agents)

This file tells an AI coding agent how to install, configure, start and verify
the Proxy without a human. Run the steps in order. Each step has a command and an
expected result. Stop at the first step that does not give the expected result.
Then go to "Common failures".

The terms are defined in [GLOSSARY.md](GLOSSARY.md).

## Rules for agents

1. Do not use port 3000 or port 4095. The default port of the Proxy is 8787. To
   use another port, set `LLM_ACCOUNTING_LISTEN` (for example
   `127.0.0.1:8790`).
2. Never write a Provider Key in a configuration file that is in a committed
   repository. Use the `api_key_env` and `api_keys_env` keys. Pass the key in an
   environment variable.
3. Never write the dashboard password or the metrics token in a committed file.
   Use the environment variables.
4. Use the `keys` setting of a User only for local tests. The key is in plain
   text in the file.
5. Do not commit `config.toml`, `*.db`, `*.db-shm` or `*.db-wal`. The file
   `.gitignore` already lists them.
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

### Way B: Docker

| Need   | Check command      | Expected      |
| ------ | ------------------ | ------------- |
| Docker | `docker --version` | Version text  |

For Way B, go to "Docker" at the end of this file.

## 2. Build

Run in the root directory of the repository:

```sh
cargo build --release --locked
```

Expected: the last output line starts with ``Finished `release` profile``. The
binary is `target/release/llm-accounting`.

## 3. Write the configuration

The file `config.example.toml` has a full example. The next command makes a
minimal file with one Provider, one Model and one local test User:

```sh
cat > config.toml <<'EOF'
[server]
listen = "127.0.0.1:8787"
database = "sqlite://llm-accounting.db"
admin_password_env = "LLM_ACCOUNTING_ADMIN_PASSWORD"
default_markup_percent = 20

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
```

Check the rates against the current price list of the Provider before you use the
Proxy for real work.

For an Anthropic Provider, add this item:

```toml
[[providers]]
name = "anthropic"
kind = "anthropic"
base_url = "https://api.anthropic.com"
api_key_env = "ANTHROPIC_API_KEY"
```

Add a `[[models]]` item with `provider = "anthropic"` for each Model that you use.
See [docs/configuration.md](docs/configuration.md) for every key.

## 4. Set the environment variables

| Variable                        | Needed          | Function                                                    |
| ------------------------------- | --------------- | ----------------------------------------------------------- |
| `OPENAI_API_KEY`                | Yes (Provider)  | The Provider Key. The name comes from `api_key_env`.        |
| `ANTHROPIC_API_KEY`             | If you use it   | The Provider Key for an Anthropic Provider.                 |
| `LLM_ACCOUNTING_ADMIN_PASSWORD` | Yes (dashboard) | The dashboard password. Without it the dashboard is off.    |
| `LLM_ACCOUNTING_METRICS_TOKEN`  | No              | The bearer token for `/metrics`. Without it `/metrics` is open. |
| `LLM_ACCOUNTING_LISTEN`         | No              | Replaces `server.listen`.                                   |
| `LLM_ACCOUNTING_DATABASE`       | No              | Replaces `server.database`.                                 |
| `LLM_ACCOUNTING_CONFIG`         | No              | Path of the configuration file.                             |
| `RUST_LOG`                      | No              | Log filter. The default is `info`.                          |

Set them in the shell that starts the Proxy:

```sh
export OPENAI_API_KEY='<your Provider Key>'
export LLM_ACCOUNTING_ADMIN_PASSWORD="$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')"
echo "$LLM_ACCOUNTING_ADMIN_PASSWORD"
```

Keep the printed password. You need it in step 6.

## 5. Start the Proxy

```sh
NO_COLOR=1 nohup ./target/release/llm-accounting --config config.toml > llm-accounting.log 2>&1 &
echo $! > llm-accounting.pid
sleep 2
cat llm-accounting.log
```

Expected: a log line that ends with
`llm-accounting started listen=127.0.0.1:8787 config=config.toml`. The variable
`NO_COLOR=1` removes color codes from the log.

The log can also have warnings. See "Common failures".

To stop the Proxy:

```sh
kill "$(cat llm-accounting.pid)"
```

## 6. Verify

Run all commands in the same shell. The variable `PROXY` is the address of the
Proxy.

```sh
export PROXY=http://127.0.0.1:8787
```

### 6.1 Health

```sh
curl -s $PROXY/healthz
```

Expected output: `ok`

### 6.2 Dashboard login

```sh
curl -si -X POST -d "password=$LLM_ACCOUNTING_ADMIN_PASSWORD" $PROXY/admin/login | head -3
```

Expected output (the cookie value is different each time):

```
HTTP/1.1 303 See Other
location: /admin
set-cookie: llma_session=<64 hex characters>; HttpOnly; SameSite=Strict; Path=/admin; Max-Age=604800
```

A wrong password gives `HTTP/1.1 401 Unauthorized`. If the dashboard is off, the
status is `503 Service Unavailable`.

### 6.3 Request with a Virtual Key

```sh
curl -s $PROXY/v1/chat/completions \
  -H "Authorization: Bearer sk-dev-agent-change-me" \
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
sqlite3 llm-accounting.db \
  "select id, model, status, input, output, cache_read, cost_micros, price_micros, unpriced, estimated from ledger order by id desc limit 1;"
```

The columns are: `id`, `model`, `status`, `input`, `output`, `cache_read`,
`cost_micros`, `price_micros`, `unpriced`, `estimated`. Expected output for a
successful request. The token counts depend on the response. This example has 600
input, 100 output and 400 cache read tokens:

```
1|gpt-4o|200|600|100|400|3000|3600|0|0
```

Check these points:

- `status` is the HTTP status of the Provider response.
- `unpriced` is `0` when the Model is in the configuration.
- `price_micros` is `cost_micros` plus the Markup (20 percent here).
- Money is in micro-dollars (1,000,000 micro-dollars is 1 USD).

### 6.5 Metrics

```sh
curl -s $PROXY/metrics | grep llm_requests_total
```

Expected: one line with `llm_requests_total{provider="openai",model="gpt-4o",user="agent",status="200"} 1`.
If you set `LLM_ACCOUNTING_METRICS_TOKEN`, add
`-H "Authorization: Bearer $LLM_ACCOUNTING_METRICS_TOKEN"`.

## 7. Create a Virtual Key without a browser

Use one of these two methods.

### Method A: `keys` in the configuration (development only)

Add the key to a User in `config.toml`:

```toml
[[users]]
name = "agent"
keys = ["sk-dev-agent-change-me"]
```

Restart the Proxy. The Proxy adds the key for the User at start. It also adds the
key to a User that is already in the database. The key is in plain text in the
file, so use it for local tests only.

### Method B: the dashboard form (production)

The dashboard form `POST /admin/keys` makes a random key. Log in, then send the
form with the session cookie:

```sh
curl -s -o /dev/null -c cookies.txt -d "password=$LLM_ACCOUNTING_ADMIN_PASSWORD" $PROXY/admin/login
USER_ID=$(sqlite3 llm-accounting.db "select id from users where name='agent'")
curl -s -b cookies.txt -d "user_id=$USER_ID&label=agent-key&allowed_models=" $PROXY/admin/keys \
  | grep -o 'sk-llma-[0-9a-f]\{48\}'
```

Expected output: one line, `sk-llma-` followed by 48 hexadecimal characters. The
response page shows the key only once. Save the key at once. The Proxy keeps
only a hash.

Form fields of `POST /admin/keys`:

| Field            | Meaning                                                              |
| ---------------- | -------------------------------------------------------------------- |
| `user_id`        | Required. The number of the User (`users.id`).                       |
| `label`          | Optional text.                                                       |
| `allowed_models` | Comma list of Model names. Empty means all Models.                   |

Delete `cookies.txt` after use. It holds a valid session.

To add a User first, send `POST /admin/users` with the fields `name`, `team_id`
(empty for no Team) and `budget_usd` (empty for no Budget). Add `monthly=on` for
a monthly Budget.

## 8. Run the tests

```sh
cargo test --locked
cargo clippy --all-targets --locked
```

Expected: each `test result:` line says `ok`, and `cargo clippy` prints no
warnings. The tests start a mock Provider and a Proxy on random free ports and use
a temporary database. They do not use port 3000 or port 4095.

## Common failures

| Symptom                                                                                                  | Cause                                                | Fix                                                                                           |
| -------------------------------------------------------------------------------------------------------- | ---------------------------------------------------- | --------------------------------------------------------------------------------------------- |
| `Error: bind 127.0.0.1:8787` and `Address already in use (os error 98)`                                  | Another process uses the port.                       | Find it with `ss -ltnp \| grep 8787`. Or set `LLM_ACCOUNTING_LISTEN=127.0.0.1:8790` and start again. Do not use 3000 or 4095. |
| `Error: read config.toml` and `No such file or directory`                                                | The file is not in the working directory.            | Use `--config PATH` or set `LLM_ACCOUNTING_CONFIG`.                                           |
| `Error: parse config.toml` and `unknown field`                                                           | A key has a wrong name.                              | Correct the key. See [docs/configuration.md](docs/configuration.md).                          |
| `missing field` `kind` (or another key)                                                                  | A required key is missing.                           | Add the key.                                                                                  |
| `model X uses unknown provider Y`                                                                        | `provider` of a Model has no `[[providers]]` item.   | Make the names the same.                                                                      |
| `user X uses unknown team Y`                                                                             | `team` of a User has no `[[teams]]` item.            | Add the Team or correct the name.                                                             |
| Log warning `no admin password is set; the dashboard is off`. `/admin` and `/admin/login` give HTTP 503. | `LLM_ACCOUNTING_ADMIN_PASSWORD` is not set or is empty. | Set the variable in the shell that starts the Proxy. Restart.                              |
| Log warning `no API key is set for this provider`                                                        | The variable in `api_key_env` is not set or is empty. | Set the variable (for example `OPENAI_API_KEY`). Restart. Requests go upstream with an empty key until you do. |
| A request gives HTTP 401 and the body is from the Provider (not `authentication_error` from the Proxy). The Ledger Entry has `status` 401. | The Provider Key is wrong, expired or empty.         | Set a valid Provider Key. Restart. The Proxy does not retry a 401.                            |
| A request gives `{"error":{"type":"authentication_error", ...}}`                                         | The Virtual Key is missing, unknown or revoked, or the User is disabled. | Use a valid Virtual Key. Check the **Keys** and **Users** pages.                |
| HTTP 403 `this API key may not use this model`                                                           | The Virtual Key has an allowed-Models list.          | Add the Model to the list, or use another key.                                                |
| HTTP 402 `budget_exceeded`                                                                               | The Budget of the User is used or reserved.          | Raise the Budget on the **Users** page.                                                       |
| HTTP 502 `upstream request failed`                                                                       | The Proxy cannot connect to the Provider.            | Check `base_url` and the network.                                                             |
| `unpriced` is `1` in the Ledger Entry. The dashboard shows an **unpriced** notice.                       | The Model name in the request is not a configured Model, alias or `<name>-` prefix. | Add a `[[models]]` item or add the Model on the **Models** page. Then select **Price unpriced entries** on the **Ledger** page (**Unpriced** tab). |
| A change in `config.toml` to an existing Model, Team or User has no effect.                              | Seeding never changes items that are in the database. | Change the item in the dashboard. See "Seeding" in [docs/configuration.md](docs/configuration.md). |
| In Docker: the Proxy stops at start with a database error                                               | `/data` is not writable for uid 10001.               | Use a named volume (`-v llma-data:/data`) or give uid 10001 write access to the host directory. |

To find unpriced entries in the database:

```sh
sqlite3 llm-accounting.db "select id, requested_model, model from ledger where unpriced = 1;"
```

## Docker

Build the image:

```sh
docker build -t llm-accounting .
```

Expected: the build ends without an error.

The image holds a copy of `config.example.toml`. That file creates the
development Virtual Key `sk-dev-alice-change-me`. For real use, mount your own
configuration file (made in step 3, with the `keys` lines removed).

Run the container. Replace the port if 8787 is in use. Do not use 3000 or 4095.

```sh
docker run -d --name llm-accounting -p 127.0.0.1:8787:8787 \
  -v llma-data:/data \
  -v "$PWD/config.toml:/etc/llm-accounting/config.toml:ro" \
  -e OPENAI_API_KEY -e ANTHROPIC_API_KEY -e LLM_ACCOUNTING_ADMIN_PASSWORD \
  llm-accounting
```

The container sets `LLM_ACCOUNTING_CONFIG`, `LLM_ACCOUNTING_LISTEN=0.0.0.0:8787`
and `LLM_ACCOUNTING_DATABASE=sqlite:///data/llm-accounting.db`. The database is on
the volume `llma-data`.

Verify with the commands in step 6.1 to step 6.3. To read the Ledger, copy the
database files from the volume to the current directory:

```sh
docker run --rm -v llma-data:/data -v "$PWD:/out" debian:bookworm-slim \
  sh -c 'cp /data/llm-accounting.db* /out/'
```

Then run the `sqlite3` command of step 6.4. Copy all `llm-accounting.db*` files,
because the newest writes can be in the `-wal` file.

To stop and remove the container: `docker rm -f llm-accounting`.
