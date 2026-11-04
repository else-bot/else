# Pstack model configuration

This repository's role choices override pstack's defaults and any user-level
`pstack-models.mdc` or `pstack-models.md`. Keep changes here when running
`setup-pstack` in this repository. The budget is custom, with the effort set
per preset below. Keep the configured effort and speed when delegating.

Role values are preset names, not API model IDs. Resolve each name through
this table before spawning an agent. Panel lists run one agent per entry.
The arena cross-judge pool selects one judge from another model family.

The JSON records the T3 `delegate_task` target for each preset. GPT fast mode
uses `serviceTier: priority`. Claude uses `effort` for its reasoning level.
Recheck provider capabilities before using a model in a new environment.

```json
{
  "sol-xhigh-fast": {
    "providerInstanceId": "codex",
    "model": "gpt-6.1-sol",
    "options": { "reasoningEffort": "xhigh", "serviceTier": "priority" }
  },
  "opus-medium": {
    "providerInstanceId": "claudeAgent",
    "model": "claude-opus-5-5",
    "options": { "effort": "medium" }
  },
  "opus-high": {
    "providerInstanceId": "claudeAgent",
    "model": "claude-opus-5-5",
    "options": { "effort": "high" }
  },
  "fable-medium": {
    "providerInstanceId": "claudeAgent",
    "model": "claude-fable-5-1",
    "options": { "effort": "medium" }
  },
  "fable-high": {
    "providerInstanceId": "claudeAgent",
    "model": "claude-fable-5-1",
    "options": { "effort": "high" }
  }
}
```

feature, refactoring: sol-xhigh-fast
bug-fix: sol-xhigh-fast
perf-issue: sol-xhigh-fast
hillclimb: sol-xhigh-fast
judgment and prose: opus-medium
hardest tasks: opus-high
how explorer: sol-xhigh-fast
how explainer: fable-medium
why investigators: sol-xhigh-fast
why synthesizer: fable-high
reflect tooling: sol-xhigh-fast
reflect judgment, divergent, synthesizer: fable-high
arena runners: sol-xhigh-fast, opus-medium, opus-high, fable-medium, fable-high
arena cross-judge pool: sol-xhigh-fast, opus-medium, opus-high, fable-medium, fable-high
swarm workers: sol-xhigh-fast
architect runners: sol-xhigh-fast, opus-medium, opus-high, fable-medium, fable-high
interrogate reviewers: sol-xhigh-fast, opus-medium, opus-high, fable-medium, fable-high

Within T3, use its delegation tools for presets that need a different provider
or explicit options. Follow the session's orchestration instructions for
native agents and background work. Preset names must never be passed as
`model`, and provider options belong in `target.options`.

Outside T3, native Claude agents can use `model: opus` or `model: fable` with
the preset's `effort` when their tool schema supports those values. Native
Codex agents can use Sol with `reasoning_effort: xhigh` when supported. If
the runtime cannot express the requested model or its settings, report the
unavailable preset. Do not silently substitute a different model or effort.
