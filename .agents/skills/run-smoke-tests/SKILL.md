---
name: run-smoke-tests
description: Run Rust smoke checks and existing integration tests, diagnose failures, and verify focused fixes.
---

Read the [local runtime rules](../../rules/plugin-compatibility.md) and [model presets](../../rules/pstack-models.md) before following this workflow.

# Run smoke tests

1. Read the workspace manifests and the
   [development guide](../../../CONTRIBUTING.md). Identify the smallest
   existing check that exercises the changed behavior.
2. Check the affected Rust targets and run the relevant tests. On Linux,
   read the installed omabox skill and use the shared runner:

   ```sh
   cargo check --workspace --all-targets
   scripts/dev.py test -p <crate> -- <test-filter>
   scripts/dev.py test -p <crate> --test <integration-target>
   scripts/dev.py test --serial
   ```

   `scripts/dev.py test` runs the full workspace by default. It builds on
   the host and runs each executable in an isolated box with dependency
   assets and private scratch storage. `--serial` limits each executable
   to one test thread. Read `results.json`, `summary.txt`, and the logs in
   the printed `.else-dev/tests/<run>/` directory after a restart before
   repeating work. On macOS, use `cargo test --workspace --locked` or a
   focused Cargo target inside the owned verification guest.

3. Use the checked-in integration or end-to-end runner when the change covers
   services or desktop flows. Follow the session's containment rules before
   tests that touch the desktop, notifications, portals, or keyrings.
4. On failure, inspect the logs and reproduce the cause before editing code.
   Apply a focused fix and rerun the affected check.
5. Report the commands, results, and any untested behavior. Do not claim that
   compilation alone proves a user flow works.

Prefer deterministic waits and observable outcomes. Quarantine a test only
when the user explicitly requests it.
