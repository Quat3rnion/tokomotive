# Provider roster

This document lists the Providers to add to Tokomotive. It groups them by the
work that each one needs. Research date: 2026-11-13. Group status is below. Facts come from the
official documentation or from public source code at a fixed commit. "Not
verified" means that the research did not confirm the fact. Do a check before
you implement an item that is not verified.

Tokomotive supports these Provider settings today:

- API formats: OpenAI (`/v1/chat/completions`, `/v1/responses`,
  `/v1/embeddings`, `/v1/images`) and Anthropic (`/v1/messages`).
- Authentication: API key, ChatGPT token sharing, Claude Pro/Max, ChatGPT
  via Codex, OpenRouter key sign-in, Hugging Face OAuth and Azure Entra ID.
  Google Vertex sign-in stores and refreshes a token; its adapter is pending.

## Rule for sign-in

Every new Provider that offers OAuth or a device-code sign-in gets that
sign-in. The sign-in is in addition to API keys, not a replacement.

- **Official**: the provider documents the flow for third-party apps. Add it
  without an ADR.
- **Unofficial**: the flow is copied from a first-party client. Each one needs
  an ADR that records the owner decision and the risk, as ADR 0003 and ADR
  0004 do. The risk is that the route can change without notice and can break
  the provider's terms.

## Group A: OpenAI-compatible presets

Status: **implemented**, commit `b24aadd`.

These Providers use the `openai` API format. Verified base URLs have editable
presets with key help and documentation links. Unverified base URLs are not
presets; `src/presets.rs` lists them as TODO items. Usage tests use doc-derived
format fixtures, not live account responses. Unverified usage rows below stay
unverified. The shared reader and the per-Provider `include_usage` switch are
implemented. See [Provider presets](usage.md#provider-presets).

| Provider | Base URL | Usage change | Sources |
| --- | --- | --- | --- |
| DeepSeek | `https://api.deepseek.com` | Read `prompt_cache_hit_tokens` when `prompt_tokens_details.cached_tokens` is absent. Streams send usage without `include_usage`. | [Chat API](https://api-docs.deepseek.com/api/create-chat-completion/) |
| Moonshot Kimi | `https://api.moonshot.ai/v1` (China URL not verified) | Streams need `include_usage`. Usage can also appear per choice. | [API](https://platform.moonshot.ai/docs/api/overview) |
| Z.ai (GLM) | `https://api.z.ai/api/paas/v4`; Coding Plan `https://api.z.ai/api/coding/paas/v4`; China `https://open.bigmodel.cn/api/paas/v4` (not verified) | Cached tokens in `prompt_tokens_details.cached_tokens`. Coding Plan keeps thinking on by default. | [Coding Plan](https://docs.z.ai/devpack/quick-start), [cache](https://docs.z.ai/guides/capabilities/cache) |
| OpenRouter | `https://openrouter.ai/api/v1` | Streams always send usage; `include_usage` has no effect. Read `usage.cost` and keep it for comparison with the Cost Card. Pass the `provider` routing object through. | [Usage](https://openrouter.ai/docs/cookbook/administration/usage-accounting), [routing](https://openrouter.ai/docs/guides/routing/provider-selection) |
| xAI | `https://api.x.ai/v1` | Streams need `include_usage`. Optional: read `cost_in_usd_ticks`. | [Cost](https://docs.x.ai/developers/cost-tracking) |
| Mistral | `https://api.mistral.ai/v1` | Streams need `include_usage`. | [Limitations](https://docs.mistral.ai/resources/known-limitations) |
| Together AI | not verified | Read `cached_tokens` at the top of `usage` and in `prompt_tokens_details`. | [Compatibility](https://docs.together.ai/docs/inference/openai-compatibility) |
| Fireworks | not verified | Streams send usage in the last chunk by default. | [Compatibility](https://docs.fireworks.ai/tools-sdks/openai-compatibility) |
| MiniMax | not verified | Responses usage in `input_tokens_details` / `output_tokens_details`. Chat returns `reasoning_content`. | [Chat](https://platform.minimax.io/docs/api-reference/text-chat-openai) |
| DeepInfra | `https://api.deepinfra.com/v1/openai` | Not verified. | [Chat](https://docs.deepinfra.com/chat/overview) |
| Nebius Token Factory | `https://api.tokenfactory.nebius.com/v1` | Not verified. | [Quick start](https://docs.studio.nebius.com/quickstart) |
| Cerebras | `https://api.cerebras.ai/v1` | Not verified. | [Docs](https://inference-docs.cerebras.ai/) |
| Groq, Perplexity, SiliconFlow, Hyperbolic, NVIDIA NIM, Volcengine Ark | not verified | Not verified. Volcengine model units do not support Responses. | See research notes |
| Alibaba DashScope | `https://dashscope.aliyuncs.com/compatible-mode/v1` (from Qwen Code source); Coding Plan `https://coding-intl.dashscope.aliyuncs.com/v1` | Responses cache in `input_tokens_details.cached_tokens`. Session cache needs `x-dashscope-session-cache: enable`. | [Responses](https://help.aliyun.com/en/model-studio/qwen-api-via-openai-responses) |
| Azure OpenAI / Foundry (v1) | `https://<resource>.openai.azure.com/openai/v1` | The v1 base needs no `api-version`. Header `api-key` or an Entra bearer token. | [v1 API](https://learn.microsoft.com/azure/foundry/openai/api-version-lifecycle) |
| AWS Bedrock (OpenAI API) | Bedrock OpenAI-compatible endpoint, with a Bedrock API key | Bearer API key. SigV4 is not necessary on this route. | [API keys](https://docs.aws.amazon.com/bedrock/latest/userguide/api-keys.html), [Chat](https://docs.aws.amazon.com/bedrock/latest/userguide/inference-chat-completions.html) |
| Hugging Face router | `https://router.huggingface.co/v1` | Chat and Responses only; no embeddings on this endpoint. | [Inference Providers](https://huggingface.co/docs/inference-providers/index) |
| Cloudflare Workers AI | not verified | Responses only for two GPT-OSS models, without streaming. | [Compatibility](https://developers.cloudflare.com/workers-ai/configuration/open-ai-compatibility) |
| Local: Ollama, LM Studio, vLLM, llama.cpp, LocalAI | user-supplied | Unpriced by default. Usage fields vary by server version. | - |

Implemented common work for group A:

1. A "preset" list on the Add Provider form that fills the base URL and API
   format.
2. One usage reader that accepts all the field locations above. Add a
   unit test for each Provider with a doc-derived format fixture. These do not
   verify live support where the table says "Not verified".
3. A per-Provider flag "send `stream_options.include_usage`", on by default,
   off for Providers that reject it.

## Group B: Anthropic-compatible endpoints

Status: **implemented**, commit `b24aadd`.

These Providers accept `/v1/messages`. They work with the existing
`anthropic` API format. A Route can use them with Translation for OpenAI
clients.

| Provider | Base URL | Notes | Sources |
| --- | --- | --- | --- |
| Moonshot Kimi | `https://api.moonshot.ai/anthropic` | Tools, streaming, thinking, prompt caching with 5m and 1h TTL. Usage fields match Anthropic. | [Messages](https://platform.moonshot.ai/docs/api/messages) |
| DeepSeek | `https://api.deepseek.com/anthropic` | Tools, streaming, thinking. `cache_control` is ignored; cache writes are 0. `anthropic-version` is ignored. | [Guide](https://api-docs.deepseek.com/guides/anthropic_api) |
| Z.ai | `https://api.z.ai/api/anthropic`; China `https://open.bigmodel.cn/api/anthropic` | Bearer key. Feature support not verified. | [Guide](https://docs.z.ai/guides/anthropic) |
| MiniMax | `https://api.minimax.io/anthropic` | Tools, streaming, thinking. Caching not listed. | [Guide](https://platform.minimax.io/docs/api-reference/text-anthropic-api) |
| OpenRouter | `https://openrouter.ai/api/v1/messages` | Support depends on the upstream model. Cache usage can be null. | [Reference](https://openrouter.ai/docs/api/api-reference/anthropic-messages/create-a-message) |
| Volcengine Ark | `https://ark.cn-beijing.volces.com/api/compatible/v1/messages` | Not verified beyond URL and auth. | [Ark](https://www.volcengine.com/docs/ark/agent-model-invocation-practice) |

Implemented work for group B: a per-Provider key-header setting selects
`Authorization: Bearer`, `x-api-key` or Azure `api-key`. Anthropic defaults to
`x-api-key`. Presets select the header for each endpoint. Missing or null
cache usage is accepted. Endpoint and model support marked "Not verified"
above still needs a live account check.

## Group C: dedicated adapters

| Provider | Work | Sources |
| --- | --- | --- |
| Google Gemini (native) | **Implemented.** `generateContent` / `streamGenerateContent`, header `x-goog-api-key`. Map `usageMetadata` (`cachedContentTokenCount`, `thoughtsTokenCount`). Translation from Chat and Messages, JSON and SSE. Native preset and model list. Mock tests pass; live account support is not verified. | [API](https://ai.google.dev/api), [generateContent](https://ai.google.dev/api/generate-content) |
| Google Vertex AI | Gemini on Vertex and Anthropic on Vertex (`rawPredict`). Auth with a Google OAuth access token from a service account or from user sign-in. | Not fully verified |
| AWS Bedrock (native) | Converse / ConverseStream with SigV4, for accounts that do not use Bedrock API keys. Usage in `usage.inputTokens` / `outputTokens`. | [Converse](https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_Converse.html) |
| Cohere | `/v2/chat`, usage in `meta.billed_units`. | [Chat](https://docs.cohere.com/v2/docs/chat-api) |

Do group C after groups A and B. Bedrock API keys and Azure v1 already cover
the most common use of Bedrock and Azure.

## Group D: OAuth and device-code sign-in

| Provider | Flow | Details | Status |
| --- | --- | --- | --- |
| OpenRouter | PKCE, issues an API key | Open `https://openrouter.ai/auth` with a PKCE challenge and `S256`, without `callback_url`. Paste the displayed code. Exchange with `POST https://openrouter.ai/api/v1/auth/keys` `{code, code_verifier, code_challenge_method}`; the response has `key`. The encrypted result is a normal Provider Key, with normal Cost and no refresh. | Official; implemented in `95fd048` ([docs](https://openrouter.ai/docs/guides/overview/auth/oauth)) |
| Hugging Face | Device code and PKCE | `POST https://huggingface.co/oauth/device`, then `POST https://huggingface.co/oauth/token` with `grant_type=urn:ietf:params:oauth:grant-type:device_code`. Scope `inference-api`, with refresh. The Operator registers an app or uses the public `/.well-known/oauth-cimd` URL as client ID. CIMD uses PKCE paste; device PKCE with CIMD is not verified. Requests go to `https://router.huggingface.co/v1` with a bearer and normal Cost. | Official; implemented in `95fd048` ([docs](https://huggingface.co/docs/hub/oauth)) |
| Azure (Entra ID) | Device code, PKCE paste fallback | `POST https://login.microsoftonline.com/{tenant}/oauth2/v2.0/devicecode`, then `/token`. The Operator supplies tenant, client ID and scope. The default is `https://cognitiveservices.azure.com/.default offline_access`; `offline_access` gives a refresh token. The Azure v1 resource gets a bearer, with normal Cost. | Official; implemented in `95fd048` ([docs](https://learn.microsoft.com/en-us/entra/identity-platform/v2-oauth2-device-code), [scope](https://learn.microsoft.com/en-us/azure/developer/ai/how-to/switching-endpoints)) |
| Google (Vertex) | Google OAuth, PKCE paste | Scope `https://www.googleapis.com/auth/cloud-platform`, with the Operator's own Desktop app client ID and secret. Stores and refreshes the token. The UI says sign-in ready, adapter pending; routing remains disabled until group C. | Official; sign-in implemented in `95fd048`, adapter pending ([docs](https://developers.google.com/identity/protocols/oauth2/native-app)) |
| GitHub Copilot | Device code, then account discovery | Device code at `https://github.com/login/device/code`, token at `https://github.com/login/oauth/access_token`. Client ID `Ov23li8tweQw6odWQebz` (opencode), scope `read:user`. Then `GET https://api.github.com/copilot_internal/user` for access and `endpoints.api`. The OAuth bearer goes directly to the discovered API, default `https://api.githubcopilot.com`: `/chat/completions`, `/responses` for GPT models, `/v1/messages` for Claude models. Headers include Tokomotive's own User-Agent, `Openai-Intent`, `X-GitHub-Api-Version`, `x-initiator`, interaction identity and optional vision attribution. The pinned opencode source sends no editor-version or copilot-integration-id. Enterprise discovery uses `api.<domain>`. The alternative copilot-api client `Iv1.b507a08c87ecfe98` uses `/copilot_internal/v2/token` and editor headers; Tokomotive does not use that adapter. | Unofficial; implemented in `08d2496`, [ADR 0006](adr/0006-github-copilot.md) ([opencode github-copilot.ts @83960f8](https://github.com/sst/opencode/blob/83960f8a56b2f2b8ad0b751f23baaf9d0eb65ad9/packages/core/src/plugin/provider/github-copilot.ts), [copilot-api api-config.ts @0ea08fe](https://github.com/ericc-ch/copilot-api/blob/0ea08febdd7e3e055b03dd298bf57e669500b5c1/src/lib/api-config.ts)) |
| Qwen Code | Device code with PKCE | Device code `https://chat.qwen.ai/api/v1/oauth2/device/code`, token `https://chat.qwen.ai/api/v1/oauth2/token`. Client ID `f0304373b74a44d2b584a3fb70ca9e56`, scope `openid profile email model.completion`. The token response can carry `resource_url`; requests go to `https://<resource_url>/v1` after the trusted-host check, otherwise `https://dashscope.aliyuncs.com/compatible-mode/v1` (OpenAI format), with `X-DashScope-AuthType: qwen-oauth` and Tokomotive's own `X-DashScope-UserAgent`. Refresh rotates the refresh token. | Unofficial; implemented in `08d2496`, [ADR 0007](adr/0007-qwen-code.md) ([qwenOAuth2.ts @6f754be](https://github.com/QwenLM/qwen-code/blob/6f754be6548903e545e6ff0ed43a34927fe3bfb9/packages/core/src/qwen/qwenOAuth2.ts), [qwenContentGenerator.ts](https://github.com/QwenLM/qwen-code/blob/6f754be6548903e545e6ff0ed43a34927fe3bfb9/packages/core/src/qwen/qwenContentGenerator.ts)) |
| Kimi Code | Device code | Host `https://auth.kimi.com`: `POST /api/oauth/device_authorization`, then `POST /api/oauth/token` (device-code grant; refresh grant rotates the refresh token). Client ID `17e5f671-d194-4dfb-9706-5516cb48c098`. Requests go to `https://api.kimi.com/coding/v1` with `X-Msh-*` headers, platform `tokomotive` and an encrypted stable device ID. The server lists the protocol per model (`kimi`, `anthropic` or `openai_responses`). Default to OpenAI format; the Provider can select Anthropic with beta Messages. The source's global `.ai` region is not selected. | Unofficial; implemented in `08d2496`, [ADR 0008](adr/0008-kimi-code.md) ([constants.ts @f082f04](https://github.com/MoonshotAI/kimi-code/blob/f082f04a093a3e71a2263c5461d52507dc31f83a/packages/oauth/src/constants.ts), [oauth.ts](https://github.com/MoonshotAI/kimi-code/blob/f082f04a093a3e71a2263c5461d52507dc31f83a/packages/oauth/src/oauth.ts), [identity.ts](https://github.com/MoonshotAI/kimi-code/blob/f082f04a093a3e71a2263c5461d52507dc31f83a/packages/oauth/src/identity.ts)) |
| Gemini CLI (Code Assist) | Google OAuth, paste mode | Client ID `681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com` with the installed-app client secret that the CLI publishes. Paste redirect `https://codeassist.google.com/authcode`. Requests go to `https://cloudcode-pa.googleapis.com/v1internal:generateContent` and `:streamGenerateContent` after `loadCodeAssist` and `onboardUser`. Needs the Gemini adapter from group C. | Unofficial ([oauth2.ts @4ed06ed](https://github.com/google-gemini/gemini-cli/blob/4ed06ed0ddc523a6d356a4ad35ddfea331041888/packages/core/src/code_assist/oauth2.ts), [server.ts](https://github.com/google-gemini/gemini-cli/blob/4ed06ed0ddc523a6d356a4ad35ddfea331041888/packages/core/src/code_assist/server.ts)) |
| Z.ai Coding Plan, Alibaba Coding Plan, xAI, Mistral, MiniMax, Cloudflare, GitHub Models | API key only | No OAuth found. | - |

Common work for group D:

1. A generic sign-in engine with three flow types: device code (RFC 8628),
   PKCE with redirect paste, and PKCE that issues an API key. The existing
   Codex and Claude code becomes two configurations of this engine.
2. Per-flow settings: endpoints, client ID, scopes, extra headers, how to
   find the inference base URL after sign-in, and refresh rules.
3. Post-sign-in discovery for Copilot (OAuth bearer to Copilot access and API
   endpoint). The selected opencode flow needs no short-lived token exchange.
4. The same protections as today: encrypted storage, single-flight refresh,
   rotated refresh tokens saved before use, the model-path allowlist,
   explicit Provider Access, Cost 0 for subscriptions, and no redirects.

## Suggested order

1. Group A presets and the shared usage reader. These have the most value for
   the least work.
2. Group B key-header setting.
3. Group D sign-in engine with the official flows: OpenRouter, Hugging Face,
   Entra.
4. Group D unofficial flows, each with an ADR: Copilot, Qwen Code, Kimi Code.
5. Group C adapters: Gemini native, then Gemini CLI sign-in, Vertex, Bedrock
   native, Cohere.
