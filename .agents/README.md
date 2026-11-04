# Shared agent configuration

This repository installs [pstack](https://github.com/cursor/plugins/tree/main/pstack)
and [cursor-team-kit](https://github.com/cursor/plugins/tree/main/cursor-team-kit)
in `.agents`. `.claude` points to the same directory.

The installation contains 76 skills and four agent definitions. Full upstream
copies, licenses, guides, scripts, and supporting files are in `plugins/`.
`sources.json` records their shared source revision. Relative symlinks expose
the skills and agents through the directories used by Codex and Claude Code.
The CI watcher is a local copy with compatible Claude agent frontmatter.

Edit [rules/pstack-models.md](rules/pstack-models.md) to change the five model
presets or their role assignments. Sol uses xhigh reasoning and fast service.
Opus 5.5 and Fable 5.1 each have medium and high presets. Each review panel
uses all five presets. Cross-provider panels require T3 delegation support.

Codex reads the shared rules through the root `AGENTS.md`. Claude Code loads
them through `.claude/rules/`. The compatibility rule maps Cursor-specific
instructions to the current runtime without changing the upstream copies.

Run `python scripts/verify-agent-setup.py` from the repository root to check
the source hashes, discovery links, rules, and model assignments. Skills are
available when the next turn or session reloads the repository's skill catalog.
