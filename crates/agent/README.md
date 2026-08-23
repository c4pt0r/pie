# pie-agent-core (Rust)

Rust port of [`@earendil-works/pie-agent-core`](https://github.com/earendil-works/pi) — the
stateful agent runtime layered on top of [`pie-ai`](../ai). Provides the agent loop, tool
execution, session persistence, compaction, skills resolution, and prompt-template plumbing.

## Status

1:1 source-level port with `pie-ai` as a path dependency. All four target subsystems —
**harness, Agent loop, session, compaction** — are functional and covered by integration tests
against a synthetic stream.

| Layer | Status |
|-------|--------|
| Core types (`types.rs`) | implemented |
| `Agent` + `run_agent_loop` | implemented (sequential tool execution, lifecycle events, abort, queues) |
| `AgentHarness` | implemented (composes Agent + Session + skills + system prompt, persists to session) |
| Skills (`harness/skills.rs`) | implemented + tested against tempdirs |
| System-prompt builder | implemented |
| Prompt templates | implemented (loader stub + `{{var}}` interpolation) |
| Session repo (jsonl + memory) | implemented; both backends share `SessionStorage` trait |
| Compaction + branch summarization | implemented (token estimation, cut-point, generate_summary, compact) |
| Native env adapter | implemented (std::fs + tokio::process) |

```
cargo test     # 28 tests across 5 binaries (lib + agent_loop + harness_e2e + session + skills)
```

## Layout

```
src/
  lib.rs                 barrel
  types.rs               AgentMessage / AgentState / AgentEvent / AgentTool / AgentLoopConfig
  agent.rs               Agent state machine
  agent_loop.rs          runAgentLoop
  proxy.rs               re-exports from pie-ai (HTTP_PROXY helpers)
  node.rs                native entry wiring the std::fs env adapter
  harness/
    types.rs             ExecutionEnv, Skill, repo interfaces
    agent_harness.rs     AgentHarness composer
    system_prompt.rs     formatSkillsForSystemPrompt
    skills.rs            loadSkills (SKILL.md discovery)
    prompt_templates.rs  template loader + interpolation
    messages.rs          custom AgentMessage variant helpers
    compaction/
      compaction.rs      auto-compaction + cut point
      branch_summarization.rs
      utils.rs
    session/
      session.rs         on-disk session shape
      jsonl_repo.rs / jsonl_storage.rs
      memory_repo.rs / memory_storage.rs
      uuid.rs            UUIDv7 helper
      repo_utils.rs      parent-chain reconstruction
    env/
      native.rs          std::fs ExecutionEnv adapter
    utils/
      shell_output.rs
      truncate.rs
```
