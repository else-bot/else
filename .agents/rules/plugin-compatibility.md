# Shared plugin installation

The canonical directory is `.agents`. `.claude` is a relative symlink to it.
Skills live in `.agents/skills/<name>/SKILL.md`, and agent definitions live
in `.agents/agents/<name>.md`. These are generated local adaptations.
Supporting scripts and assets use relative links to pinned runtime source files.
The trimmed sources in `.agents/plugins/` are provenance, not active instructions.

Invoke skills by their local name, such as `/poteto-mode`, and agents by
their local name, such as `poteto-agent`. Read another skill's local
`SKILL.md` when the runtime cannot invoke it directly. Resolve relative file
references from the instruction file's own directory, not the shell's cwd.

Use `.agents/rules/pstack-models.md` for model selection. It overrides upstream
model defaults, budget adjustments, and home-directory configuration paths.
The other `.agents/rules/*.md` apply here too. The TypeScript rule applies
only to TypeScript files. Preserve the retained upstream runtime files. To change a
bundled document, add or edit its corresponding file in `.agents/adaptations/`
and run `python scripts/sync-agent-plugins.py --write`. New project skills
live directly in `.agents/skills/`. Use the local `create-skill` workflow.

Workflow verbs describe intent. Use only the current runtime's tools and
accepted arguments. In Claude Code, delegation uses `Agent` and supported
agent types. In Codex, use its available agent tools. For a local specialized
agent without a matching tool type, include its definition's absolute path in
the brief and require that it be read before work. Read-only briefs prohibit
file edits while retaining the tools needed for evidence. Start agents only
when the user or an applicable skill authorizes them and session instructions permit it.

In T3, follow the session's native-agent and `delegate_task` routing rules.
Cross-provider settings belong in `target.options`. Follow the session's
completion, cancellation, linking, and PR subscription instructions instead
of CLI watch loops. Scheduling requires a user request. If an event or
scheduling facility is absent, report that limitation and preserve a checkpoint.

Use workspace-scoped transcript tools or an explicitly supplied transcript
path. In T3, read a thread before describing it and cite its returned link.
Do not guess a home-directory transcript layout. When history is unavailable,
use this conversation and current repository state and report the gap.
The worktree audit accepts `PSTACK_TRANSCRIPTS_DIR` for an explicitly scoped
transcript directory and otherwise leaves chat evidence unknown.

For browser, GUI, or desktop work, follow the session's browser and desktop
containment instructions before running an app or test that can open a window.
Report unavailable integrations and continue with independent work.
