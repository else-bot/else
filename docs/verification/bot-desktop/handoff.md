# Verify the bot computer viewer

The owner can wake a bot computer from its right sidebar, preview its desktop, expand it inside the app, and switch between Watching and Take over. Watching sends no input. Take over shares the visible Chromium profile with the bot's CDP helpers.

## amd64 results

The checks ran on Linux x86_64 with Microsandbox 0.7.13, Chromium 156, and the native GPUI client inside a private omabox. The API and VM used disposable state under `/tmp/else-desktop-live-76350`. No provider account or real website login was used.

- The native journey covered the sleeping card, explicit wake, live thumbnail, expansion, Watching ignoring typing and clicks, take-over typing, Tab between username and password, a pointer click on Sign in, scrolling, a drag released over the letterbox, and hand-back. The guest reported no mouse button held after the drag. CDP read the resulting cookie and localStorage session in that same browser. See [native observations](evidence/native-results.json).
- The repeatable real-VM journey passed against `else-guest:3`. It also passed for a newly created bot on its first VM boot. It checks pixels, Watching, the guest browser lock during take-over and after hand-back, manual login visible to CDP, owner stop, no implicit resume, explicit wake, and Chromium login persistence across stop/start. See [upgrade journey](evidence/real-vm.json) and [first-boot journey](evidence/first-boot.json).
- The image upgrade preserved the guest home and browser profile. The previous computer was kept stopped for rollback. See [upgrade output](evidence/upgrade.log).
- Native tests passed 136 cases. API unit tests passed 123, computers 15, screens 3, owner WebSocket 10, and desktop stream 5. The guest ownership suite passed 7 cases, including real Node and Python CDP helpers, lock contention, first-use file ownership, and killed-handler cleanup. Installer image selection passed 12 cases. Logs are in [evidence](evidence/).
- Workspace check, build, formatting, and diff checks passed. Clippy had no errors; unrelated existing style warnings remain.

The API stream suite uses an RFB server fixture with real TCP, WebSocket, VNC password authentication, pixels, and input records. It does not substitute for the real guest or native journeys above.

## Run the real-VM journey

Use a disposable API installation with Microsandbox and the version 3 guest image loaded into its private runtime home. The script stops and wakes the supplied bot. Close any other viewers first. It uses only a local guest website with synthetic credentials.

```sh
uv run --with websockets --with pillow scripts/test-desktop-stream.py \
  --api-url http://127.0.0.1:PORT \
  --data-dir /absolute/disposable/data \
  --bot BOT_UUID \
  --output /absolute/evidence
```

The output includes `results.json` and a full-screen PNG. The script reads the generated owner key from the data directory and sends it only as an authorization header. It uses the API's recorded runtime name, including after an image upgrade.

Run the guest lock suite against the built image:

```sh
docker run --rm --user elsebot \
  -v "$PWD/infra/guest:/guest:ro" --entrypoint python3 \
  else-guest:3 /guest/vnc/test_else_vnc_targets.py
```

## macOS arm64 takeover

macOS execution remains a required handoff check. No macOS result is claimed by the amd64 run.

1. Use the repository's [Lume workflow](../../../.agents/skills/verify-else/lume.md) for an isolated macOS desktop. Build both workspace binaries natively. Build the guest image on arm64 with `scripts/build-guest-image.sh DATA_DIR`, and configure `ELSE_VM_IMAGE=else-guest:3` and `ELSE_VM_DESKTOP=1`.
2. For an existing version 2 computer, close viewers and jobs, stop its API, and run `ELSE_DATA_DIR=DATA_DIR else-api upgrade-computer BOT_UUID else-guest:3`. Start that API again. Ordinary use preserves the old image until this explicit upgrade.
3. Run the real-VM journey above. Run the same Rust targets and guest lock suite. Keep its output separate from the amd64 evidence.
4. Open the native app and the right sidebar's Computer tab. Inspect the asleep card without waking it. Click once, wait for the preview, then open it. Confirm the expanded viewer stays inside the app window and starts in Watching.
5. Click and type while Watching. The guest pointer and page must stay unchanged. Choose Take over, fill the local login form with native input, use Tab and Shift+Tab, submit with a pointer click, scroll, and drag. Repeat after resizing the app and on a Retina display. Test command/control shortcuts and non-US keyboard input explicitly.
6. Return to Watching. A bot CDP helper must read the login cookie. A helper waiting during take-over must resume after hand-back. Close the sidebar, switch bots, and close the app window; confirm their viewer leases and control ownership end.
7. Stop the computer while viewing it. The app must show the asleep state. Click to wake it, then confirm the Chromium login survived. Disconnect/reconnect the API and verify the expanded retry action.

## Limits

Frames use PNG at a maximum of five changed frames per second. This is suited to observation and website login. Smooth video streaming is outside this change. Voxl was researched as a future H.264 transport, but is not installed or bundled.

Version 2 computers cannot watch without briefly connecting to their controlling VNC server. The app reports that the image needs an upgrade. Guest programs that bypass the supplied browser helpers and connect directly to CDP do not honor the advisory browser lock.
