---
name: setup-pstack
description: Configure this repository's pstack model presets and role assignments in the shared local rules.
---

Read the [local runtime rules](../../rules/plugin-compatibility.md) and [model presets](../../rules/pstack-models.md) before following this workflow.

# Setup pstack

Edit `.agents/rules/pstack-models.md`. Both Codex and Claude Code use that
repository-local file. Preserve the JSON preset table and role lines.

1. Read the current presets and all role assignments. Treat the user's explicit
   changes as the requested configuration. Preserve other values.
2. Detect the current runtime's providers, model IDs, and option schemas. In T3,
   use `orchestrator_capabilities`. Model and provider settings are separate
   fields. Effort and speed belong in the target's `options`.
3. Validate each changed preset against those capabilities. Role values name
   presets, not model IDs. Do not invent a slug or silently substitute a model.
   Ask only when a required choice cannot be inferred or verified.
4. Write the local file. Keep each role on one line. A panel list runs one agent
   per entry; the cross-judge pool chooses one eligible model from another family.
   Preserve each preset's configured reasoning level and speed.
5. Run `python scripts/verify-agent-setup.py`. Report the changed assignments
   and any unsupported runtime capability.

Do not write global configuration or flatten provider options into model slugs.
Opus medium is the repository's default for implementation. Sol xhigh fast and
Fable high handle independent review and design; Opus high handles the hardest work.
