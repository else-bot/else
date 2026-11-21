//! Why a stream ended, as the error event it sends, and the cleanup of a
//! lease whose attach was cut short.

use axum::extract::ws::Message;
use else_wire::computers::ServerEvent;
use serde_json::json;
use uuid::Uuid;

use crate::computers::{ComputerManager, ViewerLease, ViewerRefusal};

pub(super) const STOPPING: &str = "else.bot is stopping; reconnect in a moment.";

pub(super) struct End {
    pub(super) code: &'static str,
    pub(super) message: String,
}

pub(super) fn end(code: &'static str, message: impl Into<String>) -> End {
    End {
        code,
        message: message.into(),
    }
}

pub(super) fn stopping() -> End {
    end("server_stopping", STOPPING)
}

pub(super) fn disconnected() -> End {
    end("desktop_disconnected", "The desktop connection closed.")
}

pub(super) fn unavailable() -> End {
    end("computer_unavailable", "The computer is not available now.")
}

pub(super) fn lease_end(lease: ViewerLease) -> Option<End> {
    match lease {
        ViewerLease::Live => None,
        ViewerLease::OwnerStopped => {
            Some(end("computer_stopped", "The owner stopped this computer."))
        }
        ViewerLease::BotInactive => Some(end("bot_deleted", "This bot was deleted.")),
        ViewerLease::NotRunning => {
            Some(end("computer_not_running", "The computer stopped running."))
        }
        ViewerLease::Ended => Some(end("lease_ended", "The desktop session ended.")),
    }
}

pub(super) fn refused(refusal: ViewerRefusal) -> End {
    let (code, message) = match refusal {
        ViewerRefusal::NotRunning => (
            "computer_not_running",
            "The computer is not running; connect with wake=true to start it.".to_owned(),
        ),
        ViewerRefusal::OwnerStopped => (
            "computer_stopped",
            "The owner stopped this computer; resume it first.".to_owned(),
        ),
        ViewerRefusal::BotInactive => ("bot_deleted", "This bot was deleted.".to_owned()),
        ViewerRefusal::NoDesktop => (
            "desktop_unavailable",
            "This computer has no desktop.".to_owned(),
        ),
        ViewerRefusal::Starting => (
            "desktop_starting",
            "The desktop is starting; try again shortly.".to_owned(),
        ),
        ViewerRefusal::Unavailable(reason) => (
            "computer_unavailable",
            format!("The computer is not available now: {reason}"),
        ),
    };
    End { code, message }
}

pub(super) fn error_message(code: &str, message: &str) -> Message {
    let event = ServerEvent::Error {
        code: code.to_owned(),
        message: message.to_owned(),
    };
    Message::Text(json!(event).to_string().into())
}

pub(super) async fn release_abandoned(
    computers: &ComputerManager,
    db: &sqlx::SqlitePool,
    holder: &str,
) {
    let leases = async {
        // A cancelled SQLite COMMIT may still run on its worker. Take the
        // writer before reading so that commit cannot appear after cleanup.
        let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
        let leases: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM computer_leases WHERE holder = ?1 AND released_at IS NULL",
        )
        .bind(holder)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok::<_, sqlx::Error>(leases)
    }
    .await;
    let leases = match leases {
        Ok(leases) => leases,
        Err(err) => {
            tracing::warn!(error = %err, "finding an abandoned desktop lease");
            return;
        }
    };
    for lease in leases {
        if let Err(err) = computers.release(lease).await {
            tracing::warn!(error = %err, "releasing an abandoned desktop lease");
        }
    }
}
