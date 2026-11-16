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

/// The window lengths providers report, in minutes.
pub const FIVE_HOURS: u64 = 5 * 60;
pub const WEEK: u64 = 7 * 24 * 60;

impl ProviderUsage {
    pub fn selected_window(&self, minutes: Option<u64>) -> Option<&UsageWindow> {
        let has_limits = self
            .windows
            .iter()
            .any(|window| window.attribution.used().is_some());
        let available = || {
            self.windows
                .iter()
                .filter(|window| !has_limits || window.attribution.used().is_some())
        };
        minutes
            .and_then(|minutes| available().find(|window| window.minutes == minutes))
            .or_else(|| available().find(|window| window.minutes == WEEK))
            .or_else(|| available().next())
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct UsageWindow {
    pub minutes: u64,
    pub attribution: Attribution,
    pub bots: Vec<BotUsage>,
}

/// The provider's exact total and any learned attribution of that total.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
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
    /// No current total.
    #[serde(other)]
    Unreported,
}

impl Attribution {
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
    pub fn title(&self) -> String {
        match self.minutes {
            FIVE_HOURS => "5-hour limit".into(),
            WEEK => "Weekly limit".into(),
            m if m.is_multiple_of(1440) => format!("{}-day limit", m / 1440),
            m if m.is_multiple_of(60) => format!("{}-hour limit", m / 60),
            m => format!("{m}-minute limit"),
        }
    }

    /// The stretch the window's bot shares cover: since the limit last
    /// reset, or a plain rolling span when the provider reports no limit.
    pub fn period(&self) -> String {
        if self.attribution.used().is_some() {
            return format!("since the {} reset", self.title().to_lowercase());
        }
        match self.minutes {
            m if m.is_multiple_of(1440) && m > 1440 => format!("in the last {} days", m / 1440),
            m if m.is_multiple_of(60) && m > 60 => format!("in the last {} hours", m / 60),
            m => format!("in the last {m} minutes"),
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
    /// Fraction of your bots' weighted use in the window.
    pub share: f64,
    pub sources: Vec<UsageSource>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct UsageSource {
    pub source: UsageSourceKind,
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
            Self::Delegated => "Asked by other bots",
            Self::Webhook => "Webhooks",
            Self::Mail => "Mail",
            Self::Social => "Social",
            Self::Unknown => "Other",
        }
    }
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
    fn usage_window_selection_defaults_to_week_and_keeps_periods_separate() {
        let report: UsageReport = serde_json::from_str(include_str!("usage_fixture.json")).unwrap();
        let mut provider = report.providers[0].clone();
        assert_eq!(provider.selected_window(None).unwrap().minutes, WEEK);
        assert_eq!(
            provider.selected_window(Some(FIVE_HOURS)).unwrap().minutes,
            FIVE_HOURS
        );
        assert_eq!(provider.selected_window(Some(123)).unwrap().minutes, WEEK);
        provider.windows[1].minutes = 1440;
        assert_eq!(provider.selected_window(None).unwrap().minutes, FIVE_HOURS);
        provider.windows.clear();
        assert!(provider.selected_window(None).is_none());
    }

    #[test]
    fn usage_fixture_preserves_provider_windows_and_attribution() {
        let report: UsageReport = serde_json::from_str(include_str!("usage_fixture.json")).unwrap();
        let claude = &report.providers[0];
        assert_eq!(report.default_provider().unwrap().provider_id, "claude");
        assert_eq!(claude.windows[0].attribution.used(), Some(62.));
        assert_eq!(claude.windows[0].attribution.outside(), Some(11.));
        let bot = &claude.windows[0].bots[0];
        assert_eq!(bot.sources[0].source, UsageSourceKind::Routine);
        assert_eq!(bot.share, 0.51);
        let grok = &report.providers[1];
        assert_eq!(grok.windows[0].attribution, Attribution::Unreported);
        assert_eq!(grok.windows[0].bots[0].share, 1.);
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
    fn windows_are_named_for_where_they_appear() {
        let report: UsageReport = serde_json::from_str(include_str!("usage_fixture.json")).unwrap();
        let [five, week] = &report.providers[0].windows[..] else {
            panic!("two windows")
        };
        assert_eq!(five.title(), "5-hour limit");
        assert_eq!(week.title(), "Weekly limit");
        assert_eq!(week.period(), "since the weekly limit reset");
        let mut daily = five.clone();
        daily.minutes = 2880;
        assert_eq!(daily.period(), "since the 2-day limit reset");
        daily.attribution = Attribution::Unreported;
        assert_eq!(daily.period(), "in the last 2 days");
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
