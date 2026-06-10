# Archived: Client Onboarding for Removed Cross-Agent Service

> Parent: [[00-master]]
> Status: **de-scoped / removed from shipped product surface** as of 2026-06-10.

This document previously described onboarding for an experimental public cross-agent service. That
service and its client command surface are no longer release targets.

Do not use this file as an implementation plan. The current client should start cleanly without
any built-in public service profile, credential, trust file, first-contact card, or onboarding
banner for the removed feature.

Current validation belongs to the removal release gate:

- User-facing help and command registries do not advertise the removed service.
- Clean profiles do not auto-configure a public service endpoint.
- Stale local credentials or trust files are ignored by the generic MCP loader.
- Generic Streamable HTTP MCP and generic trigger delivery continue to work for explicit MCP
  server configurations.
