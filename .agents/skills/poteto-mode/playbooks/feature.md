### Feature

**You own the design. Plan, review, verify.** Apply the [routing and delegation policy](../../../rules/pstack-models.md) before assigning implementation.

1. `how` over the affected subsystem.
2. Use `architect` for a substantive unresolved design choice or an explicit user request. Otherwise keep the existing shape and proceed without a design panel.
3. Write the throughput checkpoint as four todo items. A dimension that genuinely does not apply (single file, no fan-out) keeps its item with `n/a: <reason>` rather than being dropped:
   - **Blocking first steps.** Gates run before fan-out.
   - **Independent workstreams.** Disjoint files, services, or layers parallelize. Shared writes serialize.
   - **Shared mutable state.** Default to splitting the target (the **separate-before-serializing-shared-state** principle skill). Serialize only for real invariants.
   - **Smallest safe decomposition.** If one worker is best, name why.
4. Keep routine implementation with the active owner. Delegate only a useful independent workstream or the bounded risk decision and code slice selected by the routing policy. Give each delegate specific files, the named data shape per **principle-model-the-domain**, and acceptance checks. Preserve the role's configured preset and use the local runtime rules for delegation. Use **arena** only for a substantive unresolved design choice or an explicit request for competing implementations. One independent reviewer checks correctness, comments, and risk classification. A subagent forbidden to spawn owns its scope directly and returns it for independent review. No "standing by" reply that waits on a nested agent. Comments per **Comments**. Make surgical edits, re-ground against the source for upstream-derived files, and verify every consumer of a shared primitive. Commit liberally.
5. Verify on the matching surface. "Inconclusive" or wrong-surface is not a pass. Flag it.
6. Rebase into small, ordered commits. Stack follow-ups.
   Use the **sequence-verifiable-units** principle skill, building, verifying, and committing each small unit before the next.
7. If the design is contested, `interrogate` before shipping.
8. Run **Opening a PR**.

Code-coupled work (one feature, one migration) goes to a single owner with the checkpoint inline. That owner fans out internally after the blocking phase. Parent-level fan-out is for slices that produce independent artifacts (audits, cross-subsystem investigations, competing experiments). Rewrite the checkpoint at phase boundaries. Spawn a fresh owner rather than chaining interrupts.

**Reply:** what you built, what you chose and why, the throughput checkpoint, open decisions. Tables for design alternatives.
