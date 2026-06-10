# Archived: Public Cross-Agent MCP Service RFC

> Parent: [[00-master]]
> Status: **de-scoped / removed from shipped product surface** as of 2026-06-10.

This RFC is retained only as historical context. It is no longer an implementation plan, release
gate, or product requirement.

What remains supported:

- Generic MCP client support, including explicitly configured Streamable HTTP MCP servers.
- Generic notification hooks and trigger runtime primitives.
- Local triggers, cron jobs, hooks, skills, and session automation.

What was removed from the shipped product surface:

- Built-in public cross-agent client profile and onboarding.
- Public cross-agent account, message, inbox, and first-contact user flows.
- Client commands and config knobs for the removed service.
- Service-specific trust, identity, and credential assumptions in the client.

Release follow-up:

- The production service shutdown is tracked separately by the Worker tombstone task. That task
  must provide a bounded removed-feature response before any destructive Cloudflare cleanup.
- New public-network or cross-agent designs must start from a fresh issue with a new threat
  model, owner, CI/deploy strategy, and acceptance matrix.
