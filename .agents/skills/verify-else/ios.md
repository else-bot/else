# iPhone app in the iOS Simulator

The iPhone app (`apps/mobile`) is a client for a server that runs elsewhere,
so its proof is one seeded API session on loopback plus the app in a
simulator on the same Mac. This needs macOS with Xcode, NSH and the iOS
Rust targets ([the app's README](../../../apps/mobile/README.md)). On Linux
the iPhone app is unreachable; report it as such.

## Launch

Start a session with the offline Codex fixture and keep it running in the
background for the whole pass:

```sh
cargo build --workspace --locked --bins
.agents/skills/verify-else/verify.py --session mobile \
  --api-binary "$PWD/target/debug/else-api" --fixture-provider
```

The session refuses an existing `~/else-verification/mobile` directory, so
copy an earlier run's evidence out and remove it, or pick another name. It
stops after an hour; drive in one sitting or start a new one.
`~/else-verification/mobile/session.json` gives the session's `url` and its
`data` directory. The owner key is the file `ELSE_OWNER_TOKEN_FILE` names in
`~/else-verification/mobile/desktop.env`.

Package and install the app. Homebrew's cargo has no iOS targets, so put
rustup's first:

```sh
cd apps/mobile
PATH="$HOME/.cargo/bin:$PATH" nsh package --target ios-simulator
xcrun simctl install UDID .nsh/out/else.bot.app
xcrun simctl launch UDID bot.else.mobile
```

On the welcome screen, enter the session's `http://127.0.0.1:PORT` and the
owner key, then press **Connect**. A server saved by an earlier run stays in
the More menu; switch to the session's address there.

## Doctor

Before the first drive and after any surprise:

```sh
.agents/skills/verify-else/verify.py --probe mobile
```

It lists the session's bots. Then check the app shows the same list. A
blank list with "Reconnecting… Cannot reach the API" means the app points
at a stopped server; switch servers before driving anything.

## Drive

Use the runtime's simulator driver (in T3, the device tools and the
`agent-device` command they return) for taps, typing and screenshots; use
`simctl` only to install, launch, and add media. The app's controls have
accessibility labels ("Send message", "Attach", "Details", "More",
"New Bot", "Create", "Save", "Decline"), so press by label or snapshot ref
where you can. NSH dialogs, such as **Send it now?**, are app views, not
system alerts: press their buttons by position from a screenshot.

The feature recipes are in [the mobile feature file](features/mobile.md).
Confirm each persisted effect through `--probe` or the session database
(`DATA/else.sqlite3`, read-only), not only on screen.

Text entry needs care:

- With the keyboard up, the lower half of the screen is keys. Close it
  (`keyboard dismiss`, or the keyboard's ✓ key, which also submits a form
  such as New Bot) before pressing anything it covers, and re-snapshot.
- Fill one field at a time: press the field, type, close the keyboard, then
  the next field. Typing into a secure field after another one can land both
  values in the first. `fill` on secure fields and `fill ""` are unreliable.
- A pick that rebuilds a list (a new model reloads its reasoning levels)
  invalidates snapshot refs. Snapshot again before the next press.

To attach a file, copy it into the simulator's **On My iPhone** folder,
which **Choose File** opens:

```sh
cp notes.txt "$(xcrun simctl get_app_container UDID com.apple.DocumentsApp groups \
  | awk -F'\t' '/FileProvider.LocalStorage/ {print $2}')/File Provider Storage/"
```

`xcrun simctl addmedia` can fail with PHPhotosErrorDomain 3301 on a
simulator whose Photos library is not set up; then **Photo Library** shows
"Unable to Load Photos". Cover attachments through **Choose File** and
record the photo path as unreached.

## Evidence

Save screenshots of each entry action and its result next to the session's
evidence, under a new `target/verification/` directory, together with
`~/else-verification/mobile/`. Record the simulator model and iOS version.

## Cleanup

Copy evidence first. Stop the background `--session` process, which removes
its temporary data. Delete bots and servers you added only if the simulator
is shared; `xcrun simctl uninstall UDID bot.else.mobile` removes the app and
its saved servers. Never erase a simulator you did not create.
