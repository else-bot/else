//! Per-bot usage against each provider's subscription windows.
//!
//! Providers report how much of a rolling window's cap the owner's account
//! has used, from every client, but only now and then: Claude once per
//! process and again when the rounded percentage moves, Codex during turns.
//! A turn's own cost often lands in a later reading, so readings alone can't
//! price a turn. Instead each window length learns a rate (percent of the cap
//! per cost-weighted token) from the stretches between readings that else.bot
//! turns mostly filled, prices every bot's turns at that rate, and leaves the
//! rest of the reported total to use outside else.bot. Until a rate is known
//! the total stays unattributed rather than guessed.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use uuid::Uuid;

use super::providers::LimitWindow;

/// Providers without a current limit reading use a rolling week.
const DEFAULT_WINDOW: i64 = LimitWindow::WEEK;
/// A stretch between two readings calibrates the rate only when bot turns
/// ran for at least this share of it.
const CLEAN_COVERAGE: f64 = 0.5;
/// Percentage points the clean stretches must add up to before their rate is
/// trusted; readings move in whole percents.
const CLEAN_SIGNAL: f64 = 2.0;
/// Readings with resets this close apart belong to the same window.
const SAME_RESET: Duration = Duration::seconds(90);

/// What started a turn, worked out from the tables that record each kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum Source {
    Chat,
    Routine,
    Delegated,
    Webhook,
    Mail,
    Social,
}

/// One turn that reported tokens, as the provider and model that ran it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Turn {
    pub bot_id: Uuid,
    pub bot_name: String,
    pub provider_id: String,
    pub model: Option<String>,
    pub source: Source,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub cache_write_tokens: i64,
    pub output_tokens: i64,
}

/// API list prices in USD per million tokens, used only as relative
/// weights. Long-context and batch rates are ignored; a cache write is
/// priced at the 5-minute rate.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Price {
    input: f64,
    cache_write: f64,
    cache_read: f64,
    output: f64,
}

const fn price(input: f64, cache_write: f64, cache_read: f64, output: f64) -> Price {
    Price {
        input,
        cache_write,
        cache_read,
        output,
    }
}

const SONNET: Price = price(2.0, 2.5, 0.1, 10.0);
const GPT_SOL: Price = price(2.0, 2.0, 0.1, 10.0);
const GROK: Price = price(2.0, 2.0, 0.5, 6.0);

/// Matched in order against the lowercased model id, so a more specific
/// name comes before the family it belongs to. Prices as of November 2026.
const PRICES: &[(&str, Price)] = &[
    ("fable-5-1", price(10.0, 12.5, 0.25, 50.0)),
    ("fable", price(8.0, 10.0, 0.2, 40.0)),
    ("opus", price(4.0, 5.0, 0.2, 20.0)),
    ("sonnet", SONNET),
    ("haiku", price(0.1, 0.125, 0.01, 0.5)),
    ("terra", price(1.0, 1.0, 0.05, 5.0)),
    ("luna", price(0.1, 0.1, 0.01, 0.5)),
    ("sol", GPT_SOL),
    ("grok-build", price(1.0, 1.0, 1.0, 2.0)),
    ("grok", GROK),
];

impl Price {
    /// The model's price, or its provider's mid model when the model is
    /// missing or newer than this table.
    fn of(provider: &str, model: Option<&str>) -> Self {
        let model = model.unwrap_or_default().to_ascii_lowercase();
        PRICES
            .iter()
            .find(|(name, _)| model.contains(name))
            .map(|(_, price)| *price)
            .unwrap_or(match provider {
                "codex" => GPT_SOL,
                "grok" => GROK,
                _ => SONNET,
            })
    }
}

impl Turn {
    /// Relative cost: each token kind at the model's API list price. Only
    /// ratios between turns of one provider matter, so a subscription that
    /// counts differently from API prices still splits close to right.
    fn weight(&self) -> f64 {
        let price = Price::of(&self.provider_id, self.model.as_deref());
        let cached = self.cached_input_tokens.max(0) as f64;
        let written = self.cache_write_tokens.max(0) as f64;
        let fresh =
            (self.input_tokens - self.cached_input_tokens - self.cache_write_tokens).max(0) as f64;
        fresh * price.input
            + written * price.cache_write
            + cached * price.cache_read
            + self.output_tokens.max(0) as f64 * price.output
    }
}

/// A turn with its weight and span worked out once per report.
struct Weighed<'a> {
    turn: &'a Turn,
    weight: f64,
    span: (DateTime<Utc>, DateTime<Utc>),
}

impl<'a> Weighed<'a> {
    fn new(turn: &'a Turn, now: DateTime<Utc>) -> Self {
        Self {
            turn,
            weight: turn.weight(),
            span: (
                turn.started_at,
                turn.finished_at.unwrap_or(now).max(turn.started_at),
            ),
        }
    }

    /// The weight of the part of the turn inside `from..to`, spread evenly
    /// over its span.
    fn within(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> f64 {
        self.weight * overlap(self.span, (from, to))
    }
}

/// One limit reading a provider reported during a turn.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Reading {
    /// Shared by the windows the provider reported together.
    pub report_id: Uuid,
    pub provider_id: String,
    pub window_minutes: i64,
    pub used_percent: f64,
    pub resets_at: DateTime<Utc>,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub generated_at: DateTime<Utc>,
    pub providers: Vec<ProviderUsage>,
}

#[derive(Debug, Serialize)]
pub struct ProviderUsage {
    pub provider_id: String,
    pub windows: Vec<WindowUsage>,
}

#[derive(Debug, Serialize)]
pub struct WindowUsage {
    pub minutes: i64,
    pub attribution: Attribution,
    pub bots: Vec<BotUsage>,
}

/// The provider's exact total and any learned attribution of that total.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Attribution {
    /// `outside` is the estimated part of `used` from outside else.bot.
    Priced {
        used: f64,
        outside: f64,
        resets_at: DateTime<Utc>,
        observed_at: DateTime<Utc>,
    },
    /// The provider reported `used`, but the cost of a turn is not known yet.
    Unattributed {
        used: f64,
        resets_at: DateTime<Utc>,
        observed_at: DateTime<Utc>,
    },
    /// No current total, either unreported or past its reset.
    Unreported,
}

#[derive(Debug, Serialize)]
pub struct BotUsage {
    pub bot_id: Uuid,
    pub name: String,
    /// Fraction of else.bot's weighted use in the window.
    pub share: f64,
    pub sources: Vec<SourceUsage>,
}

#[derive(Debug, Serialize)]
pub struct SourceUsage {
    pub source: Source,
    /// Fraction of the bot's own weighted use.
    pub share: f64,
}

/// Builds the report from turns and readings of the last week or so.
pub fn report(now: DateTime<Utc>, turns: &[Turn], readings: &[Reading]) -> Report {
    let providers: BTreeSet<&str> = turns
        .iter()
        .map(|t| t.provider_id.as_str())
        .chain(readings.iter().map(|r| r.provider_id.as_str()))
        .collect();
    let providers = providers
        .into_iter()
        .map(|provider| {
            let turns: Vec<Weighed> = turns
                .iter()
                .filter(|t| t.provider_id == provider)
                .map(|t| Weighed::new(t, now))
                .collect();
            let mut by_length: BTreeMap<i64, Vec<&Reading>> = BTreeMap::new();
            for reading in readings.iter().filter(|r| r.provider_id == provider) {
                by_length
                    .entry(reading.window_minutes)
                    .or_default()
                    .push(reading);
            }
            let windows: Vec<WindowUsage> = if by_length.is_empty() {
                vec![window(now, DEFAULT_WINDOW, &turns, &[], None)]
            } else {
                let series: BTreeMap<i64, Vec<Group>> = by_length
                    .into_iter()
                    .map(|(minutes, readings)| {
                        let groups = by_reset(readings)
                            .into_iter()
                            .map(|readings| Group {
                                stretches: stretches(minutes, &readings, &turns),
                                readings,
                            })
                            .collect();
                        (minutes, groups)
                    })
                    .collect();
                let rates = rates(&series);
                series
                    .iter()
                    .map(|(&minutes, windows)| {
                        window(now, minutes, &turns, windows, rates.get(&minutes).copied())
                    })
                    .collect()
            };
            let windows = if windows
                .iter()
                .all(|window| window.attribution == Attribution::Unreported)
            {
                vec![window(now, DEFAULT_WINDOW, &turns, &[], None)]
            } else {
                windows
            };
            ProviderUsage {
                provider_id: provider.to_owned(),
                windows,
            }
        })
        .collect();
    Report {
        generated_at: now,
        providers,
    }
}

/// Splits one window length's readings into the windows they describe,
/// each sorted by time.
fn by_reset(mut readings: Vec<&Reading>) -> Vec<Vec<&Reading>> {
    readings.sort_by_key(|r| r.observed_at);
    let mut windows: Vec<Vec<&Reading>> = Vec::new();
    for reading in readings {
        match windows
            .iter_mut()
            .find(|w| (w[0].resets_at - reading.resets_at).abs() <= SAME_RESET)
        {
            Some(window) => window.push(reading),
            None => windows.push(vec![reading]),
        }
    }
    windows
}

/// The readings of one window, and the stretches between them.
struct Group<'a> {
    readings: Vec<&'a Reading>,
    stretches: Vec<Stretch>,
}

/// A stretch between two readings of one window, starting from zero at the
/// window's start.
struct Stretch {
    rise: f64,
    /// Weight of bot work done inside the stretch.
    weight: f64,
    /// Share of the stretch at least one bot turn was running.
    coverage: f64,
}

/// The window's stretches, measured from the start its latest reset gives,
/// so they line up with the window the report shows.
fn stretches(minutes: i64, window: &[&Reading], turns: &[Weighed]) -> Vec<Stretch> {
    let resets_at = window.last().expect("windows are never empty").resets_at;
    let mut previous = (resets_at - Duration::minutes(minutes), 0.0);
    let mut stretches = Vec::new();
    for reading in window {
        let (from, to) = (previous.0, reading.observed_at);
        let rise = (reading.used_percent - previous.1).max(0.0);
        previous = (to, reading.used_percent);
        if to <= from {
            continue;
        }
        let (weight, covered) = activity(turns, from, to);
        stretches.push(Stretch {
            rise,
            weight,
            coverage: covered.num_milliseconds() as f64 / (to - from).num_milliseconds() as f64,
        });
    }
    stretches
}

/// Percent of the cap per weight unit, by window length. A length learns its
/// rate from stretches that bot turns mostly filled, where little else can
/// have moved the total; one without enough of them borrows the shortest
/// calibrated length's rate, scaled by how far the two totals rose between
/// the same pairs of readings. A length with neither has no rate, so its use
/// stays unattributed instead of being guessed.
fn rates(series: &BTreeMap<i64, Vec<Group>>) -> BTreeMap<i64, f64> {
    let mut learned = BTreeMap::new();
    for (&minutes, groups) in series {
        let (mut clean_rise, mut clean_weight) = (0.0, 0.0);
        for group in groups {
            for stretch in &group.stretches {
                if stretch.weight > 0.0 && stretch.coverage >= CLEAN_COVERAGE {
                    clean_rise += stretch.rise;
                    clean_weight += stretch.weight;
                }
            }
        }
        if clean_rise >= CLEAN_SIGNAL && clean_weight > 0.0 {
            learned.insert(minutes, clean_rise / clean_weight);
        }
    }
    let Some((&base, &base_rate)) = learned.iter().next() else {
        return learned;
    };
    for (&minutes, groups) in series {
        if learned.contains_key(&minutes) {
            continue;
        }
        if let Some(ratio) = rise_ratio(&series[&base], groups) {
            learned.insert(minutes, base_rate * ratio);
        }
    }
    learned
}

/// How far `other` rose for each point `base` rose, over pairs of readings
/// both lengths took in the same reports with no reset in either between
/// them. `None` until `base` rose enough to tell.
fn rise_ratio(base: &[Group], other: &[Group]) -> Option<f64> {
    let partners: HashMap<Uuid, (usize, f64)> = base
        .iter()
        .enumerate()
        .flat_map(|(group, window)| {
            window
                .readings
                .iter()
                .map(move |b| (b.report_id, (group, b.used_percent)))
        })
        .collect();
    let partner = |reading: &Reading| partners.get(&reading.report_id).copied();
    let (mut base_rise, mut other_rise) = (0.0, 0.0);
    for window in other {
        for pair in window.readings.windows(2) {
            let (Some((g0, b0)), Some((g1, b1))) = (partner(pair[0]), partner(pair[1])) else {
                continue;
            };
            if g0 == g1 {
                base_rise += (b1 - b0).max(0.0);
                other_rise += (pair[1].used_percent - pair[0].used_percent).max(0.0);
            }
        }
    }
    (base_rise >= CLEAN_SIGNAL).then(|| other_rise / base_rise)
}

fn window<'a>(
    now: DateTime<Utc>,
    minutes: i64,
    turns: &[Weighed],
    groups: &'a [Group<'a>],
    rate: Option<f64>,
) -> WindowUsage {
    let length = Duration::minutes(minutes);
    let current = groups
        .iter()
        .filter(|g| g.readings[0].resets_at > now)
        .max_by_key(|g| g.readings[0].resets_at);
    let latest = current.map(|g| *g.readings.last().expect("windows are never empty"));
    let starts_at = latest.map_or(now - length, |r| r.resets_at - length);
    let members: Vec<&Weighed> = turns
        .iter()
        .filter(|t| overlap(t.span, (starts_at, now)) > 0.0)
        .collect();
    let in_window = |turn: &Weighed, from: DateTime<Utc>, to: DateTime<Utc>| {
        turn.within(from.max(starts_at), to.min(now))
    };
    let weight_until =
        |until| -> f64 { members.iter().map(|t| in_window(t, starts_at, until)).sum() };
    let attribution = match (latest, rate) {
        (Some(latest), Some(rate)) => {
            // Bots never take more than the reported total.
            let priced = weight_until(latest.observed_at);
            let unit = if priced > 0.0 {
                rate.min(latest.used_percent / priced)
            } else {
                rate
            };
            Attribution::Priced {
                used: round(latest.used_percent),
                outside: round((latest.used_percent - priced * unit).max(0.0)),
                resets_at: latest.resets_at,
                observed_at: latest.observed_at,
            }
        }
        (Some(latest), None) => Attribution::Unattributed {
            used: round(latest.used_percent),
            resets_at: latest.resets_at,
            observed_at: latest.observed_at,
        },
        _ => Attribution::Unreported,
    };
    WindowUsage {
        minutes,
        attribution,
        bots: bots(&members, |t| in_window(t, starts_at, now)),
    }
}

/// The weight of the turns' work inside `from..to`, and how long at least
/// one of them ran there.
fn activity(turns: &[Weighed], from: DateTime<Utc>, to: DateTime<Utc>) -> (f64, Duration) {
    let mut weight = 0.0;
    let mut spans = Vec::new();
    for turn in turns {
        let inside = turn.within(from, to);
        if inside > 0.0 {
            weight += inside;
            spans.push((turn.span.0.max(from), turn.span.1.min(to)));
        }
    }
    spans.sort();
    let mut covered = Duration::zero();
    let mut reach = from;
    for (start, end) in spans {
        let start = start.max(reach);
        if end > start {
            covered += end - start;
            reach = end;
        }
    }
    (weight, covered)
}

/// The share of `span` that falls in `range`. An instant counts whole when
/// it lies in the range.
fn overlap(
    (start, end): (DateTime<Utc>, DateTime<Utc>),
    (from, to): (DateTime<Utc>, DateTime<Utc>),
) -> f64 {
    if end <= start {
        return if start > from && start <= to {
            1.0
        } else {
            0.0
        };
    }
    let inside = end.min(to) - start.max(from);
    if inside <= Duration::zero() {
        return 0.0;
    }
    inside.num_milliseconds() as f64 / (end - start).num_milliseconds() as f64
}

/// Groups the window's weighted work by bot and source kind.
fn bots(turns: &[&Weighed], weight: impl Fn(&Weighed) -> f64) -> Vec<BotUsage> {
    let total: f64 = turns.iter().map(|t| weight(t)).sum();
    let mut by_bot: BTreeMap<Uuid, Vec<&Weighed>> = BTreeMap::new();
    for turn in turns {
        by_bot.entry(turn.turn.bot_id).or_default().push(turn);
    }
    let mut bots: Vec<BotUsage> = by_bot
        .into_values()
        .map(|turns| {
            let bot_weight: f64 = turns.iter().map(|t| weight(t)).sum();
            let mut sources: BTreeMap<Source, f64> = BTreeMap::new();
            for turn in &turns {
                *sources.entry(turn.turn.source).or_default() += weight(turn);
            }
            let mut sources: Vec<SourceUsage> = sources
                .into_iter()
                .map(|(source, weight)| SourceUsage {
                    source,
                    share: ratio(weight, bot_weight),
                })
                .collect();
            sources.sort_by(|a, b| b.share.total_cmp(&a.share));
            BotUsage {
                bot_id: turns[0].turn.bot_id,
                name: turns[0].turn.bot_name.clone(),
                share: ratio(bot_weight, total),
                sources,
            }
        })
        .collect();
    bots.sort_by(|a, b| b.share.total_cmp(&a.share));
    bots
}

fn ratio(part: f64, whole: f64) -> f64 {
    if whole > 0.0 { part / whole } else { 0.0 }
}

fn round(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests;
