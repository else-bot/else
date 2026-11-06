use crate::api::{Bot, SidebarStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Message,
    Working,
    Queued,
    Answer,
    Approval,
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
            Some(SidebarStatus::WaitingForAnswer { preview }) => {
                return label(Kind::Answer, &format!("Waiting for you · {preview}"));
            }
            Some(SidebarStatus::NeedsApproval { preview }) => {
                return label(Kind::Approval, &format!("Needs approval · {preview}"));
            }
            Some(SidebarStatus::NeedsCredentials { preview }) => {
                return label(Kind::Credentials, &format!("Needs credentials · {preview}"));
            }
            _ => {}
        }
        if let Some(activity) = &bot.activity {
            match activity.phase.as_str() {
                "working" => {
                    return label(
                        Kind::Working,
                        &if activity.queued_count > 0 {
                            format!("Working · {} queued", activity.queued_count)
                        } else {
                            "Working".to_owned()
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
                        Some(latest) if !latest.preview.trim().is_empty() => latest.preview.clone(),
                        Some(_) => "No text in last reply".to_owned(),
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
            ("needs_credentials", Kind::Credentials, "Needs credentials"),
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
