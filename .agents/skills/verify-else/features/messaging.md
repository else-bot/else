# Messaging, retries and stop work

## Sub-features

- Send once and stream the assistant's UTF-8 reply.
- Retry an unconfirmed send using its original request UUID.
- Stop a running turn and wait for the cancellation receipt to finish.
- Reconnect or restart without duplicated history.

## How to get to it (user POV)

Select a bot, type in the composer and press Enter. Use Retry on an unconfirmed
message, and Stop work or Ctrl+Shift+X for a running turn.

## Driving it with verify-else

The default `messaging` lane creates a bot and posts to
`/v1/bots/{id}/turns` with `provider_id:"synthetic"`. A retry must return the
same turn. `/conversation` must contain exactly the input and the complete
`Synthetic reply ✓ ...` text. The lane sends `[synthetic:hang]`, waits until
running, and retries one cancellation UUID until its receipt is `done` and
the turn is `interrupted`. It restarts the real API against the same data and
compares saved items/turns, including a changed process-bound event cursor.

Run `verify.py --tests turns cancel ws` for queue, replay and socket behavior.
The `ws` target starts real loopback WebSockets, but does not render GPUI.

For native proof, use `--fixture-provider` with the private session in the
[native desktop guide](../native-desktop.md). It supplies an offline Codex
protocol fixture for actual composer input. Without that flag, the session's
saved Codex provider is unavailable. The API driver always
names Synthetic explicitly. If the UI cannot select it, record that missing
precondition rather than starting a paid provider. For every native
run, capture composer submission, visible intermediate chunks, Stop work,
and the final persisted conversation through the probe.

## Gotchas

- `keys` success is not proof a send landed. Confirm an item and turn ID.
- Cancellation uses a fresh UUID once; reuse it when polling the receipt.
- Synthetic is the production development-provider boundary, not model quality
  or a proof of authenticated Codex/Claude continuation.
