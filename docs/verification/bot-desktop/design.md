# Bot computer viewer

The sidebar's Computer tab inspects state without waking the VM. Clicking an asleep card wakes it and expands the viewer inside the app. Watching is the default. Take over forwards input only after server acknowledgement.

The API speaks RFB to the guest's existing VNC server and streams PNG frames over an authenticated WebSocket. The GPUI client paints those frames and sends input messages. The guest password and private endpoint stay on the API host. This avoids native WebView overlay and Linux display-server restrictions.

## Ownership and input

- `apps/api/src/computers/desktop.rs` owns viewer admission, leases, and control claims. Leaving Computer, closing the sidebar, switching bots, and closing the window end the viewer lease.
- `apps/api/src/computers/viewer.rs` implements the RFB handshake and guest WebSocket bridge. `apps/api/src/http/desktop_stream.rs` owns stream queues, held input, control transitions, and shutdown cleanup.
- `apps/desktop/src/app/computer.rs` owns Watching/Take over UI state, letterbox coordinate mapping, focus release, and native keyboard input. `computer/text_input.rs` handles macOS committed text and composition.
- Guest port 5901 is view-only. Port 5900 controls input through the browser lock shared with CDP helpers. `infra/guest/vnc/else_vnc_targets.py` retains that lock until x11vnc acknowledges input cleanup.
- `infra/guest/x11vnc/keymap-settle.patch` settles new key mappings, recycles unheld added keycodes, and releases input before acknowledging a departed controller. Its X-level regression suite covers both keyboard mapping paths and reconnects.

Frames are capped at five changed frames per second. High-motion video needs a different transport. See the [wire protocol and limits](protocol.md) and [verification guide](handoff.md).
