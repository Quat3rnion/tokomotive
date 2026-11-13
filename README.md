# llm-accounting

llm-accounting is a Proxy for LLM APIs. Clients send requests to the Proxy
instead of to the Provider. The Proxy forwards each request, reads the token
usage from the response, and records the Cost, the Price and the Profit for each
User and Model. A web dashboard shows the data and lets the Operator change the
configuration.

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
- Keeps every change to a card as a Card Version. A Ledger Entry does not change
  when you change a card.
- Issues Virtual Keys to Users. The Provider Keys stay on the server.
- Uses a Key Pool for each Provider. After a 429, a 5xx or a connection error,
  the Proxy tries the next key before it sends data to the client.
- Routes a client model name to several Deployments with priority,
  round-robin or weighted selection, Fallbacks and Circuit Breakers.
- Supports `provider/model` names when no Route or catalog Model matches.
  The Provider prefix is removed before the upstream request. Models without
  cards still have usage recorded as Unpriced Entries.
- Lists permitted Routes and catalog Models at `GET /v1/models` and
  `GET /v1/models/{id}`, in OpenAI or Anthropic format.
- Rejects requests with HTTP 402 when the Budget of the User does not cover the
  worst-case Price of the request (the Reservation).
- Shows requests, tokens, latency, time to first token, Cost, Price and Profit
  per Model, User and Team in the dashboard.
- Exports Prometheus metrics at `/metrics`, optionally with a token.

## Quick start

1. Copy the example configuration and check the rates against the current
   price lists of the Providers:

   ```sh
   cp config.example.toml config.toml
   ```

2. Set the Provider Keys and the dashboard password in the environment:

   ```sh
   export OPENAI_API_KEY=...
   export ANTHROPIC_API_KEY=...
   export LLM_ACCOUNTING_ADMIN_PASSWORD=...
   ```

3. Start the Proxy:

   ```sh
   cargo run --release -- --config config.toml
   ```

4. Open `http://127.0.0.1:8787/admin`, log in, and create a Virtual Key for a
   User on the **Keys** page. Copy the key. You cannot see it again.

5. Send a request to the Proxy with the Virtual Key:

   ```sh
   curl http://127.0.0.1:8787/v1/chat/completions \
     -H "Authorization: Bearer sk-llma-..." -H "content-type: application/json" \
     -d '{"model":"gpt-4o","messages":[{"role":"user","content":"Hello"}]}'
   ```

   The request appears on the **Ledger** page.

The Proxy accepts the Virtual Key in `Authorization: Bearer` or in `x-api-key`.
An allowed Route also permits its Fallbacks. Route and router settings come
from TOML; restart the Proxy after changes.
Docker is also available. See [INSTALL.md](INSTALL.md).

## Documentation

- [INSTALL.md](INSTALL.md): install, configure and verify the Proxy. It is
  written for AI coding agents.
- [docs/usage.md](docs/usage.md): the Operator guide. It covers clients, the
  dashboard pages, how amounts are calculated, Budgets, Key Pools, metrics and
  error responses.
- [docs/configuration.md](docs/configuration.md): every setting, every
  environment variable, seeding and Docker.
- [GLOSSARY.md](GLOSSARY.md): the terms of this project.
- [docs/adr/](docs/adr/): the architecture decisions.

## Limits

- The Proxy does not record usage for audio. It forwards these requests and
  records them as Unpriced Entries.
- The Proxy reads the Model name from a JSON request body only. A multipart
  request has no Model name. The Proxy records it as an Unpriced Entry unless
  the response names the Model.
- The Reservation uses an estimate of the input tokens. A request can cost more
  than its Reservation, for example when it refers to files or cached content
  that is not in the request body. Thus, a Budget can be exceeded by a small
  amount.
- Reservations are in memory. Run one Proxy process for each database.
- On the image endpoints, the Proxy treats cached tokens as text tokens. Cached
  image input tokens are recorded as image input, not as cache read.
- `/metrics` has no authentication when `metrics_token` is not set.

## Development

```sh
cargo test
cargo clippy --all-targets
```

The code needs Rust 1.88 or newer (edition 2024).
