---
name: control-ui
description: Drive and inspect this Rust application's native desktop UI or its guest browser through existing local harnesses and runtime tools. Use for UI reproduction, screenshots, accessibility checks, and visual verification.
---

Read the [local runtime rules](../../rules/plugin-compatibility.md) and [model presets](../../rules/pstack-models.md) before following this workflow.

# Control UI

Verify behavior on the real surface the change affects. Reuse this repository's
harnesses and the current session's UI tools. Read the
[development guide](../../../CONTRIBUTING.md) for the app's launch and test setup.

## Setup

1. Identify the surface: native Rust desktop, service-backed flow, or guest browser.
2. Inspect existing test and demo scripts before choosing a driver.
3. Follow the session's desktop containment rules before launching an app,
   running GUI tests, taking screenshots, or sending input.
4. For native desktop work, use the documented isolated desktop harness.
   For browser work, follow the session's browser-tool rules and use the
   repository's existing browser controls when appropriate.
5. Confirm the app is healthy and the selected window or page is the intended one.

## Interaction loop

1. Capture a fresh screenshot or accessibility snapshot before acting.
2. Select a target using accessible roles, labels, or stable app markers.
   Use coordinates only with a fresh screenshot and known window geometry.
3. Perform one action, then capture the resulting state.
4. Verify the expected behavior and collect the relevant logs or native profile.
5. Keep test data disposable and save evidence outside the repository unless
   the project documents another location.
6. Stop processes and clean up temporary sessions when finished.

If no suitable driver exists, report the missing capability and the affected
verification gap. Do not add an unrelated application stack just to run a probe.
