---
name: poteto-help
description: Explain the locally installed pstack skills, model configuration, and workflows. Use for pstack help and setup questions.
disable-model-invocation: true
---

Read the [local runtime rules](../../rules/plugin-compatibility.md) and [model presets](../../rules/pstack-models.md) before following this workflow.

# Poteto help

Answer the question and link the local file that supports the answer. For a
help question, explain the workflow without starting it. For an action request,
read the matching skill and do the work.

The shared installation is `.agents`, with `.claude` pointing to it. Skills are
in `.agents/skills/<name>/SKILL.md`, with supporting prompts and scripts in
the same skill folder. Use local names such as `/poteto-mode`. If slash
invocation is unavailable, read the skill by path and follow it.

## Setup and model choices

Edit [the model settings](../../rules/pstack-models.md) directly. That file owns
the default roles, selective risk escalation, and delegation policy. Resolve
its presets before choosing a model. Keep model defaults out of skill prose.

Cross-provider delegation requires a runtime that supports it, such as T3.
Report unsupported capabilities without changing the requested models.

## Pick a workflow

| Goal | Local skill |
| --- | --- |
| Complete substantial work | [poteto-mode](../poteto-mode/SKILL.md) |
| Understand behavior or ownership | [how](../how/SKILL.md) |
| Investigate reasons and history | [why](../why/SKILL.md) |
| Explain work plainly | [teach](../teach/SKILL.md) |
| Explore designs before implementation | [architect](../architect/SKILL.md) |
| Compare competing implementations | [arena](../arena/SKILL.md) |
| Challenge a diff independently | [interrogate](../interrogate/SKILL.md) |
| Partition parallel work | [swarm](../swarm/SKILL.md) |
| Verify the real app | [create-verification-skill](../create-verification-skill/SKILL.md) |
| Create or update local instructions | [create-skill](../create-skill/SKILL.md) |
| Review and ship a branch | [review-and-ship](../review-and-ship/SKILL.md) |

For other goals, read the neighboring skill descriptions and select the closest
match.
Start a new task with `/poteto-mode` and a concrete completion check. Persistent
modes, scheduling, transcript access, and browser tools depend on the current
runtime; use [its local rules](../../rules/plugin-compatibility.md).
