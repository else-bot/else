---
name: reflect
description: Spawn three parallel review subagents over the active transcript, surface learnings, and route each to a concrete edit on an existing skill. Use when the user says reflect.
disable-model-invocation: true
---

Read the [local runtime rules](../../rules/plugin-compatibility.md) and [model presets](../../rules/pstack-models.md) before following this workflow.

# Reflect

Mine the current conversation for durable learnings, then route them into skill edits.

Invoke when the user says "reflect" or "/reflect". Skip when the conversation is trivial, off-topic, or already covered by an existing skill the parent followed correctly. One-offs are not learnings.

## 1. Locate the active transcript

Locate this conversation through the current runtime's workspace-scoped history tools. In T3, read the current thread with its thread tools. Use an explicitly supplied transcript path when one exists. If history is unavailable, pass a digest of this conversation. Do not guess a transcript layout or search another workspace.

## 2. Spawn three reviewers in parallel

Spawn three reviewers using the current runtime's delegation tools. Allow the MCP access each investigation requires; prohibit file edits in each brief.

Read `.agents/rules/pstack-models.md` and resolve the role named below through its preset table. Preserve the configured provider, effort, and speed. Do not pass preset names as model IDs or silently fall back to another model. Keep read-only investigators able to use the required tools and MCPs; their briefs must prohibit edits.

| Lens | Role line | Example preset | Prompt template |
|---|---|---|---|
| Judgment | `reflect judgment, divergent, synthesizer` | `opus-medium` | `references/judgment-reviewer.md` |
| Tooling | `reflect tooling` | `sol-xhigh-fast` | `references/tooling-reviewer.md` |
| Divergent | `reflect judgment, divergent, synthesizer` | `opus-medium` | `references/divergent-reviewer.md` |

Pass each template verbatim, substituting the transcript path or digest where marked. Reviewers return findings in the delegation result.

## 3. Synthesize

Spawn one synthesizer using `reflect judgment, divergent, synthesizer`. It spot-verifies citations through the available MCPs. Pass `references/synthesizer.md` with all reviewer outputs. It returns an Accepted / Rejected / Backlog list.

## 4. Structural enforcement check

Move any Accepted item that a lint rule, script, metadata flag, or runtime check would enforce more reliably to Backlog. See the **encode-lessons-in-structure** principle skill.

## 5. Apply

Present the synthesizer's full Accepted / Rejected / Backlog output and wait for explicit approval before applying any Accepted edit. The user picks the subset and may redirect routings. Skill changes affect every future agent in the org. Do not auto-apply.

File each Backlog item to your team's devex or backlog tracker without waiting. Only the Accepted list waits for approval.

Never edit the pinned snapshots in `.agents/plugins/`. For an approved bundled-skill change, edit the corresponding `.agents/adaptations/skills/` file and regenerate with `python scripts/sync-agent-plugins.py --write`. New project skills live directly in `.agents/skills/`. Never open a PR or push.

Follow each approved row's Routing exactly:

- Trivial existing-skill edit (a one-line bullet, a tightened sentence, a stale fact corrected): the parent does it directly.
- Substantive existing-skill edit (a new section, a new pattern table, more than ~10 lines): hand to the local `create-skill` skill and run its draft / test / iterate loop.
- `tune description: <skill path>` (the skill exists but didn't trigger when it should have): hand to `create-skill` and run its description-optimization loop.
- `new skill via create-skill: <kebab-name>`: hand creation to `create-skill`. Do not invent the shape ad hoc.

If your environment ships a SKILL.md validator, run it on every touched skill before declaring done.

## 6. Summarize for the user

Short list, no preamble:

- Edits applied: `<skill path>`. What changed, one line each.
- New skills created: `<skill path>`. One line each (rare).
- Backlog filed to the devex tracker: `<issue title>` (`<tags>`). One line each.
- Dropped: one line per rejected finding + reason from the synthesizer.
