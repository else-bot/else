# Concurrent Lume verification

Two real Git worktrees ran `lume.py run --session work` concurrently on a
macOS arm64 host. Each launched its own native GPUI desktop and API in a
separate Lume guest. Both clones stopped and were deleted after collection.
The reusable `else-verify-base` remains stopped.

The [workflow](../../../.agents/skills/verify-else/lume.md) documents setup,
launch, guest input, API probes, and recovery. [results.json](results.json)
records checkout identities, executable hashes, distinct VM identities,
guest addresses, app identifiers, and cleanup proof.

## Observed behavior

- Each worktree used the same session label but received a different VM,
  machine identifier, MAC, guest address, control token, and app identifier.
- Native category input created `Worktree A only` in A and `Worktree B only`
  in B. Each API listed only its own category. B rejected A's guest control
  token with HTTP 401.
- B's native composer sent `Hello while both worktrees run` and rendered
  `Fixture reply ✓ from the desktop composer.`. Its API recorded the same
  message and completed response.
- B then sent `[ui:hang]`. A's cleanup collected evidence, stopped its VM,
  and deleted its clone. The same B turn remained running, and fresh native
  captures and API probes still worked. Input to A's closed run was refused.
- Pressing **Stop work** inside B interrupted that turn. B's cleanup collected
  its evidence and removed its clone. Both manifests recorded a stopped,
  unchanged base identity.

The screenshots show [A's category](worktree-a.png),
[B still working after A stopped](worktree-b-after-a-stopped.png), and
[B's interrupted turn](worktree-b-stopped-turn.png).

The working frame has unchanged pixels before and after A's cleanup. Its
[fresh capture record](evidence/b-after-a-cleanup.capture.json) establishes
when the screenshot was taken. The timestamped
[before probe](evidence/b-before-a-cleanup.probe.json),
[A cleanup record](evidence/a-cleanup.json), and
[after probe](evidence/b-after-a-cleanup.probe.json) establish the order and
identify the same running turn. The [native action](evidence/b-stop-work.action.json)
and [final probe](evidence/b-interrupted.probe.json) record B's cancellation.

Both APIs happened to bind port 49152 on their own guest loopback interfaces.
Their separate guest networks, credentials, databases, and native input state
establish isolation. Distinct port numbers were not required.

## Validation and limits

Eight Lume ownership and failure-path checks and eight existing verification
runner checks passed. Review found and fixed fixture Python selection,
failed VM queries being mistaken for absence, and resource paths containing
spaces or macOS filesystem aliases. Cleanup query failures and resource path
handling have focused regression checks.

The live coordinators retained the helper source loaded at startup. Later
edits changed resource-path matching and added base provenance metadata.
Those changes passed the final unit run. Query-error and other failure
branches use fake boundaries, rather than fault injection into live VMs.

The trial used Lume 0.6.1, cua 0.10.1 for guest shell calls, and the prepared
image's cua-driver 0.38.0 through direct guest MCP. Each guest used four CPUs
and 8 GiB of memory. The native Rust binaries came from `34962bb`; both
worktrees had private binary copies. This change adds tooling and documentation.
It does not change or rebuild the Rust application.

No host driver or VM viewer was used. Host pointer and focus were not
measured. The independent review used the supplied conversation and artifacts
because no full scoped transcript path was available.

The [decision trail](decisions.tsv), selected timestamped records under
`evidence/`, and [test log](evidence/unit-tests.log) are committed. Additional
raw captures, guest doctor records, and lifecycle logs remain locally under
`target/verification/lume-worktrees/`. Those supplementary files are disposable.
The second Git worktree was removed after its evidence was copied. Closed-run
control tokens were pruned from the retained A and preflight directories.
