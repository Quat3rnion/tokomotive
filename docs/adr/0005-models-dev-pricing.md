# Use models.dev to update unpinned Cost Cards

The Operator needs current model limits and Provider rates without manual entry.
The owner chose daily models.dev Sync and a manual refresh in Settings. The first
start uses a compressed snapshot in the binary. SQLite stores parsed Providers,
Models and Cost Cards. The Proxy queries this data when needed.

Sync creates a Card Version only when mapped rates change. The source records
`models.dev`, the date and the ETag. Provider matching uses an explicit override,
then the Provider preset, then its API URL. Model matching uses an exact ID,
then a canonical ID, then the longest dated prefix. The Operator can override
the Provider and Model on the Model page. A Pinned Card is never changed by sync.
Subscription Providers keep Cost zero. Old Ledger Entries keep their frozen
amounts, as required by [ADR 0002](0002-freeze-amounts-on-ledger-entries.md).

Input, output and cache read map to the same Token Classes. Reasoning uses the
output Token Class. Cache write maps to 5 minutes. For 1 hour, use an explicit
`cache_write_1h` when present. Otherwise use twice the input rate for the
Anthropic format, or the cache write rate for other formats. Context tiers and
`context_over_200k` become Context Tiers. Missing tier rates use base rates.

Upstream errors can change billing. Plan Models can be listed at zero or at
retail rates that do not match the Operator's plan. A valid price is not proof
of the Operator's contract. Pin these cards and check the Provider invoice.
Sync refuses invalid data, negative prices and a loss of more than 20% of Models.
A change above 50% adds a flag to the Overview with old and new rates and a link.
The Operator can turn sync off or use another data URL. Fetches have a 16 MB
limit and do not follow redirects. Failed checks keep the last accepted data.

For requests with no output maximum, the Reservation uses the mapped output
limit. The configured fallback applies only when no mapped limit exists. This
can reserve more of a Budget than the old fixed fallback. It prevents a request
from spending beyond its Reservation when the Model can return more tokens.
