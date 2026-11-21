use chrono::{DateTime, Utc};

pub(super) fn local_time(at: DateTime<Utc>, timezone: &str) -> String {
    match timezone.parse::<chrono_tz::Tz>() {
        Ok(tz) => at.with_timezone(&tz).format("%a %-d %b, %H:%M").to_string(),
        Err(_) => at.format("%a %-d %b, %H:%M UTC").to_string(),
    }
}

const MONTHS: [(&str, u8); 12] = [
    ("January", 31),
    ("February", 29),
    ("March", 31),
    ("April", 30),
    ("May", 31),
    ("June", 30),
    ("July", 31),
    ("August", 31),
    ("September", 30),
    ("October", 31),
    ("November", 30),
    ("December", 31),
];

const WEEKDAYS: [&str; 7] = [
    "Sundays",
    "Mondays",
    "Tuesdays",
    "Wednesdays",
    "Thursdays",
    "Fridays",
    "Saturdays",
];

/// A short phrase for the common schedules. Any other schedule the server
/// accepts shows croner's description beside its expression, because that
/// description can blur cron's rules (`*/7` restarts every hour). Any other
/// expression shows as written.
pub(super) fn describe_cron(expr: &str) -> String {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    let server_grammar = fields.len() == 5
        && fields.iter().all(|field| {
            field
                .chars()
                .all(|c| c.is_ascii_digit() || "*,-/".contains(c))
        });
    fields
        .try_into()
        .ok()
        .and_then(describe_cron_fields)
        .or_else(|| {
            if !server_grammar {
                return None;
            }
            use croner::parser::{CronParser, Seconds, Year};
            let parser = CronParser::builder()
                .seconds(Seconds::Disallowed)
                .year(Year::Disallowed)
                .build();
            let cron = parser.parse(expr).ok()?;
            let description = cron.describe();
            Some(format!(
                "{} ({})",
                description.trim_end_matches('.'),
                expr.trim()
            ))
        })
        .unwrap_or_else(|| expr.to_owned())
}

fn describe_cron_fields([minute, hour, day, month, weekday]: [&str; 5]) -> Option<String> {
    let number = |field: &str, min: u8, max: u8| {
        field
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| field.parse::<u8>().ok())
            .flatten()
            .filter(|n| (min..=max).contains(n))
    };
    let step = |field: &str, cycle: u8| match field {
        "*" => Some(1),
        _ => number(field.strip_prefix("*/")?, 1, cycle - 1).filter(|n| cycle.is_multiple_of(*n)),
    };
    let every = |n: u8, unit: &str| match n {
        1 => format!("Every {unit}"),
        _ => format!("Every {n} {unit}s"),
    };
    let unrestricted = (day, month, weekday) == ("*", "*", "*");
    if unrestricted
        && hour == "*"
        && let Some(n) = step(minute, 60)
    {
        return Some(every(n, "minute"));
    }
    let minute = number(minute, 0, 59)?;
    if unrestricted && let Some(n) = step(hour, 24) {
        let at = match minute {
            0 => String::new(),
            _ => format!(" at :{minute:02}"),
        };
        return Some(format!("{}{at}", every(n, "hour")));
    }
    let at = format!("{:02}:{minute:02}", number(hour, 0, 23)?);
    match (day, month, weekday) {
        ("*", "*", "*") => Some(format!("Daily at {at}")),
        ("*", "*", "1-5") => Some(format!("Weekdays at {at}")),
        ("*", "*", weekday) => {
            let weekday = WEEKDAYS[usize::from(number(weekday, 0, 7)? % 7)];
            Some(format!("{weekday} at {at}"))
        }
        (day, "*", "*") => Some(format!("Monthly on day {} at {at}", number(day, 1, 31)?)),
        (day, month, "*") => {
            let (name, days) = MONTHS[usize::from(number(month, 1, 12)? - 1)];
            Some(format!(
                "Yearly on {} {name} at {at}",
                number(day, 1, days)?
            ))
        }
        _ => None,
    }
}

pub(super) fn describe_hours(hours: &crate::model::ActiveHours) -> String {
    let has = |names: &[&str]| {
        hours.days.len() == names.len() && names.iter().all(|n| hours.days.iter().any(|d| d == n))
    };
    let days = if has(&["mon", "tue", "wed", "thu", "fri"]) {
        "Weekdays".to_owned()
    } else if has(&["sat", "sun"]) {
        "Weekends".to_owned()
    } else if hours.days.len() == 7 {
        "Every day".to_owned()
    } else {
        hours
            .days
            .iter()
            .map(|day| {
                let mut chars = day.chars();
                chars
                    .next()
                    .map(|first| first.to_ascii_uppercase().to_string() + chars.as_str())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!("{days} {} to {}", hours.start, hours.end)
}

#[cfg(test)]
mod tests {
    use super::{describe_cron, describe_hours};

    #[test]
    fn common_cron_schedules_read_as_prose() {
        for (cron, readable) in [
            ("0 */6 * * *", "Every 6 hours"),
            ("* * * * *", "Every minute"),
            ("*/1 * * * *", "Every minute"),
            ("*/15 * * * *", "Every 15 minutes"),
            ("0 * * * *", "Every hour"),
            ("0 */1 * * *", "Every hour"),
            ("30 * * * *", "Every hour at :30"),
            ("5 */2 * * *", "Every 2 hours at :05"),
            ("0 0 * * *", "Daily at 00:00"),
            ("59 23 * * *", "Daily at 23:59"),
            ("0 9 * * 1-5", "Weekdays at 09:00"),
            ("0 9 * * 1", "Mondays at 09:00"),
            ("0 9 * * 0", "Sundays at 09:00"),
            ("0 9 * * 7", "Sundays at 09:00"),
            ("0 9 15 * *", "Monthly on day 15 at 09:00"),
            ("0 9 1 1 *", "Yearly on 1 January at 09:00"),
            ("0 9 29 2 *", "Yearly on 29 February at 09:00"),
            ("  0\t*/6  * *   * ", "Every 6 hours"),
        ] {
            assert_eq!(describe_cron(cron), readable, "{cron}");
        }
    }

    #[test]
    fn active_hours_read_as_days_and_a_span() {
        let hours = |days: &[&str], start: &str, end: &str| crate::model::ActiveHours {
            days: days.iter().map(|d| (*d).to_owned()).collect(),
            start: start.to_owned(),
            end: end.to_owned(),
        };
        for (value, readable) in [
            (
                hours(&["mon", "tue", "wed", "thu", "fri"], "08:00", "18:00"),
                "Weekdays 08:00 to 18:00",
            ),
            (
                hours(&["sat", "sun"], "10:00", "12:00"),
                "Weekends 10:00 to 12:00",
            ),
            (
                hours(
                    &["mon", "tue", "wed", "thu", "fri", "sat", "sun"],
                    "07:00",
                    "22:00",
                ),
                "Every day 07:00 to 22:00",
            ),
            (
                hours(&["mon", "wed"], "09:00", "17:30"),
                "Mon, Wed 09:00 to 17:30",
            ),
        ] {
            assert_eq!(describe_hours(&value), readable);
        }
    }

    #[test]
    fn uncommon_schedules_show_a_description_beside_their_cron() {
        for (cron, readable) in [
            ("*/7 * * * *", "At every 7 minutes (*/7 * * * *)"),
            (
                "0 9 * * 1,3",
                "At 09:00, on Monday and Wednesday (0 9 * * 1,3)",
            ),
            ("0 9-17 * * *", "At minute 0, of hour 9-17 (0 9-17 * * *)"),
        ] {
            assert_eq!(describe_cron(cron), readable, "{cron}");
        }
    }

    #[test]
    fn schedules_croner_or_the_server_rejects_keep_their_cron() {
        for cron in [
            "@daily",
            "0 9 * * MON",
            "*/0 * * * *",
            "60 9 * * *",
            "0 24 * * *",
            "0 9 0 * *",
            "0 9 32 * *",
            "0 9 1 13 *",
            "0 9 * * 8",
            "+5 9 * * *",
            "0 9 * *",
            "0 9 * * * *",
            "",
        ] {
            assert_eq!(describe_cron(cron), cron);
        }
    }
}
