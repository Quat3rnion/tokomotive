# Operator guide

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

| Path ends with                             | Native Provider kind | Usage that the Proxy records                  |
| ------------------------------------------ | ------------- | --------------------------------------------- |
| `/chat/completions`                        | `openai`      | Input, output, cache read                     |
| `/responses`                               | `openai`      | Input, output, cache read                     |
| `/embeddings`                              | `openai`      | Input                                         |
| `/images/generations`, `/images/edits`     | `openai`      | Input, cache read, image input, image output  |
| `/messages`                                | `anthropic`   | Input, output, cache read, cache write 5 minutes, cache write 1 hour |

The Proxy forwards every path under `/v1/` to the Provider, for all HTTP
methods. The Proxy does not read usage for other paths. It records them as
Ledger Entries with the endpoint `other`. If the Proxy cannot find a Model for
the request, the Ledger Entry is an Unpriced Entry.

The Proxy reads the Model name from the JSON body of the request. A request with
a body that is not JSON, for example a multipart upload, has no Model name.
The Proxy then records an Unpriced Entry unless the response body names the
Model. This applies, for example, to multipart requests to `/v1/images/edits`.

### Routing and Translation

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

The Ledger Entry uses the client's endpoint and the upstream Model and usage.
Cost and Price use upstream Token Classes, before Translation. The Proxy
continues to read upstream streams after a client disconnect to obtain final
usage.

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

For streamed Chat Completions, the Proxy sets `stream_options.include_usage` to
get the usage from the Provider. If your request did not ask for usage, the
Proxy removes the usage chunk from the stream that you receive.
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
| `/admin/login`                          | GET, POST | Login page and login form (field `password`).                    |
| `/admin/logout`                         | POST     | Ends the session.                                                 |
| `/admin/users`                          | GET, POST | Users page. POST adds a User.                                    |
| `/admin/users/{id}`                     | POST     | Changes the Team, the Budget and the status of a User.            |
| `/admin/teams`                          | GET, POST | Teams page. POST adds a Team.                                    |
| `/admin/keys`                           | GET, POST | Keys page. POST creates a Virtual Key.                           |
| `/admin/keys/{id}/limits`               | POST     | Changes the Rate Limits of a Virtual Key.                        |
| `/admin/keys/{id}/revoke`               | POST     | Revokes a Virtual Key.                                            |
| `/admin/models`                         | GET, POST | Models page. POST adds a Model.                                  |
| `/admin/models/{id}`                    | GET, POST | Page of one Model. POST changes the Provider and the aliases.    |
| `/admin/models/{id}/cost`               | POST     | Saves a new Cost Card version.                                    |
| `/admin/models/{id}/price`              | POST     | Saves a new Price Card version.                                   |
| `/admin/models/{id}/team-price`         | POST     | Sets or removes a Team override.                                  |
| `/setup` | GET, POST | Sets the first admin password and the mode; refuses later setup requests. |
| `/admin/providers` | GET, POST | Lists or creates Providers. |
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
| `/admin/settings/markup` | POST | Saves the default Markup from the Models page. |
| `/admin/ledger`                         | GET      | Ledger page. The query `?filter=` is `unpriced`, `estimated` or `errors`. |
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

### Providers, Routes and Settings

The header has **Providers**, **Routes** and **Settings**. These pages apply
changes without a restart. The settings are in
[configuration.md](configuration.md).

**Providers**:

1. To add a Provider, enter a name, a kind, a base URL and optionally a first
   key. Select **Add provider**.
2. To add a key, enter the key in **New key or env:NAME** and select **Add key**.
   The Proxy encrypts the key. The page shows only its last 4 characters. Enter
   `env:NAME` to store a reference to an environment variable instead.
3. To remove a key, select **Remove key** in its row.
4. To check a Provider, select **Test**. The page shows the upstream HTTP status.
5. To remove a Provider, select **Delete provider**. A Route or a Model that uses
   the Provider blocks this.

**Routes**:

1. To add a Route, enter a name, select a strategy and enter Fallbacks as a comma
   list.
2. Select **Add deployment** for each Deployment. Each row has a Provider
   (select), a Model and a weight. Select **Remove** to remove a row. Only the `weighted` strategy uses the weight.
3. Select **Add route** or **Save route**. Select **Delete route** to remove it.

**Settings** has the server and router values, the mode, the metrics token and
the password. To change the password, enter the current password and a new
password of at least 8 characters. All sessions end. Back up `secret.key` with
the database. See "Master Key and backups" in
[configuration.md](configuration.md#master-key-and-backups).

### Simple mode

Select **Simple** on `/setup` or in **Settings** to use the Proxy as a router
without prices or limits. The default is **Full**. A change applies to new
requests without a restart.

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

The margin is Profit divided by Price. The page shows `-` when the Price is 0.

The table **By model** uses the name of the matched Model. A dated name such as
`gpt-4o-2024-08-06` counts as `gpt-4o`.

### Users and Budgets

The **Users** page lists the Users. Each row has these fields:

- **Team**: the Team of the User, or `(no team)`.
- **Budget (USD)**: the limit on the total Price for the User. Leave it empty
  for no Budget.
- **monthly**: if selected, the Budget starts again on the first day of each
  month (UTC). If not, the Budget is for all time.
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

1. Select the User.
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
rates, the number of Context Tiers, the Price Card and the number of Team
overrides. All rates are USD per million tokens. A Model with no Cost Card shows
the tag **no cost card**.

Tasks on the list page:

- **Default markup**: enter the Markup in percent for Models that have no Price
  Card and select **Save**. The change applies to new requests only.
- **Add model**: enter the name, the Provider, the aliases (comma list), the Cost
  Card rates and the Context Tiers. Then select **Add model**. The Provider list
   holds the Providers saved in the dashboard.

Select a Model name to open its page. The page has these parts:

- **Model**: change the Provider and the aliases.
- **Cost Card**: change the rates and Context Tiers. Select **Save new Cost Card
  version**.
- **Price Card**: select a mode and select **Save new Price Card version**.
- **Team price overrides**: set or remove a Price Card for one Team.
- **Card history**: all Card Versions of this Model.

Each save of a card makes a new Card Version. The old Card Versions stay. Old
Ledger Entries keep the amounts that they had.

Price Card modes:

| Mode                    | Result                                                           |
| ----------------------- | ---------------------------------------------------------------- |
| Default markup          | The Model has no Price Card. The default Markup applies.         |
| Markup                  | Price Card = Cost Card + the Markup that you enter.              |
| Explicit rates          | The rates in the fields are the Price Card.                      |

Team overrides:

1. Select the Team.
2. Select **Markup** and enter a percentage, or select **Explicit rates**.
3. Select **Set Team override**.

An override replaces the Model Price Card for the Users of that Team. To remove
an override, select **Remove** in its row. A new save for the same Team makes a
new Card Version.

Context Tier line format. Enter one tier on each line in the **Context Tiers**
field. Separate the values with spaces:

```
above_input_tokens input output cache_read cache_write_5m cache_write_1h [image_input image_output]
```

Example, with the rates in USD per million tokens:

```
200000 6 22.5 0.6 7.5 12
```

Rules for tiers:

- The first value is the number of input tokens. The tier applies when the input
  is more than this number.
- The input tokens include cache tokens and image input tokens.
- A missing value counts as 0. A tier replaces all base rates, so repeat each
  rate that the tier needs.
- A rate is a number of dollars with up to 6 decimal places. A leading `$` is
  allowed. An empty field counts as 0.

The **Card history** table shows the version number (`#id`), the scope (`model`
or the name of a Team), the kind (`cost` or `price`), the rates and the creation
time.

### Ledger

The **Ledger** page shows the 200 newest Ledger Entries. Use the tabs to filter
them:

| Tab           | Shows                                              |
| ------------- | -------------------------------------------------- |
| **All**       | All entries.                                       |
| **Unpriced**  | Unpriced Entries.                                  |
| **Estimated** | Estimated Entries.                                 |
| **Errors**    | Entries with an HTTP status of 400 or more.        |

The columns are the number, the time, the User, the Provider, the endpoint, the
Model, the status, the tokens for each Token Class, the Price, the Cost, the
latency, the time to first token, the Provider Key and the flags.

- **Provider key** shows the number of the key in the Key Pool, counted from 0,
  for example `#1`. After a retry it also shows the attempts, for example
  `#1 (2 tries)`.
- The flags are **stream**, **unpriced**, **estimated** and **client aborted**.

An Estimated Entry has token counts that are partly estimated. The Proxy makes an
Estimated Entry when a successful response has no usage, or when a stream ends
before the Provider sends the final usage.

To price Unpriced Entries:

1. Add a Model with a Cost Card for the name in the entry (see "Models"). An
   alias also works.
2. Open **Ledger** and select **Unpriced**.
3. Select **Price unpriced entries**.

The page shows `Priced N entries.` The Proxy prices only the Unpriced Entries
whose Model now has a Cost Card. It uses the current cards. It does not change
any other Ledger Entry.

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

A Budget is a limit on the total Price for a User. The period is all time, or the
current calendar month (UTC) if **monthly** is selected for the User. A User with
no Budget has no limit and no Reservations.

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
- The Price so far, plus the open Reservations of the User, plus the new
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

## Subscription Providers

Read [ADR 0003](adr/0003-subscription-providers.md) before you connect an
account. Claude OAuth impersonates Claude Code and can cause account
restriction. Codex plan usage uses a private route that can stop without notice.

### Sign in

1. Log in to `/admin`. Open **Providers**.
2. Add a Provider. For ChatGPT, select kind **OpenAI**, authentication
   **ChatGPT token sharing**, and base URL `https://api.openai.com`. For Claude,
   select kind **Anthropic**, authentication **Claude Pro/Max**, and base URL
   `https://api.anthropic.com`. Leave the key field empty.
3. Select **Sign in**, then **Open sign-in page**. The link opens in a new tab.
4. Complete consent. For ChatGPT, copy the final
   `http://localhost:1455/auth/callback?...` URL from the address bar. A browser
   connection error at that address is expected. Tokomotive does not run a
   callback listener on your computer. Do not copy an earlier sign-in URL.
5. For Claude, copy the final `code#state`. Return to the Tokomotive tab. Paste
   the URL or `code#state` in the field and select **Connect**. Complete this
   step within 10 minutes. Start again if the attempt expires or fails.
6. Check **Connected**, the account, the plan, the expiry and the last error.
   Account or plan metadata can show **not available** if the issuer omits it.
7. Open **Keys**. Select this Provider in **Provider Access** for each Virtual
   Key that needs it. Select any API-key Providers that the same key must use.
8. Add Models or Routes that use this Provider. Send the usual client requests.
   Chat Completions and Messages Translation works with subscription auth too.

For optional ChatGPT plan usage, select **Sign in for usage** on the same
Provider. Complete the Codex login in a new tab. Paste the final
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

Select **Disconnect** to remove the model-request tokens. **Disconnect usage
login** removes only the optional Codex tokens. API-key tests do not apply to
Subscription Providers. To change a Provider's authentication type, create a
new Provider.

### Provider Access

Create or edit Provider Access on **Keys** with the Provider checkboxes:

- No selection means all API-key Providers, but no Subscription Providers.
- A selection means only the selected Providers. Include API-key Providers
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
| 402         | `budget_exceeded`      | `the budget for this user does not cover this request`          | The Budget check failed. See "Budgets and Reservations".       |
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
