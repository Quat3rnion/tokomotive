# llm-accounting

llm-accounting is a proxy for LLM APIs. Clients send requests to the proxy
instead of to the Provider. The proxy forwards each request, reads the token
usage from the response, and records the Cost, the Price and the Profit for
each User and Model. A web dashboard shows the data and lets the Operator
change the configuration.

The terms in this document are defined in [GLOSSARY.md](GLOSSARY.md).

## Features

- Supports OpenAI-compatible APIs (`/v1/chat/completions`, `/v1/responses`,
  `/v1/embeddings`, `/v1/images/generations`, `/v1/images/edits`) and the
  Anthropic API (`/v1/messages`), with and without streaming.
- Records seven Token Classes: input, output, cache read, cache write
  (5 minutes), cache write (1 hour), image input and image output.
- Uses a Cost Card and a Price Card for each Model. A Price Card is a Markup or
  explicit rates. A Team can override the Price Card of a Model.
- Supports Context Tiers, for example higher rates above 200,000 input tokens.
- Issues Virtual Keys to Users. The Provider Keys stay on the server.
- Uses a Key Pool for each Provider. After a 429, a 5xx or a connection
  error, the proxy tries the next key before it sends data to the client.
- Rejects requests with HTTP 402 when the Budget of the User does not cover
  the worst-case Price of the request (see Reservation below).
- Shows requests, tokens, latency, time to first token, Cost, Price and
  Profit per Model, User and Team in the dashboard.
- Exports Prometheus metrics at `/metrics`, optionally with a token.

## Start the proxy

1. Copy the example configuration:

   ```sh
   cp config.example.toml config.toml
   ```

2. Check the rates in `config.toml` against the current Provider price lists.

3. Set the Provider Keys and the dashboard password:

   ```sh
   export OPENAI_API_KEY=...
   export ANTHROPIC_API_KEY=...
   export LLM_ACCOUNTING_ADMIN_PASSWORD=...
   ```

4. Start the proxy:

   ```sh
   cargo run --release -- --config config.toml
   ```

5. Open `http://127.0.0.1:8787/admin` and log in.

6. On the **Keys** page, create a Virtual Key for a User. Copy the key. You
   cannot see it again.

### Docker

```sh
docker build -t llm-accounting .
docker run -p 8787:8787 -v llma-data:/data \
  -v $PWD/config.toml:/etc/llm-accounting/config.toml:ro \
  -e OPENAI_API_KEY -e ANTHROPIC_API_KEY -e LLM_ACCOUNTING_ADMIN_PASSWORD \
  llm-accounting
```

## Connect a client

Set the base URL of the client to the proxy. Use the Virtual Key as the API
key.

```sh
# OpenAI SDKs
export OPENAI_BASE_URL=http://127.0.0.1:8787/v1
export OPENAI_API_KEY=sk-llma-...

# Anthropic SDKs
export ANTHROPIC_BASE_URL=http://127.0.0.1:8787
export ANTHROPIC_API_KEY=sk-llma-...
```

The proxy accepts the key in `Authorization: Bearer` or in `x-api-key`.

## Configuration

The configuration file seeds the database at start. The proxy adds the Models,
cards, Teams and Users that are not in the database. It does not change items
that are in the database already. Thus, changes that you make in the dashboard
stay after a restart.

Provider Keys are only in the configuration file or in environment variables.
The dashboard does not show them.

All rates are USD per million tokens. See `config.example.toml` for all
settings.

| Environment variable              | Function                                |
| --------------------------------- | --------------------------------------- |
| `LLM_ACCOUNTING_CONFIG`           | Path of the configuration file.         |
| `LLM_ACCOUNTING_LISTEN`           | Replaces `server.listen`.               |
| `LLM_ACCOUNTING_DATABASE`         | Replaces `server.database`.             |
| `LLM_ACCOUNTING_ADMIN_PASSWORD`   | The dashboard password.                 |
| `LLM_ACCOUNTING_METRICS_TOKEN`    | The bearer token for `/metrics`.        |
| `RUST_LOG`                        | Log level, for example `info`.          |

## How the proxy calculates amounts

- The proxy finds the Model from the model name in the response. A Model also
  matches names that start with `<name>-`, for example `gpt-4o-2024-08-06`.
  The longest name wins. Aliases are exact names.
- The Price Card comes from the first item that applies: the Team override for
  the Model, the Model Price Card, the Cost Card with the default Markup.
- A Context Tier applies when all input tokens, including cache tokens, are
  more than the tier limit.
- Money is stored in integer micro-dollars. The proxy rounds once for each
  request.
- A Ledger Entry does not change when you change a card. See
  [ADR 0002](docs/adr/0002-freeze-amounts-on-ledger-entries.md).
- If a Model has no card, the proxy forwards the request and records an
  Unpriced Entry. After you add a card, open **Ledger > Unpriced** and click
  **Price unpriced entries**.
- Before a request goes to the Provider, the proxy makes a Reservation of
  its worst-case Price: the request size divided by 4 as input tokens, plus
  `max_tokens` (or `server.reserve_output_tokens`) as output tokens. The
  proxy rejects the request when spend, open Reservations and this
  Reservation are more than the Budget. The proxy releases the Reservation
  after it writes the Ledger Entry.
- For streamed Chat Completions, the proxy adds
  `stream_options.include_usage`. If the client did not ask for usage, the
  proxy removes the usage chunk from the stream that the client receives.
- If a client disconnects during a stream, the proxy continues to read the
  stream to get the full usage. See
  [ADR 0001](docs/adr/0001-drain-aborted-streams.md).

## Limits

- The proxy does not fall back to a different Provider or Model.
- The proxy does not record usage for audio. It forwards these requests and
  records them as Unpriced Entries.
- The Reservation uses an estimate of the input tokens. A request can cost
  more than its Reservation, for example when it refers to files or cached
  content that is not in the request body. Thus, a Budget can be exceeded by
  a small amount.
- Reservations are in memory. Run one proxy process for each database.
- Cached image input tokens (Responses API) are recorded as image input,
  not as cache read.
- `/metrics` has no authentication when `metrics_token` is not set.

## Development

```sh
cargo test
cargo clippy --all-targets
```
