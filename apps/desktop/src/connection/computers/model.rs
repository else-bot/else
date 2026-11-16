use crate::api::{Computer, ComputerStatus, ComputerUpgrade, Computers};

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Action<'a> {
    Upgrade,
    Retry,
    Queued(u32),
    Waiting(&'a str),
    Running,
    Current,
}

pub(super) fn action(computer: &Computer) -> Action<'_> {
    match &computer.upgrade {
        Some(ComputerUpgrade::Queued { position }) => Action::Queued(*position),
        Some(ComputerUpgrade::Waiting { reason }) => Action::Waiting(reason),
        Some(ComputerUpgrade::Running) => Action::Running,
        Some(ComputerUpgrade::Failed { .. }) => Action::Retry,
        _ if computer.outdated() => Action::Upgrade,
        _ => Action::Current,
    }
}

pub(super) fn upgrade_count(report: &Computers) -> usize {
    report
        .computers
        .iter()
        .filter(|computer| {
            computer.outdated() && matches!(action(computer), Action::Upgrade | Action::Retry)
        })
        .count()
}

pub(super) fn queue_active(report: &Computers) -> bool {
    report
        .computers
        .iter()
        .any(|computer| matches!(action(computer), Action::Queued(_) | Action::Waiting(_)))
}

pub(super) fn gb(bytes: u64) -> String {
    let value = bytes as f64 / 1024_f64.powi(3);
    if value.fract() < 0.05 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

pub(super) fn summary(report: &Computers) -> String {
    let count = report.computers.len();
    let running = report
        .computers
        .iter()
        .filter(|c| c.state == ComputerStatus::Running)
        .count();
    let outdated = report.computers.iter().filter(|c| c.outdated()).count();
    let disk: u64 = report
        .computers
        .iter()
        .filter_map(|c| c.disk_used_bytes)
        .sum();
    let mut text = format!(
        "{count} {} · {running} running · {outdated} outdated · {} GB disk",
        if count == 1 { "computer" } else { "computers" },
        gb(disk)
    );
    if let Some(rollback) = report.rollback_bytes.filter(|bytes| *bytes > 0) {
        text.push_str(&format!(", plus {} GB kept for rollback", gb(rollback)));
    }
    text
}

pub(super) fn queue_line(
    report: &Computers,
    name: impl Fn(uuid::Uuid) -> String,
) -> Option<String> {
    if !queue_active(report) {
        return None;
    }
    let mut next = None;
    let mut waiting = Vec::new();
    let mut failed = Vec::new();
    let mut running = None;
    let mut total = 0;
    for computer in &report.computers {
        match &computer.upgrade {
            None => continue,
            Some(ComputerUpgrade::Running) => running = Some(name(computer.bot_id)),
            Some(ComputerUpgrade::Queued { position }) => {
                if next.as_ref().is_none_or(|(best, _)| position < best) {
                    next = Some((*position, name(computer.bot_id)));
                }
            }
            Some(ComputerUpgrade::Waiting { .. }) => waiting.push(name(computer.bot_id)),
            Some(ComputerUpgrade::Failed { .. }) => failed.push(name(computer.bot_id)),
            Some(ComputerUpgrade::Completed { .. }) => continue,
        }
        total += 1;
    }
    let mut text = match running {
        Some(bot) => format!("Upgrading {bot}, 1 of {total}."),
        None => "Upgrade queue is waiting.".into(),
    };
    if let Some((_, bot)) = next {
        text.push_str(&format!(" {bot} is next."));
    }
    if !waiting.is_empty() {
        text.push_str(&format!(" Waiting: {}.", waiting.join(", ")));
    }
    if !failed.is_empty() {
        text.push_str(&format!(" Failed: {}.", failed.join(", ")));
    }
    Some(text)
}

pub(super) fn ordinal(position: u32) -> String {
    let suffix = match (position % 100, position % 10) {
        (11..=13, _) => "th",
        (_, 1) => "st",
        (_, 2) => "nd",
        (_, 3) => "rd",
        _ => "th",
    };
    format!("{position}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use uuid::Uuid;

    fn computer(id: u128, upgrade: serde_json::Value) -> Computer {
        serde_json::from_value(json!({
            "bot_id": Uuid::from_u128(id), "state": "running", "desired_state": "auto",
            "image": "else-guest:3", "latest_image": "else-guest:4", "upgrade": upgrade,
            "disk_used_bytes": 13314398208u64, "disk_size_bytes": 34359738368u64,
            "cpus": 2, "cpu_percent": 38.5, "memory_used_bytes": 2254857830u64,
            "memory_limit_bytes": 4294967296u64, "last_used_at": null
        }))
        .unwrap()
    }

    fn report() -> Computers {
        Computers {
            latest_image: Some("else-guest:4".into()),
            rollback_bytes: Some(19650000000),
            computers: vec![
                computer(1, json!({"state":"running"})),
                computer(2, json!({"state":"queued","position":2})),
                computer(3, json!({"state":"waiting","reason":"Busy"})),
                computer(4, json!({"state":"failed","error":"No disk"})),
            ],
        }
    }

    #[test]
    fn computers_summary_and_queue_follow_snapshot_states() {
        let mut report = report();
        assert_eq!(
            summary(&report),
            "4 computers · 4 running · 4 outdated · 49.6 GB disk, plus 18.3 GB kept for rollback"
        );
        assert_eq!(
            queue_line(&report, |id| ["Ledger", "Scout", "Ada", "Mira"]
                [id.as_u128() as usize - 1]
                .into())
            .unwrap(),
            "Upgrading Ledger, 1 of 4. Scout is next. Waiting: Ada. Failed: Mira."
        );
        assert_eq!(upgrade_count(&report), 1);
        report.rollback_bytes = Some(0);
        assert!(!summary(&report).contains("rollback"));
        report.rollback_bytes = None;
        assert!(!summary(&report).contains("rollback"));
        report.computers[1].upgrade = None;
        report.computers[2].upgrade = None;
        assert!(!queue_active(&report));
        assert!(queue_line(&report, |_| "Bot".into()).is_none());
    }

    #[test]
    fn computers_actions_cover_upgrade_retry_and_queue_states() {
        let report = report();
        assert_eq!(action(&report.computers[0]), Action::Running);
        assert_eq!(action(&report.computers[1]), Action::Queued(2));
        assert_eq!(action(&report.computers[2]), Action::Waiting("Busy"));
        assert_eq!(action(&report.computers[3]), Action::Retry);
        let mut computer = computer(1, json!(null));
        assert_eq!(action(&computer), Action::Upgrade);
        computer.upgrade = Some(ComputerUpgrade::Completed {
            unrestored_packages: vec![],
        });
        assert_eq!(action(&computer), Action::Upgrade);
        computer.image = "else-guest:4".into();
        assert_eq!(action(&computer), Action::Current);
        computer.latest_image = None;
        assert_eq!(action(&computer), Action::Current);
        for (number, text) in [
            (1, "1st"),
            (2, "2nd"),
            (3, "3rd"),
            (11, "11th"),
            (12, "12th"),
            (13, "13th"),
            (21, "21st"),
        ] {
            assert_eq!(ordinal(number), text);
        }
    }
}
