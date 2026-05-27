# pie

`pie` is a Rust rewrite of the original [pi](https://github.com/earendil-works/pi) project (pi-coding-agent). `pie` is a terminal-based AI coding agent, run it inside a project, ask it to inspect files, make edits,
run shell commands, remember preferences, and continue previous sessions.

## Install / build

```bash
git clone https://github.com/c4pt0r/pie.git
cd pie
cargo build --release
```

The CLI binary will be at `./target/release/pie`.

## Configure a model

`pie` auto-detects the first available provider credential. Set an API key before starting:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
# or: OPENAI_API_KEY, OPENROUTER_API_KEY, GROQ_API_KEY,
#     MISTRAL_API_KEY, GEMINI_API_KEY, GOOGLE_API_KEY
```

You can also store a key from inside `pie`:

```text
/login anthropic sk-ant-...
```

### Local OpenAI-compatible models

`pie` can also use local OpenAI-compatible servers. Add a model definition to
`~/.pie/models.json` (user-global) or `<project>/.pie/models.json` (project-local, higher
precedence), then select it with `--provider` and `--model`.

Example for [DS4](https://github.com/antirez/ds4), the DeepSeek V4 Flash local
server. The Responses endpoint is the preferred OpenAI-compatible API for
Codex-style clients; chat completions also works for simpler integrations.

```bash
# In the DS4 checkout:
./ds4-server --ctx 100000 --kv-disk-dir /tmp/ds4-kv --kv-disk-space-mb 8192
# If launching from another directory, add: --chdir /path/to/ds4
```

```json
{
  "models": [
    {
      "id": "deepseek-v4-flash",
      "name": "DeepSeek V4 Flash (local DS4)",
      "api": "openai-responses",
      "provider": "ds4",
      "baseUrl": "http://127.0.0.1:8000/v1",
      "reasoning": true,
      "thinkingLevelMap": {
        "off": null,
        "minimal": "low",
        "low": "low",
        "medium": "medium",
        "high": "high",
        "xhigh": "xhigh"
      },
      "input": ["text"],
      "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
      "contextWindow": 100000,
      "maxTokens": 384000,
      "compat": {
        "supportsStore": false,
        "supportsDeveloperRole": false,
        "supportsReasoningEffort": true,
        "supportsUsageInStreaming": true,
        "maxTokensField": "max_tokens",
        "supportsStrictMode": false,
        "thinkingFormat": "deepseek",
        "requiresReasoningContentOnAssistantMessages": true
      }
    }
  ]
}
```

Then run:

```bash
export DS4_API_KEY=dsv4-local
./target/release/pie --provider ds4 --model deepseek-v4-flash --base-url http://127.0.0.1:8000/v1
```

DS4 is local and accepts placeholder bearer tokens. You can also store the same
local placeholder with `/login ds4 dsv4-local`. Using the `ds4` provider keeps
local model credentials separate from real `OPENAI_API_KEY` credentials.

`--base-url`, `DS4_BASE_URL` (or `DS4_URL`) registers the conventional `ds4` /
`deepseek-v4-flash` descriptor without a `models.json` file. CLI `--base-url`
wins for the current run. Keep `models.json` when you need different limits,
compatibility flags, or a project-local override.

## Quick start

```bash
# Start in the current project
./target/release/pie

# Choose a specific provider/model
./target/release/pie --provider anthropic --model claude-haiku-4-5

# Enable extended thinking where supported
./target/release/pie --thinking high

# Resume the most recent session for this project
./target/release/pie --resume
```

Once the REPL opens, type a request such as:

```text
summarize this repository
fix the failing tests
add a README example and run the relevant checks
when ~/build.done appears, run cargo test and show me the result
```

## Useful commands

Inside `pie`, slash commands control the session:

| Command | What it does |
|---------|--------------|
| `/help` | Show all commands |
| `/model [provider:model-id]` | Show or switch model |
| `/thinking` | Show or set thinking level, off, minimal, low, medium, high, xhigh |
| `/sessions` | List sessions for the current project |
| `/save [path]` | Export the transcript to Markdown |
| `/compact [instructions]` | Compact long context |
| `/undo` | Remove the most recent user/assistant turn |
| `/cost` | Show token and cost totals |
| `/login <provider> <api-key>` | Store an API key |
| `/logout <provider>` | Remove a stored API key |
| `/triggers` | Show trigger rules, sources, running actions, and audit |
| `/triggers rules` | List dynamic trigger ids and state |
| `/triggers enable <id>` / `/triggers disable <id>` | Resume or pause a trigger |
| `/triggers remove <id>` | Delete a trigger |
| `/cron` | List local scheduled jobs |
| `/cron add "<minute hour dom month dow>" <prompt>` | Run a prompt on a local schedule |
| `/cron enable <id>` / `/cron disable <id>` | Resume or pause a scheduled job |
| `/cron remove <id>` | Delete a scheduled job |
| `/quit` | Exit |

CLI helpers:

```bash
./target/release/pie --help
./target/release/pie --list-sessions
./target/release/pie --list-all-sessions
./target/release/pie --delete-session <id>
./target/release/pie --image screenshot.png
```

## What pie can do

The agent has tools for common coding workflows:

- read, write, and edit files
- list files and search with grep/find
- run shell commands
- manage persistent memory
- delegate focused sub-tasks
- resume JSONL-backed sessions per project
- attach images to the first prompt with `--image`
- create session-scoped natural-language triggers that run actions when local checks or MCP
  push events match
- create user-scoped cron jobs that run prompts on a local schedule
- run local command hooks or HTTP webhooks on agent lifecycle events; see [docs/hooks.md](docs/hooks.md)

## Triggers

Triggers let you describe an automation in normal chat:

```text
when $HOME/helloworld exists, print its contents
```

`pie` turns that request into a dynamic trigger rule. Rules are stored next to the active
session, so a new session starts cleanly and `--resume` brings that session's rules back.
Dynamic triggers fire once by default; ask for a repeating trigger when you want it to keep
running.

Trigger actions run in a separate sub-agent. The sub-agent inherits the parent model, tools,
tool hooks, thinking level, and skill catalog, but it does not receive the full parent
conversation by default. Trigger output is shown in the TUI and written to trigger audit
records. If you explicitly ask for the result to be visible to future turns, the rule is
created with `promote_to_chat=true` and successful trigger output is inserted into the main
chat context with a `[Trigger ...]` prefix.

Local dynamic checks poll every 10 minutes by default, and only emit checks while at least
one enabled dynamic rule exists. Configure the interval in `~/.pie/config.toml`:

```toml
[triggers]
poll_interval_secs = 600
```

For one run, override it with:

```bash
./target/release/pie --trigger-poll-secs 60
```

MCP notifications are treated as trigger sources too: a configured MCP server can push an
event into the same trigger runtime, where the same rules, audit, deduping, and output
behavior apply.

## Cron jobs

Cron jobs are time-based automations, separate from dynamic triggers. They are stored in
`~/.pie/cron.toml`, use local time, and support standard 5-field cron expressions:

```text
/cron add "*/30 * * * *" summarize the repo state
/cron list
/cron disable cron-...
```

When a cron job is due, it enters the same serialized agent turn queue used by prompts and
trigger inject-and-run actions. `pie` does not backfill missed ticks after downtime. If a
job is still running when its next tick arrives, that tick is skipped and recorded in the
job status. Cron config stores only the schedule and action text; control-plane audit and
UI output use bounded, redacted previews.

## Files and storage

By default, `pie` stores local state under `~/.pie`:

| Path | What |
|------|------|
| `~/.pie/sessions/<cwd-hash>/<uuidv7>.jsonl` | Session history for each project |
| `~/.pie/memory/*.md` | Cross-session memory injected into future sessions |
| `~/.pie/auth.json` | Stored API keys from `/login` |
| `~/.pie/models.json` | User-global local/custom model definitions |
| `~/.pie/history` | Prompt history |
| `~/.pie/hooks.toml` | Optional command/webhook hooks |
| `~/.pie/cron.toml` | User-global local cron jobs |
| `~/.pie/config.toml` | Optional user config, including trigger poll interval |

Set `PIE_DIR` to use a different base directory.

## Development

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

## License

[MIT](LICENSE)
