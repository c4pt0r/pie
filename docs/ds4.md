# pie + DS4: KV prefix-cache optimizations for local models

`pie` was originally built to drive long-running automation on a local
[DS4](https://github.com/antirez/ds4) (DeepSeek V4 Flash) server, so the ds4 integration
gets first-class attention. This document covers the recent round of client-side
optimizations (PR #209) that make pie's request stream *cache-exact* for DS4's KV
prefix cache — and what that buys you in practice.

Setup (model descriptor, `--base-url`, `/login ds4`) is covered in the
[README](../README.md#local-openai-compatible-models); this page is about what happens
after you're connected.

## Why this matters: byte-exact prefix caching

Hosted providers (Anthropic, OpenAI) do server-side prompt caching with explicit cache
control and fuzzy bookkeeping. A local DS4 server is stricter and simpler: it renders
the request history into a token stream and reuses KV checkpoints **only when the new
request's rendered prefix is byte-identical to what it sampled last time**. It also
persists those checkpoints to disk (`--kv-disk-dir`), so a cache hit survives eviction
and even a server restart.

The flip side: *any* divergence between what the model produced and what the client
replays — a dropped reasoning block, a reordered item — silently invalidates the prefix
from that point on. On a 100k-token agent session that's the difference between
re-prefilling a few hundred new tokens per turn and re-prefilling the whole conversation
every turn. On local hardware, prefill is the bottleneck; this is the single biggest
lever on perceived latency.

pie's job as a client is therefore: **replay history exactly, retry the way the server
expects, and report cache traffic honestly.** That's the three fixes below.

## The three fixes

### 1. Replay assistant thinking as `reasoning` input items

DeepSeek V4 is a reasoning model: each assistant turn starts with thinking content.
DS4 renders that thinking into the sampled token stream, so it is part of the KV
checkpoint. But the standard OpenAI Responses client behavior is to *drop* thinking
when replaying history — which means the rendered prefix pie sent back never matched
what the server had sampled, and disk KV checkpoints went stale the moment the live
continuation state was evicted.

The ds4 model descriptor always declared
`"requiresReasoningContentOnAssistantMessages": true`, but the `openai-responses`
provider never read the flag. Now it does: when set, pie replays each assistant
turn's thinking as a `{"type":"reasoning"}` input item, emitted **before** the
assistant message it belongs to (DS4 merges a reasoning item into the *following*
message — ordering is load-bearing).

With this, the rendered history is byte-identical across turns and DS4's checkpoints
stay valid across eviction and server restarts.

### 2. Treat HTTP 409 as retryable

When DS4 has lost the live continuation state for a session (evicted, restarted), it
answers `409 Conflict`, meaning: *"replay the full history and I'll rebuild from my
disk checkpoints."* pie always sends the full history anyway — so for pie, a plain
retry of the same request **is** the replay the server is asking for.

The retry layer (`crates/ai/src/utils/retry.rs`) now includes 409 in its retryable
status set, alongside 408/425/429/5xx, with the usual backoff. What used to surface
as a hard error mid-session now heals transparently in one round-trip.

### 3. Report cache writes in `/cost`

DS4 reports a non-standard usage field, `input_tokens_details.cache_write_tokens` —
tokens newly written into the prompt cache by this request. pie now folds it into
`Usage.cache_write`, so `/cost` shows both sides of the cache ledger (reads *and*
writes) instead of crediting reads only.

This is also your verification tool: on a healthy session you should see a large
`cache read` count and a small `cache write` count each turn. If cache reads collapse
to zero mid-session, the prefix diverged — which, after fix #1, should no longer
happen.

## Verifying on your setup

```bash
# DS4 with disk-backed KV so checkpoints survive restarts:
./ds4-server --ctx 100000 --kv-disk-dir /tmp/ds4-kv --kv-disk-space-mb 8192

export DS4_API_KEY=dsv4-local
./target/release/pie --provider ds4 --model deepseek-v4-flash \
  --base-url http://127.0.0.1:8000/v1
```

Then, inside pie:

1. Run a few turns, then `/cost` — `cache read` should dominate `input` from the second
   turn on.
2. Restart the DS4 server mid-session and send another prompt — the turn should succeed
   (one transparent 409-retry) and cache reads should resume from the disk checkpoints
   rather than starting over.

## Scope

These changes live in the `openai-responses` provider and the shared retry utility, all
gated so other backends are unaffected:

- reasoning replay only activates when the model descriptor sets
  `requiresReasoningContentOnAssistantMessages` (the ds4 descriptor does);
- `cache_write_tokens` is read only if the server sends it;
- 409-retry is generic, and correct for any API where the client always sends full
  history (pie's only mode).

If you run a different local server that does byte-exact prefix caching and consumes
reasoning input items, setting the same compat flag in your `models.json` entry gets
you the same behavior — nothing here is hard-coded to ds4.
