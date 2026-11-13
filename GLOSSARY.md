# Glossary

**Proxy** - The service that clients call instead of the Provider. It routes and forwards each request and records its usage.

**Simple Mode** - A mode that routes requests and records token metrics without Budget checks, Reservations or Rate Limits. The dashboard hides financial data and controls.

**Full Mode** - The default mode. It includes pricing, Budget checks, Reservations and Rate Limits.

**Provider** - An upstream LLM API vendor (for example OpenAI or Anthropic) that serves the request and charges the Operator. A Provider has a name, a kind (`openai` or `anthropic`) and a base URL.

**Model** - A named LLM offered by a Provider. A Model can have a Cost Card and a Price Card.

**Route** - A model name that the client sends, with an ordered set of Deployments. An explicit Route can override a catalog Model. Otherwise a catalog Model has one implicit Deployment.

**Deployment** - A Provider and an upstream model name that can serve a Route. It also has a weight for the `weighted` strategy.

**Model rewrite** - The replacement of the model name in the request with the upstream model name of the Deployment, or with the name after the `provider/` prefix.

**Translation** - Conversion between the Chat Completions and Messages API formats so that a client can use a Provider with a different API format.

**Fallback** - A Route or catalog Model tried after the primary Route's Deployments fail. A Virtual Key that allows the primary Route also allows its Fallbacks. Fallbacks do not expand recursively.

**Circuit Breaker** - The in-memory health state for a Provider and upstream model. After `failure_threshold` connection errors, 429 or 5xx responses in a row, it opens and starts a cooldown (`cooldown_secs`). After the cooldown, one probe request tests recovery. If every candidate is open, the Proxy can try them anyway.

**Token Class** - One billable category of tokens: input, output, cache read, cache write (5 minute), cache write (1 hour), image input, image output.

**Cost Card** - The rates per million tokens, per Token Class, that the Provider charges the Operator for a Model.

**Price Card** - The rates per million tokens, per Token Class, that the Operator charges a User for a Model.

**Markup** - A percentage added to a Cost Card to make a Price Card, when the Price Card has no explicit rates.

**Card Version** - One fixed state of a Cost Card or Price Card. A change to a card makes a new Card Version.

**Context Tier** - A band of input size (for example above 200k input tokens) that selects a different set of rates on a Cost Card or Price Card.

**Cost** - The amount the Operator owes the Provider for one request, from the Cost Card.

**Price** - The amount a User owes the Operator for one request, from the Price Card.

**Profit** - Price minus Cost.

**User** - The holder of one or more Virtual Keys. The unit of usage tracking.

**Team** - A group of Users that is billed together.

**Virtual Key** - An API key that the Proxy issues to a User. It is not a Provider key.

**Provider Key** - The secret credential that the Proxy uses to call a Provider. Users never see it.

**Master Key** - The secret that protects stored Provider Keys and the metrics token. It comes from `LLM_ACCOUNTING_SECRET_KEY` or from the generated `secret.key` file. Back it up with the database.

**Setup** - The first-use step in which the Operator sets the dashboard password and selects the mode. It runs once, on `/setup`.

**Seed file** - An optional TOML file that fills an empty database once. It is not the source of truth. The dashboard is.

**Key Pool** - The set of Provider Keys for one Provider. The Proxy uses them in turn, and tries the next key when the Provider refuses a request.

**Ledger Entry** - The record of one request: User, Model, token counts per Token Class, Cost, Price. A Ledger Entry does not change after it is written.

**Unpriced Entry** - A Ledger Entry for a Model that has no card. Its Cost and Price are zero until an Operator acts.

**Estimated Entry** - A Ledger Entry whose token counts are partly estimated, because the Provider did not report full usage.

**Budget** - A limit on the total Price for a User, optionally reset each month. When the User reaches it, the Proxy rejects new requests.

**Reservation** - The worst-case Price of a request in progress. It counts against the Budget until the Ledger Entry for the request is written.

**Rate Limit** - A limit on requests per minute (RPM) or completed tokens per minute (TPM) for a Virtual Key or a User. It uses the last 60 seconds. A blank limit is unlimited; zero blocks requests.

**Operator** - The person who runs the Proxy, sets cards and Budgets, and uses the dashboard.
