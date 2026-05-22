# Upstream attribution

This skill is vendored from
<https://github.com/multica-ai/andrej-karpathy-skills/tree/main/skills/karpathy-guidelines>
under the MIT license.

The guidelines text is derived from Andrej Karpathy's public observations on LLM coding
pitfalls: <https://x.com/karpathy/status/2015883857489522876>.

When the upstream repository changes its `SKILL.md`, the vendored copy in this directory
should be updated in a follow-up PR (it is bundled into the `pie` binary via `include_str!`,
so users only get changes by upgrading `pie` — there is no auto-update path in v1).

The `name`, `description`, and `license` frontmatter values are kept byte-identical to the
upstream so users can compare directly. The body is preserved verbatim.
