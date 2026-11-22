# else.bot for iOS (alpha)

A phone client for an else.bot server that runs elsewhere: a home machine,
a VPS, or the laptop next to you. It never runs a server or bots itself.

Built in Rust with [Native Shell](https://nsh.dev) (NSH). The screens are
UIKit views drawn by NSH. The API types come from `crates/wire`, shared
with the server and the desktop app.

## What works

- Connect with the server's address and owner key, checked before they
  are saved.
- The bot list, ordered like Messages. Bots waiting on you come first,
  then newest first. Shows unread dots, "Working…", and previews.
  Pull to refresh.
- A chat per bot. Replies render as Markdown and stream in live over the
  WebSocket. Older pages load as you scroll up. Send a message, or stop a
  reply that is running.
- Disconnect from the More menu, which removes the key from the phone.

Not yet: answering question cards, approving drafts, secrets, attachments,
creating bots, several servers, push notifications.

## Addresses and the owner key

Like the desktop, the app sends the key only to the one saved origin and
never follows redirects.

- `https://…` works for any host and verifies the certificate.
- `http://` works only for a Tailscale IP (`100.64.0.0/10`, `fd7a:115c:a1e0::/48`),
  and only while this phone has a Tailscale address. With Tailscale off, a
  `100.x` address could reach a carrier's network instead.
- `http://127.0.0.1` also works, which is only useful in the simulator.

Read the key on the server with `cat ~/.else/data/owner-token`.

## Build and run

Install NSH (`curl -fsSL https://nsh.dev/install.sh | sh`), the iOS Rust
targets (`rustup target add aarch64-apple-ios-sim aarch64-apple-ios`) and Xcode.
Use rustup's cargo, not Homebrew's: `export PATH=$HOME/.cargo/bin:$PATH`.

```sh
cd apps/mobile
nsh run --target ios                  # simulator, rebuilds on change
nsh package --target ios-simulator    # .nsh/out/else.bot.app
cargo test --lib                      # model and parsing tests on the Mac
cargo run                             # the same UI in a macOS window
```

For a test server, run `scripts/dev.py api --session mobile` from the
repository root. Point the real provider CLIs at missing paths
(`ELSE_CODEX_BIN=/nonexistent/codex`, and the same for `CLAUDE`, `GROK`,
`CURSOR`), so only the synthetic provider answers. In the simulator, connect
to the printed `http://127.0.0.1:PORT` with `.else-dev/mobile/data/owner-token`.

## Layout

- `src/server.rs` validates the origin and the key, checks Tailscale, and
  saves the profile.
- `src/api.rs` holds the HTTP and WebSocket client, on its own Tokio runtime.
- `src/conversation.rs` applies live events to a loaded conversation in
  cursor order.
- `src/bots.rs` decides each bot list row from the server's bot record.
- `src/ui/` holds the screens: connect, the bot list, and a chat.
  `session.rs` holds the connected server's state as signals.
