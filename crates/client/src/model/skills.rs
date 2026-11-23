#[cfg(test)]
mod tests {
    use else_wire::skills::{SkillInstallStatus, SkillScope};
    use else_wire::turns::Turn;
    use serde_json::{Value, json};
    use uuid::Uuid;

    fn turn() -> Value {
        json!({"id": Uuid::from_u128(1), "request_id": Uuid::from_u128(2),
            "turn_no": 1, "provider_id": "test", "status": "waiting", "error": null,
            "effect_uncertain": false})
    }

    fn install() -> Value {
        json!({"id": Uuid::from_u128(3), "skill_id": "rea-growth", "name": "rea-growth",
            "description": "Analyze growth", "reason": "Review the site",
            "source": {"url": "https://github.com/example/repo/tree/main/skills/growth",
                "repo": "example/repo", "path": "skills/growth", "commit": "a41c9e2ffff"},
            "files": [{"path": "SKILL.md", "size": 42, "script": false},
                {"path": "scripts/growth.py", "size": 100, "script": true}],
            "env": ["TOKEN"], "secret": "skill-rea-growth", "status": "pending",
            "scope": null, "key_stored": false, "created_at": "2026-11-09T00:00:00Z",
            "settled_at": null})
    }

    #[test]
    fn skill_installs_default_to_empty_for_older_servers() {
        let turn: Turn = serde_json::from_value(turn()).unwrap();
        assert!(turn.skill_installs.is_empty());
    }

    #[test]
    fn pending_skill_install_keeps_review_metadata() {
        let mut payload = turn();
        payload["skill_installs"] = json!([install()]);
        let turn: Turn = serde_json::from_value(payload).unwrap();
        let install = &turn.skill_installs[0];
        assert_eq!(install.status, SkillInstallStatus::Pending);
        assert_eq!(install.scope, None);
        assert_eq!(install.env, ["TOKEN"]);
        assert!(install.files[1].script);
        assert_eq!(
            install.source.label(),
            "example/repo / skills/growth @ a41c9e2"
        );
        assert!(!install.key_stored);
    }

    #[test]
    fn a_future_install_status_keeps_the_card() {
        let mut card = install();
        card["status"] = json!("superseded");
        let mut payload = turn();
        payload["skill_installs"] = json!([card]);

        let turn: Turn = serde_json::from_value(payload).unwrap();
        assert_eq!(turn.skill_installs[0].status, SkillInstallStatus::Unknown);
        assert_eq!(turn.skill_installs[0].skill_id, "rea-growth");
    }

    #[test]
    fn settled_skill_installs_keep_the_answer() {
        for (status, expected) in [
            ("installed", SkillInstallStatus::Installed),
            ("declined", SkillInstallStatus::Declined),
            ("cancelled", SkillInstallStatus::Cancelled),
        ] {
            let mut install = install();
            install["status"] = json!(status);
            install["scope"] = json!("bot");
            install["key_stored"] = json!(true);
            install["settled_at"] = json!("2026-11-09T01:00:00Z");
            let mut payload = turn();
            payload["skill_installs"] = json!([install]);
            let turn: Turn = serde_json::from_value(payload).unwrap();
            assert_eq!(turn.skill_installs[0].status, expected);
            assert_eq!(turn.skill_installs[0].scope, Some(SkillScope::Bot));
            assert!(turn.skill_installs[0].key_stored);
            assert!(turn.skill_installs[0].settled_at.is_some());
        }
    }
}
