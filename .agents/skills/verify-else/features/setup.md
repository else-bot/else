# Setup and bot profiles

## Sub-features

- Seed one stable main bot and saved provider defaults from empty storage.
- Connect the desktop to the local server, then complete the welcome card
  with a name and a supported execution profile.
- Greet the owner once, by that name, in a fresh main bot's conversation.
- Change an individual bot's provider/model without changing earlier turns.
- List each CLI's discovered models, with an upgrade offer for older ones.

## How to get to it (user POV)

A new desktop opens on **Start on this machine** with the local address and
owner key loaded. **Connect locally** (or Enter; **Start local** when the
desktop manages the local service) verifies and saves the connection; it
does not onboard. While onboarding is required, the window
shows **Welcome to else.bot**: a **Name** field, the provider/model picker
and **Get started**, which stays disabled until some provider is available.
For an existing bot, open Bot details (Ctrl+Shift+B), click the **Model**
row and use **Change model** → **Apply**, or **Switch** on the
"{model} available" upgrade row. `else-api setup` seeds storage before serving.

## Driving it with verify-else

Run `verify.py --tests installation onboarding bot_profiles` from the skill
directory, or use its full repository-relative path. These existing targets
drive the production setup/onboarding/profile code with temporary SQLite.
The default `onboarding` lane completes `POST /v1/onboarding`, repeats it,
restarts the API, and checks for exactly one greeting.

Without `--welcome`, the native helper completes onboarding through the public
API as a fixture, so it cannot prove the welcome card. For a welcome-screen
change, start the session with `--session native --welcome --fixture-provider`
so Codex is available and **Get started** is enabled. Capture the connection
page, choose **Connect locally**, type a name, and submit with Enter. Confirm
`/v1/capabilities` reports `onboarding_required:false` and `/v1/bots`
contains the chosen name. The main bot's conversation then opens with one
"Hi, I'm {name}. Welcome to else.bot." message. It survives an API restart
and no provider generates it.

For a picker change, pick another model, **Apply**, and confirm
`--path /v1/bots/ID/profile` reports the new model with `own:true`. The
fixture answers `codex debug models` with three models, so the catalog shows
`refresh_error:null` and an upgrade offer for `gpt-6.1-sol`.

## Gotchas

- Synthetic cannot be saved as an onboarding profile because it does not use
  model profiles (400 `unknown_provider`). The fixture saves the supported
  Codex profile without a provider run.
- Only the first completion greets, and only an empty main conversation.
  A repeat renames the bot and re-saves the profile without a second greeting.
- Provider availability in a private box does not prove host CLI login.
- "Couldn't refresh Codex's models" means model discovery failed; the picker
  then shows the built-in list.
- A profile change needs a later turn to prove application of the new profile;
  use the existing profiled fake boundary or a separately authorized live run.
