//! One snapshot per turn mode of what decides a native session's cached
//! prefix and identity: the fingerprint's mode line, the system prompt, and
//! the request as a session that holds everything else receives it. Rerun
//! with `UPDATE_SNAPSHOTS=1` after an intended prompt change.

use uuid::Uuid;

use crate::app::providers::{Origin, OwnerAnswer, TurnInput};

fn check(name: &str, input: &TurnInput) {
    let actual = format!(
        "== fingerprint tag\n{}\n== system\n{}\n== packet\n{}\n",
        super::registry_text(input)
            .lines()
            .next()
            .unwrap_or_default(),
        super::instructions(input, "/p"),
        packet(input),
    );
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/providers/prompt/snapshots")
        .join(format!("{name}.txt"));
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&path, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        actual == expected,
        "the {name} prompt changed; rerun with UPDATE_SNAPSHOTS=1 if that is intended\n{actual}"
    );
}

/// The packet for a session that considered everything before this turn
/// and holds the memory, default skills, and context blocks.
fn packet(input: &TurnInput) -> String {
    let skills = super::super::default_skills::Update {
        text: None,
        applied: Default::default(),
    };
    let context = super::super::context::Update {
        text: None,
        applied: Default::default(),
    };
    super::turn_packet(input, input.turn_no - 1, None, &skills, &context).0
}

fn input(origin: Origin) -> TurnInput {
    let mut input = crate::providers::cli::session::tests::input();
    input.memory = None;
    input.text = "Sort today's notes.".into();
    input.delegation.origin = origin;
    input.delegation.registry = vec![(Uuid::nil(), "Scout".into())];
    input.delegation.remaining = 2;
    input
}

#[test]
fn social_event() {
    let mut input = input(Origin::SocialEvent);
    input.text = "@ana on Telegram: is the shop open?".into();
    check("social", &input);
}

#[test]
fn email_task() {
    let mut input = input(Origin::ConnectorEvent);
    input.delegation.task_depth = 1;
    input.delegation.delegated_by = Some((Uuid::nil(), "Inbox".into()));
    check("email_task", &input);
}

#[test]
fn email_event() {
    let mut input = input(Origin::ConnectorEvent);
    input.text = "New mail from ana@example.com: Invoice 42".into();
    check("email_event", &input);
}

#[test]
fn delegated_task() {
    let mut input = input(Origin::OwnerRequest);
    input.delegation.task_depth = 1;
    input.delegation.delegated_by = Some((Uuid::nil(), "Main".into()));
    input.delegation.owner_answer = Some(OwnerAnswer {
        question: "Post the callout?".into(),
        context: Some("Draft 7".into()),
        answered_at: "2026-11-13T09:00:00Z".parse().unwrap(),
        chosen: vec!["Post it".into()],
    });
    check("delegated", &input);
}

#[test]
fn owner_conversation() {
    let mut input = input(Origin::OwnerRequest);
    input.delegation.main_bot = true;
    input.delegation.can_create_bot = true;
    check("owner", &input);
}
