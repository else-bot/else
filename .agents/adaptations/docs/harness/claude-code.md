# Use pstack in Claude Code

This is a local installation. `.claude` points to `.agents`; its `rules/`,
`skills/`, and `agents/` directories provide the shared configuration.

Read [the local runtime rules](../../rules/plugin-compatibility.md) and
[model presets](../../rules/pstack-models.md). Invoke skills and agent
definitions by their local names. If invocation is unavailable, read the
corresponding local file and follow it.

Use only fields accepted by the current tool schema. Native Claude agents use
supported model aliases and effort fields. Cross-provider presets require a
delegation runtime such as T3. Report an unavailable preset or tool rather
than silently changing model, effort, speed, or execution environment.

Use the current session's transcript tools, completion events, and browser
tools. Do not assume a global plugin installation, a fixed transcript layout,
or a built-in scheduling command. The local
[create-skill](../../skills/create-skill/SKILL.md) workflow covers skill authoring.
