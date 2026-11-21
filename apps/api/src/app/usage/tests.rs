use chrono::{DateTime, Duration, TimeZone, Utc};
use else_wire::usage::{Attribution, BotUsage, UsageReport, UsageSourceKind, UsageWindow};
use uuid::Uuid;

use super::{Reading, Source, Turn, report};

fn at(hour: u32, minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 16, hour, minute, 0).unwrap()
}

fn turn(bot: u128, provider: &str, start: DateTime<Utc>, minutes: i64, output: i64) -> Turn {
    Turn {
        bot_id: Uuid::from_u128(bot),
        bot_name: format!("bot {bot}"),
        provider_id: provider.into(),
        model: Some("claude-sonnet-5-5".into()),
        source: Source::Chat,
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
        // Readings taken at the same instant came from one report.
        report_id: Uuid::from_u128(observed.timestamp_millis() as u128),
        provider_id: "claude".into(),
        window_minutes: minutes,
        used_percent: used,
        resets_at: resets,
        observed_at: observed,
    }
}

fn used(window: &UsageWindow) -> Option<f64> {
    match window.attribution {
        Attribution::Priced { used, .. } => Some(used),
        _ => None,
    }
}

fn outside(window: &UsageWindow) -> Option<f64> {
    match window.attribution {
        Attribution::Priced { outside, .. } => Some(outside),
        _ => None,
    }
}

fn find<'a>(report: &'a UsageReport, provider: &str, minutes: i64) -> &'a UsageWindow {
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

    assert_eq!(used(window), Some(8.0));
    assert_eq!(outside(window), Some(0.0), "idle owner uses nothing");
    let bot = &window.bots[0];
    assert_eq!(bot.share, 1.0);
    assert_eq!(used(window).unwrap() - outside(window).unwrap(), 8.0);
    assert_eq!(bot.sources[0].source, UsageSourceKind::Routine);
    assert_eq!(bot.sources[0].share, 1.0);
}

#[test]
fn owner_use_between_short_runs_stays_outside() {
    let (runs, readings) = hourly_runs(12.0);

    let report = report(at(14, 0), &runs, &readings);
    let window = find(&report, "claude", 300);

    assert_eq!(used(window), Some(44.0));
    let bots = used(window).unwrap() - outside(window).unwrap();
    assert!(
        (6.0..=11.0).contains(&bots),
        "runs keep about 2% each: {bots}"
    );
    let outside = outside(window).unwrap();
    assert!(
        (33.0..=38.0).contains(&outside),
        "owner's 36% stays outside: {outside}"
    );
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

    assert_eq!(used(window), Some(10.0));
    assert_eq!(window.bots[0].bot_id, Uuid::from_u128(2));
    assert!(
        window.bots[0].share > 0.98,
        "shares include work since the reading"
    );
    assert_eq!(outside(window), Some(0.0));
}

#[test]
fn without_a_learnable_rate_the_total_stays_unattributed() {
    let resets = at(15, 0);
    let run = turn(1, "claude", at(11, 0), 1, 1_000);
    let readings = vec![reading(300, 20.0, at(11, 0), resets)];

    let report = report(at(12, 0), &[run], &readings);
    let window = find(&report, "claude", 300);

    assert!(matches!(
        window.attribution,
        Attribution::Unattributed { used, .. } if used == 20.0
    ));
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

    assert_eq!(used(week), Some(32.0));
    let bot = used(week).unwrap() - outside(week).unwrap();
    assert!(
        (1.5..=3.0).contains(&bot),
        "about a quarter of the 5-hour 8%: {bot}"
    );
}

#[test]
fn providers_without_limits_use_a_week_and_include_work_crossing_the_start() {
    let crossing = turn(
        1,
        "grok",
        at(13, 0) - Duration::days(7) - Duration::minutes(10),
        20,
        2_000,
    );
    let inside = turn(2, "grok", at(12, 30), 1, 2_000);

    let report = report(at(13, 0), &[crossing, inside], &[]);
    let grok = &report.providers[0];

    let lengths: Vec<i64> = grok.windows.iter().map(|w| w.minutes).collect();
    assert_eq!(lengths, [10080]);
    let window = &grok.windows[0];
    assert_eq!(window.attribution, Attribution::Unreported);
    assert_eq!(window.bots[0].bot_id, Uuid::from_u128(2));
    assert!((window.bots[0].share - 2.0 / 3.0).abs() < 1e-9);
    assert!(
        (window.bots[1].share - 1.0 / 3.0).abs() < 1e-9,
        "half its span is in the window"
    );
    assert_eq!(window.bots.iter().map(|bot| bot.share).sum::<f64>(), 1.0);
}

#[test]
fn a_window_that_reset_after_its_last_reading_has_no_known_total() {
    let run = turn(1, "claude", at(9, 0), 1, 1_000);
    let readings = vec![reading(300, 30.0, at(9, 0), at(10, 0))];

    let report = report(at(13, 0), &[run], &readings);
    let window = find(&report, "claude", 10080);

    assert_eq!(window.attribution, Attribution::Unreported);
    assert!(
        window
            .bots
            .iter()
            .any(|bot| bot.bot_id == Uuid::from_u128(1))
    );
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
        find(&report, "codex", 10080).bots[0].bot_id,
        Uuid::from_u128(1)
    );
}

#[test]
fn turns_weigh_by_their_model_price() {
    let sized = |provider: &str, model: Option<&str>| Turn {
        model: model.map(Into::into),
        ..turn(1, provider, at(12, 0), 1, 1_000)
    };
    let weight = |provider, model| sized(provider, model).weight();
    let opus = weight("claude", Some("claude-opus-5-5"));
    let haiku = weight("claude", Some("claude-haiku-5-5"));
    assert_eq!(opus, 40.0 * haiku);
    assert_eq!(
        weight("claude", Some("opus")),
        opus,
        "CLI aliases match too"
    );
    assert_eq!(
        weight("codex", Some("gpt-6.1-sol")),
        2.0 * weight("codex", Some("gpt-6.1-terra"))
    );
    assert!(
        weight("claude", Some("claude-fable-5-1")) > weight("claude", Some("claude-fable-5-2"))
    );
    // A model newer than the table costs like its provider's mid model.
    assert_eq!(
        weight("codex", Some("gpt-7-nova")),
        weight("codex", Some("gpt-6.1-sol"))
    );
    assert_eq!(
        weight("claude", None),
        weight("claude", Some("claude-sonnet-5-5"))
    );
}

#[test]
fn cached_input_costs_a_fraction_of_fresh_input() {
    let input = |cached: i64, written: i64| Turn {
        input_tokens: 1_000,
        cached_input_tokens: cached,
        cache_write_tokens: written,
        ..turn(1, "claude", at(12, 0), 1, 0)
    };
    let fresh = input(0, 0).weight();
    assert_eq!(input(1_000, 0).weight(), fresh / 20.0);
    assert_eq!(input(0, 1_000).weight(), fresh * 1.25);
}

#[test]
fn weekly_borrowing_pairs_readings_by_report_not_by_time() {
    let second = |s: u32| Utc.with_ymd_and_hms(2026, 11, 16, 10, 0, s).unwrap();
    let runs = [turn(1, "claude", second(0) - Duration::seconds(1), 1, 1000)];
    // Two reports three seconds apart: each weekly reading must pair with
    // the 5-hour reading of its own report, not the nearest one in time.
    let readings = vec![
        reading(300, 0.0, second(0), at(14, 0)),
        reading(300, 4.0, second(3), at(14, 0)),
        reading(10080, 0.0, second(0), at(14, 0) + Duration::days(3)),
        reading(10080, 1.0, second(3), at(14, 0) + Duration::days(3)),
    ];

    let report = report(second(10), &runs, &readings);

    let week = find(&report, "claude", 10080);
    assert_eq!(used(week).unwrap() - outside(week).unwrap(), 1.0);
}

#[test]
fn weekly_borrowing_ignores_rises_across_a_five_hour_reset() {
    let day = |d: u32, h: u32, m: u32| Utc.with_ymd_and_hms(2026, 11, d, h, m, 0).unwrap();
    let runs = [
        turn(1, "claude", day(15, 10, 0), 4, 1000),
        turn(1, "claude", day(16, 10, 0), 4, 1000),
    ];
    let week = day(19, 0, 0);
    let readings = vec![
        reading(300, 0.0, day(15, 10, 0), day(15, 14, 0)),
        reading(300, 2.0, day(15, 10, 3), day(15, 14, 0)),
        // The owner used 38% of a fresh window before the next run.
        reading(300, 40.0, day(16, 10, 0), day(16, 14, 0)),
        reading(300, 42.0, day(16, 10, 3), day(16, 14, 0)),
        reading(10080, 0.0, day(15, 10, 0), week),
        reading(10080, 0.2, day(15, 10, 3), week),
        reading(10080, 4.2, day(16, 10, 0), week),
        reading(10080, 4.4, day(16, 10, 3), week),
    ];

    let report = report(day(16, 10, 30), &runs, &readings);
    let weekly = find(&report, "claude", 10080);

    let bots = used(weekly).unwrap() - outside(weekly).unwrap();
    assert!(
        (0.3..=0.7).contains(&bots),
        "two runs at about 0.2% each: {bots}"
    );
    let outside = outside(weekly).unwrap();
    assert!(outside >= 3.5, "the owner's use stays outside: {outside}");
}

#[test]
fn shares_and_sources_follow_the_window_without_using_cap_percent() {
    let mut old_chat = turn(1, "claude", at(12, 0) - Duration::days(1), 1, 1_000);
    old_chat.model = Some("claude-opus-5-5".into());
    let recent = Turn {
        source: Source::Routine,
        ..turn(1, "claude", at(12, 0), 1, 1_000)
    };
    let other = turn(2, "claude", at(12, 0), 1, 1_000);
    let readings = [
        reading(300, 16.0, at(12, 0), at(15, 0)),
        reading(10080, 18.0, at(12, 0), at(15, 0) + Duration::days(4)),
    ];
    let report = report(at(13, 0), &[old_chat, recent, other], &readings);
    let five = find(&report, "claude", 300);
    let week = find(&report, "claude", 10080);
    assert!(matches!(
        five.attribution,
        Attribution::Unattributed { used: 16.0, .. }
    ));
    assert!(matches!(
        week.attribution,
        Attribution::Unattributed { used: 18.0, .. }
    ));
    fn bot(window: &UsageWindow) -> &BotUsage {
        window
            .bots
            .iter()
            .find(|bot| bot.bot_id == Uuid::from_u128(1))
            .unwrap()
    }
    assert_eq!(bot(five).share, 0.5);
    assert_eq!(bot(five).sources[0].source, UsageSourceKind::Routine);
    assert_eq!(bot(week).share, 0.75);
    assert_eq!(bot(week).sources[0].source, UsageSourceKind::Chat);
    assert!((bot(week).sources[0].share - 2.0 / 3.0).abs() < 1e-9);
}

#[test]
fn small_shares_keep_their_weight_and_sources_combine_by_kind() {
    let runs = [
        turn(1, "grok", at(12, 0), 1, 1),
        turn(2, "grok", at(12, 0), 1, 10_000),
        Turn {
            source: Source::Routine,
            ..turn(2, "grok", at(12, 2), 1, 100)
        },
        Turn {
            source: Source::Routine,
            ..turn(2, "grok", at(12, 4), 1, 100)
        },
    ];
    let report = report(at(13, 0), &runs, &[]);
    let week = find(&report, "grok", 10080);
    assert!(week.bots[1].share > 0.0 && week.bots[1].share < 0.0005);
    let sources = &week.bots[0].sources;
    assert_eq!(sources.len(), 2);
    assert_eq!(sources[1].source, UsageSourceKind::Routine);
    assert!((sources[1].share - 200.0 / 10_200.0).abs() < 1e-9);
}
