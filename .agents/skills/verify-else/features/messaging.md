# Messaging, retries and stop work

## Sub-features

- Send once and stream the assistant's UTF-8 reply.
- Join a follow-up to an unfinished reply (`turn.steered_into`) instead of
  stopping it.
- A typed owner message skips every pending question and computer handoff
  the bot asked before it, whether it joins a reply or queues; secret forms
  and drafts stay pending. Reactions, notice answers and draft replies skip
  only the cards of the reply they join.
- Retry an unconfirmed send using its original request UUID.
- Retry a failed owner message from its **Failed** footer:
  `POST /v1/bots/{id}/turns/{turn}/retry` with `{"attempt": N}` queues a new
  attempt of the same turn (202); repeating it answers 200, and a turn that
  cannot be retried answers 409 `not_retryable`.
- Stop a running turn and wait for the cancellation receipt to finish.
- Reconnect or restart without duplicated history.

## How to get to it (user POV)

Select a bot, type in the composer and press Enter. While a reply streams,
it shows as a collapsed row ("{first words} · {seconds} · N updates"); a
second message sent meanwhile joins it. Use **Retry** on an unconfirmed
message. Stop a running reply with the composer's square stop button or
Ctrl+Shift+X (Cmd+Shift+X on macOS); the reply then reads **Stopped**.
Stop ends generation only; jobs and tasks keep their own stop rows.

## Driving it with verify-else

The default `messaging` lane creates a bot and posts to
`/v1/bots/{id}/turns` with `provider_id:"synthetic"`. A retry must return the
same turn. `/conversation` must contain exactly the input and the complete
`Synthetic reply ✓ ...` text. The lane sends `[synthetic:hang]`, waits until
running, and retries one cancellation UUID until its receipt is `done` and
the turn is `interrupted`. It restarts the real API against the same data and
compares saved items/turns, including a changed process-bound event cursor.

Run `verify.py --tests turns steering cancel ws` for joining, replay, stop
receipts, failed-turn retries (`turns::retries`) and socket behavior.

To drive a failed-turn retry through a session, send
`[synthetic:fail] x` with `"provider_id":"synthetic"` through `--probe NAME
--method POST --path /v1/bots/{id}/turns --expected 202`, wait for the turn
to read `failed`, then POST `{"attempt":1}` to `.../turns/{turn}/retry`
(`--expected 202`, then `200` for the repeat). The fixture has no failing
marker, so natively send the failing turn through the probe and click the
footer's **Retry**. The `ws` target starts real loopback
WebSockets, but does not render GPUI.

For native proof, use `--fixture-provider` with the private session in the
[native desktop guide](../native-desktop.md). It supplies an offline Codex
protocol fixture for actual composer input. Without that flag, the session's
saved Codex provider is unavailable. The desktop never names a provider, so
`[synthetic:*]` markers do nothing from the composer; use the fixture's
`[ui:slow]` (one chunk every 4 s) and `[ui:hang]`. For every native run,
capture composer submission, the streaming row, Ctrl+Shift+X, the
**Stopped** reply, and the persisted conversation through the probe.

To prove joining natively, send `[ui:slow]`, then type a follow-up and press
Enter while the reply still streams. Both messages appear above one reply,
and the probe shows the second turn's `steered_into` equal to the first.
The fixture accepts `turn/steer` and echoes the message like Codex does.

## Gotchas

- `keys` success is not proof a send landed. Confirm the bubble left the
  composer and the probe shows an item and turn ID before sending again.
- Cancellation uses a fresh UUID once; reuse it when polling the receipt.
- Synthetic and the fixture prove the production protocol boundaries, not
  model quality or authenticated Codex/Claude continuation.
