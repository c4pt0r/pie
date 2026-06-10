# Public Webhook Endpoints

This document is intentionally archived.

The experimental public webhook relay depended on the removed public cross-agent service. The
client command surface and production service are no longer release targets. Do not implement new
work from the earlier design notes.

Current supported alternatives:

- Use local command hooks for lifecycle events; see [hooks.md](hooks.md).
- Use ordinary MCP servers configured explicitly in `mcp.toml` for external tools and
  notification sources.
- Keep HTTP ingress designs out of the shipped CLI until they have a new owner, threat model,
  and release gate independent of the removed service.
