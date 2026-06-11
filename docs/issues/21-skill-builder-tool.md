# SkillBuilder — builtin tool for authoring user skills

> Parent: master roadmap issue.
> Tier: 4 (framework depth — skills + harness).
> Status: designed + implemented in the same PR.

## Goal

Give the model a first-class way to *author* a new skill when the user asks to save a
reusable workflow, checklist, or convention ("帮我把这个流程存成一个技能"). Today the only
paths are `InstallSkill` (which expects a complete, externally sourced `SKILL.md`) or raw
`Write` (no validation, no reload, no audit). `SkillBuilder` takes structured fields and owns
the format: the model never hand-assembles frontmatter, and every produced skill is loadable
by construction.

`InstallSkill` stays the path for installing *existing third-party* skill content;
`SkillBuilder` is for *creating* skills from intent.

## Tool surface

Name: `SkillBuilder`. Module: `crates/coding-agent/src/tools/skill_builder.rs`.

Input:

```jsonc
{
  "name": "code-review-checklist",   // required; kebab-case, same rules as InstallSkill
  "description": "what + when",      // required; ≤1024 chars; carries trigger phrasing
  "instructions": "markdown body",   // required; steps / conventions / guidance
  "examples": "markdown",            // optional; rendered as its own section
  "overwrite": false,                // required true when same-name skill differs
  "confirm": false                   // two-phase: false = preview, true = write
}
```

Rendered output (canonical template, frontmatter emitted via `serde_yaml` so special
characters in `description` are always escaped correctly):

```markdown
---
name: code-review-checklist
description: what + when
---

# Code Review Checklist

## Instructions

<instructions>

## Examples        ← only when provided

<examples>
```

The `description` is collapsed to a single line (whitespace/newlines folded) before
rendering; it is the catalog trigger line, not body text.

## Behavior

Mirrors `InstallSkill`'s proven two-phase model and reuses its helpers
(`parse_and_validate`, `atomic_write_skill`, `on_disk_skill_hash`, made `pub(crate)`):

- **Preview** (`confirm: false`): render → run the rendered content through the same
  `parse_and_validate` used by InstallSkill (one validation source of truth) → report
  `{name, description, target_path, content_hash, size, existing, overwrite_required,
  warnings}`. No fs writes. Shadow warnings come from the live catalog: a same-name
  *project* skill will shadow the new user skill; a same-name *builtin* will be shadowed
  by it.
- **Write** (`confirm: true`): refuse when `overwrite_required && !overwrite`; atomic
  tempfile+rename into `~/.pie/skills/<name>/SKILL.md`; hot-reload via
  `reload_skills_from_disk()`; append a `skill_install` audit entry with
  `source_kind: "builder"` (metadata + hashes only, never the body).
- **Permission**: `Prompt` (control-plane write), bounded reason
  `create user skill \`<name>\`` — the name is included only when it passes the kebab-case
  charset check, otherwise `<invalid name>`.
- **Execution mode**: `Sequential` (writes global state + triggers reload).

Destination is user-global only (`${PIE_DIR:-~/.pie}/skills/`). Project-scoped output was
considered and deferred (decision 2026-06-10): keep v1 minimal; project skills can be added
later via a `scope` field without breaking the schema.

## System prompt routing

`render_base_prompt` gains one sentence: when the user asks to create, save, or codify a
reusable skill / workflow / checklist, call `SkillBuilder`; use `InstallSkill` only for
installing an existing `SKILL.md` from a URL, file, or pasted content.

## Testing

| Case | Assert |
|---|---|
| preview | metadata payload, no fs writes, body not echoed |
| confirm | file exists at `<root>/<name>/SKILL.md`, catalog reload contains the skill |
| invalid name | refused before any path resolution |
| overwrite | same-name different-content refused without `overwrite: true`; succeeds with it |
| template | `## Examples` present only when provided; frontmatter round-trips through the loader |
| yaml safety | `description` containing `:`'s/quotes/newlines still parses and matches after reload |
| audit | confirm result carries `audit_entry_id`; audit payload has no body |

## Out of scope

- Project-scoped (`<cwd>/.pie/skills/`) destination — future `scope` field.
- Editing an existing skill in place beyond whole-file overwrite.
- Multi-file skills (directories with resources) — v1 writes a single `SKILL.md`.
