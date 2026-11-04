# Verify the desktop in a background macOS VM

Use one Lume guest per verification run. Run the commands from the worktree
whose binaries you want to verify. The host desktop is for the owner.
Screenshots, accessibility queries, and input must use this helper's guest
endpoint. Do not open a viewer or use the host computer-use driver.

## Prepare a stopped base

Use an Apple silicon Mac with native arm64 binaries and Lume 0.6.1 or a
compatible version. Check `lume clone --help`, `lume set --help`, and
`lume run --help` for network identity overrides and `--vnc disabled`.
Put Lume on `PATH`.

The verified base is `ghcr.io/trycua/macos:26-20261021-fca2193`, with manifest
digest `sha256:7cb23670cad48a6723cf667bc63c444b83dc0c73756c23b614897127d5ba22ba`.
It contains an autologin Aqua session, cua-spacesd 0.7.0, and cua-driver 0.38.0
with Accessibility and Screen Recording permissions. A plain macOS installer
image does not supply those prerequisites.

If the base already exists, inspect it and skip the pull. Otherwise, pull the
image once into a base store outside any worktree:

```sh
mkdir -p "$HOME/.local/share/else-verify/lume-base"
lume pull macos:26-20261021-fca2193 else-verify-base \
	--storage "$HOME/.local/share/else-verify/lume-base"
lume get else-verify-base --format json \
	--storage "$HOME/.local/share/else-verify/lume-base"
cat "$HOME/.local/share/else-verify/lume-base/else-verify-base/.manifest-digest"
```

Confirm the digest and stopped state. If the earlier trial's stopped VM is
already available, pass its name and storage directory as the base instead
of downloading another image. The helper clones the base and never boots or
changes it. Do not use a base that contains personal accounts or credentials.

The download is about 22 GiB. The trial's 150 GiB virtual disk occupied about
30 GiB physically. Treat virtual capacity and allocated disk space separately.
Clones can consume more storage as they write.

## Prepare this worktree

Create a private host Python environment. Only the guest shell transport
needs the pinned Cua client. Native input uses direct MCP calls to the guest.

```sh
python3 -m venv .else-dev/lume-client
.else-dev/lume-client/bin/python -m pip install 'cua==0.10.1'
cargo build --workspace --locked --bins --target-dir .else-dev/.build
```

Finish the build before starting a run. The helper copies both binaries and
the verification sources into a payload and records their hashes. Never use a
shared `CARGO_TARGET_DIR` to identify a worktree's executable.

Development builds also need age's translation files at their compile-time
source path inside the guest. The helper discovers the dependency with Cargo
metadata. When using a binary built with a separate Cargo home, pass
`--age-i18n /absolute/cargo/source/age-VERSION/i18n` from that build instead.
The resources are staged before copying to their guest paths. No host Cargo
home or provider configuration is mounted.

## Start a private run

```sh
.else-dev/lume-client/bin/python .agents/skills/verify-else/lume.py run \
	--base else-verify-base \
	--base-storage "$HOME/.local/share/else-verify/lume-base" \
	--api-binary .else-dev/.build/debug/else-api \
	--desktop-binary .else-dev/.build/debug/else-desktop \
	--session work
```

Keep this command running. It prints the run directory after the guest API
and desktop are ready. Use that exact directory as `RUN` in another terminal.
The default is under this checkout's `.else-dev/lume/`, outside Cargo's target
directory. `--output /absolute/new/path` selects another run directory.

Each run has its own VM name, randomized machine identifier and MAC address,
read-only setup share, control token, payload, guest paths, API data, and app
bundle. The default guest has four CPUs and 8 GiB of memory. Budget CPU and
memory across simultaneous runs.

The desktop starts on its connection editor. Click **Check connection**, then
**Continue**. The helper supplies a fresh installation and an offline Codex
fixture. Add `--welcome` to complete onboarding through the actual screen.
The fixture proves streaming and cancellation through the real application.
It does not establish a live provider login or real bot-computer behavior.

## Drive only the owned guest

Set `RUN` to the printed absolute directory. Use a new evidence name for
every action. The helper refuses to overwrite an earlier capture.

```sh
.else-dev/lume-client/bin/python .agents/skills/verify-else/lume.py snapshot \
	--run "$RUN" --name initial
.else-dev/lume-client/bin/python .agents/skills/verify-else/lume.py probe \
	--run "$RUN" --path /v1/bots
```

Inspect the PNG and accessibility tree before input. A pixel click must use
the helper's fresh window capture. Prefer accessibility labels and roles when
they identify exactly one element. Composer typing needs its accessibility
element token. Pixel-focused and foreground typing missed the composer in
the trial, despite reporting delivery.

```sh
.else-dev/lume-client/bin/python .agents/skills/verify-else/lume.py click \
	--run "$RUN" --name check-connection --label 'Check connection'
.else-dev/lume-client/bin/python .agents/skills/verify-else/lume.py click \
	--run "$RUN" --name continue --label Continue
.else-dev/lume-client/bin/python .agents/skills/verify-else/lume.py type \
	--run "$RUN" --name compose --role AXTextArea \
	--label 'Message… (Enter to send, Shift+Enter for a new line)' \
	--text 'Hello from this worktree'
.else-dev/lume-client/bin/python .agents/skills/verify-else/lume.py key \
	--run "$RUN" --name send --role AXTextArea \
	--label 'Message… (Enter to send, Shift+Enter for a new line)' --key return
```

Use the exact label from the fresh tree if it differs. Never retry a mutation
because a driver reports an uncertain effect. Inspect the after screenshot
and probe the API first. The successful message must appear in both the
desktop and `/v1/bots/BOT_ID/conversation`. Confirm a completed turn and the
fixture reply. Send `[ui:hang]`, press **Stop work**, and confirm an interrupted
turn for cancellation coverage. Follow the feature map for other journeys.

The helper refreshes Lume metadata before each action, verifies the clone's
identity, and derives its guest address. A stopped guest, changed identity,
loopback address, or non-null VNC URL is an error. Never copy an earlier IP
into a generic computer-use client.

The trial's typed SDK driver bridge returned `invalid_request`. This workflow
uses raw MCP against the guest's `/mcp` endpoint instead. Driver success alone
does not prove an effect. Screenshot and API evidence remain the gate.

## Run several worktrees

Repeat preparation and launch from each worktree, including with the same
`--session work`. Keep the returned run directories separate. Do not reuse
another worktree's `.else-dev`, payload, run manifest, or live guest.

To verify isolation, keep both guests running and capture each desktop. Create
a differently named category through each desktop. Confirm each API lists
only its own new category. Stop the first run, then capture and probe the
second again. Preserve both manifests, distinct VM identities and payload
hashes, the screenshots, API transcripts, and cleanup results. Separate API
ports alone do not prove separate desktop input state.

## Collect evidence and stop

Press Ctrl-C in the foreground launcher, or request cleanup from another
terminal:

```sh
.else-dev/lume-client/bin/python .agents/skills/verify-else/lume.py down \
	--run "$RUN"
```

Cleanup exports guest evidence before stopping and deleting that run's
clone. The base and other runs remain available. Repeating `down` is safe.
After a coordinator crash, use the same run directory for recovery.
If guest control is unavailable, VM cleanup must still proceed. Report
missing guest evidence as unavailable.

Retain the run manifest, payload hashes, guest session and desktop manifests,
doctor and HTTP records, action JSON, AX trees, PNGs, launcher logs, and fresh
cleanup metadata. A boot-time VM record is not proof that cleanup succeeded.
Do not publish control or API tokens. No host pointer or focus measurements
were taken during the trial. Guest-only routing establishes where the driver
sent input, not a measured guarantee about the host pointer.

On macOS arm64, run the [failure-path checks](test_lume.py) after changing the
helper:

```sh
python3 .agents/skills/verify-else/test_lume.py
```

Then repeat the real guest journeys. Mock lifecycle checks do not prove GPUI
input. These checks skip on hosts that cannot run Lume.
