# Release else.bot

Feature pull requests target `dev`, the default branch. `main` accepts release pull requests. Installations discover releases from `vX.Y.Z` tags.

## Configure GitHub once

1. Create `dev` from the current `main` and select it as the default branch.
2. Protect `main` with required pull requests, merge commits, and the required `release-ready` check from GitHub Actions. Require the branch to be up to date. Prevent force pushes and deletion of `main` and `release/v*`. Allow normal pushes to release branches so Actions can commit the image pin.
3. In the organization's Actions settings, allow GitHub Actions to create pull requests. Enable **Allow GitHub Actions to create and approve pull requests** in this repository's Actions workflow permissions. The release workflow uses this permission to open a sync PR if `main` cannot merge into `dev` automatically. It does not approve PRs.
4. Allow this repository's Actions workflows to publish the `else-guest` container package. Images carry the repository source label so GHCR associates them with this repository.

The bootstrap manifest has `image: null`. On the first release, Actions creates the package. GitHub makes a new package private by default. Set `else-bot/else-guest` to public in its package settings, then rerun the failed `guest-image` workflow on the release branch. It reuses the uploaded image. The required check refuses private images, so no version tag is published before anonymous downloads work.

GitHub-hosted native AMD64 and ARM64 runners build the Linux guest. The ARM64 image is also the guest filesystem used on Apple silicon. The guest tests run in isolated containers. These checks do not replace a real Microsandbox boot and native app verification on each supported host platform.

## Prepare a release

1. Merge the changes to `dev`. Resolve any outstanding `main` to `dev` sync PR first.
2. Run `scripts/release.sh patch`, `minor`, or `major`. Use `-y` to skip the push confirmation.
3. Open the printed PR. The script leaves your checkout intact and creates `release/vX.Y.Z` from `origin/dev` in a temporary worktree. A rerun reuses the branch and PR.
4. Wait for `guest-image` and `release-ready`. Review the app changes and `infra/guest-image.json`. Merge the PR with a merge commit once its required checks pass.
5. Confirm `release-tag` succeeds on `main`. It verifies the image again, creates the annotated version tag, publishes the GitHub release, and syncs `main` back into `dev`.

The script never creates a version tag. Both the app and installer treat a version tag as an available update, so image publication and verification must finish first.

## Reuse and update guest images

`infra/guest-image.json` records the schema version, the SHA-256 of the guest build inputs, and the immutable multi-platform GHCR image reference. AMD64 and ARM64 platform images carry the input hash as a label. Release checks verify the index, both platform manifests, their architecture, their content digests, and their source labels through anonymous registry requests.

The input hash includes tracked files under `infra/guest`, their executable bits, the build helper, and the image workflow. Test scripts are excluded from the hash and Docker context. The manifest lives outside the build context, so pinning it does not request another build. Changes on `dev` can leave its manifest stale until the next release.

When inputs match, Actions reuses a verified image from the release manifest, `main`, or the `src-<hash>` registry tag. Otherwise native jobs build and test each architecture before publishing the combined index. The tag is a lookup aid. Installations always use the immutable digest.

Use these commands to inspect a committed candidate:

```sh
python3 scripts/guest-release.py hash
python3 scripts/guest-release.py check
```

For developer builds, keep using `scripts/build-guest-image.sh [DATA_DIR]`. Docker remains a developer dependency. Normal installations use Microsandbox's direct registry pull.

## Recover a failed run

- If a native build or test fails, fix the release branch and rerun `guest-image`. No version tag exists yet.
- If the manifest pin cannot be pushed because the branch changed, rerun `guest-image` on its new head. The workflow never force pushes.
- `release-ready` stays pending while guest images build. After Actions commits the pin, it dispatches `release-ready` explicitly on the branch head. Opening or updating the PR does not start a competing readiness run. A failed build or pin leaves the required check pending and blocks merging. PRs into `main` must come from `release/vX.Y.Z`; other branches leave this check pending.
- If `main` advanced while the release was open, merge its changes into the release branch and rerun the checks.
- If final image verification fails after merge, restore public registry access and rerun `release-tag` on `main`. The version remains unavailable until verification succeeds.
- If tag creation succeeds but publishing the GitHub release fails, rerun `release-tag`. The image is already available to installers, and the rerun reuses the tag.
- If automatic sync conflicts or branch policy refuses the push, merge the `main` to `dev` sync PR with a merge commit. If organization policy prevents Actions from opening it, open the PR yourself or enable the setting above. Do not cut another release until `dev` contains `main`.

CI can be rerun without rebuilding a verified image, creating a second tag, or opening a second sync PR. Published image digests must remain available for old releases and rollback. Candidate image cleanup is not automated.
