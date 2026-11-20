//! The states the server sends as strings. Each enum keeps a fallback
//! variant so a newer server's value still decodes.

use serde::Deserialize;

/// Declares a state decoded from its string, any other string becoming the
/// fallback variant. Unlike `#[serde(other)]`, a value that is not a string
/// fails to decode, so a malformed event still resets the stream.
macro_rules! wire_state {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $($(#[$variant_meta:meta])* $variant:ident = $text:literal,)*
        }
        $(#[$fallback_meta:meta])*
        _ => $fallback:ident
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
        #[serde(from = "String")]
        pub enum $name {
            $($(#[$variant_meta])* $variant,)*
            $(#[$fallback_meta])*
            $fallback,
        }

        impl From<String> for $name {
            fn from(text: String) -> Self {
                match text.as_str() {
                    $($text => Self::$variant,)*
                    _ => Self::$fallback,
                }
            }
        }
    };
}
pub(crate) use wire_state;

wire_state! {
    pub enum TurnStatus {
        Queued = "queued",
        Preparing = "preparing",
        Sending = "sending",
        Running = "running",
        Cancelling = "cancelling",
        Waiting = "waiting",
        Completed = "completed",
        Interrupted = "interrupted",
        Failed = "failed",
        Uncertain = "uncertain",
    }
    _ => Unknown
}

impl TurnStatus {
    /// Not yet ended: queued, parked, or holding the bot's active slot.
    pub fn is_live(self) -> bool {
        matches!(
            self,
            Self::Queued
                | Self::Preparing
                | Self::Sending
                | Self::Running
                | Self::Cancelling
                | Self::Waiting
        )
    }

    /// A provider is working on it now.
    pub fn is_working(self) -> bool {
        matches!(self, Self::Preparing | Self::Sending | Self::Running)
    }

    /// Ended without completing.
    pub fn is_unsuccessful(self) -> bool {
        matches!(self, Self::Interrupted | Self::Failed | Self::Uncertain)
    }
}

wire_state! {
    pub enum ItemKind {
        User = "user",
        Assistant = "assistant",
        Status = "status",
        Routine = "routine",
    }
    _ => Unknown
}

wire_state! {
    /// A bot's sidebar activity, projected from its turns.
    pub enum Phase {
        Idle = "idle",
        Queued = "queued",
        Working = "working",
        Cancelling = "cancelling",
    }
    _ => Unknown
}

/// A question's kind; a newer server's kind keeps its text so an approval
/// card can still name it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "String")]
pub enum QuestionKind {
    Choice,
    Destructive,
    External,
    Other(String),
}

impl From<String> for QuestionKind {
    fn from(text: String) -> Self {
        match text.as_str() {
            "choice" => Self::Choice,
            "destructive" => Self::Destructive,
            "external" => Self::External,
            _ => Self::Other(text),
        }
    }
}

impl QuestionKind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Choice => "choice",
            Self::Destructive => "destructive",
            Self::External => "external",
            Self::Other(text) => text,
        }
    }
}

wire_state! {
    pub enum QuestionStatus {
        Pending = "pending",
        Answered = "answered",
        Skipped = "skipped",
        Cancelled = "cancelled",
    }
    _ => Unknown
}

wire_state! {
    pub enum SecretRequestStatus {
        Pending = "pending",
        Stored = "stored",
        Cancelled = "cancelled",
    }
    _ => Unknown
}

/// Where a routine's last due run is: awaiting its check, skipped, ended
/// with no news, or its turn's status with live turns as `queued` or
/// `running`. A newer server's status keeps its text so it can be shown.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "String")]
pub enum RunStatus {
    Checking,
    Queued,
    Running,
    Skipped,
    NoNews,
    Completed,
    Interrupted,
    Failed,
    Uncertain,
    Other(String),
}

impl From<String> for RunStatus {
    fn from(text: String) -> Self {
        match text.as_str() {
            "checking" => Self::Checking,
            "queued" => Self::Queued,
            "running" => Self::Running,
            "skipped" => Self::Skipped,
            "no_news" => Self::NoNews,
            "completed" => Self::Completed,
            "interrupted" => Self::Interrupted,
            "failed" => Self::Failed,
            "uncertain" => Self::Uncertain,
            _ => Self::Other(text),
        }
    }
}

impl RunStatus {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Checking => "checking",
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Skipped => "skipped",
            Self::NoNews => "no_news",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
            Self::Uncertain => "uncertain",
            Self::Other(text) => text,
        }
    }
}

wire_state! {
    /// Why a routine's due run did not start a turn.
    pub enum SkipReason {
        Overlap = "overlap",
        QueueFull = "queue_full",
        ProviderUnavailable = "provider_unavailable",
        NothingNew = "nothing_new",
        CheckCancelled = "check_cancelled",
        CheckUnavailable = "check_unavailable",
    }
    _ => Unknown
}

wire_state! {
    /// Why else.bot switched a routine off.
    pub enum PausedReason {
        /// Its last failure needs the owner; retrying won't fix it.
        NeedsAction = "needs_action",
        /// It failed several runs in a row.
        Repeated = "repeated",
    }
    _ => Unknown
}

wire_state! {
    /// Who wrote a rules version.
    pub enum RuleAuthor {
        Bot = "bot",
        Owner = "owner",
    }
    _ => Unknown
}
