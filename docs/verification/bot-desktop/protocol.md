# Desktop stream protocol

`GET /v1/bots/{bot}/desktop/stream?mode=watch|control&wake=false|true` upgrades an owner-authenticated WebSocket. Authentication uses the same bearer-header and origin policy as the owner API. The client never receives the guest VNC password or host port.

`wake` defaults to false. Attaching can initialize services on a running computer, but cannot start a stopped one. Explicit wake uses normal computer admission. An owner-stopped computer also requires `POST /v1/bots/{bot}/computer/resume`.

Server text messages use a `type` tag:

- `size` contains `width` and `height` in screen pixels.
- `mode` contains `mode`, either `watch` or `control`. It acknowledges the completed mode change.
- `error` contains `code` and `message`.

Binary messages contain a full PNG frame. Size and frame messages use one ordered outgoing queue. Resizing cancels an old pending encode. Frames are capped at 4096 pixels per side and 8M pixels total, with at most one frame queued. The native client allows up to 210 seconds for the initial boot status, then enforces 45 seconds of receive inactivity. The server sends changed frames at up to five per second and pings every 15 seconds.

Client text messages use a `type` tag:

- `mode` requests `watch` or `control`.
- `pointer` supplies unsigned screen coordinates `x`, `y` and the RFB `buttons` mask. Bits 0, 1 and 2 are left, middle and right. Bits 3 and 4 scroll vertically; bits 5 and 6 scroll horizontally.
- `key` supplies an X11 keysym `key` and boolean `down`.
- `release` releases all input held by this stream without changing mode.

Watching ignores pointer and key messages on both client and server. Take over admits one stream per bot and takes the guest browser lock. The controller releases held input before hand-back or normal disconnect. Every client command rechecks the viewer lease. Leases renew while connected and end after disconnect, stop, or deletion. Guest connection setup retries within ten seconds; writes have a five-second limit.

`computer_stopped` and `computer_not_running` return the app to its asleep state. `desktop_watch_unsupported` means the computer needs the version 3 image for Watching. `control_in_use` and `control_failed` refuse a mode change. A refused take-over leaves the native client watching and shows the refusal message. Fatal errors provide a retry action.

API shutdown refuses new streams with HTTP 503 `server_stopping`. It signals existing streams and waits for their tracked cleanup within `ELSE_SHUTDOWN_GRACE_SECS`. Cleanup releases held input before waiting on database work, then returns control and viewer leases. A nonreading client cannot block that input release. A stalled guest or database can still exhaust the grace deadline. The native app shows the stopping message and offers explicit retry.

On macOS, committed text and IME composition use the platform text-input handler. One commit occupies one bounded local queue entry and is sent as ordered keysym taps. Commits above 4096 characters are refused with a visible message. Control shortcuts and named keys use key events. Command maps to guest Super. Option participates in macOS composed text and dead keys; guest Alt-letter mnemonics are not exposed by that text path. The wire messages remain `key` events.

Image 3 patches x11vnc to settle new X11 mappings and recycle added keycodes that are not held. The 20 ms mapping delay is empirical and does not guarantee ordering for a suspended application. A hung-up controlling peer stops queued presses. Held keys and buttons are released and X11 is synced before x11vnc's `-gone` hook acknowledges cleanup. The acknowledgement establishes X11 release, not application rendering or saving. The controlling websockify handler holds the browser lock until that acknowledgement. EOF alone is not sufficient. Missing acknowledgement keeps the lock held. The acknowledgement uses the source port; abrupt handler death followed by port reuse and a delayed old acknowledgement remains a crash limitation.

The browser lock is advisory. Guest programs that connect directly to CDP without the supplied helpers bypass it. Version 2 computers require an explicit image upgrade for Watching.

Shift+Tab uses X11 `ISO_Left_Tab` with Shift. Sending plain Tab with Shift allows x11vnc's modifier adjustment to clear Shift and move focus forward.
