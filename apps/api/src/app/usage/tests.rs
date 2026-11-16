use chrono::TimeZone;

use super::*;

fn at(hour: u32, minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 16, hour, minute, 0).unwrap()
}

fn turn(bot: u128, provider: &str, start: DateTime<Utc>, minutes: i64, output: i64) -> Turn {
    Turn {
        id: Uuid::new_v4(),
        bot_id: Uuid::from_u128(bot),
        bot_name: format!("bot {bot}"),
        provider_id: provider.into(),
        model: Some("claude-sonnet-5-5".into()),
        source: Source::Chat,
        label: None,
        routine_id: None,
        started_at: start,
        finished_at: Some(start + Duration::minutes(minutes)),
        input_tokens: 0,
        cached_input_tokens: 0,
        cache_write_tokens: 0,
        output_tokens: output,
    }
}

fn reading(minutes: i64, used: f64, observed: DateTime<Utc>, resets: DateTime<Utc>) -> Reading {
    Reading {
        provider_id: "claude".into(),
        window_minutes: minutes,
        used_percent: used,
        resets_at: resets,
        observed_at: observed,
    }
}

fn find<'a>(report: &'a Report, provider: &str, minutes: i64) -> &'a WindowUsage {
    report
        .providers
        .iter()
        .find(|p| p.provider_id == provider)
        .unwrap()
        .windows
        .iter()
        .find(|w| w.minutes == minutes)
        .unwrap()
}

/// Four 4-minute routine runs from 10:55, hourly, each costing 2% of the
/// 5-hour cap. Claude reports as each run starts, then again once the run
/// moves the rounded total. `outside` is what the owner adds between runs.
fn hourly_runs(outside: f64) -> (Vec<Turn>, Vec<Reading>) {
    let resets = at(15, 0);
    let runs: Vec<Turn> = (0..4)
        .map(|h| Turn {
            source: Source::Routine,
            label: Some("Hourly check".into()),
            routine_id: Some(Uuid::from_u128(9)),
            ..turn(1, "claude", at(10 + h, 55), 4, 1000)
        })
        .collect();
    let mut readings = Vec::new();
    for (k, run) in runs.iter().enumerate() {
        let before = k as f64 * (2.0 + outside);
        readings.push(reading(300, before, run.started_at, resets));
        readings.push(reading(
            300,
            before + 2.0,
            run.started_at + Duration::minutes(3),
            resets,
        ));
    }
    (runs, readings)
}

#[test]
fn an_hourly_routine_is_charged_for_what_it_used() {
    let (runs, readings) = hourly_runs(0.0);

    let report = report(at(14, 0), &runs, &readings);
    let window = find(&report, "claude", 300);

    assert_eq!(window.used_percent, Some(8.0));
    assert_eq!(window.outside_percent, Some(0.0), "idle owner uses nothing");
    let bot = &window.bots[0];
    assert_eq!(bot.percent, Some(8.0));
    assert_eq!(bot.turns, 4);
    assert_eq!(bot.sources[0].source, Source::Routine);
    assert_eq!(bot.sources[0].label.as_deref(), Some("Hourly check"));
    assert_eq!(bot.sources[0].routine_id, Some(Uuid::from_u128(9)));
}

#[test]
fn owner_use_between_short_runs_stays_outside() {
    let (runs, readings) = hourly_runs(12.0);

    let report = report(at(14, 0), &runs, &readings);
    let window = find(&report, "claude", 300);

    assert_eq!(window.used_percent, Some(44.0));
    let bots = window.bots[0].percent.unwrap();
    assert!(
        (6.0..=11.0).contains(&bots),
        "runs keep about 2% each: {bots}"
    );
    let outside = window.outside_percent.unwrap();
    assert!(
        (33.0..=38.0).contains(&outside),
        "owner's 36% stays outside: {outside}"
    );
    let timeline: f64 = window.buckets.iter().map(|b| b.outside).sum();
    assert!((timeline - outside).abs() < 1.0);
    assert_eq!(window.buckets.len(), 20);
}

#[test]
fn work_after_the_latest_reading_waits_instead_of_taking_earlier_use() {
    let resets = at(15, 0);
    let early = turn(1, "claude", at(10, 0), 20, 1_000);
    let late = turn(2, "claude", at(10, 30), 10, 50_000);
    let readings = vec![
        reading(300, 0.0, at(10, 0), resets),
        reading(300, 5.0, at(10, 10), resets),
        reading(300, 10.0, at(10, 19), resets),
    ];

    let report = report(at(10, 45), &[early, late], &readings);
    let window = find(&report, "claude", 300);

    let percent = |bot: u128| {
        let id = Uuid::from_u128(bot);
        window.bots.iter().find(|b| b.bot_id == id).unwrap().percent
    };
    assert_eq!(percent(1), Some(10.0));
    assert_eq!(percent(2), Some(0.0), "not in any reading yet");
    assert_eq!(window.outside_percent, Some(0.0));
}

#[test]
fn without_a_learnable_rate_the_total_stays_unattributed() {
    let resets = at(15, 0);
    let run = turn(1, "claude", at(11, 0), 1, 1_000);
    let readings = vec![reading(300, 20.0, at(11, 0), resets)];

    let report = report(at(12, 0), &[run], &readings);
    let window = find(&report, "claude", 300);

    assert_eq!(window.used_percent, Some(20.0));
    assert_eq!(window.outside_percent, None);
    assert_eq!(window.bots[0].percent, None);
    assert_eq!(window.bots[0].share, 1.0);
}

#[test]
fn the_weekly_window_borrows_the_five_hour_rate() {
    let (runs, mut readings) = hourly_runs(0.0);
    let week_resets = at(15, 0) + Duration::days(3);
    // The weekly total moves only between runs, so it has no clean stretch
    // of its own; overall it rose a quarter as much as the 5-hour total.
    for (k, run) in runs.iter().enumerate() {
        let used = 30.0 + if k >= 2 { 1.0 } else { 0.0 } + if k >= 3 { 1.0 } else { 0.0 };
        readings.push(reading(10080, used, run.started_at, week_resets));
        readings.push(reading(
            10080,
            used,
            run.started_at + Duration::minutes(3),
            week_resets,
        ));
    }

    let report = report(at(14, 0), &runs, &readings);
    let week = find(&report, "claude", 10080);

    assert_eq!(week.used_percent, Some(32.0));
    let bot = week.bots[0].percent.unwrap();
    assert!(
        (1.5..=3.0).contains(&bot),
        "about a quarter of the 5-hour 8%: {bot}"
    );
    assert!(week.buckets.is_empty(), "no timeline for the week");
}

#[test]
fn providers_without_limits_split_by_share_and_count_turns_crossing_the_start() {
    let crossing = turn(1, "grok", at(7, 50), 20, 2_000);
    let inside = turn(2, "grok", at(12, 30), 1, 2_000);

    let report = report(at(13, 0), &[crossing, inside], &[]);
    let grok = &report.providers[0];

    let lengths: Vec<i64> = grok.windows.iter().map(|w| w.minutes).collect();
    assert_eq!(lengths, [300, 10080]);
    let window = &grok.windows[0];
    assert_eq!(window.used_percent, None);
    assert_eq!(window.outside_percent, None);
    assert_eq!(window.bots[0].percent, None);
    assert_eq!(window.bots[0].bot_id, Uuid::from_u128(2));
    assert_eq!(window.bots[0].share, 0.667);
    assert_eq!(
        window.bots[1].share, 0.333,
        "half its span is in the window"
    );
    let shares: f64 = window
        .buckets
        .iter()
        .flat_map(|b| &b.bots)
        .map(|b| b.amount)
        .sum();
    assert!((shares - 1.0).abs() < 0.05);
}

#[test]
fn a_window_that_reset_after_its_last_reading_has_no_known_total() {
    let run = turn(1, "claude", at(9, 0), 1, 1_000);
    let readings = vec![reading(300, 30.0, at(9, 0), at(10, 0))];

    let report = report(at(13, 0), &[run], &readings);
    let window = find(&report, "claude", 300);

    assert_eq!(window.used_percent, None);
    assert_eq!(window.resets_at, None);
    assert_eq!(window.starts_at, at(8, 0));
}

#[test]
fn a_bot_that_switched_provider_shows_under_both() {
    let on_claude = turn(1, "claude", at(11, 0), 10, 1_000);
    let on_codex = turn(1, "codex", at(12, 0), 10, 1_000);
    let readings = vec![reading(300, 5.0, at(11, 5), at(15, 0))];

    let report = report(at(13, 0), &[on_claude, on_codex], &readings);

    assert_eq!(
        find(&report, "claude", 300).bots[0].bot_id,
        Uuid::from_u128(1)
    );
    assert_eq!(
        find(&report, "codex", 300).bots[0].bot_id,
        Uuid::from_u128(1)
    );
}

#[test]
fn opus_turns_weigh_more_than_haiku_turns_of_the_same_size() {
    let opus = Turn {
        model: Some("claude-opus-5-5".into()),
        ..turn(1, "grok", at(12, 0), 1, 1_000)
    };
    let haiku = Turn {
        model: Some("claude-haiku-5-5".into()),
        ..turn(2, "grok", at(12, 0), 1, 1_000)
    };
    assert!(opus.weight() > 10.0 * haiku.weight());
}
