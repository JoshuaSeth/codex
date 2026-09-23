# Codex Gateway

Broker-backed gateway for remote `codex-dev` clients.

The gateway exposes OpenAI-compatible Codex Responses endpoints while keeping
ChatGPT `auth.json` tokens and the auth broker on the server:

- `GET /healthz`
- `POST /v1/responses`
- `WS /v1/responses`
- `GET|POST|PUT|PATCH|DELETE|OPTIONS /v1/<upstream-path>`
- `WS /v1/codex-tunnel`

Clients authenticate to the gateway with `Authorization: Bearer <gateway-token>`.
The gateway acquires an auth broker lease, forwards the request to
`https://chatgpt.com/backend-api/codex`, reports the lease outcome, and retries
pre-stream quota/auth failures with another broker account.

`WS /v1/codex-tunnel` accepts:

- `{"type":"ping"}` and returns `{"type":"pong"}`
- `{"type":"response.create", ...}` for the normal Codex Responses stream
- `{"type":"http.request","id":"...","method":"GET","path":"models?client_version=...","headers":{},"body_b64":null}` and returns one `http.response` event with a base64 body

## Environment

```bash
CODEX_GATEWAY_TOKEN=...
CODEX_GATEWAY_AUTH_BROKER_URL=http://127.0.0.1:38188
CODEX_GATEWAY_AUTH_BROKER_TOKEN=...
CODEX_GATEWAY_CHATGPT_CODEX_BASE_URL=https://chatgpt.com/backend-api/codex
CODEX_GATEWAY_BIND=127.0.0.1
CODEX_GATEWAY_PORT=38288
```

If `CODEX_GATEWAY_AUTH_BROKER_TOKEN` is absent, the app reads
`AUTH_TOKEN_SERVER_CLIENT_TOKEN` or `CODEX_AUTH_BROKER_TOKEN` from
`/etc/auth-token-server/auth-token-server.env`.

Do not expose the service without TLS. In production it should sit behind nginx
or another TLS reverse proxy with WebSocket upgrade support.

## Current Deployment

- Server: `37.27.67.52`
- Service: `codex-gateway.service`
- App path: `/opt/codex-gateway`
- Env file: `/etc/codex-gateway/codex-gateway.env`
- Local base URL: `http://127.0.0.1:38288`
- Public base URL: `https://codex-cowork.pitchai.net/gateway`

The gateway token is root-only on the server. Use it as
`Authorization: Bearer <gateway-token>`.
