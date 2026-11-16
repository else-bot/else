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

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use uuid::Uuid;

/// The windows a provider reports nothing for are shown as rolling windows
/// of these lengths, by share of else.bot's own use.
const DEFAULT_WINDOWS: [i64; 2] = [5 * 60, 7 * 24 * 60];
/// Windows up to this long get a timeline of [`BUCKETS`] columns.
const TIMELINE_MAX_MINUTES: i64 = 5 * 60;
const BUCKETS: i32 = 20;
/// A stretch between two readings calibrates the rate only when bot turns
/// ran for at least this share of it.
const CLEAN_COVERAGE: f64 = 0.5;
/// Percentage points the clean stretches must add up to before their rate is
/// trusted; readings move in whole percents.
const CLEAN_SIGNAL: f64 = 2.0;
/// Readings with resets this close apart belong to the same window.
const SAME_RESET: Duration = Duration::seconds(90);
/// Readings of two window lengths this close together came from one report.
const PAIRED: Duration = Duration::seconds(5);

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
#[derive(Debug, Clone)]
pub struct Turn {
    pub id: Uuid,
    pub bot_id: Uuid,
    pub bot_name: String,
    pub provider_id: String,
    pub model: Option<String>,
    pub source: Source,
    /// The routine, webhook, or delegating bot behind the turn.
    pub label: Option<String>,
    pub routine_id: Option<Uuid>,
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

    fn span(&self, now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
        let end = self.finished_at.unwrap_or(now).max(self.started_at);
        (self.started_at, end)
    }
}

/// One limit reading a provider reported during a turn.
#[derive(Debug, Clone)]
pub struct Reading {
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
    pub starts_at: DateTime<Utc>,
    pub resets_at: Option<DateTime<Utc>>,
    pub observed_at: Option<DateTime<Utc>>,
    pub used_percent: Option<f64>,
    pub outside_percent: Option<f64>,
    pub bots: Vec<BotUsage>,
    pub buckets: Vec<Bucket>,
}

#[derive(Debug, Serialize)]
pub struct BotUsage {
    pub bot_id: Uuid,
    pub name: String,
    pub percent: Option<f64>,
    pub share: f64,
    pub turns: u32,
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub cache_write_tokens: i64,
    pub output_tokens: i64,
    pub models: Vec<String>,
    pub sources: Vec<SourceUsage>,
}

#[derive(Debug, Serialize)]
pub struct SourceUsage {
    pub source: Source,
    pub label: Option<String>,
    pub routine_id: Option<Uuid>,
    pub turns: u32,
    pub percent: Option<f64>,
    pub share: f64,
}

#[derive(Debug, Serialize)]
pub struct Bucket {
    pub starts_at: DateTime<Utc>,
    pub bots: Vec<BucketBot>,
    pub outside: f64,
}

#[derive(Debug, Serialize)]
pub struct BucketBot {
    pub bot_id: Uuid,
    pub amount: f64,
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
            let turns: Vec<&Turn> = turns.iter().filter(|t| t.provider_id == provider).collect();
            let mut by_length: BTreeMap<i64, Vec<&Reading>> = BTreeMap::new();
            for reading in readings.iter().filter(|r| r.provider_id == provider) {
                by_length
                    .entry(reading.window_minutes)
                    .or_default()
                    .push(reading);
            }
            let windows = if by_length.is_empty() {
                DEFAULT_WINDOWS
                    .iter()
                    .map(|&minutes| window(now, minutes, &turns, &[], None))
                    .collect()
            } else {
                let series: BTreeMap<i64, Vec<Vec<&Reading>>> = by_length
                    .into_iter()
                    .map(|(minutes, readings)| (minutes, by_reset(readings)))
                    .collect();
                let rates = rates(now, &series, &turns);
                series
                    .iter()
                    .map(|(&minutes, windows)| {
                        window(now, minutes, &turns, windows, rates.get(&minutes).copied())
                    })
                    .collect()
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

/// A stretch between two readings of one window, starting from zero at the
/// window's start.
struct Stretch {
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    rise: f64,
    /// Weight of bot work done inside the stretch.
    weight: f64,
    /// Share of the stretch at least one bot turn was running.
    coverage: f64,
}

/// The window's stretches, measured from the start its latest reset gives,
/// so they line up with the window the report shows.
fn stretches(
    now: DateTime<Utc>,
    minutes: i64,
    window: &[&Reading],
    turns: &[&Turn],
) -> Vec<Stretch> {
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
        let (weight, covered) = activity(now, turns, from, to);
        stretches.push(Stretch {
            from,
            to,
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
fn rates(
    now: DateTime<Utc>,
    series: &BTreeMap<i64, Vec<Vec<&Reading>>>,
    turns: &[&Turn],
) -> BTreeMap<i64, f64> {
    let mut learned = BTreeMap::new();
    for (&minutes, windows) in series {
        let (mut clean_rise, mut clean_weight) = (0.0, 0.0);
        for window in windows {
            for stretch in stretches(now, minutes, window, turns) {
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
    for (&minutes, windows) in series {
        if learned.contains_key(&minutes) {
            continue;
        }
        if let Some(ratio) = rise_ratio(&series[&base], windows) {
            learned.insert(minutes, base_rate * ratio);
        }
    }
    learned
}

/// How far `other` rose for each point `base` rose, over pairs of readings
/// both lengths took together (a provider reports its windows at once) with
/// no reset in either between them. `None` until `base` rose enough to tell.
fn rise_ratio(base: &[Vec<&Reading>], other: &[Vec<&Reading>]) -> Option<f64> {
    let partner = |reading: &Reading| {
        base.iter().enumerate().find_map(|(group, window)| {
            window
                .iter()
                .find(|b| (b.observed_at - reading.observed_at).abs() <= PAIRED)
                .map(|b| (group, b.used_percent))
        })
    };
    let (mut base_rise, mut other_rise) = (0.0, 0.0);
    for window in other {
        for pair in window.windows(2) {
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

/// How a window prices else.bot's turns.
enum Pricing<'a> {
    /// The provider reported the window's total and the rate is known.
    /// Turns count up to the latest reading, so work done after it waits
    /// for the reading that includes it.
    Priced {
        used: f64,
        rate: f64,
        latest: &'a Reading,
        stretches: Vec<Stretch>,
    },
    /// The provider reported the total, but no rate is known yet.
    Unattributed { latest: &'a Reading },
    /// No current total: the provider reports none, or the window reset
    /// after its last reading.
    Unreported,
}

fn window(
    now: DateTime<Utc>,
    minutes: i64,
    turns: &[&Turn],
    windows: &[Vec<&Reading>],
    rate: Option<f64>,
) -> WindowUsage {
    let length = Duration::minutes(minutes);
    let current = windows
        .iter()
        .filter(|w| w[0].resets_at > now)
        .max_by_key(|w| w[0].resets_at);
    let (starts_at, pricing) = match current {
        Some(window) => {
            let latest = *window.last().expect("windows are never empty");
            let used = latest.used_percent;
            let pricing = match rate {
                Some(rate) => Pricing::Priced {
                    used,
                    rate,
                    latest,
                    stretches: stretches(now, minutes, window, turns),
                },
                None => Pricing::Unattributed { latest },
            };
            (latest.resets_at - length, pricing)
        }
        None => (now - length, Pricing::Unreported),
    };
    let members: Vec<&Turn> = turns
        .iter()
        .copied()
        .filter(|t| overlap(t.span(now), (starts_at, now)) > 0.0)
        .collect();
    let in_window = |turn: &Turn, from: DateTime<Utc>, to: DateTime<Utc>| {
        turn.weight() * overlap(turn.span(now), (from.max(starts_at), to.min(now)))
    };
    let share_total: f64 = members.iter().map(|t| in_window(t, starts_at, now)).sum();
    // Percent of the cap per weight unit up to `priced_until`, and the part
    // of the total left to use outside else.bot.
    let (unit, priced_until, outside) = match &pricing {
        Pricing::Priced {
            used, rate, latest, ..
        } => {
            let measured: f64 = members
                .iter()
                .map(|t| in_window(t, starts_at, latest.observed_at))
                .sum::<f64>()
                * rate;
            let scale = if measured > *used {
                used / measured
            } else {
                1.0
            };
            (
                Some(rate * scale),
                latest.observed_at,
                Some(used - measured * scale),
            )
        }
        _ => (None, now, None),
    };
    let amount = |turn: &Turn, from: DateTime<Utc>, to: DateTime<Utc>| match unit {
        Some(unit) => in_window(turn, from, to.min(priced_until)) * unit,
        None if share_total > 0.0 => in_window(turn, from, to) / share_total,
        None => 0.0,
    };
    let bots = bots(
        &members,
        |t| amount(t, starts_at, now),
        unit.is_some(),
        |t| in_window(t, starts_at, now),
    );
    let buckets = if minutes <= TIMELINE_MAX_MINUTES {
        let step = length / BUCKETS;
        let leftovers: Vec<(DateTime<Utc>, DateTime<Utc>, f64)> = match (&pricing, unit) {
            (Pricing::Priced { stretches, .. }, Some(unit)) => stretches
                .iter()
                .map(|s| (s.from, s.to, (s.rise - s.weight * unit).max(0.0)))
                .collect(),
            _ => Vec::new(),
        };
        let leftover: f64 = leftovers.iter().map(|l| l.2).sum();
        let outside_scale = match outside {
            Some(outside) if leftover > 0.0 => outside / leftover,
            _ => 0.0,
        };
        (0..BUCKETS)
            .map(|i| {
                let from = starts_at + step * i;
                let to = from + step;
                let mut by_bot: BTreeMap<Uuid, f64> = BTreeMap::new();
                for turn in &members {
                    *by_bot.entry(turn.bot_id).or_default() += amount(turn, from, to);
                }
                let outside: f64 = leftovers
                    .iter()
                    .map(|&(a, b, rise)| rise * overlap((a, b), (from, to)))
                    .sum::<f64>()
                    * outside_scale;
                Bucket {
                    starts_at: from,
                    bots: by_bot
                        .into_iter()
                        .map(|(bot_id, amount)| BucketBot {
                            bot_id,
                            amount: fine(amount),
                        })
                        .filter(|b| b.amount > 0.0)
                        .collect(),
                    outside: fine(outside),
                }
            })
            .collect()
    } else {
        Vec::new()
    };
    let latest = match &pricing {
        Pricing::Priced { latest, .. } | Pricing::Unattributed { latest, .. } => Some(*latest),
        Pricing::Unreported => None,
    };
    WindowUsage {
        minutes,
        starts_at,
        resets_at: latest.map(|r| r.resets_at),
        observed_at: latest.map(|r| r.observed_at),
        used_percent: latest.map(|r| round(r.used_percent)),
        outside_percent: outside.map(|o| round(o.max(0.0))),
        bots,
        buckets,
    }
}

/// The weight of the turns' work inside `from..to`, spread evenly over each
/// turn's span, and how long at least one of them ran there.
fn activity(
    now: DateTime<Utc>,
    turns: &[&Turn],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> (f64, Duration) {
    let mut weight = 0.0;
    let mut spans = Vec::new();
    for turn in turns {
        let fraction = overlap(turn.span(now), (from, to));
        if fraction > 0.0 {
            weight += turn.weight() * fraction;
            let (start, end) = turn.span(now);
            spans.push((start.max(from), end.min(to)));
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

/// A bot's usage grouped by what started it: kind, label, and routine.
type SourceKey<'a> = (Source, Option<&'a str>, Option<Uuid>);

/// Groups the window's turns by bot. `amount` is a turn's percent of the cap
/// when `priced`, else its share of else.bot's use; `weight` is its weight
/// inside the window.
fn bots(
    turns: &[&Turn],
    amount: impl Fn(&Turn) -> f64,
    priced: bool,
    weight: impl Fn(&Turn) -> f64,
) -> Vec<BotUsage> {
    let total: f64 = turns.iter().map(|t| weight(t)).sum();
    let mut by_bot: BTreeMap<Uuid, Vec<&Turn>> = BTreeMap::new();
    for turn in turns {
        by_bot.entry(turn.bot_id).or_default().push(turn);
    }
    let mut bots: Vec<BotUsage> = by_bot
        .into_values()
        .map(|turns| {
            let bot_weight: f64 = turns.iter().map(|t| weight(t)).sum();
            let mut models: BTreeMap<&str, f64> = BTreeMap::new();
            let mut sources: BTreeMap<SourceKey, (u32, f64, f64)> = BTreeMap::new();
            for turn in &turns {
                if let Some(model) = turn.model.as_deref() {
                    *models.entry(model).or_default() += weight(turn);
                }
                let entry = sources
                    .entry((turn.source, turn.label.as_deref(), turn.routine_id))
                    .or_default();
                entry.0 += 1;
                entry.1 += weight(turn);
                entry.2 += amount(turn);
            }
            let mut models: Vec<(&str, f64)> = models.into_iter().collect();
            models.sort_by(|a, b| b.1.total_cmp(&a.1));
            let mut sources: Vec<SourceUsage> = sources
                .into_iter()
                .map(
                    |((source, label, routine_id), (count, w, amount))| SourceUsage {
                        source,
                        label: label.map(str::to_owned),
                        routine_id,
                        turns: count,
                        percent: priced.then(|| round(amount)),
                        share: ratio(w, bot_weight),
                    },
                )
                .collect();
            sources.sort_by(|a, b| b.share.total_cmp(&a.share));
            let sum = |f: fn(&Turn) -> i64| turns.iter().map(|t| f(t)).sum();
            BotUsage {
                bot_id: turns[0].bot_id,
                name: turns[0].bot_name.clone(),
                percent: priced.then(|| round(turns.iter().map(|t| amount(t)).sum())),
                share: ratio(bot_weight, total),
                turns: turns.len() as u32,
                input_tokens: sum(|t| t.input_tokens),
                cached_input_tokens: sum(|t| t.cached_input_tokens),
                cache_write_tokens: sum(|t| t.cache_write_tokens),
                output_tokens: sum(|t| t.output_tokens),
                models: models.into_iter().map(|(m, _)| m.to_owned()).collect(),
                sources,
            }
        })
        .collect();
    bots.sort_by(|a, b| b.share.total_cmp(&a.share));
    bots
}

fn ratio(part: f64, whole: f64) -> f64 {
    if whole > 0.0 {
        (part / whole * 1000.0).round() / 1000.0
    } else {
        0.0
    }
}

/// Timeline columns are small, so they keep two decimals.
fn fine(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn round(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests;
