use super::{Client, ClientError};
use chrono::{DateTime, Utc};
use reqwest::Method;
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize)]
pub struct UsageReport {
    pub generated_at: DateTime<Utc>,
    pub providers: Vec<ProviderUsage>,
}

impl UsageReport {
    pub fn default_provider(&self) -> Option<&ProviderUsage> {
        let peak = |provider: &ProviderUsage| {
            provider
                .windows
                .iter()
                .filter_map(|window| window.attribution.used())
                .reduce(f64::max)
        };
        self.providers.iter().reduce(|selected, next| {
            if peak(next) > peak(selected) {
                next
            } else {
                selected
            }
        })
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProviderUsage {
    pub provider_id: String,
    pub windows: Vec<UsageWindow>,
}

impl ProviderUsage {
    pub fn label(&self) -> &str {
        match self.provider_id.as_str() {
            "claude" => "Claude",
            "codex" => "Codex",
            "grok" => "Grok",
            other => other,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct UsageWindow {
    pub minutes: u64,
    pub starts_at: DateTime<Utc>,
    pub attribution: Attribution,
    pub bots: Vec<BotUsage>,
    pub buckets: Vec<UsageBucket>,
}

/// What a window's total is, and so what every bot, source and bucket
/// `percent` in it measures.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Attribution {
    /// Percents are points of the provider's cap; `outside` is the part of
    /// `used` that came from outside else.bot.
    Priced {
        used: f64,
        outside: f64,
        resets_at: DateTime<Utc>,
        observed_at: DateTime<Utc>,
    },
    /// The provider reported `used`, but else.bot hasn't learned what a turn
    /// costs yet: percents are of else.bot's own use.
    Unattributed {
        used: f64,
        resets_at: DateTime<Utc>,
        observed_at: DateTime<Utc>,
    },
    /// No current total: percents are of else.bot's own use.
    #[serde(other)]
    Unreported,
}

impl Attribution {
    pub fn is_priced(&self) -> bool {
        matches!(self, Self::Priced { .. })
    }

    /// The provider's reported total, priced or not.
    pub fn used(&self) -> Option<f64> {
        match self {
            Self::Priced { used, .. } | Self::Unattributed { used, .. } => Some(*used),
            Self::Unreported => None,
        }
    }

    pub fn outside(&self) -> Option<f64> {
        match self {
            Self::Priced { outside, .. } => Some(*outside),
            Self::Unattributed { .. } | Self::Unreported => None,
        }
    }

    pub fn resets_at(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Priced { resets_at, .. } | Self::Unattributed { resets_at, .. } => {
                Some(*resets_at)
            }
            Self::Unreported => None,
        }
    }

    pub fn observed_at(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Priced { observed_at, .. } | Self::Unattributed { observed_at, .. } => {
                Some(*observed_at)
            }
            Self::Unreported => None,
        }
    }
}

impl UsageWindow {
    pub fn label(&self) -> String {
        match self.minutes {
            300 => "5-hour window".into(),
            10080 => "Weekly limit".into(),
            m if m.is_multiple_of(1440) => format!("{}-day window", m / 1440),
            m if m.is_multiple_of(60) => format!("{}-hour window", m / 60),
            m => format!("{m}-minute window"),
        }
    }

    pub fn bot(&self, id: Uuid) -> Option<&BotUsage> {
        self.bots.iter().find(|bot| bot.bot_id == id)
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct BotUsage {
    pub bot_id: Uuid,
    pub name: String,
    /// In the unit the window's [`Attribution`] gives.
    pub percent: f64,
    pub turns: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub models: Vec<String>,
    pub sources: Vec<UsageSource>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct UsageSource {
    pub source: UsageSourceKind,
    pub label: Option<String>,
    pub routine_id: Option<Uuid>,
    pub turns: u64,
    pub percent: f64,
    /// Fraction of the bot's own use.
    pub share: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageSourceKind {
    Chat,
    Routine,
    Delegated,
    Webhook,
    Mail,
    Social,
    #[serde(other)]
    Unknown,
}

impl UsageSourceKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Chat => "Chat",
            Self::Routine => "Routines",
            Self::Delegated => "Delegated",
            Self::Webhook => "Webhooks",
            Self::Mail => "Mail",
            Self::Social => "Social",
            Self::Unknown => "Other",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct UsageBucket {
    pub starts_at: DateTime<Utc>,
    pub bots: Vec<BucketBotUsage>,
    pub outside: f64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BucketBotUsage {
    pub bot_id: Uuid,
    pub percent: f64,
}

impl Client {
    pub async fn usage(self: Arc<Self>) -> Result<UsageReport, ClientError> {
        self.call::<UsageReport>(Method::GET, "/v1/usage".into(), None)
            .await
            .map(|(_, report)| report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_fixture_preserves_provider_windows_and_attribution() {
        let report: UsageReport = serde_json::from_str(include_str!("usage_fixture.json")).unwrap();
        let claude = &report.providers[0];
        assert_eq!(report.default_provider().unwrap().provider_id, "claude");
        assert_eq!(claude.windows[0].attribution.used(), Some(62.));
        assert_eq!(claude.windows[0].attribution.outside(), Some(11.));
        let bot = &claude.windows[0].bots[0];
        assert_eq!(bot.sources[0].source, UsageSourceKind::Routine);
        assert_eq!(bot.percent, 26.);
        assert_eq!(claude.windows[0].buckets[0].bots[0].bot_id, bot.bot_id);
        let grok = &report.providers[1];
        assert_eq!(grok.windows[0].attribution, Attribution::Unreported);
        assert_eq!(grok.windows[0].bots[0].percent, 100.);
    }

    #[test]
    fn usage_defaults_to_highest_limit_then_first_provider_without_limits() {
        let mut report: UsageReport =
            serde_json::from_str(include_str!("usage_fixture.json")).unwrap();
        report.providers[1].windows[0].attribution = Attribution::Unattributed {
            used: 83.,
            resets_at: report.generated_at,
            observed_at: report.generated_at,
        };
        assert_eq!(report.default_provider().unwrap().provider_id, "grok");
        for window in report
            .providers
            .iter_mut()
            .flat_map(|provider| &mut provider.windows)
        {
            window.attribution = Attribution::Unreported;
        }
        assert_eq!(report.default_provider().unwrap().provider_id, "claude");
        report.providers.clear();
        assert!(report.default_provider().is_none());
    }

    #[test]
    fn attribution_decodes_each_state() {
        let decode = |value| serde_json::from_value::<Attribution>(value).unwrap();
        let unattributed = decode(serde_json::json!({
            "state": "unattributed", "used": 12.0,
            "resets_at": "2026-11-16T15:40:00Z", "observed_at": "2026-11-16T15:12:00Z"
        }));
        assert_eq!(unattributed.used(), Some(12.));
        assert_eq!(unattributed.outside(), None);
        assert!(!unattributed.is_priced());
        assert_eq!(
            decode(serde_json::json!({ "state": "unreported" })),
            Attribution::Unreported
        );
        assert_eq!(
            decode(serde_json::json!({ "state": "something_new" })),
            Attribution::Unreported
        );
    }

    #[test]
    fn usage_tolerates_new_sources() {
        let source: UsageSource = serde_json::from_value(serde_json::json!({
            "source": "future_source", "label": "New trigger", "turns": 1,
            "percent": 100.0, "share": 1.0
        }))
        .unwrap();
        assert_eq!(source.source, UsageSourceKind::Unknown);
        assert_eq!(source.source.label(), "Other");
    }
}
