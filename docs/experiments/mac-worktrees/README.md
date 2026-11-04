# Concurrent Mac development worktrees

Two disposable Git worktrees ran the same `default` session on native macOS
arm64 on 2026-11-04. Both used real API and GPUI binaries. The provider was
the offline Codex protocol fixture. No provider account was used.

The [results](results.json) retain the process identities, ports, and checks.
The [decision log](decisions.tsv) records the design and evidence paths.
Raw logs, screenshots, and API responses remain in
`target/verification/macos-worktrees/` in the verification checkout.

## Observed behavior

| Check | Result |
| --- | --- |
| Both APIs use `default` | Distinct ports, data directories, databases, tokens, and main bot IDs. |
| Cross-worktree authentication | Each API returns 401 for the other API's token. |
| Native category input | Each window creates a category only in its own API. |
| Close A and restart its API | A keeps its saved data. B keeps its API, window, and running provider turn. |
| Rebuild A with `dev.py desktop` | Cargo uses A's private cache despite an inherited shared target directory. A publishes a new app copy and reconnects to its new port. |
| Preserve running copies | A's previous executable inode and B's executable inode remain unchanged. |
| Preserve B during A's rebuild | B keeps its API PID, URL, desktop PID, private profile, and running turn. |
| Stop work in B | The native shortcut interrupts B's turn. Native accessibility state then removes the Stop work action. |
| Close B | A's window and both APIs remain running. |
| Shared verification publisher | A separate native verification app opens. Closing it stops its API and removes its macOS preference domain. A second run verifies interruption also removes an explicitly seeded preference. |
| Cleanup | Both generated worktrees, private homes, build caches, processes, and development preference domains are removed. |

## Repeat the automated checks

Run these commands on macOS or inside the Linux arm64 SDK container:

```sh
python3 scripts/test-dev.py
python3 .agents/skills/verify-else/test_verify.py
cargo check --locked -p else-desktop
```

The Mac run passes all 18 development checks and all eight verification
checks. The Linux arm64 run passes 17 development checks and skips the
Mac-only app-launch check. All eight verification checks and the desktop
compile check pass on Linux. The native Mac desktop build and Rust format
check also pass.

## Limits

The Mac proof uses two actual worktrees and a normal Cargo rebuild for A.
Automated checks also cover equal checkout basenames, named sessions,
custom state directories, and concurrent publication of the same app.
The Omarchy compositor and omabox are not available on this Mac and were
not rerun here. The Linux app class and default title remain unchanged.

Development keeps the existing two-terminal workflow in each checkout.
After an API restart, close and relaunch that checkout's desktop to refresh
its port. App copies remain until the session is reset or dropped.
