# Use the shared installation

Pstack and the team workflows are already installed in `.agents`. `.claude`
points to the same directory. No marketplace installation is required.

1. Read [the shared configuration](../../README.md).
2. Review [the model settings](../../rules/pstack-models.md). Run
   [setup-pstack](../../skills/setup-pstack/SKILL.md) to change them. Settings
   stay in this repository, with provider options separate from model IDs.
3. Start [poteto-mode](../../skills/poteto-mode/SKILL.md) with a goal and a
   concrete completion check. Read a skill's `SKILL.md` directly if the runtime
   cannot invoke it by name.
4. Run `python scripts/verify-agent-setup.py` from the repository root to check
   local references, generated instructions, source hashes, and model routing.

Use [create-verification-skill](../../skills/create-verification-skill/SKILL.md)
when the app needs a scripted way to prove real behavior. See
[the runtime rules](../../rules/plugin-compatibility.md) for delegation,
transcript access, continuation, PR monitoring, and local skill authoring.
