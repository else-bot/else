# Native app settings checkpoint

The user selected conversation setup A, removed the watched-folder selector, and requested a large app-settings modal with left navigation, scrolling content, an email category, and all server connection controls moved behind Settings.

The smallest native slice replaces Change server with Settings, reuses the current verified server connection editor, and adds an honest email-account empty state. The original complete email integration remains pending. No mailbox credentials or fake connection actions are added in this slice.

Data shape: setup presentation is either first-launch onboarding or app settings with a typed Server/Email section. The window shell remains the owner of the existing connection draft and attempt. Modal focus and dismissal use GPUI's existing dialog component.

risk: no change to server validation, persistence, owner-token treatment, or provider execution. Presentation now covers the existing messaging view in a modal and preserves its hidden/read-acknowledgement behavior. Review must check dismissal, notification navigation, and successful server replacement.

Coordinator and implementation owner are gpt-6.1-sol at xhigh priority. One Fable-high independent reviewer checks correctness and comments. No Opus slice is planned for presentation using the existing connection path; reclassify if credential or connection lifecycle guarantees change.

Throughput checkpoint: source tracing blocks implementation; prototype edits are independent; the Shell owns settings and draft mutations; one owner is the smallest safe decomposition because navigation, dialog closure, and server completion share that state. Native verification uses a disposable isolated omabox session, not the user's desktop.

## Verified behavior

The final native binary built without warnings using two Cargo jobs. The initial unrestricted check was killed with exit 137; bounded builds passed. A toolkit icon import mismatch and subsequent scrollbar trait/focus-borrow errors were corrected before the final build.

Native verification used the documented isolated launcher. Final evidence lives under `.else-dev/runs/20261105-094245-94640396/settings-evidence/`. This is a disposable synthetic-provider session, not a live mailbox or the user's installation.

- Settings opens by button and Ctrl+, with the Server menu focused.
- Email navigation retains focus. Escape closes the modal and restores focus to Settings.
- Twelve successive Tab moves remain inside the modal.
- Local-server navigation can be dismissed. Connecting locally succeeds and closes Settings.
- At 640 by 420, content scrolls to Save connection while the header and navigation stay fixed.
- Visible scrollbars and labelled Server address / Owner key inputs are present.
- The earlier native run rejected a fictional malformed owner key without replacing the active connection.
- Four existing connection tests passed in omabox, covering credential prefill, rejected keys, and stale/current attempts.
- The revised A prototype passed six provider paths at both 728 and 390 widths and asserts there is no watched-folder selector. See selected-flow-verification.json.

One native bug was found and fixed before handoff: removing a focused input on Email navigation left Escape without a dialog focus target. Explicit navigation focus and a tracked modal root fix it. A subsequent initial-focus change makes small-window setup open at the section menu instead of an off-screen input.

The native email account-connection card and connector API remain open work. Email settings is a bootstrap empty state. macOS menu registration and Cmd+, are wired in source but were not exercised on macOS in this Linux run.

## Independent review and final verification

Fable high completed the source review and agreed with the risk classification. All six bounded findings were resolved by the parent. See settings-review.md. Final post-review native evidence lives under `.else-dev/runs/20261105-095016-cd6e5691/settings-evidence/`.

The reviewed binary opens on the matching local configuration, keeps an existing dialog during the settings shortcut, preserves the actual conversation draft on unchanged Save, closes Email through Escape, and keeps the header and navigation fixed while scrolling at 640 by 420. The parent corrected two observation-method assertions rather than changing working application logic.

Experience First changed the initial page, focus behavior, visible scrollbar, and no-op save. Model the Domain kept onboarding and typed settings sections distinct. Prove It Works required the native window, actual draft text, and final binary checks rather than HTML as a proxy.

This slice remains in the working tree alongside the previous research artifacts. It is not published as a complete email integration.
