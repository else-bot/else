//! What an owner message's turn says under it: queued, working, waiting
//! (on the owner, other bots or jobs), stopped, failed or unknown, and a
//! turn continued after the attempt limit. Shared by the desktop and the
//! phone.

use else_wire::outgoing::DraftStatus;
use else_wire::turns::TurnStatus;

/// "Waiting for Scout and 2 jobs": what a bot waits on, by the bots its
/// open tasks went to, how many tasks, and how many jobs.
pub fn waiting_for(names: &[&str], tasks: usize, jobs: usize) -> String {
    let jobs_text = match jobs {
        1 => "a job".to_owned(),
        jobs => format!("{jobs} jobs"),
    };
    let who = match names {
        [] if jobs > 0 && tasks == 0 => return format!("Waiting on {jobs_text}"),
        [] => "another bot".to_owned(),
        [name] => (*name).to_owned(),
        many => format!("{} bots", many.len()),
    };
    if jobs > 0 {
        format!("Waiting for {who} and {jobs_text}")
    } else if tasks > names.len().max(1) {
        format!("Waiting for {who} · {tasks} tasks")
    } else {
        format!("Waiting for {who}")
    }
}

/// The turn's status line and whether it reports a failure.
pub fn label(turn: &else_wire::turns::Turn) -> (String, bool) {
    use else_wire::questions::QuestionUse;
    let (label, error) = status_label(turn);
    if !turn.status.is_unsuccessful() {
        return (label, error);
    }
    let used = |pick: fn(&QuestionUse) -> bool| {
        turn.questions
            .iter()
            .any(|q| q.used.as_ref().is_some_and(pick))
    };
    let note = if used(|u| matches!(u, QuestionUse::Completed { .. })) {
        "An approved action had already run."
    } else if used(|u| matches!(u, QuestionUse::Started { .. })) {
        "An approved action had started; its outcome is unknown."
    } else {
        return (label, error);
    };
    let label = label.trim_end().trim_end_matches('.');
    (format!("{label}. {note}"), error)
}

fn waiting_label(turn: &else_wire::turns::Turn) -> String {
    let open = turn.open_delegations();
    let mut bots: Vec<(uuid::Uuid, &str)> = Vec::new();
    for task in &open {
        if !bots.iter().any(|(id, _)| *id == task.target_bot_id) {
            bots.push((task.target_bot_id, &task.target_name));
        }
    }
    let names: Vec<&str> = bots.into_iter().map(|(_, name)| name).collect();
    let jobs = turn.mcp_waits.len() + turn.computer_job_waits.len();
    waiting_for(&names, open.len(), jobs)
}

fn status_label(turn: &else_wire::turns::Turn) -> (String, bool) {
    let detail = turn
        .error
        .as_ref()
        .and_then(|e| e.message.clone())
        .unwrap_or_default();
    let continued = turn.limit_continued.map(|c| {
        format!(
            "Continued after the {}-minute limit ({}/{})",
            c.minutes, c.count, c.max
        )
    });
    match turn.status {
        TurnStatus::Queued if turn.taken_by_reply() => (String::new(), false),
        TurnStatus::Queued | TurnStatus::Preparing | TurnStatus::Sending | TurnStatus::Running
            if continued.is_some() =>
        {
            (continued.unwrap_or_default(), false)
        }
        TurnStatus::Queued => ("Queued".into(), false),
        TurnStatus::Preparing | TurnStatus::Sending | TurnStatus::Running => {
            ("Working".into(), false)
        }
        TurnStatus::Waiting if turn.waits_on_job() => (waiting_label(turn), false),
        TurnStatus::Cancelling => ("Stopping…".into(), false),
        TurnStatus::Waiting
            if turn
                .skill_installs
                .iter()
                .any(|r| r.status == else_wire::skills::SkillInstallStatus::Pending) =>
        {
            ("Waiting for your skill approval…".into(), false)
        }
        TurnStatus::Waiting if turn.secret_requests.iter().any(|r| r.is_pending()) => (
            "Waiting for you to answer the secret request…".into(),
            false,
        ),
        TurnStatus::Waiting
            if turn
                .outgoing_drafts
                .iter()
                .any(|d| d.status == DraftStatus::Pending) =>
        {
            ("Waiting for your approval…".into(), false)
        }
        TurnStatus::Waiting
            if turn
                .questions
                .iter()
                .any(|q| q.is_pending() && q.is_approval()) =>
        {
            ("Waiting for your approval…".into(), false)
        }
        TurnStatus::Waiting if turn.questions.iter().any(|q| q.is_pending()) => {
            ("Waiting for your answer…".into(), false)
        }
        TurnStatus::Waiting if turn.social_setups.iter().any(|setup| setup.is_pending()) => {
            ("Waiting for you to connect an account…".into(), false)
        }
        TurnStatus::Waiting => (format!("{}…", waiting_label(turn)), false),
        TurnStatus::Interrupted => (
            if turn.error.as_ref().is_some_and(|e| e.code == "cancelled") {
                "Stopped before it ran".into()
            } else {
                "Stopped".into()
            },
            false,
        ),
        TurnStatus::Failed => (format!("Failed. {detail}"), true),
        TurnStatus::Uncertain => (format!("Outcome unknown. {detail}"), true),
        TurnStatus::Completed | TurnStatus::Unknown => (String::new(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::label as turn_label;

    #[test]
    fn social_setup_waits_for_the_owner_to_connect() {
        let turn = serde_json::from_value(serde_json::json!({
            "id": uuid::Uuid::nil(),
            "request_id": uuid::Uuid::nil(),
            "turn_no": 1,
            "provider_id": "synthetic",
            "status": "waiting",
            "error": null,
            "effect_uncertain": false,
            "social_setups": [{
                "id": uuid::Uuid::nil(),
                "provider": "telegram",
                "reason": "Summarize mentions",
                "status": "pending",
                "connection_id": null
            }]
        }))
        .unwrap();
        assert_eq!(
            turn_label(&turn),
            ("Waiting for you to connect an account…".into(), false)
        );
    }

    #[test]
    fn a_joined_message_is_queued_until_the_reply_takes_it() {
        let queued = |steer_status: serde_json::Value| {
            serde_json::from_value(serde_json::json!({
                "id": uuid::Uuid::nil(),
                "request_id": uuid::Uuid::nil(),
                "turn_no": 2,
                "provider_id": "claude",
                "status": "queued",
                "error": null,
                "effect_uncertain": false,
                "steered_into": uuid::Uuid::nil(),
                "steer_status": steer_status,
            }))
            .unwrap()
        };
        for status in ["pending", "sending"] {
            assert_eq!(
                turn_label(&queued(serde_json::json!(status))),
                ("Queued".into(), false)
            );
        }
        assert_eq!(
            turn_label(&queued(serde_json::json!("delivered"))),
            (String::new(), false)
        );
    }

    #[test]
    fn a_turn_continued_after_the_attempt_limit_says_so_until_it_ends() {
        let turn = |status: &str| {
            serde_json::from_value(serde_json::json!({
                "id": uuid::Uuid::nil(),
                "request_id": uuid::Uuid::nil(),
                "turn_no": 1,
                "provider_id": "codex",
                "status": status,
                "error": null,
                "effect_uncertain": false,
                "limit_continued": { "count": 2, "max": 4, "minutes": 30 },
            }))
            .unwrap()
        };
        for status in ["queued", "running"] {
            assert_eq!(
                turn_label(&turn(status)),
                ("Continued after the 30-minute limit (2/4)".into(), false)
            );
        }
        assert_eq!(turn_label(&turn("completed")), (String::new(), false));
    }

    #[test]
    fn a_failed_turn_says_whether_an_approved_action_ran() {
        let failed = |used: serde_json::Value| {
            serde_json::from_value(serde_json::json!({
                "id": uuid::Uuid::nil(),
                "request_id": uuid::Uuid::nil(),
                "turn_no": 1,
                "provider_id": "claude",
                "status": "failed",
                "error": { "code": "provider_policy",
                           "message": "Claude reported native agent activity (task_started)" },
                "effect_uncertain": false,
                "questions": [{
                    "id": uuid::Uuid::nil(), "kind": "external", "title": "Call github issue_write",
                    "choices": [], "status": "answered", "answer": "approve", "used": used
                }]
            }))
            .unwrap()
        };
        let reason = "Failed. Claude reported native agent activity (task_started)";
        let at = "2026-11-13T07:14:47Z";
        for (used, label) in [
            (serde_json::json!(null), reason.to_owned()),
            (
                serde_json::json!({ "outcome": "error", "at": at }),
                reason.to_owned(),
            ),
            (
                serde_json::json!({ "outcome": "completed", "at": at }),
                format!("{reason}. An approved action had already run."),
            ),
            (
                serde_json::json!({ "outcome": "started", "at": at }),
                format!("{reason}. An approved action had started; its outcome is unknown."),
            ),
        ] {
            assert_eq!(turn_label(&failed(used)), (label, true));
        }
    }
}
