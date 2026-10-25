# Glossary

**Proxy** — The service that clients call instead of the Provider. It forwards each request and records its usage.

**Provider** — An upstream LLM API vendor (for example OpenAI or Anthropic) that serves the request and charges the operator.

**Model** — A named LLM offered by a Provider. Each Model has one Cost Card and one Price Card.

**Token Class** — One billable category of tokens: input, output, cache read, cache write (5 minute), cache write (1 hour).

**Cost Card** — The rates per million tokens, per Token Class, that the Provider charges the operator for a Model.

**Price Card** — The rates per million tokens, per Token Class, that the operator charges a User for a Model.

**Markup** — A percentage added to a Cost Card to make a Price Card, when the Price Card has no explicit rates.

**Card Version** — One fixed state of a Cost Card or Price Card. A change to a card makes a new Card Version.

**Context Tier** — A band of input size (for example above 200k input tokens) that selects a different set of rates on a Cost Card or Price Card.

**Cost** — The amount the operator owes the Provider for one request, from the Cost Card.

**Price** — The amount a User owes the operator for one request, from the Price Card.

**Profit** — Price minus Cost.

**User** — The holder of one or more Virtual Keys. The unit of usage tracking.

**Team** — A group of Users that is billed together.

**Virtual Key** — An API key that the Proxy issues to a User. It is not a Provider key.

**Provider Key** — The secret credential that the Proxy uses to call a Provider. Users never see it.

**Ledger Entry** — The record of one request: User, Model, token counts per Token Class, Cost, Price.

A Ledger Entry does not change after it is written.

**Unpriced Entry** — A Ledger Entry for a Model that has no card. Its Cost and Price are zero until an operator acts.

**Estimated Entry** — A Ledger Entry whose token counts are partly estimated, because the Provider did not report full usage.

**Budget** — A limit on the total Price for a User, optionally reset each month. When the User reaches it, the Proxy rejects new requests.

**Operator** — The person who runs the Proxy, sets cards and Budgets, and uses the dashboard.
