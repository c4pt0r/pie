# mcp-weather-python — push live weather into pie, let the harness act on it

The sibling [`mcp-notify-python`](../mcp-notify-python/) example pushes a synthetic
heartbeat counter to show the wiring. This one does a **real** thing: it polls
[wttr.in](https://wttr.in) for the current San Francisco weather and pushes each new
observation into pie as a one-line sentence. Stdlib only — no MCP SDK, no `pip install`.

The design keeps a clean split:

- **The Python server** does only what it is uniquely placed to do — reach the network,
  parse the API, and collapse the reading into a single human-readable sentence
  (`_meta.pie_summary`). It ships **no policy**: it never decides what "should" happen.
- **pie's harness** decides how that sentence reaches you. This example's `mcp.toml` sets
  `inject_summary = true`, so each observation is injected straight into chat as a
  `[Trigger …]` message — no sub-agent, no model call, zero cost. Drop that flag and the
  same sentence instead drives a dynamic trigger rule (a sub-agent reasons over it). Both
  paths are configured in pie, not in the server.

## Files

- `weather-server.py` — the MCP server. The heartbeat demo with the counter swapped for a
  live `urllib` poll of wttr.in.
- `mcp.toml` — the pie config snippet that wires the server in.

## Run it with pie

1. Copy or merge the `[[server]]` block from `mcp.toml` into one of pie's MCP registries:

   ```sh
   # user-global (works in every project):
   mkdir -p ~/.pie
   cp mcp.toml ~/.pie/mcp.toml   # or merge the [[server]] block into an existing file

   # OR project-local (only this repo):
   mkdir -p /path/to/your/project/.pie
   cp mcp.toml /path/to/your/project/.pie/mcp.toml
   ```

   The `args` path is relative to the cwd pie is launched from. Use an absolute path to the
   `weather-server.py` if you want the config to work from anywhere.

2. Set a provider API key, then start pie:

   ```sh
   export OPENAI_API_KEY=sk-...   # or ANTHROPIC_API_KEY etc
   pie
   ```

   The startup banner should show `[mcp: connected to 1 server(s), 0 extra tool(s)]` and
   `[trigger sources: watching 1 configured MCP push source(s)]`.

3. In the REPL, type `/triggers`. Within a few seconds the first observation is accepted;
   each persists to the session JSONL as a `Custom { customType: "trigger" }` entry with a
   summary like:

   ```text
   San Francisco: 14°C (feels 13°C), Partly cloudy, humidity 81%, wind 12 km/h SSW — obs 02:07 PM
   ```

The default poll interval is 600s and the default location is San Francisco. Override
either by exporting before launch (the child inherits pie's environment):

```sh
PIE_WEATHER_LOCATION=Tokyo PIE_WEATHER_INTERVAL_SECS=300 pie
```

## Three ways the harness can deliver the weather

**1. Direct inject (this example's default — `inject_summary = true`).** Each observation's
summary is injected straight into the chat as a `[Trigger …]` message, so your next turn
sees it. No sub-agent runs, no tokens are spent, latency is ~0. The whole MCP server is
treated as a notification feed. This is the right mode when you just want to *see* the
sentence. Mechanically the runtime moves the opaque `payload_summary` and never learns it's
"weather" — the domain stays entirely in the Python server.

**2. Inject and react (`inject_and_run = true`).** Same injection, but the agent then runs
**one model turn in the chat's full context** so it can react to the update (comment on it,
call a tool, factor it into what you were doing). Costs a turn. The runtime never drives the
single-tenant agent from the trigger task: if you're mid-turn it queues a follow-up, and if
the chat is idle it asks the REPL to run the turn on the same serialized path as your own
input — so a reaction and your next prompt never collide.

**3. Dynamic rule (drop both flags).** The same sentence instead drives a session trigger
rule you install from chat:

```text
when mcp:weather fires notifications/pie/weather/observation, if rain or a storm is
mentioned in the summary, append the summary to /tmp/sf-rain.log; otherwise do nothing
```

pie's `NewTrigger` tool persists the rule to a session sidecar. Each subsequent observation
dispatches a sub-agent that inherits the parent harness config but starts with a fresh
context, sees the sentence, and applies that policy. Use this mode when "is this worth
telling me about?" needs judgment in an isolated context.

> Note: a server with `inject_summary` / `inject_and_run` is a pure feed, so dynamic rules
> are **not** consulted for it. Pick one mode per server.

## How it maps onto the runtime

| Concept | How it shows up |
| --- | --- |
| **Real IO in the server** | `fetch_weather()` does a stdlib `urllib` GET to `wttr.in/<loc>?format=j1`. Fetch errors are logged to stderr and the loop keeps going — a transient network blip never kills the source. |
| **One sentence, not a payload** | The reading is collapsed to `_meta.pie_summary`. The raw params (`fetched_at`) are dropped at the adapter (`payload_visibility = Local`); only the sentence survives into the audit and reaches the sub-agent. |
| **Notify once per observation** | `_meta.pie_dedup_key = "obs:<date>:<observation_time>"`. Re-polling the same reading produces the same key, which the runtime dedups — so you get one event per *new* observation, not one per poll. The date prefix keeps the same time-of-day on a later day distinct. The runtime namespaces it as `mcp:weather:custom:obs:<date>:<time>`. |
| **Server-push, no tools** | `tools/list` returns empty; the observation is a JSON-RPC notification (no `id`), routed to pie's notification channel rather than the response router. |
| **Delivery lives in the harness** | The server never decides how its sentence reaches you. `inject_summary` injects it verbatim (no model); `inject_and_run` injects it then runs one main-context turn; dropping both routes it through a sub-agent rule. Swap modes in `mcp.toml` without touching the server. |

## Adapt it to any source

The shape generalizes to any pollable source — stock price, CI queue depth, disk usage, a
status page. Keep the contract:

1. Speak JSON-RPC 2.0 over stdio. Respond to `initialize` + `tools/list`.
2. Emit each event as a JSON-RPC notification (no `id`) on stdout, one per line.
3. For non-spec methods, include `_meta.pie_dedup_key` — choose it so re-observing the
   same state produces the same key (dedup) and a genuinely new state produces a new one.
4. Put the human-readable, safe-to-persist string in `_meta.pie_summary`. Leave the
   judgment about what to *do* with it to a pie trigger rule.
