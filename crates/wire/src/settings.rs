use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub api_version: Option<String>,
    #[serde(default)]
    pub bot_avatar_on_create: bool,
    /// The server settles only the ask a message names; older servers
    /// refuse the field, so the app sends it only when this is set.
    #[serde(default)]
    pub answers_notice: bool,
    #[serde(default)]
    pub server_instance_id: Option<Uuid>,
    /// Absent from servers that do not keep a name.
    #[serde(default)]
    pub server: Option<ServerInfo>,
    #[serde(default)]
    pub default_provider_id: Option<String>,
    /// What new bots start with.
    #[serde(default)]
    pub defaults: Option<ProfileChoice>,
    /// The welcome screen is still to be completed.
    #[serde(default)]
    pub onboarding_required: bool,
    pub providers: Vec<ProviderInfo>,
    pub limits: Limits,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub max_text_bytes: usize,
    #[serde(default)]
    pub max_bot_name_chars: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderInfo {
    #[serde(flatten)]
    pub state: ProviderState,
    /// The exact models, efforts, and tiers it applies; empty for a
    /// provider without models (it is not offered in the pickers).
    #[serde(default)]
    pub catalog: Catalog,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderState {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub unavailable_reason: Option<String>,
    /// Whether the provider's CLI runs on the API's host (older APIs do
    /// not say: assume it does).
    #[serde(default = "yes")]
    pub cli_available: bool,
    #[serde(default)]
    pub cli_unavailable_reason: Option<String>,
    #[serde(default)]
    pub requires_profile: bool,
    #[serde(default)]
    pub profile_configured: Option<bool>,
}

fn yes() -> bool {
    true
}

/// The models a provider applies, each with its own efforts and tiers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "WireCatalog", into = "WireCatalog")]
pub struct Catalog {
    /// Featured models first, then older ones.
    pub models: Vec<ModelInfo>,
    /// When the server's CLI listed these; `None` for the bundled list.
    pub listed_at: Option<DateTime<Utc>>,
    /// Why the server's latest ask failed.
    pub refresh_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub label: String,
    pub efforts: Vec<String>,
    pub tiers: Vec<String>,
    #[serde(default)]
    pub featured: bool,
    /// The newest model of the same family, when this one is older.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade: Option<String>,
}

impl Catalog {
    pub fn model(&self, id: &str) -> Option<&ModelInfo> {
        self.models.iter().find(|model| model.id == id)
    }

    pub fn ids(&self) -> Vec<&str> {
        self.models.iter().map(|model| model.id.as_str()).collect()
    }

    /// Whether the provider applies `choice` exactly.
    pub fn applies(&self, choice: &ProfileChoice) -> bool {
        self.model(&choice.model).is_some_and(|model| {
            model.efforts.contains(&choice.reasoning_effort)
                && model.tiers.contains(&choice.service_tier)
        })
    }
}

/// APIs before per-model details send only the flat lists, which apply
/// to every model; empty featured models feature all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WireCatalog {
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    featured_models: Vec<String>,
    #[serde(default)]
    efforts: Vec<String>,
    #[serde(default)]
    tiers: Vec<String>,
    #[serde(default)]
    details: Vec<ModelInfo>,
    #[serde(default)]
    listed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    refresh_error: Option<String>,
}

impl From<Catalog> for WireCatalog {
    fn from(catalog: Catalog) -> Self {
        let (efforts, tiers) = catalog
            .models
            .first()
            .map(|first| (first.efforts.clone(), first.tiers.clone()))
            .unwrap_or_default();
        // Older desktops apply the same efforts and tiers to every model.
        let mut models = Vec::new();
        let mut featured_models = Vec::new();
        for model in &catalog.models {
            if efforts.iter().all(|effort| model.efforts.contains(effort))
                && tiers.iter().all(|tier| model.tiers.contains(tier))
            {
                models.push(model.id.clone());
                if model.featured {
                    featured_models.push(model.id.clone());
                }
            }
        }
        Self {
            details: catalog.models,
            efforts,
            featured_models,
            listed_at: catalog.listed_at,
            models,
            refresh_error: catalog.refresh_error,
            tiers,
        }
    }
}

impl From<WireCatalog> for Catalog {
    fn from(wire: WireCatalog) -> Self {
        let models = if wire.details.is_empty() {
            let featured =
                |id: &String| wire.featured_models.is_empty() || wire.featured_models.contains(id);
            let (first, rest): (Vec<_>, Vec<_>) = wire.models.into_iter().partition(featured);
            first
                .into_iter()
                .map(|id| (id, true))
                .chain(rest.into_iter().map(|id| (id, false)))
                .map(|(id, featured)| ModelInfo {
                    label: model_label(&id),
                    id,
                    efforts: wire.efforts.clone(),
                    tiers: wire.tiers.clone(),
                    featured,
                    upgrade: None,
                })
                .collect()
        } else {
            wire.details
        };
        Self {
            models,
            listed_at: wire.listed_at,
            refresh_error: wire.refresh_error,
        }
    }
}

/// A provider and an exact profile for it, as the API takes and reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileChoice {
    pub provider_id: String,
    pub model: String,
    pub reasoning_effort: String,
    pub service_tier: String,
}

/// The profile a bot's next messages use: its own (`own`), else the
/// defaults it follows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotProfile {
    #[serde(flatten)]
    pub choice: ProfileChoice,
    pub own: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotProfileEnvelope<P> {
    pub profile: P,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefaultsEnvelope {
    pub defaults: Option<ProfileChoice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderProfile {
    #[serde(flatten)]
    pub choice: ProfileChoice,
    pub revision: i64,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    /// The owner's name for the server, shared by every desktop; `None`
    /// until someone names it.
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Shared::is_unsupported")]
    pub icon: Shared,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerEnvelope<S = ServerInfo> {
    pub server: S,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedIconBody {
    #[serde(default)]
    pub icon: Shared,
    pub picture: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IconOnly {
    pub icon: Shared,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Onboarded {
    pub main_bot_id: Uuid,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub profile: Option<ProfileChoice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
    pub request_id: Option<String>,
}

wire_state! {
    pub enum LetterStyle {
        Gradient = "gradient",
        Monogram = "monogram",
    }
    _ => Unknown
}

/// A server icon as advertised to desktops. A picture may include its SHA-256 digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Stored {
    Default {},
    Letters {
        style: LetterStyle,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        letters: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hue: Option<u16>,
    },
    Pattern {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hue: Option<u16>,
    },
    Emoji {
        emoji: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hue: Option<u16>,
    },
    Picture {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        digest: Option<String>,
    },
}

/// The icon a server keeps for every desktop.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Shared {
    /// The server is too old to keep one, or could not read it.
    #[default]
    Unsupported,
    /// Nobody chose one yet.
    Unset,
    Set(Stored),
    /// An icon document the server read from disk and can still relay.
    Preserved(Value),
}

impl Shared {
    fn is_unsupported(&self) -> bool {
        matches!(self, Self::Unsupported)
    }
}

impl<'de> Deserialize<'de> for Shared {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Option::<Value>::deserialize(deserializer)? {
            None => Self::Unset,
            Some(value) => match serde_json::from_value(value) {
                Ok(Stored::Letters {
                    style: LetterStyle::Unknown,
                    ..
                })
                | Err(_) => Self::Unsupported,
                Ok(icon) => Self::Set(icon),
            },
        })
    }
}

/// "gpt-6.1-sol" -> "GPT 6.1 Sol", "claude-opus-5-5" -> "Claude Opus 5.5".
pub fn model_label(id: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    for part in id.split('-') {
        match words.last_mut() {
            Some(last)
                if part.chars().all(|c| c.is_ascii_digit())
                    && last.chars().all(|c| c.is_ascii_digit() || c == '.') =>
            {
                last.push('.');
                last.push_str(part);
            }
            _ if part == "gpt" => words.push("GPT".to_owned()),
            _ => {
                let mut chars = part.chars();
                words.push(match chars.next() {
                    Some(first) => first.to_uppercase().chain(chars).collect(),
                    None => String::new(),
                });
            }
        }
    }
    words.join(" ")
}
