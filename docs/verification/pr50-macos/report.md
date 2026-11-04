# PR #50 macOS verification

The goal is a macOS installation that opens without a terminal and recovers its
local API through its own launchd service. This report covers production bundles
and installed commands in a disposable Aqua VM. The merge decision stays with the owner.

**Recommendation: do not merge yet.** Gate 2 still has a Gatekeeper failure and
unverified notification attribution. Other results below apply only to the
conditions and revisions stated here.

## Revisions and environment

- Fetched PR head and baseline native binaries came from `1a9a8ed564db772b6e30980aa64edb3ccd98e0bc`.
- Final automated checks used `84b8601a5bd2723ebc090d08e7947c0da9be93ee`. Native journeys used
  `c4dd88be105a12fe70ee1a2a64323c69a33e61ea`; the later revision removes only comments
  and one test docstring. Executable hashes match the final build.
- Source changes fix reentrant native window closing, missing password-control
  icons, canonical-path assertions, and generated dotenv values containing spaces
  or quotes. The approved welcome layout is unchanged.
- Host build ran on macOS 26.6.2 arm64 with Rust 1.99.1. Guest ran macOS 26.5.2
  arm64 with an Aqua login session. This checkout used `.else-dev/.build`.
- The stopped Lume base had digest
  `sha256:7cb23670cad48a6723cf667bc63c444b83dc0c73756c23b614897127d5ba22ba`.
  The clone had its own randomized identity, private setup share, and no VNC endpoint.
- Only the disposable guest received app publications, launchd definitions,
  Dock changes, and GUI input. The owner's installation and services were untouched.

| Executable | SHA-256 |
| --- | --- |
| PR API, baseline and final | `df512d65e162d8c1f966e45ab8b9af0d2d738a5a3645d5ed2f6c440b36d77132` |
| Baseline desktop | `2fcfbc1697d0cc65c3aa446b7669601ee880147ceae7941687aeef60eb83ff39` |
| Update experiment desktop | `5dff600aeb1e0a69e66007b5375c12f572b02d5d992516dc2e674efaf655e537` |
| Final desktop | `5ad0b08164ddf352703798ea63f2ef92d263d12f829246dfe59f969744262d0b` |

All supplied binaries used Cargo's debug profile. They were copied into the actual
installation bundle by the production publisher. Release optimization was not tested.
The update experiment rebuilt the same PR source with a distinct rustc metadata
value. Its version remained 0.0.12. The fixture used version directories 0.0.13
and 0.0.14 to exercise switch and rollback code. It did not fetch or test public
release tags. Installed and published copies were compared with the supplied
binaries. The publisher ran without a signing transformation. The quoted installation's
[pre-reboot executable hashes](logs/production/quoted/hashes.json) identify its API.
The post-reboot shell probe mistakenly hashed the default root's API copy rather
than the quoted root's copy; both had the same pre-reboot API hash. The quoted
service path and authenticated recovery are recorded separately.

## Gates

| Gate | Result | Evidence and limits |
| --- | --- | --- |
| 1. Native build and automated checks | PASS | Workspace binary build, rustfmt, all 108 macOS desktop tests, and all 40 installer checks pass. The latter include actual setup with this PR's API. Workspace Clippy with all targets completes. Warnings are in unchanged files. See [logs](logs/). |
| 2. Production launch and terminal independence | FAIL | Finder, pinned Dock, installed PATH symlink from `/tmp`, repeated launch, `--version`, terminal exit, app/menu identity, and fixed Cmd+W/menu close/Cmd+Q pass. The linker ad hoc signature does not seal the bundle. A deliberately quarantined published copy is blocked as damaged with Gatekeeper enabled. No delivered native notification was observed, so attribution remains INCONCLUSIVE. Normal locally built, unquarantined installation bundles did launch. See [launch log](logs/02-finder-launch-stdout.txt), [terminal proof](logs/12-cli-terminal-stdout.txt), and [quarantine screenshot](screenshots/quarantine-blocked.png). |
| 3. One-click launchd startup and recovery | PASS | One click starts `bot.else.pr50-local`, authenticated capabilities succeed, the profile is mode 0600, and onboarding opens. The API survives app quit. Finder relaunch recovers a stopped API from the saved profile. No-autostart definitions are outside `~/Library/LaunchAgents`. After an actual reboot and Aqua autologin, all three fixture services were unloaded and ports 7878, 7889, and 7893 had no ready API. See the [executed probe](logs/reboot-probe.sh), [reboot proof](logs/reboot-proof.txt) and [post-reboot authenticated Finder recovery](logs/146-post-reboot-finder-stdout.txt). |
| 4. Configuration and credential isolation | PASS | Default and custom roots, bind 7889, custom data/key paths, root alias, simultaneous installations, an explicit development session, and app-only remote authentication pass. A root with spaces and an apostrophe also works after the dotenv fix, including the actual CLI database command and launchd startup. Local keys stay out of the remote form. Editing a prefilled dummy remote address clears its key. Invalid and rejected attempts preserve the existing profile. |
| 5. Failure and cancellation | PASS | Missing/unreadable key, rejected authentication, missing service definition, a non-listening service, timeout, repair/retry, and quitting during startup were exercised. Errors are actionable. The desktop/helper exit and no late profile save were checked. Back and Cancel preserve the active connection. [Exported-text secret scanning](logs/secret-scan.json) found no generated credential or control-token matches. Screenshots were inspected manually; OCR failed and is not claimed. |
| 6. Native UI and persistence | PASS | Normal and 640×420 layouts, traffic lights, scrolling, focused inputs, Enter submission, visible password toggle, and hand/text/resize cursors were inspected. A resized 310px sidebar persisted beside the resolved profile and survived Finder relaunch. Cursor images are native guest reads; they are not inferred from driver delivery. |
| 7. Update and rollback | PASS | A mapped old desktop remained usable after bundle replacement and sent a message through the real fixture adapter. After quitting, the pinned Dock launched the new executable. The actual `install_server` failure branch restored `current` and the published bundle, and restarted the prior API. The [Dock pin and click](logs/49-pinned-dock/) plus [new process/hash proof](logs/50-two-installation-processes-stdout.txt) identify the relaunched executable. Foreign public app paths select the private fallback. Unrelated app and backup markers survive. This used supplied native artifacts and the real installer functions, with package downloads and compilation performed separately. |

## Focused regression checks

`python3 scripts/test-install-config.py` needs `ELSE_API_BIN` set to this
checkout's native API to run its real dotenv-consumer check. The final run used
that setting and passed all 40 tests. Plain values stay readable; quoted values
round-trip through both installer and CLI readers without evaluating `.env`.

Run the native close regression with the pinned Lume Python environment:

```sh
.else-dev/lume-client/bin/python scripts/test-macos-launch.py \
    --run RUN \
    --app "/Users/lume/pr50/quoted install it's/Applications/else.bot.app" \
    --service bot.else.pr50-quoted \
    --output NEW_OUTPUT
```

The original PR executable fails the Cmd+W check. The final executable passes
Cmd+W, Close Window, and Cmd+Q, with an unchanged API PID. See the
[baseline failure](logs/baseline-native-regression.log) and [post-reboot final results](logs/149-post-reboot-final-close/results.json).

## Remaining blockers and exclusions

- Resolve the published bundle's invalid signature and verify the intended
  Gatekeeper behavior. No owner signing identity or Keychain credentials were used.
- Observe a real else.bot notification and confirm its attribution. Public API
  turns were sent while the app was backgrounded, but no notification or permission
  prompt was captured. This is an unverified gate, not a passing inference.
- The verification clone was stopped and deleted. Fresh cleanup metadata confirms
  the clone is absent and the base is stopped with unchanged identity. The helper's
  `/tmp` directory disappeared during reboot, so its second collection failed.
  Its evidence was collected before reboot, and production artifacts and post-reboot
  checks were exported separately. See [cleanup](logs/cleanup.json) and
  [pre-reboot collection](logs/pre-reboot-collection.json).
- The VM's development helper supplied transport and initial guest setup only.
  Its API and copied development app are excluded from production launch claims.
- No live provider accounts, Microsandbox computers, or remote backup services
  were tested. The composer used the offline Codex protocol fixture.
- Cursor attempts before explicit driver activation read a background arrow and
  were inconclusive. Driver actions refused for stale captures or a stale window
  ID are excluded from passing evidence. The decision trail names their replacements.

Screenshots expose only masked local keys or a public fixed dummy credential used
for password-visibility checks. Sanitized production profiles redact owner tokens.
The [decision trail](logs/decisions.tsv) and [sanitized production logs](logs/production/)
retain the checks and their limits.

The audit export preserves the original rows in [decision-source.tsv](logs/decision-source.tsv).
[decisions.tsv](logs/decisions.tsv) resolves their evidence pointers into this directory.
Fixture shell files record guest commands. They were sent through the identity-checked
Lume transport and must only be run inside a disposable guest.

[Independent review](review.md) found no blocking defect in the focused code changes.
Its evidence corrections and remaining test limitations are recorded with the report.

Full original AX trees and structured window captures are in
[full-capture-records.tgz](full-capture-records.tgz). Readable capture summaries
record their archive paths and SHA-256 digests. Screenshots and action records
remain directly browsable.
