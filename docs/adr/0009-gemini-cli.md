# ADR 0009: Gemini CLI Code Assist sign-in

Status: accepted. Owner decision: 2026-11-13.

## Decision

The owner approved the unofficial Gemini CLI subscription adapter. It uses
PKCE with a pasted code. It does not use device sign-in. Use can break Google's
service terms. The private routes can change without notice.

The reference is google-gemini/gemini-cli at commit
`4ed06ed0ddc523a6d356a4ad35ddfea331041888`. Files are under
`packages/core/src/code_assist/`:

- `oauth2.ts:76-92`: client ID, public installed-app client secret and scopes.
  The secret is published in the first-party client. It is not an Operator
  secret. It is sent only to the token endpoint, never to HTML or logs.
- `oauth2.ts:443`: paste redirect `https://codeassist.google.com/authcode`.
- `setup.ts:179-307`: `loadCodeAssist`, tier selection, `onboardUser` and
  operation polling. Tokomotive stops after 12 polls, 5 seconds apart. A
  request timeout also bounds each call. An account that does not return a
  project must complete its Google project setup before it can connect.
- `server.ts:415-489,537-550`: POST generation routes and SSE query `alt=sse`.
- `converter.ts:129-160`: `{model, project, request}` and the `response` wrapper.

## Limits

The exact trusted hosts are `accounts.google.com` (consent),
`oauth2.googleapis.com` (tokens), `codeassist.google.com` (paste redirect), and
`cloudcode-pa.googleapis.com` (setup and inference). No wildcard host is trusted.
The inference base is fixed. Operation names cannot change the host or query.
HTTP redirects are not followed.

Each Virtual Key needs explicit Provider Access. Only canonical Chat
Completions and Messages POST paths can use this credential. Cost is 0;
Price follows the Price Card. Credentials are encrypted. Refresh is
single-flight, and a refresh response need not rotate Google's refresh token.
The native Gemini translator handles JSON, SSE and usage. Tests use local
mock endpoints. Live account support is not verified.

Source: <https://github.com/google-gemini/gemini-cli/tree/4ed06ed0ddc523a6d356a4ad35ddfea331041888/packages/core/src/code_assist>.
