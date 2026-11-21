use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderUsage {
    pub provider_id: String,
    pub windows: Vec<UsageWindow>,
}

/// The window lengths providers report, in minutes.
pub const FIVE_HOURS: i64 = 5 * 60;
pub const WEEK: i64 = 7 * 24 * 60;

impl ProviderUsage {
    pub fn selected_window(&self, minutes: Option<i64>) -> Option<&UsageWindow> {
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    pub minutes: i64,
    pub attribution: Attribution,
    pub bots: Vec<BotUsage>,
}

/// The provider's exact total and any learned attribution of that total.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
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
            m if m % 1440 == 0 => format!("{}-day limit", m / 1440),
            m if m % 60 == 0 => format!("{}-hour limit", m / 60),
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
            m if m % 1440 == 0 && m > 1440 => format!("in the last {} days", m / 1440),
            m if m % 60 == 0 && m > 60 => format!("in the last {} hours", m / 60),
            m => format!("in the last {m} minutes"),
        }
    }

    pub fn bot(&self, id: Uuid) -> Option<&BotUsage> {
        self.bots.iter().find(|bot| bot.bot_id == id)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BotUsage {
    pub bot_id: Uuid,
    pub name: String,
    /// Fraction of your bots' weighted use in the window.
    pub share: f64,
    pub sources: Vec<UsageSource>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageSource {
    pub source: UsageSourceKind,
    /// Fraction of the bot's own use.
    pub share: f64,
}

wire_state! {
    pub enum UsageSourceKind {
        Chat = "chat",
        Routine = "routine",
        Delegated = "delegated",
        Webhook = "webhook",
        Mail = "mail",
        Social = "social",
    }
    _ => Unknown
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
