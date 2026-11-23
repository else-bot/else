//! A bot's provider and model: the provider, its model, the reasoning
//! effort and the speed, as grouped lists the way iOS Settings picks one
//! option. The chat's Model screen saves it to the bot; New Bot sends it
//! with the create.

use else_wire::settings::{
    BotProfile, FAST_TIER, ProfileChoice, ProviderInfo, effort_label, initial_choice, model_label,
};
use nsh_base::nav_page;
use nsh_core::element::header::HeaderItem;
use nsh_core::prelude::*;
use std::rc::Rc;

use uuid::Uuid;

use super::list;
use super::parts::{self, colors};
use super::session;
use else_client::request::Request;

use std::sync::Arc;

use crate::api::{Client, ClientError};

/// A profile in a few words, such as "Claude Opus 5.5, High".
pub fn summary(providers: &[ProviderInfo], choice: &ProfileChoice) -> String {
    let model = providers
        .iter()
        .find(|p| p.state.id == choice.provider_id)
        .and_then(|p| p.catalog.model(&choice.model))
        .map_or_else(|| model_label(&choice.model), |m| m.label.clone());
    let fast = if choice.service_tier == FAST_TIER {
        ", Fast"
    } else {
        ""
    };
    format!("{model}, {}{fast}", effort_label(&choice.reasoning_effort))
}

/// The lists that edit `choice`. Each pick keeps the rest of the profile
/// when the provider still offers it.
pub fn picker(providers: Vec<ProviderInfo>, choice: Signal<Option<ProfileChoice>>) -> View {
    let providers = Rc::new(providers);
    let all = providers.clone();
    let current = move || {
        let choice = choice.get()?;
        let provider = all
            .iter()
            .find(|p| p.state.id == choice.provider_id)
            .cloned()?;
        Some((provider, choice))
    };
    let provider_rows = {
        providers
            .iter()
            .map(|provider| {
                let id = provider.state.id.clone();
                let usable = provider.state.cli_available;
                let detail = (!usable)
                    .then(|| provider.state.cli_unavailable_reason.clone())
                    .flatten()
                    .or_else(|| (!usable).then(|| "Not set up on the server.".to_owned()));
                let picked = {
                    let id = id.clone();
                    move || choice.with(|c| c.as_ref().is_some_and(|c| c.provider_id == id))
                };
                let info = provider.clone();
                list::option_row(
                    provider.state.label.clone(),
                    detail,
                    picked,
                    usable,
                    move || {
                        let previous = choice.get_untracked();
                        if let Some(next) = info.choice(previous.as_ref()) {
                            choice.set(Some(next));
                        }
                    },
                )
                .into_element()
            })
            .collect::<Vec<_>>()
    };
    view()
        .gap_6()
        .child(list::section(Some("Provider"), provider_rows, None))
        .child(dynamic(move || {
            let (provider, picked) = current()?;
            let model = provider.catalog.model(&picked.model)?.clone();
            // A saved provider the server cannot run stays shown, but its
            // options cannot change until the owner picks a runnable one.
            let usable = provider.state.cli_available;
            let models = provider.catalog.models.iter().map(|model| {
                let id = model.id.clone();
                let info = provider.clone();
                let on = {
                    let id = id.clone();
                    move || choice.with(|c| c.as_ref().is_some_and(|c| c.model == id))
                };
                list::option_row(model.label.clone(), None, on, usable, move || {
                    let Some(mut next) = choice.get_untracked() else {
                        return;
                    };
                    next.model = id.clone();
                    choice.set(info.choice(Some(&next)));
                })
                .into_element()
            });
            let efforts = model.efforts.iter().map(|effort| {
                let value = effort.clone();
                let on = {
                    let value = value.clone();
                    move || choice.with(|c| c.as_ref().is_some_and(|c| c.reasoning_effort == value))
                };
                list::option_row(
                    effort_label(effort).to_owned(),
                    None,
                    on,
                    usable,
                    move || {
                        choice.update(|c| {
                            if let Some(c) = c {
                                c.reasoning_effort = value.clone();
                            }
                        });
                    },
                )
                .into_element()
            });
            let speeds = model.tiers.iter().any(|tier| tier == FAST_TIER).then(|| {
                let speed = |label: &str, tier: &'static str| {
                    let on =
                        move || choice.with(|c| c.as_ref().is_some_and(|c| c.service_tier == tier));
                    list::option_row(label.to_owned(), None, on, usable, move || {
                        choice.update(|c| {
                            if let Some(c) = c {
                                c.service_tier = tier.to_owned();
                            }
                        });
                    })
                };
                list::section(
                    Some("Speed"),
                    [speed("Standard", "default"), speed("Fast", FAST_TIER)],
                    None,
                )
            });
            Some(
                view()
                    .gap_6()
                    .child(list::section(Some("Model"), models, None))
                    .child(list::section(Some("Reasoning"), efforts, None))
                    .child(speeds),
            )
        }))
}

/// The providers a bot can pick from and its profile as the server reports
/// it (its own, or the defaults it follows).
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileInfo {
    pub providers: Vec<ProviderInfo>,
    pub saved: Option<BotProfile>,
}

/// Reads a bot's profile and the providers to label it with, together.
pub async fn load_profile(client: Arc<Client>, bot: Uuid) -> Result<ProfileInfo, String> {
    let (caps, saved) = futures_util::join!(client.clone().capabilities(), client.bot_profile(bot));
    Ok(ProfileInfo {
        providers: caps.map_err(|err| err.to_string())?.offered(),
        saved: saved.map_err(|err| err.to_string())?,
    })
}

/// What a retry of a save whose outcome is unknown finds on the server
/// before saving again, so it neither repeats a save that landed nor
/// overwrites a change made meanwhile on another device.
enum Reconciled {
    /// The earlier save landed.
    Saved(BotProfile),
    /// Someone changed the profile since it was loaded.
    Changed(Option<BotProfile>),
    /// Nothing changed: saving again is safe.
    Unchanged,
    /// The profile could not be read back, so the save stays unconfirmed.
    Unread(String),
}

fn key(bot: Uuid) -> String {
    format!("model-{bot}")
}

pub fn open(bot: Uuid) {
    super::push(key(bot), move || screen(bot));
}

fn screen(bot: Uuid) -> Screen {
    let load = resource(
        || Some(()),
        move |(), _| load_profile(session::server().client.clone(), bot),
    );
    let choice = signal(None::<ProfileChoice>);
    let save = signal(Request::<ProfileChoice>::Idle);
    // The picker opens on what the bot uses now, once it has loaded.
    let opened = stored(false);
    effect(move || {
        if let Some(info) = load.get()
            && !opened.replace(true)
        {
            choice.set(shown(&info.providers, info.saved.as_ref()));
        }
    });

    // Changed from what the bot uses now, own or followed: opening the
    // screen and tapping Save cannot pin the defaults by accident.
    let dirty = computed(move || match load.get() {
        Some(ProfileInfo { saved, .. }) => {
            let chosen = choice.get();
            chosen.is_some() && chosen != saved.map(|s| s.choice)
        }
        None => false,
    });
    let ready = computed(move || dirty.get() && !save.with(Request::busy));

    let submit = move || {
        let Some(picked) = choice.get_untracked() else {
            return;
        };
        if !ready.get_untracked() {
            return;
        }
        let retry = save.with_untracked(|r| r.unconfirmed_op() == Some(&picked));
        let loaded = untracked(|| load.get()).and_then(|info| info.saved);
        let client = session::server().client.clone();
        let retried = picked.clone();
        session::submit(
            save,
            picked,
            move |picked| async move {
                if retry {
                    // A read that fails says nothing of the earlier save:
                    // it stays unconfirmed, so the next Save reads again
                    // instead of writing blind.
                    match reconcile(&client, bot, &picked, loaded.as_ref()).await {
                        Ok(Reconciled::Unchanged) => {}
                        Ok(found) => return Ok(found),
                        Err(err) => return Ok(Reconciled::Unread(err.to_string())),
                    }
                }
                client
                    .set_bot_profile(bot, picked)
                    .await
                    .map(Reconciled::Saved)
            },
            move |found| match found {
                Reconciled::Saved(saved) => {
                    save.try_set(Request::Idle);
                    if let Some(info) = untracked(|| load.get()) {
                        load.mutate(ProfileInfo {
                            saved: Some(saved),
                            ..info
                        });
                    }
                    super::routes().update(|r| r.0.retain(|route| *route.key != *key(bot)));
                }
                Reconciled::Changed(latest) => {
                    save.try_set(Request::Failed(
                        "The model was changed on another device. This shows it now.".into(),
                    ));
                    if let Some(info) = untracked(|| load.get()) {
                        choice.set(shown(&info.providers, latest.as_ref()));
                        load.mutate(ProfileInfo {
                            saved: latest,
                            ..info
                        });
                    }
                }
                Reconciled::Unread(error) => {
                    save.try_set(Request::Unconfirmed { op: retried, error });
                }
                Reconciled::Unchanged => {}
            },
        );
    };

    let save_item = move || {
        vec![
            HeaderItem::new(if save.with(Request::busy) {
                "Saving…"
            } else {
                "Save"
            })
            .key("save")
            .disabled(!ready.get())
            .on_press(submit),
        ]
    };

    let body = dynamic(move || match (load.get(), load.error()) {
        (None, None) => Some(
            text("Loading…")
                .mx_4()
                .text_size(px(15.0))
                .text_color(colors::secondary())
                .into_element(),
        ),
        (None, Some(message)) => Some(
            text(message)
                .mx_4()
                .text_size(px(15.0))
                .text_color(colors::destructive())
                .into_element(),
        ),
        (Some(info), _) if info.providers.is_empty() => {
            Some(parts::footnote("No provider on the server lists models yet.").into_element())
        }
        (Some(ProfileInfo { providers, saved }), _) => {
            let follows = saved.as_ref().is_none_or(|s| !s.own);
            let gone = saved
                .as_ref()
                .filter(|s| !providers.iter().any(|p| p.state.id == s.choice.provider_id))
                .map(|s| s.choice.provider_id.clone());
            Some(
                view()
                    .gap_2()
                    .child(gone.map(|id| {
                        parts::footnote(format!(
                            "The server no longer offers {id}, which this bot uses. Pick another provider."
                        ))
                    }))
                    .child(untracked(|| picker(providers, choice)))
                    .child(follows.then(|| {
                        parts::footnote(
                            "The bot follows the server's defaults until you save a model here.",
                        )
                    }))
                    .into_element(),
            )
        }
    });

    let status = dynamic(move || {
        let message = match save.get() {
            Request::Failed(message) => message,
            Request::Unconfirmed { error, .. } => {
                format!("{error} It may have saved. Save checks the server before saving again.")
            }
            _ => return None,
        };
        Some(
            text(message)
                .mx_4()
                .text_size(px(13.0))
                .line_height(px(18.0))
                .text_color(colors::destructive()),
        )
    });

    nav_page()
        .key(key(bot))
        .title("Model")
        .header_large_title(false)
        .header_right_items(save_item)
        .child(
            scroll()
                .flex_1()
                .bg(colors::background())
                .child(view().py_6().gap_4().child(status).child(body)),
        )
}

/// The choice the screen opens on: the bot's profile as the server's
/// catalog applies it now, else the first runnable provider. A provider
/// the server no longer offers opens with nothing picked.
fn shown(providers: &[ProviderInfo], saved: Option<&BotProfile>) -> Option<ProfileChoice> {
    let gone = saved.is_some_and(|s| !providers.iter().any(|p| p.state.id == s.choice.provider_id));
    if gone {
        return None;
    }
    initial_choice(providers, saved.map(|s| &s.choice))
}

/// Reads the profile back after a save whose outcome is unknown.
async fn reconcile(
    client: &std::sync::Arc<crate::api::Client>,
    bot: Uuid,
    tried: &ProfileChoice,
    loaded: Option<&BotProfile>,
) -> Result<Reconciled, ClientError> {
    let now = client.clone().bot_profile(bot).await?;
    Ok(match now {
        Some(now) if now.own && now.choice == *tried => Reconciled::Saved(now),
        now if now.as_ref().map(|p| (p.own, p.revision)) != loaded.map(|p| (p.own, p.revision)) => {
            Reconciled::Changed(now)
        }
        _ => Reconciled::Unchanged,
    })
}
