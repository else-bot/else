//! How a bot's waits read to the owner: the tray's header, each wait's
//! title and where its job stands. Shared by the desktop and the phone.

use else_wire::waits::JobStatus;

use crate::model::Wait;

/// `text` on one line, its runs of whitespace as single spaces.
pub fn one_line(task: &str) -> String {
    task.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Wait<'_> {
    /// "2 jobs and 1 reply": what the tray's header says the bot waits on.
    pub fn summary(waits: &[Wait<'_>]) -> String {
        let replies = waits.iter().filter(|w| matches!(w, Wait::Task(_))).count();
        let jobs = waits.len() - replies;
        let count =
            |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        match (jobs, replies) {
            (0, replies) => count(replies, "reply", "replies"),
            (jobs, 0) => count(jobs, "job", "jobs"),
            (jobs, replies) => format!(
                "{} and {}",
                count(jobs, "job", "jobs"),
                count(replies, "reply", "replies")
            ),
        }
    }

    /// What the bot said it waits for, as the row's label.
    pub fn title(&self) -> String {
        let wait = self;
        let purpose = match wait {
            Wait::Mcp(wait) if !wait.purpose.trim().is_empty() => one_line(&wait.purpose),
            Wait::Mcp(wait) => format!("{} job", wait.mcp_id),
            Wait::Job(job) => job
                .purpose
                .as_deref()
                .map(one_line)
                .filter(|purpose| !purpose.is_empty())
                .unwrap_or_else(|| "Computer job".to_owned()),
            Wait::Task(task) => one_line(&task.task),
        };
        format!("Waiting: {purpose}")
    }

    /// Where its job stands: its checks and last readable status.
    pub fn status(&self) -> Option<String> {
        let wait = self;
        match wait {
            Wait::Mcp(wait) => {
                let checks = match wait.checks {
                    0 => None,
                    1 => Some("1 check".to_owned()),
                    n => Some(format!("{n} checks")),
                };
                let parts: Vec<String> =
                    checks.into_iter().chain(wait.last_status.clone()).collect();
                (!parts.is_empty()).then(|| parts.join(" · "))
            }
            Wait::Job(job) => Some(
                job.job_status
                    .as_ref()
                    .unwrap_or(&JobStatus::Running)
                    .as_str()
                    .to_owned(),
            ),
            Wait::Task(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::model::Wait;
    use else_wire::waits::{ComputerJobWait, McpWait};
    use uuid::Uuid;

    fn mcp(purpose: &str, checks: i64, status: Option<&str>) -> McpWait {
        McpWait {
            id: Uuid::from_u128(1),
            mcp_id: "t3-code".into(),
            tool: "task_status".into(),
            purpose: purpose.into(),
            interval_seconds: 30,
            deadline_at: None,
            checks,
            last_status: status.map(str::to_owned),
            last_result: None,
            created_at: None,
        }
    }

    #[test]
    fn a_wait_reads_as_its_purpose_and_status() {
        let wait = mcp("T3 thread for #465\n(else_status)", 2, Some("in_progress"));
        assert_eq!(
            Wait::Mcp(&wait).title(),
            "Waiting: T3 thread for #465 (else_status)"
        );
        assert_eq!(
            Wait::Mcp(&wait).status().as_deref(),
            Some("2 checks · in_progress")
        );
        let job = ComputerJobWait {
            id: Uuid::from_u128(2),
            job_id: Uuid::from_u128(3),
            command: "for i in $(seq 1 50); do curl -s https://api.github.com/x; done".into(),
            purpose: Some("CI checks on PR #482".into()),
            job_status: None,
            created_at: None,
        };
        assert_eq!(Wait::Job(&job).title(), "Waiting: CI checks on PR #482");
        assert_eq!(Wait::Job(&job).status().as_deref(), Some("running"));
        let unlabeled = ComputerJobWait {
            purpose: None,
            ..job
        };
        assert_eq!(
            Wait::Job(&unlabeled).title(),
            "Waiting: Computer job",
            "never the raw command"
        );
    }

    #[test]
    fn the_header_counts_jobs_and_replies() {
        let one = mcp("a", 1, None);
        let two = mcp("b", 1, None);
        assert_eq!(Wait::summary(&[Wait::Mcp(&one)]), "1 job");
        assert_eq!(Wait::summary(&[Wait::Mcp(&one), Wait::Mcp(&two)]), "2 jobs");
    }
}
