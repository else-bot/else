//! Routine rules: a saved task's name and prompt, and its schedule as a
//! standard five-field cron expression evaluated in an IANA time zone,
//! optionally limited to active hours.

use std::str::FromStr;

use chrono::{DateTime, Datelike, NaiveTime, SubsecRound, TimeDelta, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};

use super::providers::Profile;

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
    #[error(
        "this schedule never occurs inside its active hours ({0}); if the owner asked for these \
         times, pass active_hours that include them, or \"always\""
    )]
    OutsideActiveHours(String),
    #[error(
        "active_hours must be \"always\" or {{\"days\": [\"mon\", ...], \"start\": \"HH:MM\", \
         \"end\": \"HH:MM\"}} with at least one day and start before end"
    )]
    ActiveHours,
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

/// A parsed schedule. Minute resolution; day-of-month and day-of-week
/// combine with OR, as in Vixie cron. Occurrences outside active hours are
/// skipped.
#[derive(Debug, Clone)]
pub struct Schedule {
    expr: String,
    cron: Cron,
    tz: Tz,
    hours: Option<ActiveHours>,
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
        let tz = Tz::from_str(timezone.unwrap_or(DEFAULT_TIMEZONE).trim())
            .map_err(|_| InvalidRoutine::Timezone)?;
        let schedule = Self {
            expr,
            cron: parsed,
            tz,
            hours,
        };
        Ok(schedule)
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

    pub fn cron(&self) -> &str {
        &self.expr
    }

    pub fn timezone(&self) -> &str {
        self.tz.name()
    }

    pub fn hours(&self) -> Option<ActiveHours> {
        self.hours
    }

    /// The first occurrence inside active hours strictly after `after`, if
    /// any.
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let mut from = after.trunc_subsecs(0).with_timezone(&self.tz);
        for _ in 0..MAX_SKIPPED {
            let next = self.cron.find_next_occurrence(&from, false).ok()?;
            match self.hours {
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
        self.hours
            .is_none_or(|hours| hours.contains(&at.with_timezone(&self.tz)))
    }
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
}
