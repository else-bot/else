# Setup and bot profiles

## Sub-features

- Seed one stable main bot and saved provider defaults from empty storage.
- Complete the welcome screen with a name and supported execution profile.
- Greet the owner once, by that name, in a fresh main bot's conversation.
- Change an individual bot's provider/model without changing earlier turns.

## How to get to it (user POV)

Launch a new installation to see the welcome screen. For an existing bot,
open its provider/model picker. `else-api setup` seeds storage before serving.

## Driving it with verify-else

Run `verify.py --tests installation onboarding bot_profiles` from the skill
directory, or use its full repository-relative path. These existing targets
drive the production setup/onboarding/profile code with temporary SQLite.
The default smoke doctor checks the seeded main bot and provider availability.

The native helper completes onboarding through the public API as a fixture.
It therefore cannot prove the welcome screen's controls. For a welcome-screen
change, launch an empty API without that fixture step and complete the actual
screen; confirm `/v1/capabilities` reports `onboarding_required:false` and
`/v1/bots` contains the chosen name. Capture the screen before and after.
The main bot's conversation then opens with one assistant greeting that uses
the chosen name. It survives an API restart and no provider generates it.
The native helper's fixture onboarding creates that greeting too.

## Gotchas

- Synthetic cannot be saved as an onboarding profile because it does not use
  model profiles. The fixture saves the supported Codex profile without a provider run.
- Only the first completion greets, and only an empty main conversation.
  A repeat renames the bot without a second greeting.
- Provider availability in a private box does not prove host CLI login.
- A profile change needs a later turn to prove application of the new profile;
  use the existing profiled fake boundary or a separately authorized live run.
