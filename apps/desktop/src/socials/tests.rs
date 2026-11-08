use super::Access;
use crate::api::socials::{
    Binding, BindingInput, Connection, ConnectionDetail, Destination, Permissions, Provider,
    WakePolicy,
};
use uuid::Uuid;

#[test]
fn discovery_failure_and_retry_preserve_saved_access_and_owner_edits() {
    let bot = Uuid::new_v4();
    let connection = Connection {
        id: Uuid::new_v4(),
        provider: Provider::Telegram,
        remote_id: "123".into(),
        display_name: "Support bot".into(),
        username: None,
        state: "active".into(),
        health: "healthy".into(),
        generation: 3,
        instructions: String::new(),
    };
    let saved = BindingInput {
        destinations: vec!["100".into()],
        permissions: Permissions {
            read: true,
            write: true,
            send: false,
            wake: true,
        },
        wake_policy: WakePolicy::Mentions,
        instructions: "Only summarize billing questions. Ask before replying.".into(),
    };
    let mut access = Access::from_detail(
        ConnectionDetail {
            connection,
            bindings: vec![Binding {
                bot_id: bot,
                access: saved.clone(),
            }],
        },
        Some(bot),
    );
    let failure = crate::api::ClientError::Unreachable {
        origin: "https://platform.invalid".into(),
        cause: "offline".into(),
    };
    assert!(access.discovered(Err(failure)).is_err());
    assert_eq!(access.bot, Some(bot));
    assert_eq!(access.draft.destinations, saved.destinations);
    assert_eq!(access.draft.permissions, saved.permissions);
    assert_eq!(access.draft.instructions, saved.instructions);
    assert_eq!(access.draft.wake_policy, saved.wake_policy);

    access.draft.instructions = "Summarize billing questions once per day.".into();
    access.draft.permissions.write = false;
    access
        .discovered(Ok(vec![Destination {
            id: "100".into(),
            name: "Billing".into(),
            kind: "group".into(),
            can_send: true,
        }]))
        .unwrap();
    assert_eq!(access.destinations[0].name, "Billing");
    assert_eq!(access.draft.destinations, ["100"]);
    assert!(!access.draft.permissions.write);
    assert!(access.draft.permissions.read);
    assert_eq!(
        access.draft.instructions,
        "Summarize billing questions once per day."
    );
    assert_eq!(access.draft.wake_policy, WakePolicy::Mentions);
}

#[test]
fn saved_revocation_and_empty_instructions_do_not_turn_into_defaults() {
    let bot = Uuid::new_v4();
    let detail: ConnectionDetail = serde_json::from_value(serde_json::json!({
        "connection": {
            "id": Uuid::new_v4(), "provider": "discord", "remote_id": "123",
            "display_name": "Support", "username": null, "state": "active", "health": "healthy",
            "generation": 1, "instructions": ""
        },
        "bindings": [{
            "bot_id": bot, "destinations": [],
            "permissions": {"read": false, "write": false, "send": false, "wake": false},
            "wake_policy": "never", "instructions": ""
        }]
    }))
    .unwrap();
    let access = Access::from_detail(detail, Some(bot));
    assert_eq!(access.draft.permissions, Permissions::default());
    assert!(access.draft.destinations.is_empty());
    assert!(access.draft.instructions.is_empty());
}

#[test]
fn a_bot_whose_messages_go_elsewhere_says_so_instead_of_its_raw_health() {
    let connection = |health: &str| -> crate::api::socials::Connection {
        serde_json::from_value(serde_json::json!({
            "id": Uuid::new_v4(), "provider": "telegram", "remote_id": "1",
            "display_name": "Proliquid", "username": "proliquid_bot", "state": "active",
            "health": health, "generation": 1, "instructions": ""
        }))
        .unwrap()
    };
    let conflicted = connection("receiver_conflict");
    assert!(conflicted.receiver_conflict());
    assert_eq!(conflicted.health_label(), "messages go elsewhere");
    assert_eq!(connection("connected").health_label(), "connected");
}

#[test]
fn refreshing_updates_the_conflict_warning_and_keeps_owner_edits() {
    let connection = |health: &str| Connection {
        id: Uuid::from_u128(7),
        provider: Provider::Telegram,
        remote_id: "123".into(),
        display_name: "Proliquid".into(),
        username: Some("proliquid_bot".into()),
        state: "active".into(),
        health: health.into(),
        generation: 3,
        instructions: String::new(),
    };
    let mut access = Access::from_detail(
        ConnectionDetail {
            connection: connection("receiver_conflict"),
            bindings: Vec::new(),
        },
        Some(Uuid::new_v4()),
    );
    access.draft.instructions = "Post only approved updates.".into();
    access.reloaded(connection("connected"));
    assert!(!access.connection.receiver_conflict());
    assert_eq!(access.draft.instructions, "Post only approved updates.");
}
