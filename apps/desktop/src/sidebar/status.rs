use chrono::{DateTime, Utc};

use crate::api::{Bot, SidebarStatus, WaitingOn};

/// How long a request has waited, in its largest whole unit: `now`, `5m`,
/// `2h`, `3d`.
pub fn waited(since: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let minutes = (now - since).num_minutes().max(0);
    match minutes {
        0 => "now".to_owned(),
        1..60 => format!("{minutes}m"),
        60..1440 => format!("{}h", minutes / 60),
        _ => format!("{}d", minutes / 1440),
    }
}

/// What a bot waiting on other bots shows: `Waiting for Dev`,
/// `Waiting for Dev · 3 tasks`, `Waiting for 2 bots · 4 tasks`. The count
/// appears only when it adds to the names.
pub fn waiting_for(names: &[&str], tasks: usize) -> String {
    let who = match names {
        [] => "another bot".to_owned(),
        [name] => (*name).to_owned(),
        many => format!("{} bots", many.len()),
    };
    if tasks > names.len().max(1) {
        format!("Waiting for {who} · {tasks} tasks")
    } else {
        format!("Waiting for {who}")
    }
}

pub fn waiting_label(waiting: &WaitingOn) -> String {
    let names: Vec<&str> = waiting.bots.iter().map(|b| b.name.as_str()).collect();
    let tasks = waiting.bots.iter().map(|b| b.tasks as usize).sum();
    waiting_for(&names, tasks)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Message,
    Working,
    /// Parked on tasks other bots are doing; nothing for the owner to do.
    Waiting,
    Queued,
    Answer,
    Approval,
    Takeover,
    Credentials,
    Stopping,
    Stopped,
    Failed,
    Uncertain,
    Unavailable,
    Deleting,
}

pub struct Subtitle {
    pub kind: Kind,
    pub text: String,
}

impl Subtitle {
    pub fn new(bot: &Bot, connected: bool, stopping: bool) -> Self {
        let mut subtitle = Self::current(bot, stopping);
        if !connected && subtitle.kind != Kind::Message {
            subtitle.text = format!("Last known · {}", subtitle.text);
        }
        subtitle
    }

    fn current(bot: &Bot, stopping: bool) -> Self {
        let label = |kind, text: &str| Self {
            kind,
            text: text.to_owned(),
        };
        if let Some(cleanup) = &bot.cleanup {
            return if cleanup.error.is_some() {
                label(Kind::Failed, "Deletion failed")
            } else {
                label(Kind::Deleting, "Deleting…")
            };
        }
        let phase = bot
            .activity
            .as_ref()
            .map(|activity| activity.phase.as_str());
        if stopping || phase == Some("cancelling") {
            return label(Kind::Stopping, "Stopping…");
        }
        match &bot.sidebar_status {
            Some(SidebarStatus::WaitingForAnswer { preview, .. }) => {
                return label(Kind::Answer, &format!("Waiting for you · {preview}"));
            }
            Some(SidebarStatus::NeedsApproval { preview, .. }) => {
                return label(Kind::Approval, &format!("Needs approval · {preview}"));
            }
            Some(SidebarStatus::NeedsTakeover { preview, .. }) => {
                return label(Kind::Takeover, &format!("Take over · {preview}"));
            }
            Some(SidebarStatus::NeedsYou { preview, .. }) => {
                return label(Kind::Answer, &format!("Needs you · {preview}"));
            }
            Some(SidebarStatus::NeedsCredentials { preview, .. }) => {
                return label(Kind::Credentials, &format!("Needs credentials · {preview}"));
            }
            _ => {}
        }
        if let Some(activity) = &bot.activity {
            match activity.phase.as_str() {
                "working" => {
                    let (kind, text) = match &activity.waiting_on {
                        Some(waiting) => (Kind::Waiting, waiting_label(waiting)),
                        None => (Kind::Working, "Working".to_owned()),
                    };
                    return label(
                        kind,
                        &if activity.queued_count > 0 {
                            format!("{text} · {} queued", activity.queued_count)
                        } else {
                            text
                        },
                    );
                }
                "queued" => {
                    return label(Kind::Queued, &format!("{} queued", activity.queued_count));
                }
                "unavailable" => return label(Kind::Unavailable, "Provider unavailable"),
                _ => {}
            }
        }
        match &bot.sidebar_status {
            Some(SidebarStatus::Failed) => label(Kind::Failed, "Task failed"),
            Some(SidebarStatus::Stopped) => label(Kind::Stopped, "Stopped"),
            Some(SidebarStatus::Uncertain) => label(Kind::Uncertain, "Result uncertain"),
            _ => {
                let text = match &bot.read_state {
                    Some(state) => match &state.latest {
                        Some(latest) => {
                            let text =
                                markdown::to_mdast(&latest.preview, &markdown::ParseOptions::gfm())
                                    .map(|root| root.to_string())
                                    .unwrap_or_else(|_| latest.preview.clone());
                            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
                            if text.is_empty() {
                                "No text in last reply".to_owned()
                            } else {
                                text
                            }
                        }
                        None => "No replies yet".to_owned(),
                    },
                    None => "No message preview".to_owned(),
                };
                Self {
                    kind: Kind::Message,
                    text,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn bot(extra: Value) -> Bot {
        let mut value = json!({
            "id": uuid::Uuid::nil(), "name": "Else",
            "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 },
            "read_state": { "latest": null, "read_through": { "item_seq": 0, "text_bytes": 0 }, "marked_unread": false }
        });
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn idle_uses_the_bot_reply_and_distinguishes_empty_from_missing_data() {
        let mut bot = bot(json!({}));
        assert_eq!(Subtitle::new(&bot, true, false).text, "No replies yet");
        bot.read_state.as_mut().unwrap().latest = Some(crate::api::LatestReply {
            turn_id: uuid::Uuid::nil(),
            item_id: uuid::Uuid::nil(),
            position: Default::default(),
            preview: "Here is your report ✓".into(),
        });
        assert_eq!(
            Subtitle::new(&bot, true, false).text,
            "Here is your report ✓"
        );
        bot.read_state
            .as_mut()
            .unwrap()
            .latest
            .as_mut()
            .unwrap()
            .preview
            .clear();
        assert_eq!(
            Subtitle::new(&bot, true, false).text,
            "No text in last reply"
        );
        bot.read_state = None;
        assert_eq!(Subtitle::new(&bot, true, false).text, "No message preview");
    }

    #[test]
    fn reply_previews_show_plain_text_without_changing_the_reply() {
        for (source, expected) in [
            (
                "Created **Github**, your specialist.",
                "Created Github, your specialist.",
            ),
            (
                "**Ready** _now_ with `cargo test`",
                "Ready now with cargo test",
            ),
            ("## [Report](https://example.com) ~~old~~", "Report old"),
            ("  **Résumé**\nwith   details  ", "Résumé with details"),
            ("`a_b * c` + **bold**", "a_b * c + bold"),
            ("2 * 3 = 6 and foo_bar", "2 * 3 = 6 and foo_bar"),
            ("\\*literal\\* &amp; café", "*literal* & café"),
            ("---", "No text in last reply"),
        ] {
            let bot = bot(json!({"read_state": {
                "latest": {
                    "turn_id": uuid::Uuid::nil(),
                    "item_id": uuid::Uuid::nil(),
                    "position": {"item_seq": 1, "text_bytes": source.len()},
                    "preview": source
                },
                "read_through": {"item_seq": 0, "text_bytes": 0},
                "marked_unread": false
            }}));
            let subtitle = Subtitle::new(&bot, true, false);
            assert_eq!(subtitle.kind, Kind::Message);
            assert_eq!(subtitle.text, expected, "{source}");
            assert_eq!(bot.read_state.unwrap().latest.unwrap().preview, source);
        }
    }

    #[test]
    fn activity_and_owner_requests_replace_the_reply() {
        for (phase, queued, kind, text) in [
            ("working", 0, Kind::Working, "Working"),
            ("working", 2, Kind::Working, "Working · 2 queued"),
            ("queued", 1, Kind::Queued, "1 queued"),
            ("cancelling", 0, Kind::Stopping, "Stopping…"),
            ("unavailable", 0, Kind::Unavailable, "Provider unavailable"),
        ] {
            let bot = bot(
                json!({ "activity": { "phase": phase, "active_turn_id": null, "queued_count": queued } }),
            );
            let subtitle = Subtitle::new(&bot, true, false);
            assert_eq!((subtitle.kind, subtitle.text.as_str()), (kind, text));
        }
        for (status, kind, prefix) in [
            ("waiting_for_answer", Kind::Answer, "Waiting for you"),
            ("needs_approval", Kind::Approval, "Needs approval"),
            ("needs_takeover", Kind::Takeover, "Take over"),
            ("needs_credentials", Kind::Credentials, "Needs credentials"),
            ("needs_you", Kind::Answer, "Needs you"),
        ] {
            let bot =
                bot(json!({ "sidebar_status": { "kind": status, "preview": "Which account?" } }));
            let subtitle = Subtitle::new(&bot, true, false);
            assert_eq!(subtitle.kind, kind);
            assert_eq!(subtitle.text, format!("{prefix} · Which account?"));
            assert_eq!(Subtitle::new(&bot, true, true).kind, Kind::Stopping);
        }
    }

    #[test]
    fn needs_you_status_keeps_owner_attention_while_the_bot_works() {
        let since = "2026-11-13T12:00:00Z";
        let bot = bot(json!({
            "activity": {"phase": "working", "active_turn_id": null, "queued_count": 0},
            "sidebar_status": {"kind": "needs_you", "preview": "Choose a bot", "since": since}
        }));
        let subtitle = Subtitle::new(&bot, true, false);
        assert_eq!(subtitle.kind, Kind::Answer);
        assert_eq!(subtitle.text, "Needs you · Choose a bot");
        assert_eq!(bot.waiting().unwrap().since, Some(since.parse().unwrap()));
    }

    #[test]
    fn a_turn_parked_on_other_bots_waits_instead_of_working() {
        let waiting = |bots: Value, queued: u32| {
            let bot = bot(json!({ "activity": {
                "phase": "working", "active_turn_id": null, "queued_count": queued,
                "waiting_on": { "since": "2026-11-13T12:00:00Z", "bots": bots } } }));
            let subtitle = Subtitle::new(&bot, true, false);
            (subtitle.kind, subtitle.text)
        };
        let dev = |tasks| json!({ "id": uuid::Uuid::nil(), "name": "Dev", "tasks": tasks });
        let qa = json!({ "id": uuid::Uuid::max(), "name": "QA", "tasks": 1 });
        for (bots, queued, text) in [
            (json!([dev(1)]), 0, "Waiting for Dev"),
            (json!([dev(3)]), 0, "Waiting for Dev · 3 tasks"),
            (json!([dev(1), qa.clone()]), 0, "Waiting for 2 bots"),
            (
                json!([dev(3), qa]),
                1,
                "Waiting for 2 bots · 4 tasks · 1 queued",
            ),
        ] {
            assert_eq!(waiting(bots, queued), (Kind::Waiting, text.to_owned()));
        }
        assert_eq!(waiting_for(&[], 0), "Waiting for another bot");
    }

    #[test]
    fn waits_read_in_their_largest_whole_unit() {
        let now: DateTime<Utc> = "2026-11-10T12:00:00Z".parse().unwrap();
        for (ago, text) in [
            (0, "now"),
            (59, "59m"),
            (60, "1h"),
            (1439, "23h"),
            (2880, "2d"),
        ] {
            assert_eq!(waited(now - chrono::Duration::minutes(ago), now), text);
        }
        assert_eq!(waited(now + chrono::Duration::minutes(3), now), "now");
    }

    #[test]
    fn outcomes_cleanup_and_connection_loss_stay_explicit() {
        for (status, text) in [
            ("failed", "Task failed"),
            ("stopped", "Stopped"),
            ("uncertain", "Result uncertain"),
        ] {
            let bot = bot(json!({ "sidebar_status": { "kind": status } }));
            assert_eq!(Subtitle::new(&bot, true, false).text, text);
        }
        let deleting = bot(json!({ "cleanup": { "state": "pending", "error": null } }));
        assert_eq!(Subtitle::new(&deleting, true, false).text, "Deleting…");
        let failed =
            bot(json!({ "cleanup": { "state": "pending", "error": { "message": "failed" } } }));
        assert_eq!(Subtitle::new(&failed, true, false).text, "Deletion failed");
        let working = bot(
            json!({ "activity": { "phase": "working", "active_turn_id": null, "queued_count": 0 } }),
        );
        assert_eq!(
            Subtitle::new(&working, false, false).text,
            "Last known · Working"
        );
        assert_eq!(
            Subtitle::new(&bot(json!({})), false, false).text,
            "No replies yet"
        );
        assert_eq!(
            Subtitle::new(
                &bot(json!({"sidebar_status":{"kind":"new_status"}})),
                true,
                false
            )
            .text,
            "No replies yet"
        );
    }
}
