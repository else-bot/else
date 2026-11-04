# Shared agent configuration

This repository installs [pstack](https://github.com/cursor/plugins/tree/main/pstack)
and [cursor-team-kit](https://github.com/cursor/plugins/tree/main/cursor-team-kit)
in `.agents`. `.claude` points to the same directory.

The installation contains 76 bundled skills, a local `create-skill` helper,
and four agent definitions. Active instructions in `skills/`, `agents/`, and
`docs/` are generated local adaptations. Supporting scripts and assets use
relative symlinks to the pinned upstream copies in `plugins/`.
`sources.json` records the upstream revision and file hashes. Original
marketplace files remain there as provenance and are not active instructions.

Edit [rules/pstack-models.md](rules/pstack-models.md) to change the five model
presets or their role assignments. Sol uses xhigh reasoning and fast service.
Opus 5.5 and Fable 5.1 each have medium and high presets. Each review panel
uses Sol xhigh fast and Fable high. Opus medium handles most coding,
explanations, and synthesis. Opus high handles the hardest tasks.
Cross-provider panels require T3 delegation support.

Codex reads the shared rules through the root `AGENTS.md`. Claude Code loads
them through `.claude/rules/`. Every bundled skill and agent links directly
to the local runtime rules and model presets.

Run `python scripts/verify-agent-setup.py` from the repository root to check
source hashes, local links, generation consistency, rules, and model assignments.
Run `python scripts/sync-agent-plugins.py --write` to regenerate active files.
Keep document changes in `adaptations/`, using the same path below `.agents`.
The generator handles shared path and tool mappings; leave the snapshots intact.

Transcripts come from workspace-scoped runtime tools or an explicitly supplied
path. The worktree audit accepts `PSTACK_TRANSCRIPTS_DIR` and leaves chat
evidence unknown when no directory is supplied. Native runtimes may lack
cross-provider delegation, scheduling, or a hosted automation integration;
workflows report those gaps rather than inventing tools or changing models.

Skills are available when the next turn or session reloads the repository's catalog.
