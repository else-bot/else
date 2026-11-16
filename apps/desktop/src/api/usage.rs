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
                .filter_map(|window| window.used_percent)
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
    pub resets_at: Option<DateTime<Utc>>,
    pub observed_at: Option<DateTime<Utc>>,
    pub used_percent: Option<f64>,
    pub outside_percent: Option<f64>,
    pub bots: Vec<BotUsage>,
    pub buckets: Vec<UsageBucket>,
}

/// How a window's figures read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Attribution {
    /// Bot and outside figures are percents of the provider's cap.
    Priced { used: f64 },
    /// The provider reported its total, but else.bot hasn't learned what a
    /// turn costs yet, so bots are shown by share of else.bot's use.
    Unattributed { used: f64 },
    /// The provider reports no total; bots are shown by share.
    Unreported,
}

impl UsageWindow {
    pub fn attribution(&self) -> Attribution {
        match (self.used_percent, self.outside_percent) {
            (Some(used), Some(_)) => Attribution::Priced { used },
            (Some(used), None) => Attribution::Unattributed { used },
            (None, _) => Attribution::Unreported,
        }
    }

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
    pub percent: Option<f64>,
    pub share: f64,
    pub turns: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub models: Vec<String>,
    pub sources: Vec<UsageSource>,
}

impl BotUsage {
    pub fn display_percent(&self) -> f64 {
        self.percent.unwrap_or(self.share * 100.)
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct UsageSource {
    pub source: UsageSourceKind,
    pub label: Option<String>,
    pub routine_id: Option<Uuid>,
    pub turns: u64,
    pub percent: Option<f64>,
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
    pub amount: f64,
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
        assert_eq!(claude.windows[0].used_percent, Some(62.));
        assert_eq!(claude.windows[0].outside_percent, Some(11.));
        let bot = &claude.windows[0].bots[0];
        assert_eq!(bot.sources[0].source, UsageSourceKind::Routine);
        assert_eq!(bot.display_percent(), 26.);
        assert_eq!(claude.windows[0].buckets[0].bots[0].bot_id, bot.bot_id);
        let grok = &report.providers[1];
        assert_eq!(grok.windows[0].used_percent, None);
        assert_eq!(grok.windows[0].bots[0].display_percent(), 100.);
        assert_eq!(grok.windows[0].resets_at, None);
    }

    #[test]
    fn usage_defaults_to_highest_limit_then_first_provider_without_limits() {
        let mut report: UsageReport =
            serde_json::from_str(include_str!("usage_fixture.json")).unwrap();
        report.providers[1].windows[0].used_percent = Some(83.);
        assert_eq!(report.default_provider().unwrap().provider_id, "grok");
        for window in report
            .providers
            .iter_mut()
            .flat_map(|provider| &mut provider.windows)
        {
            window.used_percent = None;
        }
        assert_eq!(report.default_provider().unwrap().provider_id, "claude");
        report.providers.clear();
        assert!(report.default_provider().is_none());
    }

    #[test]
    fn windows_tell_priced_unattributed_and_unreported_apart() {
        let mut report: UsageReport =
            serde_json::from_str(include_str!("usage_fixture.json")).unwrap();
        let claude = &mut report.providers[0].windows[0];
        assert_eq!(claude.attribution(), Attribution::Priced { used: 62. });
        claude.outside_percent = None;
        assert_eq!(
            claude.attribution(),
            Attribution::Unattributed { used: 62. }
        );
        assert_eq!(
            report.providers[1].windows[0].attribution(),
            Attribution::Unreported
        );
    }

    #[test]
    fn usage_tolerates_new_sources() {
        let source: UsageSource = serde_json::from_value(serde_json::json!({
            "source": "future_source", "label": "New trigger", "turns": 1,
            "percent": null, "share": 1.0
        }))
        .unwrap();
        assert_eq!(source.source, UsageSourceKind::Unknown);
        assert_eq!(source.source.label(), "Other");
    }
}
