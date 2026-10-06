# Parallel Search MCP with pie

This example adds web search and page extraction through pie's existing
Streamable HTTP MCP client. [Parallel Search MCP](https://docs.parallel.ai/integrations/mcp/search-mcp)
works anonymously at `https://search.parallel.ai/mcp`, without a Parallel API key
or a local server process. The anonymous tier has lower rate limits and uses
server-managed search settings. Model inference is separate: pie still needs a
configured model provider or local model.

## Enable it

Merge the `[[server]]` block from [`mcp.toml`](mcp.toml) into either:

- `~/.pie/mcp.toml` for all projects (or `$PIE_DIR/mcp.toml` if set).
- `<project>/.pie/mcp.toml` for one project.

For a project with no existing MCP config, run from that project's directory:

```sh
mkdir -p .pie
cp /path/to/pie/examples/mcp-parallel-search/mcp.toml .pie/mcp.toml
pie
```

If a config already exists, append the block instead of replacing the file. Each
server name must be unique within a registry; a project entry with the same name
as a user entry overrides it. Restart pie after changing the config.

With only this server configured, the startup banner reports:

```text
[mcp: connected to 1 server(s), 2 extra tool(s)]
```

The server exposes `web_search` (source URLs and excerpts) and `web_fetch`
(focused page excerpts). The example sets `tool_prefix = "parallel_"`, so pie
exposes them as `parallel_web_search` and `parallel_web_fetch`. The prefix only
changes agent-visible names; calls still use the original server names. Pie's
built-in `web_search` and `web_fetch` remain available. Existing MCP configs
without a prefix keep their original tool names. Try:

```text
Use parallel_web_search to find the official Rust ownership documentation, then use
parallel_web_fetch to read https://doc.rust-lang.org/book/ch04-01-what-is-ownership.html.
Explain the ownership rules and cite the source URLs.
```

The tool schemas are discovered at startup. For related search and fetch calls,
reuse a stable `session_id`; if supplying `model_name`, use the exact active
model ID from pie's configuration. The endpoint's live schemas and
[documentation](https://docs.parallel.ai/integrations/mcp/search-mcp) describe
current arguments and limits.

No authentication block is needed. This example uses pie's existing HTTP
User-Agent, request timeouts, cancellation and output handling. It does not
change the default model, built-in search provider or other saved configuration.
To disable it, remove this server block and restart pie. Startup connection
failures are reported as MCP diagnostics; the remaining tools stay available.

## Live smoke test

From the pie checkout, this explicitly ignored test loads the example through
`mcp_loader::load_all`, then calls both tools through the agent adapter. It checks
for source URLs and ownership excerpts. It makes real anonymous network calls
and is excluded from ordinary CI:

```sh
smoke_dir=$(mktemp -d)
env -i PATH="$PATH" HOME="$smoke_dir" PIE_DIR="$smoke_dir/pie" \
  cargo test -p pie-coding-agent parallel_search_live -- --ignored --nocapture
rm -rf "$smoke_dir"
```

If cargo is installed with rustup, retain your toolchain locations while clearing
provider credentials:

```sh
smoke_dir=$(mktemp -d)
env -i PATH="$PATH" HOME="$smoke_dir" PIE_DIR="$smoke_dir/pie" \
  RUSTUP_HOME="$HOME/.rustup" CARGO_HOME="$HOME/.cargo" \
  cargo test -p pie-coding-agent parallel_search_live -- --ignored --nocapture
rm -rf "$smoke_dir"
```
