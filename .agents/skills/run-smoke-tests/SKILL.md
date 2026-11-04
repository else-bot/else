---
name: run-smoke-tests
description: Run Rust smoke checks and existing integration tests, diagnose failures, and verify focused fixes.
---

Read the [local runtime rules](../../rules/plugin-compatibility.md) and [model presets](../../rules/pstack-models.md) before following this workflow.

# Run smoke tests

1. Read the workspace manifests and the
   [development guide](../../../docs/development.md). Identify the smallest
   existing check that exercises the changed behavior.
2. Check the affected Rust targets and run the relevant headless tests:

   ```sh
   cargo check --workspace --all-targets
   cargo test -p <crate> <test-filter>
   ```

3. Use the checked-in integration or end-to-end runner when the change covers
   services or desktop flows. Follow the session's containment rules before
   tests that touch the desktop, notifications, portals, or keyrings.
4. On failure, inspect the logs and reproduce the cause before editing code.
   Apply a focused fix and rerun the affected check.
5. Report the commands, results, and any untested behavior. Do not claim that
   compilation alone proves a user flow works.

Prefer deterministic waits and observable outcomes. Quarantine a test only
when the user explicitly requests it.
