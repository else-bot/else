//! Routine rules: a saved task's name and prompt, and its schedule as a
//! standard five-field cron expression evaluated in an IANA time zone.

use std::str::FromStr;

use chrono::{DateTime, SubsecRound, Utc};
use chrono_tz::Tz;
use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};

pub const MAX_NAME_CHARS: usize = 80;
pub const MAX_PROMPT_BYTES: usize = 8_000;
const MAX_CRON_BYTES: usize = 100;
/// Routines one bot may have at once (deleted ones do not count).
pub const MAX_PER_BOT: i64 = 20;
/// Used when a routine names no time zone. Never the host's zone.
pub const DEFAULT_TIMEZONE: &str = "UTC";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidRoutine {
    #[error("name must be 1 to 80 characters after trimming")]
    Name,
    #[error("prompt must not be empty")]
    Prompt,
    #[error("prompt must be at most 8000 bytes")]
    PromptTooLarge,
    #[error("text must not contain NUL characters")]
    Nul,
    #[error(
        "cron must be five fields (minute hour day-of-month month day-of-week) using only digits, \
         '*', ',', '-' and '/': {0}"
    )]
    Cron(String),
    #[error("timezone must be an IANA time zone name such as Europe/Paris or UTC")]
    Timezone,
    #[error("this schedule never occurs")]
    NeverDue,
}

impl InvalidRoutine {
    pub fn is_size_limit(&self) -> bool {
        matches!(self, Self::PromptTooLarge)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutineName(String);

impl RoutineName {
    pub fn parse(raw: &str) -> Result<Self, InvalidRoutine> {
        if raw.contains('\0') {
            return Err(InvalidRoutine::Nul);
        }
        let name = raw.trim();
        let chars = name.chars().count();
        if chars == 0 || chars > MAX_NAME_CHARS {
            return Err(InvalidRoutine::Name);
        }
        Ok(Self(name.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The complete task a run executes, trimmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutinePrompt(String);

impl RoutinePrompt {
    pub fn parse(raw: &str) -> Result<Self, InvalidRoutine> {
        if raw.contains('\0') {
            return Err(InvalidRoutine::Nul);
        }
        let prompt = raw.trim();
        if prompt.is_empty() {
            return Err(InvalidRoutine::Prompt);
        }
        if prompt.len() > MAX_PROMPT_BYTES {
            return Err(InvalidRoutine::PromptTooLarge);
        }
        Ok(Self(prompt.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A parsed schedule. Minute resolution; day-of-month and day-of-week
/// combine with OR, as in Vixie cron.
#[derive(Debug, Clone)]
pub struct Schedule {
    expr: String,
    cron: Cron,
    tz: Tz,
}

impl Schedule {
    pub fn parse(cron: &str, timezone: Option<&str>) -> Result<Self, InvalidRoutine> {
        let fields: Vec<&str> = cron.split_whitespace().collect();
        let expr = fields.join(" ");
        let allowed = |c: char| c.is_ascii_digit() || matches!(c, '*' | ',' | '-' | '/' | ' ');
        if fields.len() != 5 || expr.len() > MAX_CRON_BYTES || !expr.chars().all(allowed) {
            return Err(InvalidRoutine::Cron(expr));
        }
        let parsed = CronParser::builder()
            .seconds(Seconds::Disallowed)
            .year(Year::Disallowed)
            .build()
            .parse(&expr)
            .map_err(|err| InvalidRoutine::Cron(format!("{expr} ({err})")))?;
        let tz = Tz::from_str(timezone.unwrap_or(DEFAULT_TIMEZONE).trim())
            .map_err(|_| InvalidRoutine::Timezone)?;
        let schedule = Self {
            expr,
            cron: parsed,
            tz,
        };
        Ok(schedule)
    }

    /// Parse and require at least one occurrence after `now`.
    pub fn parse_due(
        cron: &str,
        timezone: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Self, InvalidRoutine> {
        let schedule = Self::parse(cron, timezone)?;
        schedule.next_after(now).ok_or(InvalidRoutine::NeverDue)?;
        Ok(schedule)
    }

    pub fn cron(&self) -> &str {
        &self.expr
    }

    pub fn timezone(&self) -> &str {
        self.tz.name()
    }

    /// The first occurrence strictly after `after`, if any.
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let local = after.trunc_subsecs(0).with_timezone(&self.tz);
        self.cron
            .find_next_occurrence(&local, false)
            .ok()
            .map(|next| next.with_timezone(&Utc))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text).unwrap().to_utc()
    }

    #[test]
    fn names_and_prompts_are_trimmed_and_bounded() {
        assert_eq!(RoutineName::parse("  Digest ").unwrap().as_str(), "Digest");
        assert_eq!(RoutineName::parse(" "), Err(InvalidRoutine::Name));
        assert_eq!(
            RoutineName::parse(&"é".repeat(81)),
            Err(InvalidRoutine::Name)
        );
        assert!(RoutineName::parse(&"é".repeat(80)).is_ok());
        assert_eq!(RoutinePrompt::parse("\n "), Err(InvalidRoutine::Prompt));
        assert_eq!(
            RoutinePrompt::parse(&"x".repeat(MAX_PROMPT_BYTES + 1)),
            Err(InvalidRoutine::PromptTooLarge)
        );
        assert_eq!(RoutinePrompt::parse("a\0b"), Err(InvalidRoutine::Nul));
    }

    #[test]
    fn only_standard_five_field_cron_is_accepted() {
        let schedule = Schedule::parse(" 0  9 * * 1-5 ", None).unwrap();
        assert_eq!(schedule.cron(), "0 9 * * 1-5");
        assert_eq!(schedule.timezone(), "UTC", "UTC is the explicit default");
        for bad in [
            "",
            "* * * *",
            "0 0 9 * * *",
            "@daily",
            "0 9 * * MON",
            "0 9 L * *",
            "61 * * * *",
            "0 9 * * 8",
        ] {
            assert!(
                matches!(Schedule::parse(bad, None), Err(InvalidRoutine::Cron(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn unknown_time_zones_are_refused() {
        for bad in ["Mars/Olympus", "utc ", "Europe/paris", "+02:00", "Local"] {
            assert!(
                matches!(
                    Schedule::parse("0 9 * * *", Some(bad)),
                    Err(InvalidRoutine::Timezone)
                ),
                "{bad:?}"
            );
        }
        assert_eq!(
            Schedule::parse("0 9 * * *", Some("Europe/Paris"))
                .unwrap()
                .timezone(),
            "Europe/Paris"
        );
    }

    #[test]
    fn a_schedule_that_never_occurs_is_refused() {
        let now = at("2026-11-04T10:00:00Z");
        assert_eq!(
            Schedule::parse_due("0 0 30 2 *", None, now).unwrap_err(),
            InvalidRoutine::NeverDue
        );
        assert!(Schedule::parse_due("0 0 29 2 *", None, now).is_ok());
    }

    #[test]
    fn next_occurrence_is_strictly_after_and_minute_aligned() {
        let daily = Schedule::parse("30 9 * * *", None).unwrap();
        assert_eq!(
            daily.next_after(at("2026-11-04T09:29:59.900Z")),
            Some(at("2026-11-04T09:30:00Z"))
        );
        assert_eq!(
            daily.next_after(at("2026-11-04T09:30:00Z")),
            Some(at("2026-11-05T09:30:00Z"))
        );
        assert_eq!(
            daily.next_after(at("2026-11-04T09:30:00.400Z")),
            Some(at("2026-11-05T09:30:00Z"))
        );
        let every_quarter = Schedule::parse("*/15 * * * *", None).unwrap();
        assert_eq!(
            every_quarter.next_after(at("2026-11-04T10:07:12Z")),
            Some(at("2026-11-04T10:15:00Z"))
        );
        let either = Schedule::parse("0 8 1 * 1", None).unwrap();
        assert_eq!(
            either.next_after(at("2026-11-04T10:00:00Z")),
            Some(at("2026-11-09T08:00:00Z"))
        );
    }

    #[test]
    fn time_zones_and_daylight_saving_follow_the_wall_clock() {
        let paris = Schedule::parse("0 9 * * *", Some("Europe/Paris")).unwrap();
        assert_eq!(
            paris.next_after(at("2026-10-24T12:00:00Z")),
            Some(at("2026-10-25T08:00:00Z"))
        );
        assert_eq!(
            paris.next_after(at("2026-10-25T08:00:00Z")),
            Some(at("2026-10-26T08:00:00Z"))
        );
        assert_eq!(
            paris.next_after(at("2026-10-23T12:00:00Z")),
            Some(at("2026-10-24T07:00:00Z"))
        );
        // 02:30 does not exist on 29 March 2026 in Paris: that day's run
        // happens right after the gap, then the schedule resumes.
        let gap = Schedule::parse("30 2 * * *", Some("Europe/Paris")).unwrap();
        let in_gap = gap.next_after(at("2026-03-28T12:00:00Z")).unwrap();
        assert_eq!(in_gap, at("2026-03-29T01:00:00Z"));
        assert_eq!(gap.next_after(in_gap), Some(at("2026-03-30T00:30:00Z")));
        // 02:30 happens twice on 25 October 2026: it runs once.
        let first = gap.next_after(at("2026-10-24T12:00:00Z")).unwrap();
        assert_eq!(first, at("2026-10-25T00:30:00Z"));
        assert_eq!(gap.next_after(first), Some(at("2026-10-26T01:30:00Z")));
    }
}
