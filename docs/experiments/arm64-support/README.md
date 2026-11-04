# Native arm64 support

The verification workflow now supports native macOS arm64 and Linux arm64.
The existing Linux amd64 omabox path remains available. This pass ran on an
Apple arm64 host with a native aarch64 Docker engine. No architecture emulation
was used.

## Results

[results.json](results.json) records the final coverage. Raw logs and
screenshots remain in this checkout under `target/verification/`.

| Check | macOS arm64 | Linux arm64 container |
| --- | --- | --- |
| Four public API journeys | Passed | Passed |
| Offline Rust workspace tests and doctests | Passed | Passed |
| Private S3 replication fixture | Passed | Passed |
| Installation, update, and idempotent reinstall | Passed with launchd | Passed with managed process |
| Actual GPUI keyboard and pointer journeys | Passed | Passed with software Vulkan |
| Real Microsandbox computers | Four journeys passed | Not run without KVM |
| Runner failure and protocol checks | Eight passed | Eight passed |

The matrix command was:

```sh
python3 scripts/test-e2e.py macos-arm64 ubuntu-arm64 --compile-live --test-threads 2 --test-jobs 4 --output target/verification/arm64-complete
```

`--compile-live` compiles gated targets. Their provider gates stay disabled.
It does not establish live provider coverage. Real VM execution was a
separate explicit run with `TMPDIR=/tmp ELSE_LIVE_VM=1`. Its four journeys
covered coordinated backup/restore, guest tools and desktop isolation,
acquisition races, and persistence across stops and restarts. The portable
host-canary address check then passed in a focused rerun.

Desktop proof covers connection checking, onboarding and the greeting,
composer submission and intermediate streaming, keyboard stop, settings
rename/save/collapse, category creation and dragging, and bot creation/deletion.
The offline Codex fixture uses the real adapter without a model account.
Mac screenshots and persisted API state are in `target/verification/macos-ui`.
Linux screenshots and persisted state are in `target/verification/linux-desktop`.
The fixture's initial resume experiment matched a historical hang marker.
It now inspects only the latest message and owns cancellation per turn.

## Corrections found by real execution

- Five guest shell snippets contained SQLite placeholders instead of shell
  positional parameters. File operations and screenshot paths failed. The
  screenshot cleanup could target unrelated files in its working directory.
  Real Linux shell regressions now check the actual arguments and preserve
  an unrelated canary. Real Mac VM file and desktop checks pass.
- Reinstalling the current version ran setup while its API held the data
  lock. The installer now preserves the running API for current-version
  reinstalls. Fresh installations still run setup.
- The Mac development probe accepted an unrelated PID when another process
  held the lock. It now verifies the named PID has that file open.
- Python's HTTP fixtures could stall in Mac reverse DNS. Their numeric
  loopback listeners now avoid that lookup.
- Private Mac runtime homes hid Docker contexts and Compose plugins.
  Fixture management retains the original Docker configuration path.

## Review and audit

[decisions.tsv](decisions.tsv) retains decisions, failed experiments, and
evidence pointers. Claude Opus 5.5 reviewed the implementation, comments,
and the trail. Its installer and worker-cleanup findings were corrected.
The process backend now drives its lanes directly with cooperative
interruption. A regression interrupts a real blocking child and verifies
its process and private home are gone. Native sessions watch both their API
and desktop. Evidence-copy failures retain a failed result record.

Earlier passing runs are baseline evidence. The final matrix includes the
review corrections. The newest fixture and interruption tests were also run
separately on both native platforms. All owned UI sessions and disposable
containers were removed. Build caches and evidence remain for reuse.

Live provider accounts, Linux host KVM, and Linux hardware GPU rendering
remain unverified. This arm64 host did not rerun amd64 executables.

Model the Domain kept platform orchestration in the existing E2E runner and
API journeys in verify-else. The Laziness Protocol avoided a second Docker
coordinator. Prove It Works required actual GPUI input and real VM execution
in addition to compilation and public API checks.
