# Verify the bot computer viewer

Use disposable API state and bot computers. Native macOS automation must run in an isolated desktop through the [Lume workflow](../../../.agents/skills/verify-else/lume.md). Use `scripts/fixtures/desktop-login.html` with synthetic credentials. The real-VM script serves it in the guest at `http://127.0.0.1:8421/desktop-login.html`.

## Build and upgrade

Build the native binaries and load image 3 into the disposable API's private Microsandbox home:

```sh
cargo build --workspace --locked --bins --target-dir .else-dev/.build
scripts/build-guest-image.sh /absolute/disposable/data
```

Configure that API with `ELSE_VM_IMAGE=else-guest:3` and `ELSE_VM_DESKTOP=1`. Existing computers retain their image. To upgrade one, close its viewers and jobs, stop its API, then run:

```sh
ELSE_DATA_DIR=/absolute/disposable/data \
	.else-dev/.build/debug/else-api upgrade-computer BOT_UUID else-guest:3
```

Restart the API and confirm the computer's files, Chromium cookie, and saved login survive. The previous runtime stays stopped for rollback.

## Run automated checks

```sh
cargo test --workspace --locked --target-dir .else-dev/.build --lib --bins
cargo test -p else-api --locked --target-dir .else-dev/.build \
	--test computers --test desktop_stream --test screens --test ws
ELSE_API_BIN="$PWD/.else-dev/.build/debug/else-api" python3 scripts/test-install-config.py -v
cargo fmt --all -- --check
cargo clippy --workspace --locked --target-dir .else-dev/.build
```

The build script prints the Docker image ID. Run the guest suites against that image:

```sh
GUEST_IMAGE=IMAGE_ID_FROM_BUILD
docker run --rm --user elsebot -v "$PWD/infra/guest:/guest:ro" \
	--entrypoint python3 "$GUEST_IMAGE" /guest/vnc/test_else_vnc_targets.py
docker run --rm --user elsebot -v "$PWD/infra/guest:/guest:ro" \
	--entrypoint python3 "$GUEST_IMAGE" /guest/x11vnc/test_keymap_settle.py
```

Run the real-VM journey with other viewers closed. It stops and wakes the supplied bot, tests Watching and browser ownership, and checks login persistence:

```sh
uv run --with websockets --with pillow scripts/test-desktop-stream.py \
	--api-url http://127.0.0.1:PORT \
	--data-dir /absolute/disposable/data \
	--bot BOT_UUID \
	--output "$PWD/.else-dev/desktop-stream"
```

## Check native behavior

1. Open Details and then Computer. Neither action may wake a sleeping VM or acquire its viewer lease. Click the asleep card once. The live preview must open expanded inside the app in Watching mode.
2. Try pointer, keyboard, and scroll input while Watching. Guest state must stay unchanged and bot CDP helpers must remain usable.
3. Choose Take over. Fill and submit the local login fixture with native input. Check Tab, Shift+Tab, shortcuts, modifiers, dead keys, IME composition, long Unicode commits, dragging, and scrolling. Repeat after resizing and on a 2x display, including drags into letterbox margins.
4. Confirm a CDP helper waits during take-over, then reads that same Chromium session's cookie and saved login after hand-back. Refused take-over must retain the live preview and show the reason.
5. Hold input while testing focus loss, mode changes, disconnect, tab changes, sidebar closure, bot switching, and window closure. Confirm held input and leases are released. Stop the API while controlling and check cleanup before restarting and retrying.
6. Check first-ever VM creation, an idle viewer longer than 45 seconds, owner stop, and explicit wake. The app must show the asleep state after stop and preserve the browser profile after wake.

## Verification status

Real-VM and native GPUI journeys passed on Linux amd64 and macOS arm64. The macOS run also covered a virtual 2x display, Japanese composition, a 100-character CJK commit, shutdown/reconnect, and version 2 upgrade persistence. A physical Retina panel was not tested. See the [protocol](protocol.md) for input semantics and remaining ownership and timing limits.
