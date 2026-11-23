# iPhone app

## Sub-features

- Servers: add one by address and owner key (checked before saving), keep
  several, switch between them from the More menu. A stopped server shows
  "Reconnecting… Cannot reach the API".
- Bot list: bots waiting on you first, then the newest; unread dots, previews
  and "Working…", updated live. Opening a chat marks it read on the server.
- Chat: send text, stream the reply, stop a running reply (it reads
  **Stopped**, the turn `interrupted`).
- Files: attach photos or files (up to 8 of 10 MB) from **Attach**; images
  show inline, other files as cards. The message text spells them as
  `[File: name]`.
- Asks: answer a question with one tap; edit, discard or send a draft (after
  the turn ended, Send asks the bot to send it as it is); save or decline a
  secret request.
- New bot: a name, an avatar shape and color (a random one to start), and
  optionally a model.
- Details: the info button shows the bot's model (editable: provider, model,
  reasoning, speed), rules, routines, reminders, watchers and triggers, kept
  live.

## How to get to it (user POV)

Launch the app, enter the server address and owner key, and press
**Connect**. The **Bots** list has **More** (saved servers, the current one
checked, and **Add Server…**) and **New Bot** in its header. Tap a bot to
chat: **+** attaches, the arrow sends and turns into a stop button while a
reply runs. Ask cards appear in the transcript. The info button (**Details**) opens the bot's
details; **Model** there opens the model picker with **Save**.

## Driving it with verify-else

Follow [the iOS guide](../ios.md) for the session, the app and text entry.
Send the fixture's `[ui:*]` markers from the composer, then check the result
on the phone and through `--probe mobile` or the session database:

- Plain text gets `Fixture reply ✓ ...`. `[ui:hang]` keeps the turn running;
  stop it and expect `interrupted`.
- `[ui:question]` and `[ui:email]` give a question card and a draft card.
  Saving a draft edit bumps its version; Send asks **Send it now?** first.
- `[ui:secret]` asks for `fixture-smtp` with two fields. Save, then expect
  it in `GET /v1/task-secrets`; Decline leaves "fixture-smtp was not saved".
- `[ui:rule]`, `[ui:reminder]` and `[ui:routine]` add a rule, two reminders
  and a weekday routine; Details shows all three sections.
- On Model, pick another model, then snapshot again before picking its
  reasoning; `bot_profiles` shows the saved model, effort and revision.
- New Bot with a picked shape and color; the `bots` row has both.
- Send a message to another bot through `--probe mobile --method POST
  --path /v1/bots/{id}/turns --expected 202` while the list is open: the bot
  moves to the top with an unread dot, which clears after opening its chat.
- Switch to a stopped server from More and back.

## Gotchas

- `[ui:secret]` is the fixture's only secret request; its env names come
  back sorted (`SMTP_PASSWORD`, `SMTP_USERNAME`).
- Reminder and routine names must be unique per bot, so a second
  `[ui:reminder]` fails the turn with `NAME_TAKEN` until the first ones are
  deleted (`DELETE /v1/bots/{id}/routines/{routine}`; rows stay
  soft-deleted).
- The app's own unit tests run on the Mac with `cargo test --lib` in
  `apps/mobile`; they do not cover screens.
