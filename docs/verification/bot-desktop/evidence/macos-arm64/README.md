# macOS arm64 verification

This run starts at PR head `78b6c8303f7d7b164dec200bf6782648baf446a4` on macOS 26.6.2 arm64, Apple M5 Pro. It preserves the separate amd64 evidence.

The native GPUI app runs in a private Lume clone of the documented stopped Cua base. All desktop input and captures target that clone. The real bot computers run in a disposable native host API's private Microsandbox home. A relay restricted to the clone's IP connects the clone's loopback desktop connection to the host API's loopback listener. No owner desktop, provider login, real website credentials, or installed API is used. The local website uses synthetic credentials.

The initial long worktree path exceeded macOS's Unix socket limit in Microsandbox. The rerun uses `/private/tmp/else62-arm64-fd6449` and the upgrade uses `/private/tmp/else62-v2-fd6449`. The initial failure remains in the evidence. Lume's automatic age resource discovery could not resolve offline metadata for all targets. Passing the documented `--age-i18n` path allowed the run to start.

Full local evidence is retained in `.else-dev/pr62-arm64` and `.else-dev/pr62-upgrade` in this checkout. The committed subset below includes the results, commands, representative captures, and test logs. Never copy the setup share's control token, desktop profile, owner token, or the legacy desktop route response into public evidence.

## Commands

Run from the repository root. The native architecture is checked with `uname -sm`, `sw_vers`, `file` on both binaries, and Docker image inspection.

```sh
cargo build --workspace --locked --bins --target-dir .else-dev/.build
scripts/build-guest-image.sh /private/tmp/else62-arm64-fd6449
cargo test --workspace --locked --target-dir .else-dev/.build --lib --bins
cargo test -p else-api --locked --target-dir .else-dev/.build \
  --test computers --test screens --test ws --test desktop_stream
cargo test -p else-desktop --locked --target-dir .else-dev/.build
ELSE_API_BIN="$PWD/.else-dev/.build/debug/else-api" python3 scripts/test-install-config.py -v
python3 .agents/skills/verify-else/test_lume.py
cargo fmt --all -- --check
cargo clippy --workspace --locked --target-dir .else-dev/.build
```

The primary API sets `ELSE_VM_IMAGE=else-guest:3` and `ELSE_VM_DESKTOP=1`. The separate upgrade API sets `ELSE_VM_IMAGE=else-guest:2` to create genuine legacy computers, then passes `else-guest:3` explicitly to the offline upgrade command. Its API is stopped before each upgrade. Provider executables are disabled except the repository's offline Codex fixture for onboarding.

```sh
.else-dev/lume-client/bin/python .agents/skills/verify-else/lume.py run \
  --base else-verify-base \
  --base-storage "$HOME/.local/share/else-verify/lume-base" \
  --api-binary .else-dev/.build/debug/else-api \
  --desktop-binary .else-dev/.build/debug/else-desktop \
  --age-i18n "$HOME/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/age-0.12.1/i18n" \
  --output "$PWD/.else-dev/pr62-arm64/lume" --session pr62
uv run --with websockets --with pillow scripts/test-desktop-stream.py \
  --api-url http://127.0.0.1:64838 \
  --data-dir /private/tmp/else62-arm64-fd6449 \
  --bot d0e98efd-1dfa-4ef3-ae35-c1a5910d5328 \
  --output "$PWD/.else-dev/pr62-arm64/evidence/real-vm-short"
```

Native steps use the Lume helper's identity check, fresh window snapshot, and guest MCP endpoint. Each action records the exact driver request and before/after captures. The early separate-key driver attempt did not prove pending composition. Its focus changes were a possible cause, not an established diagnosis. The atomic native dead-key sequence and menu-selected Japanese IME rerun below provide the passing composition evidence.

## Display and verification limits

The initial clones exposed only 1x modes. The fourth owned clone adds a private CoreGraphics virtual HiDPI display. Its 1440x900 point mode has 2880x1800 pixels, and the GPUI window reports 1100x720 points with a 2200x1440 screenshot and scale 2. This proves the tested 2x path in the isolated guest. It does not establish physical Retina panel behavior. No owner desktop or real provider or website credentials are used.

The guest's 20 ms keymap settle is empirical. It does not guarantee a stalled application will process a mapping before a press. The release acknowledgement means X11 applied input releases, not that Chromium rendered or saved its state. A stalled guest or database can exhaust the API shutdown grace period.

A narrow crash limitation remains in the port-based release acknowledgement. A killed websockify handler, server-initiated close, reused source port and delayed old acknowledgement can let a new handler accept the old acknowledgement. Independent review reproduced the identity failure with real TCP and forced same-port reuse. Normal random port allocation and real x11vnc hook timing were not reproduced. The requested graceful hand-back, disconnect and API-stop paths passed; this is not an all-crash ownership guarantee. See the [probe](guest-keymap/pr62-guest-stale-ack-probe.py) and [result](guest-keymap/pr62-guest-stale-ack-probe.log).

## Native input evidence

The committed native action records include guest MCP requests for clicks, typing, key presses, scrolling, dragging, and resizing. The same browser helper reads the synthetic login after hand-back. The guest reports `scrollY=1200` after native scrolling and no held buttons or keys after a drag into a letterbox margin. A CDP helper remains blocked during Take over and completes after Watching is selected.

A raw native Option-E, E sequence, posted atomically through CoreGraphics inside the owned Lume guest, commits `é` to the remote browser. The driver's separate `press_key` calls did not establish this result. The literal `native-é漢字` journey separately proves committed Unicode text. These are distinct checks.

The guest's actual mode initially reports 1024x768 despite the Lume configuration's 1440x1000 setting. Enlarging a window beyond the actual display produced screenshots of off-screen content but could not establish physical clicks there. Resizing to 900x600 was independently confirmed by the driver and allows the preview to reopen through a physical click. Oversized-window attempts remain in the local evidence as unsuccessful harness actions.

## Recent cookies and image upgrade

A real old image 2 computer lost a cookie set immediately before the old shutdown path. The fix closes Chromium with SIGINT before the offline upgrade's normal checkpoint. New image 3 also configures supervisor to stop Chromium with SIGINT. The version 2 fixture retains its original SIGTERM configuration, so this checks compatibility with a genuine older image.

The parent reran the upgrade with its newly built API. Its persistent cookie, localStorage value, and home file survived. Its API stopped before `upgrade-computer` ran. See `cookie-upgrade-parent-verified.json` and its log. The pre-fix failure is retained as `cookie-upgrade-baseline.json`. Ordinary stop and backup of an unupgraded version 2 computer still use its older guest shutdown behavior; upgrade it to receive the guest fix.


## Earlier font-image checks

The earlier guest image is `sha256:c362a5cb8b4f470d909815bb548658b7dbf6de5e1d90ee8b543ee5d89bac0f34`, built for arm64 and loaded as `else-guest:3` into `/private/tmp/else62-arm64-fd6449/microsandbox`. It contains `fonts-noto-cjk` and the Chromium SIGINT stop configuration. [Architecture and binary hashes](architecture.json) identify the final native build. The shutdown run preceded comment-only corrections. The final patched guest and native reruns are recorded below.

```sh
docker run --rm --user elsebot -v "$PWD/infra/guest:/guest:ro" \
  --entrypoint python3 \
  sha256:c362a5cb8b4f470d909815bb548658b7dbf6de5e1d90ee8b543ee5d89bac0f34 \
  /guest/vnc/test_else_vnc_targets.py
uv run --with websockets --with pillow scripts/test-desktop-stream.py \
  --api-url http://127.0.0.1:64838 \
  --data-dir /private/tmp/else62-arm64-fd6449 \
  --bot 3d54093f-f74a-4616-a9ff-e9b1bc235664 \
  --output "$PWD/.else-dev/pr62-arm64/evidence/real-vm-final"
```

| Check | Outcome and evidence |
| --- | --- |
| Native API and GPUI build | Mach-O arm64, [build log](build-final.log) and [hashes](architecture.json). |
| Rust unit checks | API 122 and desktop 128, [log](rust-unit-final.log). |
| Rust integration | Computers 16, desktop stream 10, screens 3, WebSocket 10, [log](rust-integration-final.log). |
| Guest browser lock | 7 passes against the earlier font image, [log](guest-lock-final.log). |
| Installer | 40 passes with the native API, [final log](installer-final.log). The earlier run skipped one because `ELSE_API_BIN` was unset. |
| Lume helper | 8 passes, [log](test-lume.log). |
| Real guest journey | [Passed on the final image](real-vm-final-image.json), including cookie after explicit wake. |
| Watching input | Pointer, buttons, and keys [before](r2-watch-before.json) and [after](r2-watch-after.json) are equal. |
| Japanese IME | The same 60 physical K/A input pairs gave [59 before](native-ime-long.json), [60 in reference composer](composer-ime-reference.json), and [60 after fix](native-ime-fixed-gui.json). |
| Modifier release | Ctrl down, A down, Ctrl up, A up left key38 held before the fix. [Final native events](key-order-events.log) show the key-up with Ctrl released. |
| Native shutdown | [Before](shutdown-final-before.json) held key114; [after API exit](shutdown-final-result.json) has no held key and zero leases. [Stopping](native-api-stopped.png) and [retry](native-api-reconnected.png) captures. |
| Bot switch | Previous viewer lease [1 before](bot-switch-before.json), [0 after](bot-switch-after.json). |
| Idle session | [50 seconds](idle.json), same running viewer. |
| CJK rendering | [Readable native preview](native-cjk-fonts.png), [browser value](font-native-text.json). This font check claims the CJK glyphs only. |
| Offline version 2 upgrade | [Parent rerun](cookie-upgrade-parent-verified.json) preserved a fresh persistent cookie, localStorage, and file. |

The shutdown regression suite uses real TCP with a fake guest. The separate native shutdown check uses the actual GPUI app and real guest. It posts Right arrow down through CoreGraphics inside the owned Lume guest, checks X11 key114, sends SIGTERM to the disposable host API's verified PID, waits for process exit, and queries X11 directly through the private Microsandbox home. It also reads the disposable database in read-only mode. The [request](shutdown-final-request.json) and [result](shutdown-final-result.json) contain no authorization token.

The SQLx cancellation [proof source](commit-race/src/main.rs) and [manifest](commit-race/Cargo.toml) reproduce a COMMIT completing after its future was cancelled. The [baseline](commit-race-baseline.log) leaves one lease; the [writer barrier](commit-race-fixed.log) sees and releases it. Independent Sol [review](shutdown-review-tests.log) reran all 10 stream tests and both proof variants. Blocked database cleanup or a stalled guest can still exhaust the API shutdown grace deadline.

The native [action records](native-actions.json) retain the exact guest MCP arguments and delivery outcomes. A driver acknowledgement alone is not counted as a feature pass. Native screenshots and guest state queries establish the reported outcomes. Command maps to Linux Super; Control drives Linux browser shortcuts. Option produces macOS text and dead keys, so guest Alt-letter mnemonics remain unavailable through this path. Text commits above 4096 characters show a refusal instead of overflowing the local command queue.

The first Lume clone expired after the helper's one-hour limit and was removed. Later clones continued the native checks, and the fourth supplied the virtual 2x display. The existing base stayed stopped and unchanged. All four runs keep separate cleanup proofs. Full logs and unsuccessful setup attempts remain local; the committed subset records the reported results.


The Japanese comparison establishes character retention, not identical conversion. The viewer samples contain a mix of hiragana and katakana, while the regular composer keeps a different marked buffer and conversion. The unmark correction is supported by one actual menu-selected IME rerun, in addition to the source review.

The two HTTP 404 entries in `key-order-events.log` are the local fixture page's key-event beacons. Their query strings report the key-down with Control held and key-up after Control was released. The final line is the guest's held-key query. The missing beacon route returns 404 after the HTTP server has logged the event; 404 is not a successful application API call.

A diagnostic relay was added after step r2-40, so `native-key-wire.jsonl` does not cover the earlier r2-39 retype. The complete mixed-sample key sequence belongs to r2-43. The later lone keysym233 belongs to r2-46. The earlier retype is not counted as a resolved accent check. The final CJK font proof and the accent investigation are separate.

The font package remains in a separate pinned-snapshot resource layer so font-only changes can reuse the large compiler toolchain cache. The reviewer suggested folding it into the main install list; this run keeps the explicit resource boundary after building and verifying that shape.

## Final patched-image run

The final arm64 image is `sha256:984912971d717ec71939681f58ef600a62a0dd1c8eb15085c34574747da0a542`, loaded separately into both disposable API homes. The guest source commit is `04166ee`. [Native binary hashes](architecture.json) identify the actual deployed Mach-O files. The [guest build log](guest-keymap/build-final.log) records the pinned Debian x11vnc rebuild and load. [Guest commands and original gates](guest-keymap/README.md), [real-VM gate results](guest-keymap/results.jsonl), and the [X-level lever](../../../../../infra/guest/x11vnc/test_keymap_settle.py) retain the reproduction details.

Independent Sol review found that forced cleanup also needed to clear x11vnc's internal modifier trackers. The [baseline reconnect test](guest-keymap/modifier-baseline-final.log) typed `a`, then re-pressed Shift. Resetting both trackers fixes all three paths. The [final lever](guest-keymap/lever-final-image.log) passes 21 cases, including the new forced-disconnect/reconnect check, recycling, held keys and 4096-character cancellation. The [browser lock suite](guest-keymap/lock-final-image.log) passes 12 cases, including EOF before acknowledgement, missing acknowledgement and wrong-port acknowledgement.

The parent reran [the full real-guest journey](real-vm-final-image.json) on fresh bot `6e312fbb-a855-4601-a060-a0153cd100d9`. The [command log](real-vm-final-image.log) includes native guest pixels, input-free Watching, browser ownership, synthetic login, owner stop, no implicit resume, explicit wake and cookie persistence. The harness now waits for a fixture click beacon before hand-back. Hand-back intentionally drops pending presses, so sending a whole login and immediately switching modes was an invalid completion check. The earlier failed run remains local. The beacon does not type, click or bypass the browser lock.

A genuine old image 2 with its original TERM shutdown configuration was upgraded to this final image with its API stopped. The [result](cookie-upgrade-final-image.json) and [command log](cookie-upgrade-final-image.log) preserve its fresh persistent cookie, localStorage and home file.

The final workspace [command manifest](checks-head.json) records successful native build, Rust tests, check, format and clippy. Rust code was at `42dd19f` for that run. The subsequent small native layout and reverse-Tab changes have separate desktop 128-test reruns. Installer [40 cases](installer-head.log) and Lume helper [8 cases](test-lume-head.log) passed. Existing unrelated clippy warnings remain.

The early native Shift+Tab action acknowledgements did not prove reverse focus traversal. The explicit 2x rerun found plain Tab reached Chromium with Shift cleared and moved from password to the login button. The final native mapping sends X11 `ISO_Left_Tab`, with a local mapping assertion and a repeat of the actual focus journey.

## Native 2x rerun

The [wake capture](native-2x-wake.png) opens the expanded viewer after one asleep-card click. A native CoreGraphics event carrying [100 UTF-16 units](native-actions.json) produces [100 exact distinct CJK characters](retina-100-commit.json), with a [live preview](native-2x-100.png). This is committed text, separately from the earlier actual Japanese marked-composition check.

The [synthetic native login](retina-login.json) has `native-é漢字かな` in both the field and saved session, with the matching encoded cookie. The [helper](helper-resumed-final.json) waits during Take over and reads the same session after Watching. The [reverse-Tab baseline](retina-backtab-baseline.json) focuses `login`; [after the mapping fix](retina-backtab-fixed.json) it focuses `user`, and a native suffix replaces the selected user value. The local HTTP key beacons return 404 after logging the event. Their query strings establish Chromium's received key and Shift state; 404 is not counted as an API success.

After [resizing](native-2x-resized.json), GPUI reports 901x776 points, 1802x1552 pixels and scale 2. Native pointer input selects the password field, Control+A and typing replace it with `resized-password`, and [scrolling](retina-resized-state.json) reaches 960 pixels. A [drag into the top letterbox margin](native-2x-drag.png) leaves [no button or key held](retina-drag-state.json). Watching [before](retina-watch-before.json) and [after](retina-watch-after.json) are identical after native pointer, text and scroll attempts.

The final native shutdown [request](retina-shutdown-request.json) holds Right arrow and two leases. After the verified disposable API exits, [X11 and SQLite](retina-shutdown-result.json) show no held input and no active lease. The [expanded stopping message](native-2x-stopping.png) remains visible. The footer-only error attempt was clipped; the final correction renders the reason inside the placeholder. No input is sent by viewing that failure.

The [native retry](native-2x-retry.png) returns to a live viewer. The [final idle session](idle-final-image.json) stays connected for 50 seconds. Window closure releases a [held Right arrow](retina-close-before.json), leaving [no held input](retina-close-after.json) and [zero leases](retina-close-computer.json). These are actual native actions on the final image.

All four owned Lume clones were removed. The [fourth cleanup](lume-fourth-cleanup.json) confirms the clone is absent and the original base remains stopped with unchanged identity. The [primary API home](primary-cleanup-after.json) and [upgrade API home](upgrade-final-cleanup-after.json) contain no runtimes after cleanup. Both disposable APIs and the clone-restricted relay are stopped. Build caches and local evidence remain. The owner desktop was never an automation target.

[Independent reviews](review.md) describe accepted findings and their reruns. The [append-only decision trail](decisions.tsv) preserves corrections to earlier interpretations. The [checkpoint](checkpoint.md) records model roles, risk and final cleanup. Owner-authored guest gate timestamps use local UTC+4; the host command manifests use UTC. [Artifact checks](artifact-checks.json) found no known disposable credential bytes and no missing Markdown file targets.
