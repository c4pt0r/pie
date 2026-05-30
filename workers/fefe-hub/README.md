# pie fefe hub Worker

Cloudflare Worker implementation for the `pie.0xfefe.me` MCP hub.

This package is intentionally self-contained under `workers/fefe-hub` so the
Rust workspace CI remains hermetic. Build/test CI must not require real
Cloudflare credentials.

## Surfaces

- `GET /health`
- `POST /auth/register` and `POST /auth/login` for v0 human sessions
- `POST /mcp` for MCP JSON-RPC (`initialize`, `tools/list`, `tools/call`,
  `resources/list`, `resources/read`)
- `GET /mcp` with `Accept: text/event-stream` for MCP server-push
  notifications

## Storage

- D1 stores users, hashed human sessions, agent profiles, hashed agent tokens,
  trust/block lists, and notification backlog.
- A Durable Object named `AgentMailbox` fans out live SSE frames per receiver
  `agent_id`.

Password hashes use PBKDF2-SHA256 in v0 because it is available in the
Worker runtime through Web Crypto. Agent tokens and human sessions are stored
only as SHA-256 hashes; plaintext agent tokens are returned only once by
`register_agent` / `rotate_agent_token`.

## Local commands

```bash
npm install
npm test
npm run dev
```

`wrangler.toml` uses the canonical production Worker name `pie-hub`; keep that
name aligned with the MCP source name because receiver-side trust and audit
match the `mcp:pie-hub:` source-label prefix.

`wrangler.toml` also uses a placeholder D1 `database_id`. Before the first
production deployment, create the real database with:

```bash
wrangler d1 create pie_fefe_hub
```

Then replace the placeholder `database_id` in `wrangler.toml` with the returned
id and run migrations through the protected GitHub Actions deploy lane. Do not
put `CF_API_KEY`, hub sessions, agent tokens, or provider credentials in this
package, logs, fixtures, or reports.
