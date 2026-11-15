# Qwen Code sign-in

Status: accepted by the owner on 2026-11-13.

## Decision

Add `auth = "qwen_code"` as an always-available Subscription Provider. Use
device code with PKCE S256 and the public Qwen Code client. This is an
unofficial integration. The private routes and client registration can change
without notice. Third-party use can break the service terms or cause account
restriction. The owner accepted these risks. An API-key Provider is the
alternative. Local source and mock checks do not establish live permission.

## Protocol

Use form requests to `https://chat.qwen.ai/api/v1/oauth2/device/code` and
`https://chat.qwen.ai/api/v1/oauth2/token`. The client ID is
`f0304373b74a44d2b584a3fb70ca9e56`; the scope is
`openid profile email model.completion`. Keep the PKCE verifier in memory.
The refresh token rotates. Save pending state before refresh and the new token
pair before use. Issue #18 replaces disconnect-before-send with persisted
backoff and one recovery attempt after a lost response. A definite rejection,
invalid response or ambiguous recovery requires sign-in. See
[OAuth refresh](../configuration.md#oauth-refresh).

The token response's `resource_url` selects the inference base. Add HTTPS when
the scheme is absent, remove trailing slashes, and add `/v1` only once. When
the field is absent at sign-in, use
`https://dashscope.aliyuncs.com/compatible-mode/v1`. A refresh without the field
keeps the previous base. A refresh with the field validates and stores it with
the new token pair. A retry reads that new base before sending the new bearer.

Only HTTPS hosts equal to, or below, these trusted domains are accepted:

- `dashscope.aliyuncs.com`, `dashscope-intl.aliyuncs.com`,
  `dashscope-us.aliyuncs.com`;
- `portal.qwen.ai`;
- `maas.aliyuncs.com`, `maas.qianwenaiapi.com`, `maas.qwencloudapi.com`.

The public regional and MaaS domains come from the pinned source's endpoint
checks. That source also detects internal Alibaba gateways, configurable
proxies and shared API Gateway domains. Detection does not establish trust for
an OAuth bearer. Tokomotive deliberately excludes those broad classes.
HTTP, IP literals, foreign domains, user information, custom ports, fragments
and queries are refused. An invalid base prevents credential use.

Inference uses the OpenAI format and only canonical Chat Completions POSTs.
Existing Messages-to-Chat Translation can use this endpoint. Requests send
`X-DashScope-AuthType: qwen-oauth`, `X-DashScope-UserAgent: Tokomotive/0.1.0`,
and Tokomotive's own User-Agent. CacheControl is optional and is not added.
Client DashScope, identity and cookie headers cannot override these values.
Tokomotive does not identify itself as Qwen Code.

Credentials use the existing encrypted SQLite store. Provider Access needs an
explicit grant. Cost is 0; Price uses the Rate Cards. No schema change is needed.

## Sources

Verified against QwenLM/qwen-code commit
`6f754be6548903e545e6ff0ed43a34927fe3bfb9`:

- `packages/core/src/qwen/qwenOAuth2.ts`: client, endpoints, scope, device PKCE
  and refresh;
- `packages/core/src/qwen/qwenContentGenerator.ts`: token-derived resource URL;
- `packages/core/src/core/openaiContentGenerator/provider/dashscope.ts`:
  regional hosts and OAuth headers;
- `packages/core/src/core/dashscope-source-config.ts`: public MaaS hosts.

See the pinned links in [the Provider roster](../provider-roster.md).
Mocks verify polling, PKCE, rotation, encrypted storage, trusted-base rejection,
path limits, identity headers, access and accounting. Live consent, account
entitlement and model support remain unverified.
