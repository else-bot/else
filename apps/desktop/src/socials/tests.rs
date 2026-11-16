use uuid::Uuid;

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
fn trigger_card_titles_prefer_the_chat_name_then_id_then_any_chat() {
    let mut trigger: crate::api::socials::Trigger = serde_json::from_value(serde_json::json!({
        "id": Uuid::new_v4(), "connection_id": Uuid::new_v4(), "bot_id": Uuid::new_v4(),
        "provider": "telegram", "connection_name": "Team bot", "username": null,
        "destination_name": "Proliquid", "destination": "-100123", "wake": "all",
        "instructions": "Watch for questions.", "enabled": true,
        "created_at": "2026-11-01T07:00:00.123+00:00", "updated_at": "2026-11-01T07:00:00.123+00:00"
    }))
    .unwrap();
    assert_eq!(trigger.title(), "Proliquid");
    assert_eq!(trigger.wake.label(), "every message");
    trigger.destination_name = None;
    assert_eq!(trigger.title(), "-100123");
    trigger.destination = None;
    assert_eq!(trigger.title(), "Any chat");
}

#[test]
fn a_pairing_view_from_the_api_shows_its_code_until_linked() {
    use crate::api::socials::{Pairing, PairingView, Provider};
    let code: PairingView = serde_json::from_value(serde_json::json!({
        "revision": 3, "state": "code", "code": "2@AbC,xyz==,def", "expires_in": 20
    }))
    .unwrap();
    assert_eq!(code.revision, 3);
    assert!(!code.pairing.finished());
    let Pairing::Code { code, .. } = code.pairing else {
        panic!("a code");
    };
    assert!(
        super::qr_image(&code).is_some(),
        "the code renders as a QR image"
    );
    let linked: PairingView = serde_json::from_value(serde_json::json!({
        "revision": 5, "state": "paired", "connection": {
            "id": Uuid::new_v4(), "provider": "whatsapp", "remote_id": "33612345678",
            "display_name": "Ada", "username": "+33612345678", "state": "active",
            "health": "connecting", "generation": 1, "instructions": ""
        }
    }))
    .unwrap();
    assert!(linked.pairing.finished());
    assert!(matches!(
        linked.pairing,
        Pairing::Paired { connection } if connection.provider == Provider::WhatsApp
    ));
    assert!(Provider::WhatsApp.pairs() && !Provider::Telegram.pairs());
}
