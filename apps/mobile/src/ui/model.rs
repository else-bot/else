//! A bot's provider and model: the provider, its model, the reasoning
//! effort and the speed, as grouped lists the way iOS Settings picks one
//! option. The chat's Model screen saves it to the bot; New Bot sends it
//! with the create.

use else_wire::settings::{
    BotProfile, FAST_TIER, ProfileChoice, ProviderInfo, effort_label, model_label,
};
use nsh_base::nav_page;
use nsh_core::element::header::HeaderItem;
use nsh_core::prelude::*;
use std::rc::Rc;

use uuid::Uuid;

use super::parts::{self, colors};
use super::session;
use crate::api::ClientError;

/// The providers a profile can name: those listing models.
pub fn offered(providers: Vec<ProviderInfo>) -> Vec<ProviderInfo> {
    providers
        .into_iter()
        .filter(|p| !p.catalog.models.is_empty())
        .collect()
}

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
                row(
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
        .child(section("Provider", view().children(provider_rows)))
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
                row(model.label.clone(), None, on, usable, move || {
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
                row(
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
                    row(label.to_owned(), None, on, usable, move || {
                        choice.update(|c| {
                            if let Some(c) = c {
                                c.service_tier = tier.to_owned();
                            }
                        });
                    })
                };
                section(
                    "Speed",
                    view()
                        .child(speed("Standard", "default"))
                        .child(speed("Fast", FAST_TIER)),
                )
            });
            Some(
                view()
                    .gap_6()
                    .child(section("Model", view().children(models)))
                    .child(section("Reasoning", view().children(efforts)))
                    .child(speeds),
            )
        }))
}

fn section(title: &'static str, rows: View) -> View {
    view()
        .child(
            text(title.to_uppercase())
                .mx_4()
                .px_4()
                .pb(px(6.0))
                .text_size(px(13.0))
                .text_color(colors::secondary()),
        )
        // The group clips the last row's separator, as iOS lists end.
        .child(parts::group().child(rows.mb(px(-0.5))))
}

/// One option: its label, a detail line when it has one, and a checkmark
/// when picked. A row that cannot be picked is dimmed.
fn row(
    label: String,
    detail: Option<String>,
    picked: impl Fn() -> bool + Clone + 'static,
    enabled: bool,
    on_press: impl Fn() + 'static,
) -> View {
    let mark = picked.clone();
    view()
        .flex_row()
        .items_center()
        .min_h(px(44.0))
        .pl_4()
        .active(move |s| if enabled { s.bg(colors::fill()) } else { s })
        .role(AccessibilityRole::Button)
        .label({
            let label = label.clone();
            let picked = picked.clone();
            move || {
                let mark = if picked() { ", selected" } else { "" };
                format!("{label}{mark}")
            }
        })
        .on_press(move || {
            if enabled {
                on_press();
            }
        })
        .child(
            view()
                .flex_1()
                .flex_row()
                .items_center()
                .py(px(11.0))
                .pr_4()
                .border_b(px(0.5))
                .border_color(colors::separator())
                .child(
                    view()
                        .flex_1()
                        .gap(px(2.0))
                        .child(text(label).text_size(px(17.0)).text_color(if enabled {
                            colors::label()
                        } else {
                            colors::secondary()
                        }))
                        .child(detail.map(|detail| {
                            text(detail)
                                .text_size(px(13.0))
                                .line_height(px(18.0))
                                .text_color(colors::secondary())
                        })),
                )
                // Only the picked row has a mark: VoiceOver reads the
                // row's label with ", selected" instead.
                .child(dynamic(move || {
                    mark().then(|| {
                        text("✓")
                            .text_size(px(17.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors::tint())
                    })
                })),
        )
}

/// The Model screen's data: the providers and the bot's profile as the
/// server reports it (its own, or the defaults it follows).
#[derive(Clone, PartialEq)]
enum Load {
    Loading,
    Failed(String),
    Ready {
        providers: Vec<ProviderInfo>,
        saved: Option<BotProfile>,
    },
}

/// The save request. A lost response keeps what it tried: the retry reads
/// the profile back first, so it neither repeats a save that landed nor
/// overwrites a change made meanwhile on another device.
#[derive(Clone, PartialEq)]
enum Save {
    Idle,
    Saving,
    Failed(String),
    Unknown { tried: ProfileChoice },
}

/// What a retry of an unknown save finds on the server.
enum Reconciled {
    /// The earlier save landed.
    Saved(BotProfile),
    /// Someone changed the profile since it was loaded.
    Changed(Option<BotProfile>),
    /// Nothing changed: saving again is safe.
    Unchanged,
}

fn key(bot: Uuid) -> String {
    format!("model-{bot}")
}

pub fn open(bot: Uuid) {
    super::push(key(bot), move || screen(bot));
}

fn screen(bot: Uuid) -> Screen {
    let load = signal(Load::Loading);
    let choice = signal(None::<ProfileChoice>);
    let save = signal(Save::Idle);
    let client = session::server().client.clone();
    spawn(async move {
        let (caps, saved) = futures_util::join!(
            client.clone().capabilities(),
            client.clone().bot_profile(bot)
        );
        match (caps, saved) {
            (Ok(caps), Ok(saved)) => {
                let providers = offered(caps.providers);
                choice.set(shown(&providers, saved.as_ref()));
                load.set(Load::Ready { providers, saved });
            }
            (Err(err), _) | (_, Err(err)) => load.set(Load::Failed(err.to_string())),
        }
    });

    // Changed from what the bot uses now, own or followed: opening the
    // screen and tapping Save cannot pin the defaults by accident.
    let dirty = computed(move || match load.get() {
        Load::Ready { saved, .. } => {
            let chosen = choice.get();
            chosen.is_some() && chosen != saved.map(|s| s.choice)
        }
        Load::Loading | Load::Failed(_) => false,
    });
    let ready = computed(move || dirty.get() && save.get() != Save::Saving);

    let submit = move || {
        let Some(picked) = choice.get_untracked() else {
            return;
        };
        if !ready.get_untracked() {
            return;
        }
        let retry = matches!(save.get_untracked(), Save::Unknown { tried } if tried == picked);
        let loaded = load.with_untracked(|l| match l {
            Load::Ready { saved, .. } => saved.clone(),
            Load::Loading | Load::Failed(_) => None,
        });
        save.set(Save::Saving);
        let client = session::server().client.clone();
        let done = move |saved: BotProfile| {
            if save.try_set(Save::Idle) {
                load.update(|l| {
                    if let Load::Ready { saved: slot, .. } = l {
                        *slot = Some(saved);
                    }
                });
                super::routes().update(|r| r.0.retain(|route| *route.key != *key(bot)));
            }
        };
        session::spawn_held(async move {
            if retry {
                match reconcile(&client, bot, &picked, loaded.as_ref()).await {
                    Ok(Reconciled::Saved(saved)) => return done(saved),
                    Ok(Reconciled::Changed(latest)) => {
                        if save.try_set(Save::Failed(
                            "The model was changed on another device. This shows it now.".into(),
                        )) {
                            load.update(|l| {
                                if let Load::Ready { providers, saved } = l {
                                    choice.set(shown(providers, latest.as_ref()));
                                    *saved = latest;
                                }
                            });
                        }
                        return;
                    }
                    Ok(Reconciled::Unchanged) => {}
                    Err(_) => {
                        save.try_set(Save::Unknown { tried: picked });
                        return;
                    }
                }
            }
            match client.set_bot_profile(bot, picked.clone()).await {
                Ok(saved) => done(saved),
                Err(err) if err.outcome_unknown() => {
                    save.try_set(Save::Unknown { tried: picked });
                }
                Err(ClientError::Api { message, .. }) => {
                    save.try_set(Save::Failed(message));
                }
                Err(err) => {
                    save.try_set(Save::Failed(err.to_string()));
                }
            }
        });
    };

    let save_item = move || {
        vec![
            HeaderItem::new(if save.get() == Save::Saving {
                "Saving…"
            } else {
                "Save"
            })
            .key("save")
            .disabled(!ready.get())
            .on_press(submit),
        ]
    };

    let body = dynamic(move || match load.get() {
        Load::Loading => Some(
            text("Loading…")
                .mx_4()
                .text_size(px(15.0))
                .text_color(colors::secondary())
                .into_element(),
        ),
        Load::Failed(message) => Some(
            text(message)
                .mx_4()
                .text_size(px(15.0))
                .text_color(colors::destructive())
                .into_element(),
        ),
        Load::Ready { providers, saved } if providers.is_empty() => {
            let _ = saved;
            Some(parts::footnote("No provider on the server lists models yet.").into_element())
        }
        Load::Ready { providers, saved } => {
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
            Save::Failed(message) => message,
            Save::Unknown { .. } => "The connection dropped before the server answered. It may have saved. Save checks the server before saving again.".into(),
            Save::Idle | Save::Saving => return None,
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
    match saved {
        Some(saved) => providers
            .iter()
            .find(|p| p.state.id == saved.choice.provider_id)
            .and_then(|p| p.choice(Some(&saved.choice))),
        None => providers
            .iter()
            .filter(|p| p.state.cli_available)
            .find_map(|p| p.choice(None)),
    }
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
