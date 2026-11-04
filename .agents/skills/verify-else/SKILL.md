---
name: verify-else
description: Verify else.bot on native Linux amd64 with parallel disposable API journeys, existing Rust tests, and the native GPUI desktop in omabox. Use after behavior changes or to reproduce messaging, bot, category, setup, or persistence failures.
---

# Verify else.bot

Read [the runtime rules](../../rules/plugin-compatibility.md) and the
[feature map](features/README.md). This pass targets Linux x86_64 only. Use
the existing `scripts/test-e2e.py` on a later arm64/macOS pass; it deliberately
rejects this architecture. Never emulate another architecture to claim coverage.

The desktop is the primary user interface. The fast automated driver uses the
same public HTTP API as the GPUI client. A desktop change also needs the native
journey below. Passing API checks does not establish keyboard, layout, or streaming
rendering behavior.

## Launch

Run from the repository root. Read the installed `omabox` skill before using
the runner. Requirements are Python 3.11+, the installed Rust toolchain, the
Linux desktop build libraries listed in `docs/development.md`, and omabox.
SQLite is bundled; no database server, Docker, account, or VM is needed.

```sh
.agents/skills/verify-else/verify.py
```

This builds `else-api` with the existing Cargo dev cache, creates a new
network-isolated omabox, and runs three public HTTP journeys concurrently.
Each journey owns its own API, database, project directory, generated
credentials, and logs. The API binds `127.0.0.1:0` and publishes its PID and
actual URL in `data/api.json`; the runner uses that URL only when the PID is
its own child. Setup runs on an empty directory
before readiness. No checkout `.env` is loaded at runtime. The Synthetic
provider echoes real messages without a model subscription. Codex, Claude,
and Microsandbox paths are explicitly unavailable in those API instances.

For the existing test coverage as well:

```sh
.agents/skills/verify-else/verify.py --tests bots categories turns cancel ws
.agents/skills/verify-else/verify.py --full
```

`--full` builds both workspace binaries and compiles unit/binary tests plus
every offline integration target. It excludes `*_live` and `replication`;
the report lists every excluded integration target. S3, real providers,
VMs, installer/update containers, doctests, and native desktop interaction
remain separate checks. Some existing tests internally skip when a provider
or executable is unavailable; read their logs before claiming those paths.

Builds run sequentially in one Cargo cache. After compilation, the runner
uses Cargo's emitted executable paths and runs the test binaries directly in
the box. No Cargo process competes for the build lock during driving. Up to
four independent lanes run at once with two test threads per binary and two
Tokio worker threads per runtime. Longer suites start first so their waits
overlap the shorter checks. Change
the limits with `--jobs N --test-threads N`; the product is the maximum test
thread fan-out, plus each test's async tasks. Increase it only if this machine
has spare CPU and memory. All state and log files are owned by one lane; only
the coordinator writes the aggregate report. Two invocations have separate
boxes/data/evidence; their Cargo builds may still wait on Cargo's cache lock.
The box mounts age's translation files read-only from the resolved Cargo
dependency source. Dev builds load those error messages from disk; the mount
does not expose provider logins or the rest of the Cargo home.

For an interactive GUI session without these verification probes,
`scripts/dev.py run` starts the API and desktop in a new isolated box and
prints the exact box, window address, and commands; Ctrl-C saves its evidence
and removes the box. See [the development guide](../../../docs/development.md#test-the-desktop-in-a-private-box).

For native desktop verification, build once on the host, then start a private
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
opens on its connection editor, seeded with the session's address and token,
and keeps its profile in the session's evidence directory: press Check
connection, then Continue. The session
helper creates a fresh usable installation and completes onboarding through
`POST /v1/onboarding`. It saves a supported Codex profile but starts no Codex
turn. `--probe` defaults to a read-only bot listing; `--path`, `--method`, and
`--body` can drive this session's public API. Set `--expected 201` when creating
a bot/category and `--expected 202` when sending a turn. Pass `--api-binary /absolute/path`
to `--session` for another target directory. The session expires after an hour.

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

The default command drives bot creation/retry/deletion, messaging/retry/stop/
restart, and category creation/rename/assignment/deletion. API calls use
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
proof out of the box, and removes the exact box it created in a `finally`
block, including after failed journeys. Timeout defaults to 180 seconds per
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

All helper modes are in executable [verify.py](verify.py). Run `--help` for
the flags. No dependencies beyond the Python standard library are installed.
After changing the runner, run its executable [failure-path checks](test_verify.py)
in an owned box before the public journeys:

```sh
omabox run -b BOX -- python3 .agents/skills/verify-else/test_verify.py
```

They verify child reaping on timeout, failure exit codes despite success-like
output, retained failure evidence, and removal of inherited live configuration.
Use `/maintain-verification-skill` after app behavior or entry points change
to reconcile the feature map with the implementation.
