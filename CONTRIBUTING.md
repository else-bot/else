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
git config core.hooksPath .githooks
cargo build --workspace --locked
```

The pre-commit hook runs `cargo fmt --all -- --check` and `scripts/check-clippy.sh`. The pre-push hook runs `cargo test --workspace --locked`. Fix failures before committing or pushing. Pull requests only need to be up to date with main to merge; CI still runs the slower checks but does not block on them, so these hooks are the gate. Git does not enable repository hooks on clone, so run the hook configuration command once per clone. It applies to every linked worktree of that clone, including agent worktrees.

For real provider replies, install Codex, Claude Code, or Grok Build and sign in as your development user. Set `ELSE_CODEX_BIN`, `ELSE_CLAUDE_BIN`, or `ELSE_GROK_BIN` if a CLI is outside `PATH`.

## Run a development session

To include Gmail sign-in in a desktop build, set `ELSE_GOOGLE_OAUTH_CONFIG` to the absolute path of your downloaded Google **Desktop app** client JSON before running Cargo or the development launcher:

```sh
export ELSE_GOOGLE_OAUTH_CONFIG=/absolute/path/to/client_secret_desktop.json
```

Keep this file outside the repository. You can set the variable in your checkout's ignored `.cargo/config.toml` under `[env]` instead. Cargo regenerates the embedded configuration when the path or file changes. Gmail sign-in requires no API build credentials. Builds without this variable still work, with Gmail sign-in disabled. Tests use fixture registrations and require no Google credentials. Fork maintainers register their own Google Desktop client.

Official release builds read the complete downloaded JSON from the `GOOGLE_OAUTH_CLIENT_JSON` GitHub Actions repository secret. The release workflow writes it to a private temporary file, builds the binaries, and removes the input file. A missing or invalid registration fails the release build. The registration is extractable from released binaries and does not authenticate the app as a confidential client. Keep it out of Git, and keep mailbox tokens encrypted on each API.

For Microsoft 365, set `ELSE_MICROSOFT_CLIENT_ID` to your app registration's Application client ID at build time. Official releases read it from the `MICROSOFT_CLIENT_ID` Actions repository secret. No client secret is needed. Configure the registration for **Mobile and desktop applications** with `http://localhost` as its redirect URI and the supported account types you intend to serve. For organizational and personal accounts, select the multitenant option that includes personal Microsoft accounts. The directory tenant ID identifies the registration's home tenant; it is not a restriction on accounts connecting through the app. Authorization uses Microsoft's `common` endpoint.

Add the Microsoft Graph delegated permissions `IMAP.AccessAsUser.All`, `SMTP.Send`, and `offline_access`. Runtime mail scopes use the Outlook resource URLs. Builds without a Microsoft client ID disable Microsoft sign-in. Tests use fixture IDs.

For GitHub MCP sign-in, set `ELSE_GITHUB_OAUTH_CONFIG` to an absolute path outside the repository before building the API. The JSON contains `client_id` and `client_secret` from a dedicated GitHub OAuth App with callback `http://127.0.0.1/mcp/callback`. Cargo embeds the registration and rebuilds when the path or file changes. Builds without this file need runtime registration to enable GitHub sign-in.

Official releases read the `ELSE_GITHUB_CLIENT_ID` and `ELSE_GITHUB_CLIENT_SECRET` Actions repository secrets. The workflow writes a private temporary JSON file, builds the API, and removes the input file even when the build fails. Missing or invalid credentials fail the release. These app credentials are extractable from distributed binaries. PKCE protects each authorization attempt, and each bot still needs owner consent. Account access and refresh tokens remain encrypted on the API server. Runtime `ELSE_GITHUB_CLIENT_ID` and `ELSE_GITHUB_CLIENT_SECRET` replace the embedded registration only when both are set.

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

## Reuse compiled dependencies across worktrees

Install sccache once with `cargo install sccache --locked --no-default-features`. Add the wrapper to your user-level `~/.cargo/config.toml`, merging with any existing `[build]` section. Use the absolute path reported by `command -v sccache` so editors and hooks can find it:

```toml
[build]
rustc-wrapper = "/home/you/.cargo/bin/sccache"
```

Set a shared-cache limit in `~/.config/sccache/config` on Linux:

```toml
[cache.disk]
size = 21474836480 # 20 GiB
```

Each checkout keeps its own build directory. The launcher passes `--target-dir` to Cargo instead of exporting `CARGO_TARGET_DIR`, because sccache hashes `CARGO_*` environment variables. For direct Cargo builds with a custom target directory, use `--target-dir` too. The first build fills the shared cache. Later checkouts reuse a share of registry and git dependency compilations, plus native object files. Binaries and procedural macros still compile. Crates that read a per-checkout `OUT_DIR` at compile time, such as serde and thiserror, miss the cache. Changes in their outputs can also invalidate dependent libraries.

The launcher disables incremental compilation to limit disk growth in development worktrees. This trades faster edits to local crates for less disk use. Other Cargo builds keep their usual incremental settings. The cache limit bounds sccache storage, but private target directories still need cleanup when you remove worktrees. These settings do not remove existing build data. Use `RUSTC_WRAPPER= cargo build` for a direct build without sccache.

## Check changes

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
python3 scripts/check_migrations.py
python3 scripts/test-migrations.py
scripts/dev.py test
```

Run `scripts/check-clippy.sh` for the strict Clippy gate alone. It runs the exact command above from the repository root and returns Clippy's exit status. It checks all workspace targets, including tests, without running the test suite. Reuse the same checkout and Cargo target directory to reuse compiled dependencies.

Fix every finding before opening a PR, including findings in untouched files. Fix the cause instead of adding blanket lint suppressions. Justify targeted `#[allow]` and `#[expect]` attributes inline.

On Linux, read the installed omabox skill before running tests. `scripts/dev.py test`
requires omabox. It builds the workspace test binaries on the host, then runs them in a new
network-isolated omabox. Cargo registry sources and GPUI debug assets are
mounted read-only. Tests use writable scratch storage in the box's private
runtime directory instead of host `/tmp`. The runtime PATH excludes mise shims.

Builds use this checkout's `.else-dev/.build` cache and honor `CARGO_BUILD_JOBS`.
The default build limit is four jobs. Test binaries run sequentially with
two test threads per binary. Use `--serial` for one test thread per binary.
Set `CARGO_BUILD_JOBS=2` when other workspaces build concurrently.
For focused checks:

```sh
scripts/dev.py test -p else-api --test bots
scripts/dev.py test -p else-api --serial -- wrong_keys_damage_and_existing_databases_never_produce_a_restore
```

Each invocation prints `.else-dev/tests/<run>/`. Read its `results.json`,
`summary.txt`, build logs, and per-binary logs before repeating a run after an
agent restart. The worker runs independently of the waiting terminal and
records progress after each binary. Ctrl+C stops the owned worker and box.
To follow an existing run or cancel its worker, use the printed run directory:

```sh
python3 scripts/dev_tests.py follow .else-dev/tests/RUN
python3 scripts/dev_tests.py cancel .else-dev/tests/RUN
```

On macOS, use `cargo test --workspace --locked` inside the owned verification
guest. The omabox test command is Linux only. Doctests remain a separate Cargo
check.

Database tests create temporary SQLite files. Live provider and VM tests require their explicit `ELSE_LIVE_*` gates and dependencies. Ordinary test runs do not need provider accounts or a running VM. Keep those gates off unless you intend to run the live scenarios.

To check Grok through the API, sign in with `grok login` and run:

```sh
ELSE_LIVE_GROK=1 cargo test -p else-api --test grok_live --locked -- --nocapture
```

This journey creates disposable bots and checks session reuse, host files, MCP calls, cancellation, and provider handoffs with the installed Grok CLI.

Pull requests run two independent Rust jobs on GitHub-hosted Ubuntu runners. `cargo-check` runs formatting, checks all workspace targets, enforces strict Clippy, checks migration numbering, and tests bundled-icon coverage. `cargo-test` runs the full locked workspace suite, including API integration tests. Runtime assertions such as the memory tests can fail even when compilation passes, so the full suite also gates PRs. This costs another runner and test build per update. Separate caches and cancellation of superseded runs limit that cost without delaying the fast job behind the suite.

The release workflow calls `cargo-test` against every release commit. Manual runs check the selected branch with both jobs.

The `release-ready` check tests the release tooling and installers and rejects a changed workspace version, which only the release workflow sets. Releases never add commits to `main`.

### Branch protection

In the [Main pull requests and checks ruleset](https://github.com/else-bot/else/rules/26617470), require only `up-to-date` from GitHub Actions and keep **Require branches to be up to date before merging** enabled. GitHub enforces that setting through required checks, and `up-to-date` finishes in seconds. `cargo-check`, `cargo-test`, and `release-ready` still run on every PR to `main` without path filters, but merging does not wait for them; the repository hooks run the same Rust checks before each commit and push. Read a red check before merging anyway.

Fetch `origin/main` and rebase before the first push and again before merging. Rerun `python3 scripts/check_migrations.py` after rebasing. An up-to-date branch runs main's workflow and scripts against its own changes. Force pushes, branch deletion, and non-linear history on `main` are blocked.

CI caches Cargo downloads and compiled output by Rust version and lockfile, with separate caches for fast checks, the full suite, and each release platform. It saves the cache after compilation succeeds, before running tests or packaging releases, so a later failure keeps the completed build available. A compiler change requires a fresh cache. Lockfile changes can restore the previous cache and rebuild changed dependencies.

Use a focused target while developing, for example:

```sh
scripts/dev.py test -p else-api --test bots
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
| `ELSE_CODEX_BIN`, `ELSE_CLAUDE_BIN`, `ELSE_GROK_BIN` | Provider executable paths |
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

Commit the base inputs before publishing so the image records their hash. Commit the resulting `infra/guest-base.json` pin and merge it before releasing. Native builders can publish each platform separately with `--platform linux/amd64` or `--platform linux/arm64`. Use `--skip-build` to finish the index and pin once both exist. Published packages must allow anonymous pulls.

Release the head of `main`:

```sh
scripts/release.sh patch
```

Use `minor` or `major` for a larger bump, `--sha SHA` to release an earlier commit on `main`, and `--wait` to follow the run until it finishes. The script only starts the `release` workflow and prints its run; you can also start it from the Actions tab. Nothing needs a pull request, a rebase, an approval, or a merge, so pull requests keep merging while a release runs. Releases run one at a time. If several wait, GitHub starts only the newest and cancels the others.

The workflow takes the next version from the newest release tag. It reuses the guest image already published for the commit's guest inputs, or builds one, tests its keymap and VNC behavior, and publishes it. It then pushes one release commit on top of the chosen commit to `release/vX.Y.Z`. That commit only bumps the workspace version in `Cargo.toml` and `Cargo.lock` and pins the image digest in `infra/guest-image.json`. `cargo-test`, `release-ready`, and the native builds run on it in parallel. Linux AMD64 and ARM64 build in Debian 12 containers to support Debian 12+, Ubuntu 24.04+, and Arch Linux. macOS ARM64 builds on a `macos-15` Apple silicon runner with Xcode and Metal. Windows is unsupported.

Once everything passes, the workflow verifies the guest image again, tags the release commit, tags the commit on `main` it was cut from as `main/vX.Y.Z`, publishes the GitHub release, and deletes the `release/vX.Y.Z` branch. It refuses to tag or resume a commit that is neither on `main` nor a release commit on top of `main` whose Cargo files differ only by the version bump and whose only other change is the guest pin, so a release only ships reviewed code. Release jobs run the release helpers from the workflow's own revision against the selected commit. The version and guest pin in `main`'s files stay at their last merged values; tagged commits carry the released ones. `git pull` fetches the `main/vX.Y.Z` tags, since they point into `main`'s history, and the API and desktop build scripts report the highest one in a checkout's history, so local builds from `main` report the newest release they contain. The scripts rerun when tags change, the branch changes, or a pull, merge, rebase, or reset moves it, but not on plain commits. Without Git or tags, a build reports the workspace version.

`scripts/release.py` coordinates native publication, `scripts/release_binaries.py` owns archive creation, and `scripts/guest_release.py` owns guest-image pins and verification. The guest-image fingerprint covers the guest files, base pin, and build recipe. Version bumps, release helper edits, and CI runner changes reuse the image. When a helper or workflow changes how the image is built, update the recipe in `scripts/guest_release.py` to invalidate the fingerprint.

Images published before release orchestration was excluded from the fingerprint need one rebuild to adopt the new fingerprint. Later releases reuse that image until its build inputs change.

All three builds must pass their binary version checks before the release commit is tagged. The GitHub release stays a draft until all six archives and `SHA256SUMS` are uploaded. Assets are named `else-api-vX.Y.Z-<platform>.tar.gz` and `else-desktop-vX.Y.Z-<platform>.tar.gz`, where `<platform>` is `linux-amd64`, `linux-arm64`, or `macos-arm64`. The macOS desktop archive contains `else.bot.app`; other archives contain the named executable. The app is not notarized. Linux desktops still need the runtime libraries and a Vulkan driver listed above.

Rerun the failed jobs of a release run to retry it. A run that fails before tagging leaves nothing behind but its `release/vX.Y.Z` branch, and the next run replaces it. If the newest tag's binaries were never fully published, the next run finishes that release instead of cutting a new one; run it again afterwards. The workflow tags only after every build passes, so a new tag normally has its binaries within minutes and installers rarely fall back to a source build. The installer installs the newest `vX.Y.Z` tag. It downloads only the selected components from that version's release, verifies the checksums and executable versions, and keeps the existing backup and rollback flow. When that release has no `SHA256SUMS` or a selected archive yet, it builds the tag from source with a private Rust toolchain. `--from-source` always does that; use it for older releases without binary assets. `ELSE_RELEASES_URL` overrides the repository's GitHub releases URL for local fixtures or another release server.

To package a local native release build, use the matching platform:

```sh
cargo build --release --locked --workspace --bins --target x86_64-unknown-linux-gnu
python3 scripts/release_binaries.py --platform linux-amd64 --binaries target/x86_64-unknown-linux-gnu/release
```

Archives go under `target/release-assets`. To check packaging and publication retry behavior locally, run `python3 scripts/test-release.py`. This uses temporary repositories and a fake GitHub CLI without publishing a release.

If the base pin is stale, publish it and update the release branch. If a build or image check fails, fix it and rerun the workflow. Keep published image digests available for old releases and rollback.
