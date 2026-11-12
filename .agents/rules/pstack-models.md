# Pstack model configuration

This repository's role choices override pstack's defaults and any user-level
`pstack-models.mdc` or `pstack-models.md`. Edit this file directly to change
presets and role assignments. The effort is set per preset below. Keep the
configured effort and speed when delegating.

Sol high fast is the default for coordination, coding, explanations, and
synthesis. Fable high provides independent design and review. Opus medium
owns risky decisions and their hardest bounded implementation slice under
the routing policy below. Use Opus high only when the user requests it.

Role values are preset names, not API model IDs. Resolve each name through
this table before spawning an agent. Panel lists run one agent per entry.
The arena cross-judge pool selects one judge from another model family.

The JSON records the T3 `delegate_task` target for each preset. GPT fast mode
uses `serviceTier: priority`. Claude uses `effort` for its reasoning level.
Recheck provider capabilities before using a model in a new environment.

```json
{
  "sol-high-fast": {
    "providerInstanceId": "codex",
    "model": "gpt-6.1-sol",
    "options": { "reasoningEffort": "high", "serviceTier": "priority" }
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
    "model": "claude-fable-5-2",
    "options": { "effort": "medium" }
  },
  "fable-high": {
    "providerInstanceId": "claudeAgent",
    "model": "claude-fable-5-2",
    "options": { "effort": "high" }
  }
}
```

coordinator: sol-high-fast
feature, refactoring: sol-high-fast
bug-fix: sol-high-fast
perf-issue: sol-high-fast
hillclimb: sol-high-fast
judgment and prose: sol-high-fast
hardest tasks: opus-medium
risk decision and implementation: opus-medium
escalation: opus-medium
routine reviewer: fable-high
Claude-authored code reviewer: sol-high-fast
how explorer: sol-high-fast
how explainer: sol-high-fast
why investigators: sol-high-fast
why synthesizer: sol-high-fast
reflect tooling: sol-high-fast
reflect judgment: sol-high-fast
reflect divergent: fable-high
reflect synthesizer: sol-high-fast
arena runners: sol-high-fast, fable-high
arena cross-judge pool: sol-high-fast, fable-high
swarm workers: sol-high-fast
architect runners: sol-high-fast, fable-high
interrogate reviewers: sol-high-fast, fable-high

## Routing policy

Explicit user model restrictions and requested panels take precedence.
Role assignments guide delegation. They do not change an active session's
model. Use the coordinator preset when starting a new coordinator.

After tracing the affected code, record `risk: <changed invariant or none>`
in the task checkpoint. Recheck it against the final diff. Route a decision
to Opus medium when the change alters any of these guarantees:

- Actor ownership, concurrency, backpressure, or cancellation.
- Permission checks or secret handling.
- Database migrations, durable identity, or persisted lifecycle transitions.
- Cross-provider execution or session lifecycle.
- Native OS packaging, process launch, or terminal independence.

Changing labels, padding, or presentation in one of these files does not
trigger Opus. For example, cron display wording stays routine when schedule
execution is unchanged. Message steering triggers Opus when it changes
cancellation or pending-secret withdrawal, even if requested as a UI fix.

Escalate a concrete cross-model disagreement that source evidence cannot
settle. Also escalate after two failed focused fix or verification attempts
with root-cause evidence. An unavailable test platform is a verification gap,
not a reason to repeat the task on another model.

Give Opus one compact assignment with the decision, source locations,
invariants, alternatives, evidence, owned files, and acceptance checks. Combine
the decision and the hardest bounded code slice when a slice exists. The
routine owner handles surrounding code and integration. Record why a
decision-only assignment has no code slice. Do not replay the whole transcript
or add routine Opus tracing, prose, cosmetic review, or a final pass to every
task. Opus-authored code receives independent Sol review. Raise effort to
Opus high only on an explicit user request.

## Delegation policy

For routine work, the active owner traces, implements, and verifies the
change. Use one independent reviewer for correctness and comments. Fable
reviews Sol-authored work. Sol reviews Claude-authored work. Fable and Opus
are the same model family for this purpose. If user restrictions prevent a
different-family reviewer, use a fresh permitted reviewer and report the gap.
For mixed Sol and Claude work, assign review by authored scope so each scope
has a reviewer from the other family, subject to the same user restrictions.
The reviewer also checks the risk classification. Use these reviewer roles
for pre-merge verification. Preserve required native and platform checks.

Automatic architect or arena work requires a substantive unresolved choice
about state, ownership, interfaces, or behavior. Crossing a function boundary,
touching several files, or choosing error wording does not suffice. Reuse
grounding already gathered. Keep simple how exploration and synthesis with
the active owner. Combine comment review with correctness review. Preserve
explicitly requested design variants, panels, and specialist checks.

During the initial rollout adopted on 2026-11-04, the coordinator records the
actual presets, risk classification, Opus decision and code ownership, rework
rounds, and verification results in each task checkpoint. Review the first ten
completed checkpoints for missed risk and unnecessary fanout before expanding
the policy. Routine tasks should use no Opus. Qualifying risky changes should
give Opus substantive decision and coding work unless the user restricts it.
Compare actual provider usage on similar work before claiming credit savings.

## Runtime targets

Within T3, use its delegation tools for presets that need a different provider
or explicit options. Follow the session's orchestration instructions for
native agents and background work. Preset names must never be passed as
`model`, and provider options belong in `target.options`.

Outside T3, native Claude agents can use `model: opus` or `model: fable` with
the preset's `effort` when their tool schema supports those values. Native
Codex agents can use Sol with `reasoning_effort: high` when supported. If
the runtime cannot express the requested model or its settings, report the
unavailable preset. Do not silently substitute a different model or effort.
