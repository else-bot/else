# Original guest implementation record

This is the implementation owner's record before the final independent review. Its original image was 615f091a. The parent then fixed retained modifier bookkeeping, added the reconnect check on all three paths, removed narration and closed test handles. The final image is `sha256:984912971d717ec71939681f58ef600a62a0dd1c8eb15085c34574747da0a542`. Run the two commands below against it. Their final logs show 21 and 12 passes.

```sh
docker run --rm --user elsebot -v "$PWD/infra/guest:/guest:ro" --entrypoint python3 \
  sha256:984912971d717ec71939681f58ef600a62a0dd1c8eb15085c34574747da0a542 \
  /guest/x11vnc/test_keymap_settle.py
docker run --rm --user elsebot -v "$PWD/infra/guest:/guest:ro" --entrypoint python3 \
  sha256:984912971d717ec71939681f58ef600a62a0dd1c8eb15085c34574747da0a542 \
  /guest/vnc/test_else_vnc_targets.py
```

The archived `gates.py`, generator and Rust probe use this run's private local paths. The probe was inserted into a temporary copy of the desktop crate, named `else62-keymap-probe`, never into production source. Its test function is preserved in `rust-client-probe.rs.txt`. Fresh source can be rebuilt through the pinned Dockerfile; the production X-level lever needs no temporary crate.

# Initial commands and outcomes

Fix shape. Patched Debian x11vnc 0.9.17-1+else1 (infra/guest/x11vnc/keymap-settle.patch),
built from the pinned snapshot source package in a Dockerfile builder stage over HTTPS.
websockify's controlling handler keeps the browser lock until x11vnc's `-gone` command
acknowledges that it has released the viewer's input and synced with the X server.

## Source and build

- `sh gen-patch.sh` regenerates the patch from the DSC-verified pristine source via
  `make-patch.py` (every edit must match exactly once). Rerun gives a byte-identical patch.
- `docker build --pull=false --iidfile image.id infra/guest` → build-image.log. Final
  image sha256:083d39ec9a56… (built), loaded as else-guest:3 by
  `scripts/build-guest-image.sh /private/tmp/else62-arm64-fd6449` → sha256:615f091a2fc3…
  (load-image.log). Both have identical RootFS layer lists; only config metadata differs.
- `dpkg-source: info: applying keymap-settle.patch`; no compiler warnings in keyboard.c or
  connections.c. image.json records `"x11vnc": "0.9.17-1+else1"`.
- HTTP snapshot downloads from the bare base were throttled; apt was OOM-killed in its
  retry queue (build-stage.log, earlier runs). The builder now reuses the runtime stage
  (CA certs, compiler) and uses HTTPS snapshot URLs; checksums still come from the signed
  snapshot indexes.

## Docker checks (image 083d39ec)

- Lock suite: `docker run --rm --user elsebot -v "$PWD/infra/guest:/guest:ro" --entrypoint
  python3 <image> /guest/vnc/test_else_vnc_targets.py` → 12/12 OK (lock-suite.log). New
  cases: lock waits for the ack and not the socket close (peer closes, ack 1 s later),
  no ack keeps the lock (fail closed), an ack for another port does not release, queued
  input past the socket is dropped. All five new fence tests fail against HEAD's module.
- Lever: same with `/guest/x11vnc/test_keymap_settle.py` → 18/18 OK on 3 full runs
  (lever.log, lever-run2.log, lever-run3.log), for x11vnc's default keymap path, -xkb and
  -noxkb, using the image's supervisord x11vnc command line. Covers 100 distinct new
  characters at zero gap, mixed Latin-1/CJK then 40 recycling characters then the same
  mixed text again, a held key never losing its keycode, 4096 commit then hang-up
  (lock ~0.12 s, no X key event after unlock, nothing held), held Shift + button + key
  with no releases sent: X state is clean at the moment a waiter gets the lock, and the
  all-held fail-closed log names no key.
- One earlier full lever run (before the ack change) had one misread press (keysym 0) in
  the default-path held-key test; not reproduced in 3 isolated runs and 3 later full runs.
- Baseline lever on the unpatched image c362a5c fails (lever-baseline-c362a5c.log).

## Real VM gates (disposable bot 1cd65d62-f6e2-4020-829e-251ac29ed9da, image 615f091a)

Driver: `PR62_BOT=<bot> python3 gates.py setup|gate1 6|gate2|gate3 watch|gate3 disconnect`,
plus `gate3_stop` / `gate3_stop_report` for API stop. Client: the real Rust desktop client
through the real API, from the temporary probe crate /private/tmp/else62-keymap-probe-fd6449
(copy of apps/desktop renamed else62-keymap-probe, test else62_keymap_probe; not in the repo).
All results in results.jsonl.

- Gate 1: fresh display each run, 30 ms per-character commits. `native-é漢字かな` 6/6 exact,
  100 distinct CJK 6/6 exact, nothing held after.
- Gate 2: 60 distinct CJK at 30 ms while x11vnc is SIGSTOPped 100 ms every 300 ms: exact.
  Ctrl+A replace, Tab focus change, then Ctrl+L and a URL with `é漢字` + 3 CJK typed into the
  address bar: all exact.
- Gate 3 (4096 distinct CJK in one commit, Shift held, then immediately):
  watch: 16 typed, exact prefix, last input 71 ms before lock free, 0 inputs after unlock,
  nothing held, lock free ~0.42 s after the Watch request (host/guest clocks).
  disconnect: 0 typed, lock free ~18 ms after the drop, nothing held.
  API stop (SIGTERM to verified PID 3263): 190 typed, exact prefix, last input 277 ms before
  lock free, 0 after, nothing held; API restarted with `.else-dev/pr62-arm64/api.py serve`
  (PID 95778, api.json matches).
  "Inputs after unlock" is DOM input-event time vs the guest lock-free time; the X-level
  ordering proof is the lever's held-input and events-after-lock checks.

Font bot 3d54093f-… was not used for gates (it was stopped; its image is the old c362a5c).
