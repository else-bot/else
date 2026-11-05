# Native settings review resolution

Reviewed by claude-fable-5-1 at high effort. The reviewer inspected source only and agreed that the change preserves the existing permission, secret, provider, and persistence guarantees. The parent performed native checks separately.

## Findings and fixes

- Settings now selects This machine when the configured local origin matches the active profile. The startup-only `LocalServer::owns` predicate additionally requires a managed service, so using it for this presentation choice would incorrectly exclude the documented development launcher. Native verification confirms the matching local page.
- Settings refuses to replace an active toolkit dialog. A stale presentation with no active modal is cleared before reopening. A native shortcut check confirms an existing Create bot dialog stays open. A pointer press on the title-bar button can first dismiss the earlier dialog through the toolkit's normal backdrop handling; that is a normal cancel path rather than programmatic orphaning.
- The local/remote toggle is only shown when a local configuration exists. Redundant Edit configuration controls are hidden inside Settings; first-launch onboarding keeps them.
- Saving an unchanged, already-connected profile closes Settings without replacing the messaging view. Native verification copies the actual fictional draft back through the private box clipboard and confirms it survives.
- Settings uses an immediate dialog entrance/exit so clearing its private form cannot briefly show an empty fading panel. No secret-bearing render tree is cached.
- The now-unreachable onboarding Cancel control was removed.

Zero new comments were added. No comments required deletion or restoration during review. No comment constraint encodings were needed. No new architecture panel or delegated re-check was required for these bounded fixes.

## Verification

The reviewed final binary passed native checks in `.else-dev/runs/20261105-095016-cd6e5691/settings-evidence/`. Earlier focus-cycle and local-reconnect checks are in the checkpoint's prior native evidence directory. All GUI work used owned isolated omabox sessions and the Synthetic provider.

Two verification assertions were corrected after inspecting their observation method. The accessibility tree does not include the textarea's value, so JSON text search could not establish draft preservation. A baseline screenshot and private-box copy/read proved the actual text survived. The earlier assumption that clicking the title bar must leave a dialog open also ignored the toolkit's normal backdrop cancellation; the shortcut check verifies non-interruption directly.

No live email provider or macOS native verification is claimed. The email category is an empty state. Account connectors and conversation setup cards remain open implementation work.
