---
name: verify-else
description: Verify else.bot on macOS arm64 and Linux arm64 or amd64 with disposable API journeys, Rust tests, installer checks, and native GPUI input. Use Lume for background macOS desktop verification and independent concurrent worktrees.
---

# Verify else.bot

Read [the runtime rules](../../rules/plugin-compatibility.md) and the
[feature map](features/README.md). Supported native hosts are macOS arm64,
Linux aarch64, and Linux x86_64. Never emulate another architecture to claim
coverage. Linux hosts use omabox by default. macOS API checks use the process
backend. macOS desktop automation uses a headless [Lume guest](lume.md).
Use `--backend process` inside an owned Linux container or VM.

The desktop is the primary user interface. The fast automated driver uses the
same public HTTP API as the GPUI client. A desktop change also needs the native
journey below. Passing API checks does not establish keyboard, layout, or streaming
rendering behavior.

## Launch

Run from the repository root with Python 3.11+ and the native Rust toolchain.
Build prerequisites are in [the development guide](../../../CONTRIBUTING.md).
For the Linux omabox backend, read the installed omabox skill first.
SQLite is bundled. The macOS fast pass needs no Docker or provider account.

```sh
.agents/skills/verify-else/verify.py
```

This builds `else-api` with this checkout's `.else-dev/.build` Cargo cache and
runs four public HTTP journeys concurrently. `--build-dir /absolute/path`
selects another private cache. macOS runs each owned process with a disposable
HOME and XDG directories. Linux omabox also isolates the filesystem and network.
The process backend shares the host network and is not a security sandbox.
Its API listens only on a kernel-assigned loopback port.
Each journey owns its own API, database, project directory, generated
credentials, and logs. The API binds `127.0.0.1:0` and publishes its PID and
actual URL in `data/api.json`; the runner uses that URL only when the PID is
its own child. Setup runs on an empty directory
before readiness. No checkout `.env` is loaded at runtime. The Synthetic
provider echoes real messages without a model subscription. Codex, Claude,
and Microsandbox paths are explicitly unavailable in those API instances.

For the existing test coverage as well:

```sh
scripts/dev.py test
scripts/dev.py test --test bots
scripts/dev.py test -p else-api --serial -- wrong_keys_damage_and_existing_databases_never_produce_a_restore
```

On Linux, use this shared command for Rust tests. It mounts Cargo dependency
sources and GPUI debug assets, uses private scratch storage, and removes mise
shims from the test PATH. Binaries run concurrently with a CPU-based limit,
overridden by `--jobs N`. Use `--serial` for one binary with one test thread.
Longer binaries start first using previous run timings. Read `results.json`,
`summary.txt`, and logs under the printed `.else-dev/tests/<run>/` directory
after a runtime restart before starting another run.

On macOS, use `cargo test --workspace --locked` inside the owned verification
guest. With the process backend, the journey runner's
`--tests bots categories turns cancel ws` and `--full` options remain available
for combined API verification. On Linux with omabox, run Rust tests through
`scripts/dev.py test` for the required dependency mounts.
`--full` builds both workspace binaries and compiles unit/binary tests plus
every offline integration target. It excludes `*_live` and `replication`;
the report lists every excluded integration target. S3, real providers,
VMs, installer/update containers, doctests, and native desktop interaction
remain separate checks. Run the broader native arm64 matrix with:

```sh
python3 scripts/test-e2e.py macos-arm64 ubuntu-arm64 --compile-live
```

This reuses the journeys on macOS and native Ubuntu arm64 in Docker, adds
the workspace tests, doctests, private S3 fixture, and real install/update
scenario, and preserves each platform's logs under its evidence directory.
`--suite verification` selects the public API journeys.
Live provider gates remain disabled even with `--compile-live`.
On Linux arm64, select `ubuntu-arm64` alone. Docker must be native arm64.
Some existing tests internally skip when a provider or executable is
unavailable. Read their logs before claiming those paths.

The API journey runner builds sequentially in the checkout's private Cargo cache. A shared
`CARGO_TARGET_DIR` cannot replace another checkout's executables. After compilation, the runner
uses Cargo's emitted executable paths and runs the test binaries directly in
private lanes. No Cargo process competes for the build lock during driving. Up to
four independent lanes run at once with two test threads per binary and two
Tokio worker threads per runtime. Longer suites start first so their waits
overlap the shorter checks. Change
the limits with `--jobs N --test-threads N`; the product is the maximum test
thread fan-out, plus each test's async tasks. Increase it only if this machine
has spare CPU and memory. All state and log files are owned by one lane; only
the coordinator writes the aggregate report. Two invocations have separate
processes, data, and evidence; their Cargo builds may still wait on Cargo's cache lock.
The box mounts age's translation files read-only from the resolved Cargo
dependency source. Dev builds load those error messages from disk; the mount
does not expose provider logins or the rest of the Cargo home.

On Linux with omabox, for an interactive GUI session without these verification probes,
`scripts/dev.py run` starts the API and desktop in a new isolated box and
prints the exact box, window address, and commands; Ctrl-C saves its evidence
and removes the box. See [the development guide](../../../CONTRIBUTING.md#run-a-development-session).

For macOS desktop automation, follow [the Lume workflow](lume.md). Each
invocation clones a stopped base into a private VM, including when several
worktrees use the same session name. All screenshots and input target the
guest. Do not use the host computer-use driver or open a VM viewer.

For human-operated macOS sessions and Linux container desktop verification,
use [the native desktop guide](native-desktop.md). Both workflows use an
offline Codex protocol fixture so the actual composer can stream and stop
without a login.

For Linux omabox desktop verification, build once on the host, then start a private
session. The following uses the default Cargo target directory; if
`CARGO_TARGET_DIR` is configured, use the path from
`cargo metadata --locked --no-deps --format-version 1` and add
`--ro-bind /absolute/target/path` when creating the box.

```sh
cargo build --workspace --locked --bins
omabox up --new --net isolated --json
```

Keep the returned box name. Substitute it for `BOX` in every command below.
Always name the box explicitly; never double-drive another agent's box.

```sh
omabox run -b BOX -d -- python3 .agents/skills/verify-else/verify.py --session native
omabox wait -b BOX until -- python3 .agents/skills/verify-else/verify.py --probe native
omabox run -b BOX -- python3 .agents/skills/verify-else/verify.py --probe native
omabox run -b BOX -d -- sh -c '. "$HOME/else-verification/native/desktop.env"; export ELSE_API_URL ELSE_OWNER_TOKEN_FILE ELSE_DESKTOP_CONFIG_FILE; cd /tmp; exec /absolute/checkout/target/debug/else-desktop'
omabox wait -b BOX window else-bot
```

Replace `/absolute/checkout` with this checkout's absolute path. The desktop
opens on its welcome screen with the session's address and key loaded.
Choose **Connect locally** to verify and save the connection in the session's
evidence directory. The session
helper creates a fresh usable installation and completes onboarding through
`POST /v1/onboarding`. It saves a supported Codex profile but starts no Codex
turn. `--probe` defaults to a read-only bot listing; `--path`, `--method`, and
`--body` can drive this session's public API. Set `--expected 201` when creating
a bot/category and `--expected 202` when sending a turn. Pass `--api-binary /absolute/path`
to `--session` for another target directory. The session expires after an hour.

## Concurrent worktrees

Run commands from each worktree's root. Keep Cargo artifacts private to that
checkout and finish compilation before launching the verification session.
Lume snapshots the binaries and helper sources into the run's payload.
Rebuilding another checkout does not replace that payload or its running app.

Use a separate Lume run for each agent. A session name is a label, not a VM
identity. Run UUIDs separate VM names, network identities, guest paths,
credentials, and evidence even within the same worktree. Use the printed run
directory for every screenshot, input, probe, and cleanup command. Never
select a VM by its display title, IP from an earlier run, or a global listing.

The default Lume run directory lives under `.else-dev/lume/`, outside Cargo's
target directory. Each run owns its lifecycle and cleanup. There is no global
input lock or shared guest desktop. Read the [two-worktree procedure](lume.md#run-several-worktrees)
before claiming that concurrent native desktop sessions are isolated.

## Doctor

The automated runner performs a read-only doctor before each journey and
after a restart. `doctor.json` proves the owned process is alive and published `api.json`, the running
version matches the built binary, `/readyz` reaches SQLite, the generated
owner token authenticates, the database's main-bot UUID matches `/v1/bots`,
and Synthetic is available. A health response alone is insufficient.

Each probe also checks that the session's PID still owns its `api.json` and
data lock. For a native session, run the default probe above and inspect
`~/else-verification/native/doctor.json` inside that box. If a probe fails,
read its API log and `omabox jobs -b BOX`; do not retry input into an unknown
window. Doctor and probes never contact the installed API on 7878 or 7879.

## Drive

The default command drives bot creation/retry/settings/deletion, messaging/retry/stop/
restart, category creation/rename/assignment/deletion, and onboarding with
exactly one persisted greeting across retries and restart. API calls use
`request_id` UUIDs, the same routes and payloads as the client, and actual
SQLite persistence. They never call internal setters or test-only endpoints.

For a native category journey, capture the initial window, open the dialog,
capture the action, type once, submit, and confirm the committed category:

```sh
omabox shot -b BOX --window else-bot
omabox keys -b BOX --window else-bot --wait ctrl+shift+g
omabox shot -b BOX --window else-bot
omabox keys -b BOX --window else-bot --wait -t 'Verification category'
omabox keys -b BOX --window else-bot --wait Return
omabox run -b BOX -- python3 .agents/skills/verify-else/verify.py --probe native --path /v1/categories
omabox shot -b BOX --window else-bot
```

Inspect the screenshots, including the focused category-name field, before
typing. `keys` exit 0 proves delivery only; the category must appear in both
the sidebar and the API. GPUI may lack an accessibility tree, so use screenshots
and the public API. If `--wait` returns 124, inspect before resending anything.
Other journeys and entry points are in the feature map.

## Evidence

Each automated run prints a unique `target/verification/<run>/` directory.
`--output /absolute/new/path` overrides it. Existing output directories are
refused. Preserve:

- `results.json` with revision, dirty-tree description, toolchain, architecture,
  build/drive/total duration, per-lane outcomes, exclusions, and cleanup status.
- `build.log`, Cargo `build.jsonl`, and `spec.json` with the actual executables.
- `worker.log` and `evidence/<journey>/http.jsonl`, `doctor.json`, and API/setup logs.
- `evidence/test-<target>/test.log` for every requested Rust target.

The HTTP transcript captures each action's payload and response without its
authorization header. Bot deletion also checks the retained row through a
read-only SQLite connection. Messaging compares complete items/turns before
and after a real API restart. Never report a green compile as a passing flow.
Synthetic and existing fake VM boundaries establish deterministic behavior,
not live-provider, real-VM, or S3 behavior. Do not enable live gates to improve
the apparent test count.

For native checks, copy the three screenshots and the box's
`~/else-verification/native` directory to a new host directory under
`target/verification/` before removing the box. `omabox path -b BOX --home`
gives the host-visible private home. Capture the entry action and the resulting
state; verify persisted effects through the probe. An API-created category is
not proof of the Ctrl+Shift+G gesture. Record native results separately.

Durations describe this run, including whether Cargo artifacts were cached.
They are not a speedup claim. Compile cost, test count, exclusions, and job
limits must match when comparing runs.

## Cleanup

The runner stops each API process group, removes only its temporary data, copies
proof out of the box when using omabox, and removes the exact box it created
in a `finally` block, including after failed journeys. Timeout defaults to 180 seconds per
test binary and can be changed with `--timeout`. Build deadlines are 30 minutes.
Evidence survives cleanup. An interrupted build may have no drive evidence.

After manual native verification, copy evidence first, then:

```sh
omabox down -b BOX
```

That removes the session helper, API, desktop, and disposable data together.
Never kill by executable name, reuse a `scripts/dev.py` session, or stop an installed
service. After an uncatchable runner termination, identify its box from the
logs and remove that exact box; never use `omabox down --all`.

## Helpers

API helper modes are in executable [verify.py](verify.py). Run `--help` for
the flags. The API helper uses only the Python standard library. The separate
[Lume helper](lume.py) uses a private Python environment with a pinned guest
shell client, as described in [the Lume workflow](lume.md).
Use `/maintain-verification-skill` after app behavior or entry points change
to reconcile the feature map with the implementation.
