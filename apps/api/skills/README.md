# Default bot skills

Bots receive the bundled `unslop` skill with the first message sent to a new native
provider session. Existing sessions receive it with their next turn. The API sends
the skill text inline, so Codex and Claude receive the same instructions without
reading files or enabling native skill tools.

`providers/default_skills.rs` owns the default registry. Each entry has a stable
name and a bundled `SKILL.md`. The API hashes the complete file with SHA-256 and
compares it with `provider_sessions.default_skills`. Each session stores its own
map of names to acknowledged hashes. Old database rows start with an empty map.

An update contains only added or edited skills and explicit removal instructions
for deleted defaults. Unchanged turns carry no skill update. Reverting a skill's
contents also produces an update. A rebuilt native session receives all current
defaults, even when its predecessor had already received them.

The adapters record the new map only after the provider acknowledges the input,
alongside its history position and memory revision. A failure before delivery
leaves the previous map intact. Existing recovery rules retire uncertain or
unfinished sessions. If delivery succeeds but bookkeeping fails, sending the
same skill again is safe. No skill change resets a session or changes its
compatibility fingerprint.

A global version would resend every skill when one changes and require a manual
version bump. Per-skill content hashes avoid both. Installing native skill files
would depend on provider-specific discovery and would not establish whether an
existing session had read them.

To add a default, place its self-contained instructions in
`skills/<name>/SKILL.md` with `name` and `description` frontmatter, then register
it with `include_str!` in `providers/default_skills.rs`. Use a unique, stable name.
Keep repository workflows, external file references, and tool permissions out of
bundled skills. Editing the file changes its hash automatically. Removing its
registry entry sends a removal on each existing session's next turn.

These instructions guide model output. They do not rewrite replies or guarantee
that a model follows every writing rule. Quoted source text and code keep their
original punctuation.
