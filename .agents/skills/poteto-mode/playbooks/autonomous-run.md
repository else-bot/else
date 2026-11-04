### Autonomous run

**You own the exit condition. Define done, then drive to it without stopping.**

1. State the exit condition as a checkable predicate before the first iteration (tests green, repro fixed, all N PRs merged, pixel-diff zero).
2. Use the runtime's supported event subscription or scheduling tools. In T3, PR waits use its PR watcher and end the turn. Schedule recurring work only when the user asks for it. If no continuation mechanism exists, report the limitation and preserve a checkpoint; do not claim a background run is armed.

**Reply:** the exit condition, iterations run, what landed, what was discarded, final predicate state.
