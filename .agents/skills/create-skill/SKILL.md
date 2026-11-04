---
name: create-skill
description: Create or update a repository-local skill with valid frontmatter, resolvable references, and verified supporting scripts.
---

# Create a local skill

Read [the runtime rules](../../rules/plugin-compatibility.md) before working.

1. Establish the workflow the skill should handle and inspect an existing
   related skill. Preserve the user's scope and authorization.
2. Write `.agents/skills/<name>/SKILL.md` with a lowercase hyphenated name and
   YAML frontmatter containing `name` and a specific `description`. Explain
   the workflow and reference only tools available in the target runtime.
3. Keep detailed templates and examples in supporting files when they are
   needed. Use relative links that resolve inside the local installation.
4. Validate the frontmatter and every referenced file. Run any added script
   against an isolated example and verify its actual result. For subjective
   style instructions, review the draft with the user instead of inventing a benchmark.

For bundled skills, put changes in `.agents/adaptations/` using the same path
below `.agents`, then run `python scripts/sync-agent-plugins.py --write`.
Do not edit the pinned snapshots in `.agents/plugins/`. New project skills
live directly in `.agents/skills/`. Run `python scripts/verify-agent-setup.py`
before reporting completion.
