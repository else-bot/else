# Verify the native desktop

Build both native binaries with `cargo build --workspace --locked --bins`.
Use the actual executable paths when selecting a different Cargo cache.

## macOS arm64

For automated computer use, follow [the Lume workflow](lume.md). It gives each
run its own background macOS VM and guest-side input driver. Use that workflow
when the owner is using the host desktop or several worktrees need native input
at the same time. Never point the host computer-use driver at these checks.

### Run a human-operated host session

From your own terminal, start an owned API and desktop. The output directory
must be new.

```sh
python3 .agents/skills/verify-else/verify.py --session mac-ui \
    --backend process --output target/verification/mac-ui \
    --api-binary target/debug/else-api \
    --desktop-binary target/debug/else-desktop --fixture-provider --welcome
```

The helper creates a private API HOME, database, desktop profile, and a local
`Else Verification.app` bundle under `apps/HASH/` in the output directory.
`desktop-process.json` records its exact executable path for native
automation. The bundle comes from the same publisher as `scripts/dev.py
desktop`. It holds a copy of the built binary, so another build cannot
replace the running desktop. Each run gets a random bundle identifier. After
the desktop stops, the helper removes that identifier's macOS preferences,
which macOS can write outside the private HOME. It never installs the app or
a service.

Select that exact app bundle when manually operating the host session.
Choose **Connect locally**. With `--welcome`, complete onboarding through
the actual screen. Without it, the helper seeds onboarding as a fixture.
The generated server address and key are already loaded in the local card.

`--fixture-provider` supplies [fake-codex.py](fake-codex.py) through the real
Codex adapter. Its private auth file contains only `{}`. No provider account
or network request is involved. It streams `Fixture reply ✓ from the desktop
composer.`. Send `[ui:hang]` for a turn that waits until Stop work. The fixture
proves the application's protocol, streaming, and cancellation behavior. It
does not establish provider authentication, tools, or model quality.

Probe the owned session from another terminal. Use the same output directory.

```sh
python3 .agents/skills/verify-else/verify.py --backend process \
    --probe mac-ui --output target/verification/mac-ui --path /v1/bots
```

Capture the connection editor, greeting, intermediate streamed text, stopped
turn, settings save, settings collapse, category dialog, and committed drag.
Use `/v1/bots/{id}/conversation` to check actual composer input and the final
turn status. Check `/v1/bots` for saved names and category IDs. Never substitute
API creation for a failed desktop gesture.

Close the verification app or interrupt its foreground helper. The helper
stops its API and removes disposable data. Screenshots, API transcripts, and
logs remain in the output directory. Its session manifest then becomes stale
and probes must refuse it.

## Linux arm64 containers

Use the native arm64 SDK image from `infra/e2e/ubuntu.Dockerfile`. Mount a
filtered source snapshot at `/src` and a private build cache at `/cache`, as
`scripts/test-e2e.py` does. Never mount the host display or provider homes.
The API and desktop must both report aarch64 through `file`.

Inside an owned disposable container, install these desktop packages:

```sh
apt-get update
apt-get install -y xvfb openbox xdotool imagemagick mesa-vulkan-drivers dbus-x11 x11-utils fonts-dejavu-core
```

Create a mode-0700 runtime directory. Start a private display and window
manager. Mesa lavapipe supplies software Vulkan when the container has no GPU.

```sh
mkdir -m 700 /tmp/else-ui-runtime
export XDG_RUNTIME_DIR=/tmp/else-ui-runtime DISPLAY=:99
export LIBGL_ALWAYS_SOFTWARE=1 VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.json
Xvfb :99 -screen 0 1440x1000x24 -nolisten tcp &
openbox &
python3 /src/.agents/skills/verify-else/verify.py --session linux-ui \
    --backend process --output /tmp/else-ui-evidence \
    --api-binary /cache/build/debug/else-api \
    --desktop-binary /cache/build/debug/else-desktop --fixture-provider --welcome
```

Follow the same journeys with the container's display driver. Inspect a fresh
screenshot before every coordinate action. Copy evidence to the host before
removing only the container you created. Software rendering proves native
GPUI input and layout, not hardware GPU behavior.

## Real computers

Container application tests use fake VM boundaries. Docker on this Mac does
not expose `/dev/kvm` to the Ubuntu worker. Run real Microsandbox checks on
the macOS host with Hypervisor.framework or on Linux with working KVM.

The live `computers_live` target owns its runtime homes and needs the current
`else-guest:2` Docker image. On macOS, use a short temporary directory because
Microsandbox's Unix socket path must fit the platform limit.

```sh
TMPDIR=/tmp ELSE_LIVE_VM=1 cargo test -p else-api --locked --test computers_live -- --test-threads 1 --nocapture
```

Keep this result separate from offline or container coverage. Report missing
virtualization, image, or runtime prerequisites as unavailable, never passed.
