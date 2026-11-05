# Release else.bot

All pull requests target `main`, the default branch. Squash each PR into one descriptive commit. A release contributes one `chore(release): vX.Y.Z` commit; there are no development-branch syncs. Installations discover releases from `vX.Y.Z` tags.

## Configure GitHub once

1. Select `main` as the default branch. Enable squash merging only and automatically delete merged branches. Use the PR title for the squash commit title.
2. Protect `main` with required pull requests, linear history, resolved review threads, and the required `release-ready` check from GitHub Actions. Require branches to be up to date, and prevent force pushes and deletion. Feature PRs run the release tooling checks and keep the workspace version unchanged. Release PRs also verify the pinned guest image.
3. Leave `release/v*` branches unprotected so they can be rebased and deleted after merging. Actions needs normal push access to commit the image pin. It never force pushes.
4. Allow this repository's Actions workflows to publish the `else-guest` container package. Images carry the repository source label so GHCR associates them with this repository.

The bootstrap manifest has `image: null`. On the first release, Actions creates the package. GitHub makes a new package private by default. Set `else-bot/else-guest` to public in its package settings, then rerun the failed `guest-image` workflow on the release branch. It reuses the uploaded image. The required check refuses private images, so no version tag is published before anonymous downloads work.

GitHub-hosted native AMD64 and ARM64 runners build the thin Linux guest layer from a published base. The ARM64 image is also the guest filesystem used on Apple silicon. The guest tests run in isolated containers. These checks do not replace a real Microsandbox boot and native app verification on each supported host platform.

## Publish the guest base

The base contains Debian packages, Chromium, the toolchain, fonts, and the compiled x11vnc patch. Publish it before the first release and whenever `base.Dockerfile`, `apt-downloads.conf`, or `x11vnc/keymap-settle.patch` changes. Helper-only changes reuse it. Release CI refuses a missing or stale base and never builds it itself.

Use Bash 4 or newer, Python 3, and Docker with Buildx. Commit the base inputs first so the publisher can bind the image to their hash. Log in with an account that can publish container packages to `else-bot`:

```sh
docker login ghcr.io
scripts/build-guest-base.sh
```

The publisher checks the builder supports both `linux/amd64` and `linux/arm64` before starting. It uses your existing builder and cache. Emulated compilation can take hours; it does not install emulation or change your selected builder. Pass `--builder NAME` to use another configured builder. Log in and check registry connectivity first: a failed lookup of a platform tag is treated as a missing image and can start a new build.

Alternatively, build each platform on a native machine at the same commit:

```sh
# On the AMD64 machine:
scripts/build-guest-base.sh --platform linux/amd64
# On the ARM64 machine:
scripts/build-guest-base.sh --platform linux/arm64
```

Each invocation pushes its platform image. Once both exist, the publisher joins them into one index and verifies both architectures, source labels, content digests, and anonymous access. If the new `else-guest-base` package is private, [make it public in its package settings](https://github.com/orgs/else-bot/packages/container/else-guest-base/settings), then finish without rebuilding:

```sh
scripts/build-guest-base.sh --skip-build
git add infra/guest-base.json
python3 scripts/guest-release.py --base check --rev "$(git write-tree)"
```

Commit `infra/guest-base.json` and squash its PR into `main` before cutting a release. The publisher never commits or creates an application release. Reruns reuse pushed platform tags for the same input hash.

Fresh base builds resolve current trixie packages from the regular Debian CDN, so package versions can change between builds. Existing pins and reruns keep using the published image for their input hash. Installations use the immutable image digest.

The final guest tests run in release CI after building on this base. Publication alone does not prove the guest passes them. Keep published base digests available for later builds.

## Prepare a release

1. Squash the changes into `main`, including a fresh `infra/guest-base.json` pin if heavy inputs changed.
2. Run `scripts/release.sh patch`, `minor`, or `major`. Use `-y` to skip the push confirmation.
3. Open the printed PR. The script leaves your checkout intact and creates `release/vX.Y.Z` from `origin/main` in a temporary worktree. A rerun reuses the branch and PR.
4. Wait for `guest-image` and the `release-ready` status. Review the version bump and `infra/guest-image.json`. Squash and merge with the title `chore(release): vX.Y.Z`. The version bump and image pin become a single commit on `main`.
5. Confirm `release-tag` succeeds. It verifies the image, creates the annotated version tag on the squash commit, and publishes the GitHub release. It adds no commits or sync branches.

The script never creates a version tag. Both the app and installer treat a version tag as an available update, so image publication and verification must finish first.

## Reuse and update guest images

`infra/guest-image.json` records the schema version, the SHA-256 of the guest build inputs, and the immutable multi-platform GHCR image reference. AMD64 and ARM64 platform images carry the input hash as a label. Release checks verify the index, both platform manifests, their architecture, their content digests, and their source labels through anonymous registry requests.

The input hash includes tracked files under `infra/guest`, their executable bits, `infra/guest-base.json`, the build helper, and the image workflow. Test scripts are excluded from the hash and Docker context. The guest manifest lives outside the build context, so pinning it does not request another build. Feature changes on `main` can leave its manifest stale until the next release.

When inputs match, Actions reuses a verified image from the release manifest, `main`, or the `src-<hash>` registry tag. Otherwise native jobs build and test each architecture before publishing the combined index. The tag is a lookup aid. Installations always use the immutable digest.

Use these commands to inspect a committed candidate:

```sh
python3 scripts/guest-release.py hash
python3 scripts/guest-release.py check
```

For developer builds, keep using `scripts/build-guest-image.sh [DATA_DIR]`. It builds on the verified base pin. To try unpublished base changes locally, build `infra/guest/base.Dockerfile` with Docker and set `ELSE_GUEST_BASE` to that local image. Docker remains a developer dependency. Normal installations use Microsandbox's direct registry pull.

## Recover a failed run

- If a native build or test fails, fix the release branch and rerun `guest-image`. No version tag exists yet.
- If planning reports a missing or stale base, publish it locally, commit the pin to the candidate, and rerun `guest-image`.
- If the manifest pin cannot be pushed because the branch changed, rerun `guest-image` on its new head. The workflow never force pushes.
- `release-ready` stays pending while guest images build. After Actions commits the pin, it dispatches the readiness workflow on the branch head. Its `verify-release` job reports a `release-ready` commit status for that exact commit: pending before validation, then success, failure, or error. GitHub excludes job checks from `workflow_dispatch` runs from required PR checks, so the workflow reports the commit status through the API. Release PR events do not start a competing readiness run. Feature PRs use a normal `release-ready` job check, which also works with read-only tokens on fork PRs. It checks the tooling and confirms the workspace version is unchanged. Dispatch runs name their job `verify-release` so only the explicit status satisfies the release gate. Both paths verify the PR head commit. A failed build or pin leaves readiness pending and blocks merging.
- If `main` advanced while the release was open, rebase the release branch onto `origin/main`, push with `--force-with-lease`, and rerun `guest-image`. The image is reused if its inputs still match.
- If final image verification fails after merge, restore public registry access and rerun `release-tag` on `main`. The version remains unavailable until verification succeeds.
- If tag creation succeeds but publishing the GitHub release fails, rerun `release-tag`. The image is already available to installers, and the rerun reuses the tag.

CI can be rerun without rebuilding a verified image, creating a second tag or adding commits to `main`. Published image digests must remain available for old releases and rollback. Candidate image cleanup is not automated.
