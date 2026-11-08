//! Offline context for tool-use evals, built without an installation or credentials.

use serde_json::{Value, json};
use uuid::Uuid;

use crate::app::providers::{DelegationContext, InstalledMcp, Origin, TurnInput};
use crate::app::tools::DEFERRED_TOOLS;

/// A main bot's owner turn. Keep schemas on the same path as MCP tools/list.
pub fn export() -> Value {
    let input = TurnInput {
        bot_id: Uuid::from_u128(1),
        conversation_id: Uuid::from_u128(2),
        turn_id: Uuid::from_u128(3),
        turn_no: 1,
        bot_name: "Main".into(),
        text: "Help me with my tasks.".into(),
        parts: vec![],
        files: vec![],
        profile: None,
        history: vec![],
        session: None,
        tool_grant: None,
        memory: None,
        attempt: 1,
        delegation: DelegationContext {
            origin: Origin::OwnerRequest,
            main_bot: true,
            can_create_bot: true,
            registry: vec![
                (Uuid::from_u128(4), "Research".into()),
                (Uuid::from_u128(5), "GitHub".into()),
            ],
            remaining: crate::db::delegations::MAX_CHILDREN,
            ..DelegationContext::default()
        },
        secrets: vec![],
        questions: vec![],
        standing_permissions: vec![],
        outgoing_drafts: vec![],
        social_setups: vec![],
        mcp_waits: vec![],
        mcps: Some(vec![InstalledMcp {
            id: "github".into(),
            name: "GitHub".into(),
            description: "Repositories, issues, and pull requests.".into(),
        }]),
        handoff: None,
        jobs: vec![],
        run: None,
        rules: None,
    };
    let instructions = crate::providers::prompt::instructions(&input, "/eval/project");
    let all = super::mcp::all_tools();
    let deferred: Vec<&Value> = all
        .iter()
        .filter(|tool| {
            tool["name"]
                .as_str()
                .is_some_and(|n| DEFERRED_TOOLS.contains(&n))
        })
        .collect();
    let listed = super::tool_search::listed(all.clone());
    // What a first owner turn's packet carries ahead of the message.
    let blocks = crate::providers::context::for_session(&input, None)
        .text
        .unwrap_or_default();
    json!({
        "sizes": {
            "instructions": instructions.len(),
            "listed_tools": serde_json::to_vec(&listed).expect("tool JSON").len(),
            "deferred_tools": serde_json::to_vec(&deferred).expect("tool JSON").len(),
            "context_blocks": blocks.len(),
        },
        "instructions": instructions,
        "listed_tools": listed,
        "deferred_tools": deferred,
        "context_blocks": blocks,
        "server_instructions": super::mcp::SERVER_INSTRUCTIONS,
        "parked_next": super::mcp::PARKED_NEXT,
        "parking_tools": crate::app::tools::PARKING_TOOLS,
        "host_tools": crate::providers::claude_home::HOST_TOOLS,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tools::TOOL_NAMES;
    use std::collections::BTreeSet;

    #[test]
    fn exported_context_is_valid_and_lists_the_allowed_tools() {
        let value: Value = serde_json::from_str(&export().to_string()).unwrap();
        assert!(value["instructions"].as_str().unwrap().contains("main bot"));
        for (key, expected) in [
            ("listed_tools", TOOL_NAMES),
            ("deferred_tools", DEFERRED_TOOLS),
        ] {
            let tools = value[key].as_array().unwrap();
            let names: BTreeSet<_> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
            assert_eq!(names, expected.iter().copied().collect());
            assert_eq!(tools.len(), expected.len());
            assert!(tools.iter().all(|t| t["inputSchema"].is_object()));
            assert_eq!(
                value["sizes"][key],
                serde_json::to_vec(tools).unwrap().len()
            );
        }
        assert_eq!(
            value["sizes"]["instructions"],
            value["instructions"].as_str().unwrap().len()
        );
        assert_eq!(value, export(), "export must be deterministic");
    }
}
