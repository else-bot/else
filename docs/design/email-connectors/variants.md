# Connection flow variants

These self-contained HTML files are design prototypes for the native GPUI app. All account connections, notifications, and failures are simulated. No password is saved, no external account is connected, and no message is sent. Use fictional details.

## A. Connection inside the conversation

[Open variant A](variant-a.html). A private setup card appears where the bot requests the connection. Best fit for conversational setup and the existing native secret-card pattern. The conversation becomes tall during detailed permission choices.

## B. Workspace Connections hub

[Open variant B](variant-b.html). A dedicated page owns shared accounts and bot access. Best fit for several inboxes reused by several bots. It needs a clear handoff back to the requesting conversation.

## C. Guided setup

[Open variant C](variant-c.html). A focused wizard has an explicit step rail and defaults to iCloud in the provider picker. Best fit for explaining app-specific passwords and a longer first connection. It takes the owner out of the conversation while they finish setup.

My recommendation combines A as the entry from a bot request with B as the permanent account-management home. C is an alternative full setup flow. The three artifacts let the owner choose the interaction before native implementation.

## Try the flows

1. Choose Gmail, Microsoft, iCloud, Yahoo, Fastmail, or Other email.
2. Continue to simulated OAuth or the masked app-password form.
3. Choose read, organize, draft, send, and trash permissions. Choose whether new email wakes this bot.
4. Connect, then simulate a new arrival, pause, expired sign-in, and reconnection. The duplicate-notification result is illustrative UI, not evidence of backend deduplication.

OAuth opens no external window. The optional Apple instructions link is a real support link. The prototype does not model consent rejection, network delay, pending owner cancellation, all provider-specific settings, or accessible native focus behavior. Those remain implementation work.

## Reproduce and verify

Run `python docs/design/email-connectors/build_prototypes.py` to regenerate the three standalone HTML files. The generator is their source of truth. No server is required.

The T3 `html_preview` checks inject `check_prototypes.js` after the page script and read console results. Six rendering runs cover every provider path at widths 728 and 390. They check navigation, password clearing, permissions, wake dependency, absence of folder filters, simulated events, pause, reconnect, overflow, and pointer cursors. They end on the access screen for inspection. See `prototype-verification.json`.

The initial provider-picker previews and the checked access-state previews had no console errors. Layout inspection is recorded in the checkpoint. Prototype checks do not prove any application security or actual email delivery behavior.

## Selected direction

The user selected A for conversation-based connection setup and removed the watched-folder selector. New-email wake is one checkbox. Account management belongs to Email in the native app Settings modal, with left navigation and a scrollable content area. B and C remain comparison artifacts, not implementation targets.
