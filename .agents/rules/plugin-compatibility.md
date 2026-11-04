# Shared plugin installation

The canonical directory is `.agents`. `.claude` is a relative symlink to it.
Skills live in `.agents/skills/<name>/SKILL.md`, and agent definitions live
in `.agents/agents/<name>.md`. Both directories expose relative symlinks to
the complete upstream copies in `.agents/plugins/`.

This is a repository installation, not a marketplace installation. Invoke
skills by their local name, such as `/poteto-mode`, and agents by their local
name, such as `poteto-agent`. Do not add `pstack:` prefixes. Read another
skill's local `SKILL.md` when the runtime cannot invoke it directly.

Use `.agents/rules/pstack-models.md` for model selection. It overrides upstream
model defaults, budget adjustments, and home-directory configuration paths.
The other `.agents/rules/*.md` apply here too. The TypeScript rule applies
only to TypeScript files. Preserve vendored upstream files when applying
repository coding rules.

Cursor tool names describe intent. Use the current runtime's corresponding
tools and accepted arguments. In Claude Code, `Task` maps to `Agent`,
`generalPurpose` maps to `general-purpose`, and `AskQuestion` maps to
`AskUserQuestion` when available. Read-only work must say not to edit files.
In Codex, use its available tools and respect the session's delegation limits.
T3 orchestration instructions take precedence over upstream agent lifecycle,
background, model fallback, and PR monitoring instructions. Cursor's agent
frontmatter `model: fast` and `is_background` do not select a model or enable
background execution in these runtimes. Use the configured role preset and
the current tool schema instead.

For additional Claude mappings, read
`.agents/plugins/pstack/docs/harness/claude-code.md`. The local paths, agent
names, and model choices above take precedence over that marketplace adapter.
Report unavailable Cursor-only features and continue with independent work.
