//! Routine rules: a saved task's name and prompt, and its schedule as a
//! standard five-field cron expression evaluated in an IANA time zone,
//! optionally limited to active hours, and the optional check that decides
//! whether a run wakes the model.

use std::str::FromStr;

use chrono::{DateTime, Datelike, NaiveTime, SubsecRound, TimeDelta, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};

use super::providers::Profile;

pub const MAX_NAME_CHARS: usize = 80;
pub const MAX_PROMPT_BYTES: usize = 8_000;
pub const MAX_CHECK_BYTES: usize = 8_000;
/// How long a routine's check may run before it is stopped.
pub const CHECK_DURATION: std::time::Duration = std::time::Duration::from_secs(3600);
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
    #[error("check must not be empty; pass null to remove it")]
    Check,
    #[error("check must be at most 8000 bytes")]
    CheckTooLarge,
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
    #[error(
        "this schedule never occurs inside its active hours ({0}); if the owner asked for these \
         times, pass active_hours that include them, or \"always\""
    )]
    OutsideActiveHours(String),
    #[error(
        "pass exactly one of in_minutes (a whole number of minutes from now) or at (local \
         YYYY-MM-DDTHH:MM that exists in the time zone)"
    )]
    When,
    #[error("that time has passed; pick a time in the future")]
    Passed,
    #[error("a reminder can be at most {MAX_REMINDER_DAYS} days ahead; use a routine instead")]
    TooFar,
    #[error(
        "a reminder runs once at a fixed time; delete it and create another reminder or a routine \
         to change when it runs"
    )]
    FixedTime,
    #[error(
        "active_hours must be \"always\" or {{\"days\": [\"mon\", ...], \"start\": \"HH:MM\", \
         \"end\": \"HH:MM\"}} with at least one day and start before end"
    )]
    ActiveHours,
}

impl InvalidRoutine {
    pub fn is_size_limit(&self) -> bool {
        matches!(self, Self::PromptTooLarge | Self::CheckTooLarge)
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

/// A shell command the bot's computer runs before each run, trimmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutineCheck(String);

impl RoutineCheck {
    pub fn parse(raw: &str) -> Result<Self, InvalidRoutine> {
        if raw.contains('\0') {
            return Err(InvalidRoutine::Nul);
        }
        let check = raw.trim();
        if check.is_empty() {
            return Err(InvalidRoutine::Check);
        }
        if check.len() > MAX_CHECK_BYTES {
            return Err(InvalidRoutine::CheckTooLarge);
        }
        Ok(Self(check.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// How a routine's check job ended, read from its `guest_jobs` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckEnd {
    /// Exit 0 without a byte of output: nothing new.
    Clear,
    /// Exit 0 with output for the run.
    Found(String),
    Failed {
        exit_code: i32,
        output: String,
    },
    /// Stopped before it exited: at its deadline, or by a cancel other than
    /// the owner's Stop (which settles the run itself).
    Stopped(String),
    /// It ended without a readable exit status, or its computer stopped
    /// first: whether it finished is unknown.
    Unknown(String),
    /// It never held the computer, so it never ran.
    NotStarted,
}

/// What a run does once its check ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterCheck {
    Skip(CheckSkip),
    /// Queue the turn with this note after the routine's prompt.
    Wake(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckSkip {
    NothingNew,
    Cancelled,
    Unavailable,
}

impl CheckEnd {
    /// `leased` is whether the job ever held the computer.
    pub fn of(
        status: crate::computers::jobs::JobStatus,
        exit_code: Option<i32>,
        output: Option<String>,
        leased: bool,
    ) -> Option<Self> {
        use crate::computers::jobs::JobStatus;
        let output = output.unwrap_or_default();
        Some(match (status, exit_code) {
            (JobStatus::Completed, _) if output.is_empty() => Self::Clear,
            (JobStatus::Completed, _) => Self::Found(output),
            (JobStatus::Failed, Some(exit_code)) => Self::Failed { exit_code, output },
            (JobStatus::Failed, None) if !leased => Self::NotStarted,
            (JobStatus::Failed, None) | (JobStatus::Lost, _) => Self::Unknown(output),
            (JobStatus::Cancelled, _) => Self::Stopped(output),
            (JobStatus::Starting | JobStatus::Running | JobStatus::Cancelling, _) => return None,
        })
    }

    pub fn after(&self) -> AfterCheck {
        let minutes = CHECK_DURATION.as_secs() / 60;
        let (head, output) = match self {
            Self::Clear => return AfterCheck::Skip(CheckSkip::NothingNew),
            Self::NotStarted => return AfterCheck::Skip(CheckSkip::Unavailable),
            Self::Found(output) => ("This routine's check found this:".to_owned(), output),
            Self::Failed { exit_code, output } => (
                format!(
                    "This routine's check failed with exit code {exit_code}. Run the task without \
                     it if you can, and tell the owner if it keeps failing. Its output:"
                ),
                output,
            ),
            Self::Stopped(output) => (
                format!(
                    "This routine's check was stopped before it finished, at its {minutes}-minute \
                     limit or by a cancel. Its output so far:"
                ),
                output,
            ),
            Self::Unknown(output) => (
                "This routine's check has an unknown outcome: it ended without reporting an exit \
                 code. Its output so far:"
                    .to_owned(),
                output,
            ),
        };
        let output = output.trim();
        let output = if output.is_empty() { "(none)" } else { output };
        AfterCheck::Wake(format!("{head}\n\n{output}"))
    }
}

/// The text a run's turn starts with: the routine's prompt, then what its
/// check reported, if it has one.
pub fn run_prompt(prompt: &str, note: Option<&str>) -> String {
    match note {
        Some(note) => format!("{prompt}\n\n---\n\n{note}"),
        None => prompt.to_owned(),
    }
}

const DAY_NAMES: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
/// Occurrences outside active hours skipped before a schedule counts as
/// never due.
const MAX_SKIPPED: usize = 1_000;

/// The days and daily span, in the routine's time zone, when a run may
/// start: from `start` up to but excluding `end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveHours {
    /// Bit 0 is Monday, bit 6 Sunday. Never zero.
    days: u8,
    start: NaiveTime,
    end: NaiveTime,
}

impl ActiveHours {
    /// Monday to Friday, 08:00 to 18:00: the default for new routines.
    pub const WORKING: Self = Self {
        days: 0b001_1111,
        start: NaiveTime::from_hms_opt(8, 0, 0).unwrap(),
        end: NaiveTime::from_hms_opt(18, 0, 0).unwrap(),
    };

    pub fn parse(days: &[String], start: &str, end: &str) -> Result<Self, InvalidRoutine> {
        let mut mask = 0u8;
        for day in days {
            let index = DAY_NAMES
                .iter()
                .position(|name| day.trim().eq_ignore_ascii_case(name))
                .ok_or(InvalidRoutine::ActiveHours)?;
            mask |= 1 << index;
        }
        let time = |text: &str| {
            NaiveTime::parse_from_str(text.trim(), "%H:%M").map_err(|_| InvalidRoutine::ActiveHours)
        };
        let (start, end) = (time(start)?, time(end)?);
        Self::from_parts(mask, start, end).ok_or(InvalidRoutine::ActiveHours)
    }

    fn from_parts(days: u8, start: NaiveTime, end: NaiveTime) -> Option<Self> {
        (days != 0 && days < 0x80 && start < end).then_some(Self { days, start, end })
    }

    /// From the stored day mask and minutes after midnight.
    pub fn from_columns(days: i64, start: i64, end: i64) -> Option<Self> {
        let time = |minutes: i64| {
            let minutes = u32::try_from(minutes).ok()?;
            NaiveTime::from_hms_opt(minutes / 60, minutes % 60, 0)
        };
        Self::from_parts(u8::try_from(days).ok()?, time(start)?, time(end)?)
    }

    /// The day mask and minutes after midnight, as stored.
    pub fn columns(&self) -> (i64, i64, i64) {
        let minutes = |time: NaiveTime| i64::from(time.num_seconds_from_midnight() / 60);
        (i64::from(self.days), minutes(self.start), minutes(self.end))
    }

    pub fn days(&self) -> Vec<&'static str> {
        (0..7)
            .filter(|i| self.days & (1 << i) != 0)
            .map(|i| DAY_NAMES[i])
            .collect()
    }

    pub fn start(&self) -> String {
        self.start.format("%H:%M").to_string()
    }

    pub fn end(&self) -> String {
        self.end.format("%H:%M").to_string()
    }

    fn on(&self, day: chrono::Weekday) -> bool {
        self.days & (1 << day.num_days_from_monday()) != 0
    }

    /// Whether a run may start at `at`, read on its local wall clock.
    pub fn contains<Z: TimeZone>(&self, at: &DateTime<Z>) -> bool {
        let time = at.time();
        self.on(at.weekday()) && self.start <= time && time < self.end
    }

    /// The first instant after `at` when the window opens. A start inside a
    /// daylight-saving gap opens at the first valid minute of the span; a
    /// start repeated by a fall-back overlap opens at either instant.
    fn next_opening<Z: TimeZone>(&self, at: &DateTime<Z>) -> Option<DateTime<Z>> {
        let zone = at.timezone();
        let opens_on = |date: chrono::NaiveDate| {
            let mut local = date.and_time(self.start);
            let end = date.and_time(self.end);
            while local < end {
                match zone.from_local_datetime(&local) {
                    chrono::LocalResult::Single(only) => return [Some(only), None],
                    chrono::LocalResult::Ambiguous(first, second) => {
                        return [Some(first), Some(second)];
                    }
                    chrono::LocalResult::None => {}
                }
                local += TimeDelta::minutes(1);
            }
            [None, None]
        };
        (0..=14)
            .filter_map(|offset| at.date_naive().checked_add_days(chrono::Days::new(offset)))
            .filter(|date| self.on(date.weekday()))
            .flat_map(opens_on)
            .flatten()
            .find(|opening| opening > at)
    }
}

impl std::fmt::Display for ActiveHours {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {}-{}",
            self.days().join(","),
            self.start(),
            self.end()
        )
    }
}

/// A parsed schedule: a cron expression, or one instant for a reminder.
/// Cron has minute resolution; day-of-month and day-of-week combine with
/// OR, as in Vixie cron. Occurrences outside active hours are skipped.
#[derive(Debug, Clone)]
pub enum Schedule {
    Cron {
        expr: String,
        cron: Box<Cron>,
        tz: Tz,
        hours: Option<ActiveHours>,
    },
    /// A reminder: due once at `at`, at any hour. `tz` is how the owner
    /// reads its time.
    Once { at: DateTime<Utc>, tz: Tz },
}

/// What a reminder stores in the cron column.
pub const ONCE: &str = "once";
/// How far ahead a reminder may be set.
pub const MAX_REMINDER_DAYS: i64 = 366;

fn zone(timezone: Option<&str>) -> Result<Tz, InvalidRoutine> {
    Tz::from_str(timezone.unwrap_or(DEFAULT_TIMEZONE).trim()).map_err(|_| InvalidRoutine::Timezone)
}

impl Schedule {
    pub fn parse(
        cron: &str,
        timezone: Option<&str>,
        hours: Option<ActiveHours>,
    ) -> Result<Self, InvalidRoutine> {
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
        Ok(Self::Cron {
            expr,
            cron: Box::new(parsed),
            tz: zone(timezone)?,
            hours,
        })
    }

    /// Parse and require at least one occurrence after `now`.
    pub fn parse_due(
        cron: &str,
        timezone: Option<&str>,
        hours: Option<ActiveHours>,
        now: DateTime<Utc>,
    ) -> Result<Self, InvalidRoutine> {
        let schedule = Self::parse(cron, timezone, hours)?;
        if schedule.next_after(now).is_none() {
            return Err(match hours {
                Some(hours) if Self::parse(cron, timezone, None)?.next_after(now).is_some() => {
                    InvalidRoutine::OutsideActiveHours(hours.to_string())
                }
                _ => InvalidRoutine::NeverDue,
            });
        }
        Ok(schedule)
    }

    /// A reminder due at `when`. A wall-clock time repeated by a
    /// daylight-saving fall-back is its first occurrence after `now`.
    /// [`Self::check_due`] bounds it.
    pub fn once(
        when: When<'_>,
        timezone: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Self, InvalidRoutine> {
        let tz = zone(timezone)?;
        let at = match when {
            When::InMinutes(minutes) => now
                .trunc_subsecs(0)
                .checked_add_signed(TimeDelta::try_minutes(minutes).ok_or(InvalidRoutine::When)?)
                .ok_or(InvalidRoutine::When)?,
            When::At(local) => {
                let local = chrono::NaiveDateTime::parse_from_str(local.trim(), "%Y-%m-%dT%H:%M")
                    .map_err(|_| InvalidRoutine::When)?;
                match tz.from_local_datetime(&local) {
                    chrono::LocalResult::Single(at) => at,
                    chrono::LocalResult::Ambiguous(first, second) => {
                        if first > now {
                            first
                        } else {
                            second
                        }
                    }
                    chrono::LocalResult::None => return Err(InvalidRoutine::When),
                }
                .with_timezone(&Utc)
            }
        };
        Ok(Self::Once { at, tz })
    }

    /// A reminder must be due after `now` and at most
    /// [`MAX_REMINDER_DAYS`] ahead; a cron schedule always passes.
    pub fn check_due(&self, now: DateTime<Utc>) -> Result<(), InvalidRoutine> {
        match self.run_at() {
            Some(at) if at <= now => Err(InvalidRoutine::Passed),
            Some(at) if at > now + TimeDelta::days(MAX_REMINDER_DAYS) => {
                Err(InvalidRoutine::TooFar)
            }
            _ => Ok(()),
        }
    }

    /// The schedule a routine row stores: a reminder when `run_at` is set.
    pub fn stored(
        cron: &str,
        run_at: Option<DateTime<Utc>>,
        timezone: &str,
        hours: Option<ActiveHours>,
    ) -> Result<Self, InvalidRoutine> {
        match run_at {
            Some(at) => Ok(Self::Once {
                at,
                tz: zone(Some(timezone))?,
            }),
            None => Self::parse(cron, Some(timezone), hours),
        }
    }

    /// The cron column's value: [`ONCE`] for a reminder.
    pub fn cron(&self) -> &str {
        match self {
            Self::Cron { expr, .. } => expr,
            Self::Once { .. } => ONCE,
        }
    }

    /// When a reminder is due; `None` for a cron schedule.
    pub fn run_at(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Cron { .. } => None,
            Self::Once { at, .. } => Some(*at),
        }
    }

    pub fn timezone(&self) -> &str {
        self.tz().name()
    }

    fn tz(&self) -> Tz {
        match self {
            Self::Cron { tz, .. } | Self::Once { tz, .. } => *tz,
        }
    }

    pub fn hours(&self) -> Option<ActiveHours> {
        match self {
            Self::Cron { hours, .. } => *hours,
            Self::Once { .. } => None,
        }
    }

    /// The first occurrence inside active hours strictly after `after`, if
    /// any.
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let (cron, tz, hours) = match self {
            Self::Once { at, .. } => return (*at > after).then_some(*at),
            Self::Cron {
                cron, tz, hours, ..
            } => (cron, tz, hours),
        };
        let mut from = after.trunc_subsecs(0).with_timezone(tz);
        for _ in 0..MAX_SKIPPED {
            let next = cron.find_next_occurrence(&from, false).ok()?;
            match hours {
                Some(hours) if !hours.contains(&next) => {
                    from = hours.next_opening(&next)? - TimeDelta::seconds(1);
                }
                _ => return Some(next.with_timezone(&Utc)),
            }
        }
        None
    }

    /// Whether a run may start at `at`: always without active hours.
    pub fn allows(&self, at: DateTime<Utc>) -> bool {
        self.hours()
            .is_none_or(|hours| hours.contains(&at.with_timezone(&self.tz())))
    }
}

/// When a reminder runs, as a bot asks for it.
#[derive(Debug, Clone, Copy)]
pub enum When<'a> {
    InMinutes(i64),
    /// Local `YYYY-MM-DDTHH:MM` in the reminder's time zone.
    At(&'a str),
}

/// A routine's own model: a provider and a complete profile its adapter
/// validated. Stored as JSON with these four fields.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RoutineModel {
    pub provider_id: String,
    #[serde(flatten)]
    pub profile: Profile,
}

/// Which model a routine runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSetting {
    /// The bot's live choice at each run. `requested` keeps a model the bot
    /// was asked for but could not resolve, for the owner to see.
    Inherit {
        requested: Option<String>,
    },
    Own(RoutineModel),
}

fn squash(text: &str) -> String {
    text.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// The one catalog model `query` names: its exact ID, or its ID or display
/// name ignoring case, spaces, dots, and dashes, with or without the
/// vendor's first word ("GPT 6.1 Sol", "6.1 sol", "Opus 5.5"). `None` when
/// nothing or more than one model matches; never a nearest guess.
pub fn match_model(
    query: &str,
    catalogs: &[(&'static str, &'static [&'static str])],
) -> Option<(&'static str, &'static str)> {
    let query = query.trim();
    let all = || {
        catalogs
            .iter()
            .flat_map(|(provider, models)| models.iter().map(move |model| (*provider, *model)))
    };
    if let Some(exact) = all().find(|(_, model)| model.eq_ignore_ascii_case(query)) {
        return Some(exact);
    }
    let wanted = squash(query);
    if wanted.is_empty() {
        return None;
    }
    let mut found = all().filter(|(_, model)| {
        squash(model) == wanted
            || model
                .split_once('-')
                .is_some_and(|(_, rest)| squash(rest) == wanted)
    });
    let first = found.next()?;
    found.next().is_none().then_some(first)
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
        let schedule = Schedule::parse(" 0  9 * * 1-5 ", None, None).unwrap();
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
                matches!(
                    Schedule::parse(bad, None, None),
                    Err(InvalidRoutine::Cron(_))
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn unknown_time_zones_are_refused() {
        for bad in ["Mars/Olympus", "utc ", "Europe/paris", "+02:00", "Local"] {
            assert!(
                matches!(
                    Schedule::parse("0 9 * * *", Some(bad), None),
                    Err(InvalidRoutine::Timezone)
                ),
                "{bad:?}"
            );
        }
        assert_eq!(
            Schedule::parse("0 9 * * *", Some("Europe/Paris"), None)
                .unwrap()
                .timezone(),
            "Europe/Paris"
        );
    }

    #[test]
    fn a_schedule_that_never_occurs_is_refused() {
        let now = at("2026-11-04T10:00:00Z");
        assert_eq!(
            Schedule::parse_due("0 0 30 2 *", None, None, now).unwrap_err(),
            InvalidRoutine::NeverDue
        );
        assert!(Schedule::parse_due("0 0 29 2 *", None, None, now).is_ok());
    }

    #[test]
    fn next_occurrence_is_strictly_after_and_minute_aligned() {
        let daily = Schedule::parse("30 9 * * *", None, None).unwrap();
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
        let every_quarter = Schedule::parse("*/15 * * * *", None, None).unwrap();
        assert_eq!(
            every_quarter.next_after(at("2026-11-04T10:07:12Z")),
            Some(at("2026-11-04T10:15:00Z"))
        );
        let either = Schedule::parse("0 8 1 * 1", None, None).unwrap();
        assert_eq!(
            either.next_after(at("2026-11-04T10:00:00Z")),
            Some(at("2026-11-09T08:00:00Z"))
        );
    }

    #[test]
    fn time_zones_and_daylight_saving_follow_the_wall_clock() {
        let paris = Schedule::parse("0 9 * * *", Some("Europe/Paris"), None).unwrap();
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
        let gap = Schedule::parse("30 2 * * *", Some("Europe/Paris"), None).unwrap();
        let in_gap = gap.next_after(at("2026-03-28T12:00:00Z")).unwrap();
        assert_eq!(in_gap, at("2026-03-29T01:00:00Z"));
        assert_eq!(gap.next_after(in_gap), Some(at("2026-03-30T00:30:00Z")));
        let first = gap.next_after(at("2026-10-24T12:00:00Z")).unwrap();
        assert_eq!(first, at("2026-10-25T00:30:00Z"));
        assert_eq!(gap.next_after(first), Some(at("2026-10-26T01:30:00Z")));
    }

    #[test]
    fn active_hours_skip_nights_and_weekends_in_the_routine_zone() {
        let working = Some(ActiveHours::WORKING);
        let half_hourly = Schedule::parse("*/30 * * * *", Some("Europe/Paris"), working).unwrap();
        assert_eq!(
            half_hourly.next_after(at("2026-11-06T16:45:00Z")),
            Some(at("2026-11-09T07:00:00Z")),
            "Friday 17:45 in Paris resumes Monday 08:00"
        );
        assert_eq!(
            half_hourly.next_after(at("2026-11-09T07:00:00Z")),
            Some(at("2026-11-09T07:30:00Z"))
        );
        assert_eq!(
            half_hourly.next_after(at("2026-11-09T16:30:00Z")),
            Some(at("2026-11-10T07:00:00Z")),
            "18:00 is outside the span"
        );
        let daily = Schedule::parse("0 9 * * *", Some("Europe/Paris"), working).unwrap();
        assert_eq!(
            daily.next_after(at("2026-11-06T09:00:00Z")),
            Some(at("2026-11-09T08:00:00Z"))
        );
        let always = Schedule::parse("0 9 * * *", Some("Europe/Paris"), None).unwrap();
        assert_eq!(
            always.next_after(at("2026-11-06T09:00:00Z")),
            Some(at("2026-11-07T08:00:00Z"))
        );
    }

    #[test]
    fn active_hours_open_on_valid_instants_across_daylight_saving() {
        let sunday = |start: &str, end: &str| {
            Some(ActiveHours::parse(&["sun".to_owned()], start, end).unwrap())
        };
        let gap =
            Schedule::parse("* * * * *", Some("Europe/Paris"), sunday("02:30", "04:00")).unwrap();
        assert_eq!(
            gap.next_after(at("2027-03-28T00:00:00Z")),
            Some(at("2027-03-28T01:00:00Z")),
            "a start in the spring gap opens at 03:00 local"
        );
        let overlap =
            Schedule::parse("* * * * *", Some("Europe/Paris"), sunday("02:30", "02:45")).unwrap();
        assert_eq!(
            overlap.next_after(at("2027-10-31T01:10:00Z")),
            Some(at("2027-10-31T01:30:00Z")),
            "the repeated 02:30 after fall-back opens again"
        );
        let swallowed = Schedule::parse(
            "0 3 * * 0",
            Some("Antarctica/Troll"),
            sunday("01:30", "02:30"),
        )
        .unwrap();
        let next = swallowed
            .next_after(at("2027-03-21T02:59:59Z"))
            .map(|n| n.with_timezone(&"Antarctica/Troll".parse::<Tz>().unwrap()));
        assert!(
            next.is_none_or(|n| (n.time() >= NaiveTime::from_hms_opt(1, 30, 0).unwrap())
                && n.time() < NaiveTime::from_hms_opt(2, 30, 0).unwrap()),
            "a window lost to a gap never authorizes a run outside it: {next:?}"
        );
    }

    #[test]
    fn a_schedule_entirely_outside_active_hours_is_refused() {
        let now = at("2026-11-04T10:00:00Z");
        let working = Some(ActiveHours::WORKING);
        assert_eq!(
            Schedule::parse_due("0 22 * * *", None, working, now).unwrap_err(),
            InvalidRoutine::OutsideActiveHours("mon,tue,wed,thu,fri 08:00-18:00".into())
        );
        assert_eq!(
            Schedule::parse_due("0 10 * * 6", None, working, now).unwrap_err(),
            InvalidRoutine::OutsideActiveHours("mon,tue,wed,thu,fri 08:00-18:00".into())
        );
        assert_eq!(
            Schedule::parse_due("0 0 30 2 *", None, working, now).unwrap_err(),
            InvalidRoutine::NeverDue
        );
    }

    #[test]
    fn active_hours_parse_and_round_trip_their_columns() {
        let days = |names: &[&str]| names.iter().map(|d| (*d).to_owned()).collect::<Vec<_>>();
        let parsed = ActiveHours::parse(
            &days(&["mon", "Tue", "wed", "thu", "fri"]),
            "08:00",
            "18:00",
        )
        .unwrap();
        assert_eq!(parsed, ActiveHours::WORKING);
        let (d, s, e) = parsed.columns();
        assert_eq!((d, s, e), (0b001_1111, 480, 1080));
        assert_eq!(ActiveHours::from_columns(d, s, e), Some(parsed));
        for (names, start, end) in [
            (days(&[]), "08:00", "18:00"),
            (days(&["monday"]), "08:00", "18:00"),
            (days(&["sat"]), "18:00", "08:00"),
            (days(&["sat"]), "09:00", "09:00"),
            (days(&["sat"]), "9am", "18:00"),
        ] {
            assert_eq!(
                ActiveHours::parse(&names, start, end),
                Err(InvalidRoutine::ActiveHours),
                "{names:?} {start} {end}"
            );
        }
    }

    #[test]
    fn model_names_match_one_catalog_entry_or_none() {
        let catalogs: &[(&str, &[&str])] = &[
            ("codex", &["gpt-6.1-sol", "gpt-6-sol", "gpt-6-luna"]),
            ("claude", &["claude-opus-5-5", "claude-fable-5-1"]),
        ];
        let found = |q| match_model(q, catalogs);
        assert_eq!(found("gpt-6.1-sol"), Some(("codex", "gpt-6.1-sol")));
        assert_eq!(found("GPT 6.1 Sol"), Some(("codex", "gpt-6.1-sol")));
        assert_eq!(found("6.1 sol"), Some(("codex", "gpt-6.1-sol")));
        assert_eq!(
            found("Claude Opus 5.5"),
            Some(("claude", "claude-opus-5-5"))
        );
        assert_eq!(found("opus 5.5"), Some(("claude", "claude-opus-5-5")));
        assert_eq!(found("Sol"), None, "ambiguous names are not guessed");
        assert_eq!(
            found("opus"),
            None,
            "a family without a version is not guessed"
        );
        assert_eq!(found("Gemini 3"), None);
        assert_eq!(found("  "), None);
    }

    #[test]
    fn a_reminder_takes_the_next_occurrence_of_a_repeated_local_time() {
        let now = at("2026-11-01T06:10:00Z");
        let ny = Some("America/New_York");
        let repeated = Schedule::once(When::At("2026-11-01T01:30"), ny, now).unwrap();
        assert_eq!(
            repeated.run_at(),
            Some(at("2026-11-01T06:30:00Z")),
            "the EST 01:30"
        );
        let early = at("2026-11-01T04:00:00Z");
        let first = Schedule::once(When::At("2026-11-01T01:30"), ny, early).unwrap();
        assert_eq!(
            first.run_at(),
            Some(at("2026-11-01T05:30:00Z")),
            "the EDT 01:30"
        );
        assert_eq!(
            Schedule::once(When::At("2026-03-08T02:30"), ny, early).unwrap_err(),
            InvalidRoutine::When,
            "a time skipped by spring-forward never occurs"
        );
    }

    #[test]
    fn a_reminder_is_due_after_now_and_within_a_year() {
        let now = at("2026-11-10T12:00:00Z");
        let check = |when| Schedule::once(when, None, now).unwrap().check_due(now);
        assert_eq!(check(When::InMinutes(360)), Ok(()));
        assert_eq!(
            check(When::At("2026-11-10T11:59")),
            Err(InvalidRoutine::Passed)
        );
        assert_eq!(check(When::InMinutes(0)), Err(InvalidRoutine::Passed));
        assert_eq!(
            check(When::InMinutes((MAX_REMINDER_DAYS + 1) * 24 * 60)),
            Err(InvalidRoutine::TooFar)
        );
        assert!(
            Schedule::parse("0 9 * * *", None, None)
                .unwrap()
                .check_due(now)
                .is_ok()
        );
    }

    #[test]
    fn a_check_skips_only_when_it_exits_zero_with_nothing_or_never_ran() {
        use crate::computers::jobs::JobStatus;
        let end = |status, exit, output: &str, leased| {
            CheckEnd::of(status, exit, Some(output.to_owned()), leased)
                .unwrap()
                .after()
        };
        assert_eq!(
            end(JobStatus::Completed, Some(0), "", true),
            AfterCheck::Skip(CheckSkip::NothingNew)
        );
        assert_eq!(
            end(JobStatus::Failed, None, "", false),
            AfterCheck::Skip(CheckSkip::Unavailable)
        );
        let wake = |after| match after {
            AfterCheck::Wake(note) => note,
            AfterCheck::Skip(skip) => panic!("skipped: {skip:?}"),
        };
        let note = wake(end(JobStatus::Completed, Some(0), " \n", true));
        assert!(
            note.ends_with("(none)"),
            "any byte of output is new: {note}"
        );
        let note = wake(end(JobStatus::Completed, Some(0), "3 leads\n", true));
        assert!(note.ends_with("\n\n3 leads"), "{note}");
        let note = wake(end(JobStatus::Failed, Some(2), "", true));
        assert!(
            note.contains("exit code 2") && note.ends_with("(none)"),
            "{note}"
        );
        let note = wake(end(JobStatus::Cancelled, Some(130), "half", true));
        assert!(note.contains("stopped before it finished"), "{note}");
        let note = wake(end(JobStatus::Failed, None, "new lead", true));
        assert!(
            note.contains("unknown outcome") && note.ends_with("new lead"),
            "an unreadable exit status after launch is unknown, not unstarted: {note}"
        );
        let note = wake(end(JobStatus::Lost, None, "", true));
        assert!(note.contains("unknown outcome"), "{note}");
        assert_eq!(
            CheckEnd::of(JobStatus::Running, None, None, true),
            None,
            "a running check has no end yet"
        );
        assert_eq!(run_prompt("Do it.", None), "Do it.");
        assert_eq!(
            run_prompt("Do it.", Some("Found x.")),
            "Do it.\n\n---\n\nFound x."
        );
    }

    #[test]
    fn checks_are_trimmed_and_bounded() {
        assert_eq!(
            RoutineCheck::parse("  ./a.sh \n").unwrap().as_str(),
            "./a.sh"
        );
        assert_eq!(RoutineCheck::parse(" \n"), Err(InvalidRoutine::Check));
        assert_eq!(
            RoutineCheck::parse(&"x".repeat(MAX_CHECK_BYTES + 1)),
            Err(InvalidRoutine::CheckTooLarge)
        );
    }
}
