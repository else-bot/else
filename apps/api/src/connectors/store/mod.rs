//! Persistence for mail connectors: accounts, their encrypted credentials,
//! per-bot permission overrides, the arrival cursor, recorded mail events, and the
//! durable log of mail actions.
//!
//! A credential is age-encrypted with an identity derived from `SECRET_KEY`,
//! a per-write salt, and a purpose naming this account, credential version
//! and provider, so a ciphertext copied to another row or kept across a
//! provider change does not decrypt. It is never returned with an
//! [`Account`] and is erased when the account is removed.
//!
//! `generation` fences runtime work. Pausing, resuming, removing and
//! restoring increment it, and scans, credential refreshes and actions that
//! carry an older generation are refused.
//!
//! Mail actions go through the shared [`ACTIONS`] ledger before anything
//! reaches the mail server.

use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::types::{
    Account, Arrival, Credential, Cursor, Envelope, Health, MailCommand, MailConfig, MailError,
    MailTrigger, MessageRef, MoveKind, Permissions, Place, Provider, Scan,
};
use crate::connector_core::actions::{BeginError, Column, Columns, Ledger, Start, Status};
use crate::credentials::SecretKey;
use crate::db::turns::TurnStatus;

mod accounts;
mod actions;
mod grants;
mod scan;
mod whereabouts;

pub use accounts::*;
pub use actions::*;
pub use grants::*;
pub use scan::*;
pub use whereabouts::*;

pub const PURPOSE: &str = "else.bot/mail-connectors/v1";
/// Accounts that are not removed.
pub const MAX_ACCOUNTS: i64 = 32;
/// Grants across all accounts.
pub const MAX_GRANTS: i64 = 128;
/// Events one scan records; the cursor stops at the last one recorded.
pub const MAX_ARRIVALS: usize = 100;
const MAX_CONFIG: usize = 8192;
/// Bytes of a recorded event's folder, as `connector_events` allows.
const MAX_EVENT_FOLDER: usize = 255;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("no such mail account")]
    NotFound,
    #[error("the bot is not allowed to do this with this mail account")]
    Forbidden,
    #[error("the request is no longer running")]
    NotRunning,
    #[error("the mail account is paused")]
    Inactive,
    #[error("the mail account was changed; reload it and retry")]
    Changed,
    #[error("this action ID was already used for a different request")]
    Conflict,
    #[error("another mail account already uses this address")]
    AddressTaken,
    #[error("{0}")]
    Limit(&'static str),
    #[error("{0}")]
    Invalid(&'static str),
    #[error("a mail credential could not be encrypted or decrypted")]
    Crypto,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

impl From<StoreError> for MailError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::NotFound => MailError::new("mail_account_not_found"),
            StoreError::Forbidden => MailError::new("mail_forbidden"),
            StoreError::NotRunning => MailError::new("mail_not_running"),
            StoreError::Inactive => MailError::new("mail_account_paused"),
            StoreError::Changed => MailError::new("mail_account_changed"),
            StoreError::Conflict => MailError::new("mail_action_conflict"),
            StoreError::AddressTaken => MailError::new("mail_account_exists"),
            StoreError::Limit(limit) => MailError::new("connector_limit").detail(limit),
            StoreError::Invalid(why) => MailError::new("invalid_mail_request").detail(why),
            StoreError::Crypto => MailError::new("mail_credential_unavailable"),
            StoreError::Db(error) => {
                // sqlx text can quote stored values, so it is logged, never
                // shown to the owner or the bot.
                tracing::warn!(%error, "mail store failed");
                MailError::new("mail_unavailable")
            }
        }
    }
}

fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Gmail => "gmail",
        Provider::Microsoft => "microsoft",
        Provider::Yahoo => "yahoo",
        Provider::Icloud => "icloud",
        Provider::Imap => "imap",
    }
}

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')";
