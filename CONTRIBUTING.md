# Contributing to else.bot

## Set up

Install current stable Rust with Cargo, rustfmt, and Clippy, plus Python 3.11 or newer. SQLite is bundled into the API. The session SQL command needs the `sqlite3` CLI.

On Debian or Ubuntu, install the build and desktop dependencies:

```sh
sudo apt-get install build-essential pkg-config git curl ca-certificates jq sqlite3 \
  libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libxcb1-dev \
  libx11-xcb-dev libx11-dev libfontconfig-dev libfreetype-dev libvulkan1 libzstd-dev
```

On Arch Linux / Omarchy:

```sh
sudo pacman -S --needed base-devel pkgconf git curl ca-certificates jq sqlite \
  libxkbcommon libxkbcommon-x11 wayland libxcb libx11 fontconfig freetype2 \
  vulkan-icd-loader zstd
```

The Linux desktop needs a working Vulkan driver. On macOS, install Xcode with the Metal compiler for the desktop. The API alone needs only the Command Line Tools.

```sh
git clone https://github.com/else-bot/else.git
cd else
git config --local extensions.worktreeConfig true
git config --worktree core.hooksPath .githooks
cargo build --workspace --locked
```

The pre-commit hook runs `cargo test --workspace --locked` and blocks commits when tests fail. Git does not enable repository hooks on clone, so run the hook configuration commands once per checkout. For an existing linked worktree, run `git config --worktree core.hooksPath .githooks` there.

For real provider replies, install Codex or Claude Code and sign in as your development user. Set `ELSE_CODEX_BIN` or `ELSE_CLAUDE_BIN` if a CLI is outside `PATH`.

## Run a development session

To include Gmail sign-in in a desktop build, set `ELSE_GOOGLE_OAUTH_CONFIG` to the absolute path of your downloaded Google **Desktop app** client JSON before running Cargo or the development launcher:

```sh
export ELSE_GOOGLE_OAUTH_CONFIG=/absolute/path/to/client_secret_desktop.json
```

Keep this file outside the repository. You can set the variable in your checkout's ignored `.cargo/config.toml` under `[env]` instead. Cargo regenerates the embedded configuration when the path or file changes. The API needs no build credentials. Builds without this variable still work, with Gmail sign-in disabled. Tests use fixture registrations and require no Google credentials. Fork maintainers register their own Google Desktop client.

Official release builds read the complete downloaded JSON from the `GOOGLE_OAUTH_CLIENT_JSON` GitHub Actions repository secret. The release workflow writes it to a private temporary file, builds the binaries, and removes the input file. A missing or invalid registration fails the release build. The registration is extractable from released binaries and does not authenticate the app as a confidential client. Keep it out of Git, and keep mailbox tokens encrypted on each API.

From the repository root, run these in separate terminals:

```sh
scripts/dev.py api
```

```sh
scripts/dev.py desktop
```

The launcher builds each binary and gives this checkout its own database, credentials, bot projects, and API port. State lives in `.else-dev/default`; builds live in `.else-dev/.build`. It supplies configuration itself and ignores the checkout's `.env`.

The Synthetic provider is enabled for local checks without a provider login. Set `ELSE_SYNTHETIC_PROVIDER=0` before starting the API to disable it. Real provider CLIs use your host login and run with your user account's permissions.

To run another session, pass the same `--session NAME` to both commands. After restarting the API, relaunch the desktop so it picks up the new port. On macOS the launcher creates a separate app bundle for each session.

| Command | Action |
| --- | --- |
| `scripts/dev.py status` | Show this session's verified API URL and process |
| `scripts/dev.py curl /v1/capabilities` | Call the API with the session's owner token |
| `scripts/dev.py sql 'select count(*) from bots'` | Run read-only SQL |
| `scripts/dev.py reset` | Delete this session's state and run setup again |
| `scripts/dev.py drop` | Delete this session's state |

Stop the API with Ctrl+C. `reset` and `drop` refuse a running session. To keep conversations between runs, restart the API without resetting it.

On Linux, agents checking the UI use `scripts/dev.py run`. It creates an isolated omabox with its own API and Synthetic provider, prints the box and window identifiers, and removes the box on Ctrl+C. Read the installed omabox skill before running GUI commands. Host desktop launches belong in your own terminal. For macOS UI checks, use the repository's [Lume workflow](.agents/skills/verify-else/lume.md).

## Repository layout

| Path | Contents |
| --- | --- |
| `apps/api/src` | Axum API, scheduler, provider adapters, bot computers, backups, and connectors |
| `apps/api/migrations` | Versioned SQLite migrations |
| `apps/api/tests` | API integration tests and gated live-provider/VM tests |
| `apps/desktop/src` | GPUI app and API client |
| `infra/guest` | Bot computer image, desktop helpers, and browser tools |
| `scripts` | Development launcher, installer checks, and release tools |
| `.github/workflows` | Guest image builds, native release binaries, and release gates |

Both binaries use the workspace version in `Cargo.toml`. The workspace pins `gpui-base` to the else-bot fork for native link previews. Checkout builds omit debug information by default; use `CARGO_PROFILE_DEV_DEBUG=2` when debugging needs source lines and variables.

## Check changes

```sh
cargo fmt --all -- --check
cargo check --workspace --locked
cargo clippy --workspace --all-targets --locked
cargo test --workspace --locked
```

Database tests create temporary SQLite files. Live provider and VM tests require their explicit `ELSE_LIVE_*` gates and dependencies. Ordinary test runs do not need provider accounts or a running VM. Keep those gates off unless you intend to run the live scenarios.

The pre-commit hook runs the full Cargo suite locally. Fix test failures before committing. Ordinary pull requests skip the `cargo-test` job. Release pull requests from `release/v*` branches and manual workflow runs execute the full suite on Ubuntu with stable Rust. The required `cargo-test` check accepts the skipped job on ordinary pull requests and requires passing tests on release pull requests.

The `release-ready` check must also pass before merging. Changes require a pull request, but branches do not need to be up to date with `main`. Force pushes, branch deletion, and non-linear history are blocked. Release automation pushes to release branches and publishes tags without adding commits directly to `main`.

CI caches Cargo downloads and compiled output by Rust version and lockfile, with separate caches for release tests and each release platform. It saves the cache after compilation succeeds, before running tests or packaging releases, so a later failure keeps the completed build available. A compiler change requires a fresh cache. Lockfile changes can restore the previous cache and rebuild changed dependencies.

Use a focused target while developing, for example:

```sh
cargo test -p else-api --test bots --locked
```

For public API journeys with fresh state and the Synthetic provider:

```sh
.agents/skills/verify-else/verify.py
```

The helper uses an isolated omabox on Linux and private processes on macOS. See [verify-else](.agents/skills/verify-else/SKILL.md) for selecting journeys or testing the native desktop.

S3 replication checks need Docker with Compose. This command owns and removes its emulator:

```sh
scripts/test-replication.py
```

For prebuilt installer coverage, run `python3 scripts/test-install-downloads.py`. It serves disposable release archives locally and checks platform selection, checksums, updates, and source-build opt-in without installing system packages. For source-build installer coverage on Linux, run `scripts/test-install.sh` with a distribution such as `debian-12`, `ubuntu-24.04`, or `arch`. It installs and updates disposable containers with `--from-source`. Native ARM64 application and installer lanes run with:

```sh
python3 scripts/test-e2e.py macos-arm64 ubuntu-arm64
```

These lanes need an ARM64 host, Docker with Compose, and the platform build dependencies. Choose one lane to run it alone. Use `--suite application`, `--suite installer`, or `--suite verification` for a focused run. Results go under `target/e2e`.

Add tests for observable behavior when a change needs coverage. Reuse existing fixtures. Avoid source-text assertions, tests of test runners, and tests that repeat a tool's own checks. Keep useful comments about external constraints and public contracts; make the rest clear through names and code structure.

## Work on bot computers

Install Microsandbox 0.7.13. Linux needs `/dev/kvm` access; Apple silicon uses the Hypervisor framework. Docker is needed to build the guest image locally.

Create a session before loading an image into its runtime home. `reset` deletes that session's existing state:

```sh
scripts/dev.py reset
scripts/build-guest-image.sh "$PWD/.else-dev/default/data"
ELSE_VM_IMAGE=else-guest:3 ELSE_VM_DESKTOP=1 scripts/dev.py api
```

Start the desktop in another terminal. The image builder uses the published base in `infra/guest-base.json`. Set `ELSE_GUEST_BASE` to a local Docker image to try unpublished base changes. The builder loads the resulting guest into this session's Microsandbox home.

## Configure the API directly

Prefer the session launcher for day-to-day development. For direct binary runs, copy `.env.example` to `.env` and set separate absolute data and project paths plus a free loopback address for your checkout. Process environment variables override `.env`.

| Variable | Purpose |
| --- | --- |
| `ELSE_DATA_DIR` | Runtime state directory; direct runs default to `~/.else` |
| `DATABASE_URL` | SQLite URL; defaults to the database under `ELSE_DATA_DIR` |
| `ELSE_API_BIND` | Loopback listener; direct runs default to `127.0.0.1:7879` |
| `ELSE_BOTS_DIR` | Bot project directory root |
| `OWNER_TOKEN_FILE`, `SECRET_KEY_FILE` | Owner token and encryption key paths |
| `ELSE_CODEX_BIN`, `ELSE_CLAUDE_BIN` | Provider executable paths |
| `ELSE_MSB_BIN`, `ELSE_VM_IMAGE`, `ELSE_VM_DESKTOP` | Microsandbox and guest image settings |
| `ELSE_BACKUP_DIR`, `ELSE_BACKUP_INTERVAL_SECS`, `ELSE_BACKUP_KEEP` | Backup location, interval, and retention |
| `ELSE_SYNTHETIC_PROVIDER` | Enable the deterministic provider with `1` |
| `RUST_LOG` | Server logging filter |

Run `cargo run -p else-api -- setup` once, then `cargo run -p else-api -- serve`. Setup creates missing credentials and initial state. It keeps existing credentials and settings. The server applies pending migrations at startup. Add new migration files rather than editing released migrations.

## Prepare a release

Pull requests target `main` and are squash merged. Keep the workspace version unchanged in feature PRs.

If guest base inputs changed, publish a new base first. This needs Bash 4+, Python 3, Docker Buildx with AMD64 and ARM64 support, and permission to publish to GHCR:

```sh
docker login ghcr.io
scripts/build-guest-base.sh
```

Commit the base inputs before publishing so the image records their hash. Commit the resulting `infra/guest-base.json` pin and merge it before cutting a release. Native builders can publish each platform separately with `--platform linux/amd64` or `--platform linux/arm64`. Use `--skip-build` to finish the index and pin once both exist. Published packages must allow anonymous pulls.

From `main`, prepare a version bump:

```sh
scripts/release.sh patch
```

Use `minor` or `major` for a larger bump. The script creates a release branch and PR without changing your checkout. CI builds or reuses the guest image, tests its VNC behavior, and commits the immutable digest to `infra/guest-image.json`.

Review the version and image pin, wait for `cargo-test` and `release-ready`, then squash merge with the title `chore(release): vX.Y.Z`. The guest-image workflow dispatches both checks after pinning the image so they verify the latest release commit.

After the merge, `release-tag` builds the desktop and API on native runners for Linux AMD64, Linux ARM64, and macOS ARM64. Linux builds use Debian 12 containers to support Debian 12+, Ubuntu 24.04+, and Arch Linux. macOS builds use the `macos-15` Apple silicon runner with Xcode and Metal. Windows is unsupported.

`scripts/release.py` coordinates native publication, `scripts/release_binaries.py` owns archive creation, and `scripts/guest_release.py` owns guest-image pins and verification. Native publication edits do not invalidate the guest-image build fingerprint.

All three builds must pass their binary version checks before the workflow verifies the guest image and tags the release commit. The GitHub release stays a draft until all six archives and `SHA256SUMS` are uploaded. Assets are named `else-api-vX.Y.Z-<platform>.tar.gz` and `else-desktop-vX.Y.Z-<platform>.tar.gz`, where `<platform>` is `linux-amd64`, `linux-arm64`, or `macos-arm64`. The macOS desktop archive contains `else.bot.app`; other archives contain the named executable. The app is not notarized. Linux desktops still need the runtime libraries and a Vulkan driver listed above.

Rerun a failed `release-tag` run to resume publication at the same commit. Ordinary pushes after a version is tagged skip the binary builds. The workflow builds before tagging because [tags pushed with `GITHUB_TOKEN` do not trigger another workflow](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/trigger-a-workflow#triggering-a-workflow-from-a-workflow). The installer discovers the latest published binary release through `releases/latest/download/SHA256SUMS`, so a tag with unfinished uploads is not installed. It downloads only the selected components, verifies the checksums and executable versions, and keeps the existing backup and rollback flow. `--from-source` uses version tags and a private Rust toolchain instead; use it for older releases without binary assets. `ELSE_RELEASES_URL` overrides the repository's GitHub releases URL for local fixtures or another release server.

To package a local native release build, use the matching platform:

```sh
cargo build --release --locked --workspace --bins --target x86_64-unknown-linux-gnu
python3 scripts/release_binaries.py --platform linux-amd64 --binaries target/x86_64-unknown-linux-gnu/release
```

Archives go under `target/release-assets`. To check packaging and publication retry behavior locally, run `python3 scripts/test-release.py`. This uses temporary repositories and a fake GitHub CLI without publishing a release.

If the base pin is stale, publish it and update the release branch. If a build or image check fails, fix it and rerun the workflow. Keep published image digests available for old releases and rollback.
