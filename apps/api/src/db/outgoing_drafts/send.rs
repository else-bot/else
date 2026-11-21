//! The send half of a draft's lifecycle: claiming an approved draft for the
//! one call that sends it, and settling that send.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::{DraftError, OutgoingDraft, SELECT, caller, events, load, reload};
use crate::app::outgoing::send::{self, Approved, FileChanged};
use crate::app::outgoing::{self, Channels, Content, Delivery, Outcome, Status};
use crate::db::in_ids;

/// Claim an approved draft for the one call that sends it, atomically, in
/// the request allowed to send it. A second claim is refused: sending again
/// needs a new approval. A draft bound to a connected account is claimed
/// only by [`claim_connected`].
pub async fn claim(
    pool: &SqlitePool,
    turn_id: Uuid,
    draft_id: Uuid,
    delivery: &Delivery,
) -> Result<OutgoingDraft, DraftError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let caller = caller(&mut tx, turn_id).await?;
    let draft = load(&mut tx, caller.bot_id, draft_id).await?;
    match (draft.content.account(), delivery) {
        (None, Delivery::Tool { .. } | Delivery::Host) => {}
        (Some(_), _) => return Err(DraftError::Connected),
        (None, Delivery::Mail { .. } | Delivery::Social { .. }) => {
            return Err(DraftError::NotConnected);
        }
    }
    let draft = mark_sending(&mut tx, caller.bot_id, turn_id, draft, delivery).await?;
    tx.commit().await?;
    Ok(draft)
}

/// What a connected send claimed, and what it sends.
#[derive(Debug)]
pub struct Claimed {
    pub draft: OutgoingDraft,
    pub account_id: Uuid,
    /// The stable ID of the send: a retry of the claim reuses it, so the
    /// connector's receipt deduplicates it.
    pub action_id: Uuid,
    pub payload: Payload,
    /// The approved bytes of the draft's files, read in the claim.
    pub files: Vec<Approved>,
}

/// The approved content a connected send carries.
#[derive(Debug)]
pub enum Payload {
    Mail(outgoing::Email),
    Social {
        message: outgoing::Message,
        destination: String,
    },
}

/// How a connected send's claim found the draft.
#[derive(Debug)]
pub enum Claim {
    /// This call claimed it: nothing was sent yet.
    Fresh(Claimed),
    /// An earlier call of this request claimed it and may have sent it.
    Resumed(Claimed),
    /// An earlier call of this request already settled it.
    Settled(OutgoingDraft),
    /// An approved file changed before this call's claim: the claim failed
    /// the draft and sent nothing. `draft` is what was stored: uncertain,
    /// naming that send, when an earlier send of it may have gone out.
    FailedUnsent {
        draft: OutgoingDraft,
        changed: FileChanged,
    },
    /// An approved file changed after an earlier call of this request
    /// claimed the send, which may have gone out. The draft stays as it is.
    FileChangedAfterClaim(FileChanged),
}

/// Claim a draft bound to a connected account for its account's send, in
/// the request allowed to send it, if `channels` allow that account's
/// transport. A repeated claim in the same request resumes the first, with
/// its action ID, or returns the draft it settled. The approved files are
/// checked before the claim commits, so a draft is never `sending` with
/// unchecked files, and a changed file fails only a fresh claim.
pub async fn claim_connected(
    pool: &SqlitePool,
    turn_id: Uuid,
    draft_id: Uuid,
    channels: Channels,
) -> Result<Claim, DraftError> {
    // Read and hash the files outside the write lock: a message may carry
    // up to 100 MiB. The claim sends exactly the bytes checked here.
    let (files, checked, observed) = {
        let mut conn = pool.acquire().await?;
        let snapshot: OutgoingDraft =
            sqlx::query_as(sqlx::AssertSqlSafe(format!("{SELECT} WHERE d.id = ?1")))
                .bind(draft_id)
                .fetch_optional(&mut *conn)
                .await?
                .ok_or(DraftError::NotFound)?;
        let checked = send::approved(&mut conn, &snapshot).await;
        let observed = snapshot.claim_action_id;
        (snapshot.content.files().to_vec(), checked, observed)
    };
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let caller = caller(&mut tx, turn_id).await?;
    let draft = load(&mut tx, caller.bot_id, draft_id).await?;
    // An approval since the read changed which files it carries, or ended
    // the claim this call found and began another, whose outcome is not this
    // call's to report.
    let reclaimed = observed.is_some() && observed != draft.claim_action_id;
    if reclaimed || draft.content.files() != files.as_slice() {
        return Err(DraftError::Changed(draft.version));
    }
    let account_id = draft.content.account().ok_or(DraftError::NotConnected)?;
    let payload = match &draft.content.0 {
        Content::Email(email) if channels.mail => Payload::Mail(email.clone()),
        Content::Message(message) if channels.social => Payload::Social {
            destination: message
                .destination
                .clone()
                .ok_or(DraftError::NotConnected)?,
            message: message.clone(),
        },
        _ => return Err(DraftError::NoTransport),
    };
    let earlier = draft
        .claim_action_id
        .filter(|_| draft.send_turn_id == Some(turn_id));
    match (draft.status, earlier) {
        (Status::Sending, Some(action_id)) => {
            return Ok(match checked {
                Ok(files) => Claim::Resumed(Claimed {
                    draft,
                    account_id,
                    action_id,
                    payload,
                    files,
                }),
                Err(changed) => Claim::FileChangedAfterClaim(changed),
            });
        }
        (Status::Sent | Status::Failed | Status::Uncertain, Some(_)) => {
            return Ok(Claim::Settled(draft));
        }
        _ => {}
    }
    let action_id = Uuid::new_v4();
    let delivery = match payload {
        Payload::Mail(_) => Delivery::Mail {
            account_id,
            action_id,
        },
        Payload::Social { .. } => Delivery::Social {
            account_id,
            action_id,
        },
    };
    let draft = mark_sending(&mut tx, caller.bot_id, turn_id, draft, &delivery).await?;
    let claim = match checked {
        Ok(files) => Claim::Fresh(Claimed {
            draft,
            account_id,
            action_id,
            payload,
            files,
        }),
        Err(changed) => Claim::FailedUnsent {
            draft: fail_claimed(&mut tx, draft.id, &changed.to_string()).await?,
            changed,
        },
    };
    tx.commit().await?;
    Ok(claim)
}

/// Move an approved draft of request `turn_id` to `sending` through
/// `delivery`, recording a connected delivery's action as the claim.
async fn mark_sending(
    tx: &mut SqliteConnection,
    bot_id: Uuid,
    turn_id: Uuid,
    draft: OutgoingDraft,
    delivery: &Delivery,
) -> Result<OutgoingDraft, DraftError> {
    if draft.status != Status::Approved {
        return Err(DraftError::Settled(draft.status));
    }
    if draft.send_turn_id != Some(turn_id) {
        return Err(DraftError::OtherRequest);
    }
    let claim = match delivery {
        Delivery::Mail {
            account_id,
            action_id,
        }
        | Delivery::Social {
            account_id,
            action_id,
        } => Some((*account_id, *action_id)),
        Delivery::Tool { .. } | Delivery::Host => None,
    };
    let moved = sqlx::query(
        "UPDATE outgoing_drafts SET status = 'sending', delivery = ?2, claim_action_id = ?3,
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 AND status = 'approved'",
    )
    .bind(draft.id)
    .bind(Json(delivery))
    .bind(claim.map(|(_, action_id)| action_id))
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let current = reload(&mut *tx, draft.id).await?;
    if moved == 0 {
        return Err(DraftError::Settled(current.status));
    }
    if let Some((account_id, action_id)) = claim {
        sqlx::query(
            "INSERT INTO draft_claims (account_id, action_id, draft_id) VALUES (?1, ?2, ?3)",
        )
        .bind(account_id)
        .bind(action_id)
        .bind(draft.id)
        .execute(&mut *tx)
        .await?;
    }
    events::turn_changed(&mut *tx, bot_id, draft.turn_id).await?;
    Ok(current)
}

/// A tool or host send this request just claimed stopped before any
/// external effect, such as an approved file whose bytes changed: it fails,
/// and only a new approval sends it. A connected send is refused: a
/// resumed call may be sending it, and its claim already checked its files.
pub async fn fail_unsent(
    pool: &SqlitePool,
    draft_id: Uuid,
    detail: &str,
) -> Result<OutgoingDraft, DraftError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let draft = reload(&mut tx, draft_id).await?;
    if draft.claim_action_id.is_some() {
        return Err(DraftError::Connected);
    }
    let draft = fail_claimed(&mut tx, draft_id, detail).await?;
    tx.commit().await?;
    Ok(draft)
}

/// Fail a `sending` draft whose send never started.
async fn fail_claimed(
    tx: &mut SqliteConnection,
    draft_id: Uuid,
    detail: &str,
) -> Result<OutgoingDraft, DraftError> {
    let failed = sqlx::query(
        "UPDATE outgoing_drafts SET status = 'failed', result_detail = ?2,
             settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 AND status = 'sending'",
    )
    .bind(draft_id)
    .bind(detail)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let draft = reload(&mut *tx, draft_id).await?;
    if failed == 0 {
        return Err(DraftError::Settled(draft.status));
    }
    events::turn_changed(&mut *tx, draft.bot_id, draft.turn_id).await?;
    Ok(draft)
}

/// The bot reports how its claimed send ended. Only the send request's own
/// claim can be reported; `failed` may also release an approval never
/// claimed (nothing was sent). The same report again changes nothing.
pub async fn report(
    pool: &SqlitePool,
    turn_id: Uuid,
    draft_id: Uuid,
    outcome: Outcome,
    detail: Option<&str>,
) -> Result<(OutgoingDraft, bool), DraftError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let caller = caller(&mut tx, turn_id).await?;
    let draft = load(&mut tx, caller.bot_id, draft_id).await?;
    if draft.send_turn_id != Some(turn_id) {
        return Err(DraftError::OtherRequest);
    }
    let connected = draft.content.account().is_some();
    match (draft.status, outcome) {
        (status, outcome) if status == outcome.status() => return Ok((draft, false)),
        (Status::Sending, _) if connected => return Err(DraftError::Connected),
        (Status::Sending, _) | (Status::Approved, Outcome::Failed) => {}
        (status, _) => return Err(DraftError::Settled(status)),
    }
    sqlx::query(
        "UPDATE outgoing_drafts SET status = ?2, result_detail = ?3,
             settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1",
    )
    .bind(draft.id)
    .bind(outcome.status().as_str())
    .bind(detail)
    .execute(&mut *tx)
    .await?;
    events::turn_changed(&mut tx, caller.bot_id, draft.turn_id).await?;
    let draft = reload(&mut tx, draft_id).await?;
    tx.commit().await?;
    Ok((draft, true))
}

/// Refuse a call whose claim `claim` no longer owns the draft: a new
/// approval claimed it again since, so the draft holds another send's state.
fn own_claim(draft: &OutgoingDraft, claim: Uuid) -> Result<(), DraftError> {
    if draft.claim_action_id == Some(claim) {
        Ok(())
    } else {
        Err(DraftError::Settled(draft.status))
    }
}

/// After the connector send of claim `claim` returned. A send the mail
/// connector refused before admitting it (`refused`) fails; the connector
/// then never admits a copy of it still queued. One admitted first settles
/// through its own receipt. Announces the draft's current state.
pub async fn after_mail_send(
    pool: &SqlitePool,
    draft_id: Uuid,
    claim: Uuid,
    refused: Option<&str>,
) -> Result<OutgoingDraft, DraftError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let draft = reload(&mut tx, draft_id).await?;
    own_claim(&draft, claim)?;
    if let (Some(code), Some((account_id, action_id))) = (refused, claimed_mail(&draft)) {
        let admitted: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM connector_actions WHERE account_id = ?1 AND action_id = ?2)",
        )
        .bind(account_id)
        .bind(action_id)
        .fetch_one(&mut *tx)
        .await?;
        if !admitted {
            let detail = format!("The mail connector refused it ({code}). Nothing was sent.");
            fail_claimed(&mut tx, draft_id, &detail).await?;
        }
    }
    events::turn_changed(&mut tx, draft.bot_id, draft.turn_id).await?;
    let draft = reload(&mut tx, draft_id).await?;
    tx.commit().await?;
    Ok(draft)
}

/// What a claimed connected email does next.
#[derive(Debug, Clone)]
pub enum MailSend {
    /// Send it, with this Message-ID. The attempt is recorded first.
    Dispatch { message_id: String },
    /// The claim is settled, and nothing is sent: an earlier send of it
    /// succeeded, so it is now sent naming that send, or another call of
    /// this request settled it first.
    Settled(Box<OutgoingDraft>),
    /// Earlier sends that may have gone out (`earlier`, newest first) block
    /// it: search Sent for `message_id`, then [`record_sent_check`]. `None`
    /// when an earlier send carried no stable Message-ID to search for.
    CheckSent {
        message_id: Option<String>,
        earlier: Vec<Uuid>,
    },
}

/// What a search of the Sent folder found for an earlier, uncertain send.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum SentCheck {
    Found,
    Absent,
}

/// One mail action that sent, or tried to send, a draft, oldest first.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct MailAttempt {
    pub action_id: Uuid,
    /// The action's status; none when the connector refused it before
    /// recording it, so nothing was sent.
    pub status: Option<crate::connector_core::actions::Status>,
    pub phase: Option<String>,
    pub reason: Option<String>,
    pub message_id: Option<String>,
    pub sent_check: Option<SentCheck>,
    pub created_at: DateTime<Utc>,
}

/// The mail action this request claimed for a connected email.
fn claimed_mail(draft: &OutgoingDraft) -> Option<(Uuid, Uuid)> {
    match (draft.status, draft.delivery.as_deref()) {
        (
            Status::Sending,
            Some(Delivery::Mail {
                account_id,
                action_id,
            }),
        ) => Some((*account_id, *action_id)),
        _ => None,
    }
}

/// Decide whether the connected email claim `claim` may be sent. An
/// earlier send that may have gone out, and that Sent was not searched
/// for, blocks it. The search moves the draft out of `sending`, so the next
/// claim needs a new approval. Otherwise its attempt is recorded, once,
/// before the mail connector sends it. A claim another call settled since
/// returns that receipt.
pub async fn next_mail_send(
    pool: &SqlitePool,
    draft_id: Uuid,
    claim: Uuid,
    message_id: &str,
) -> Result<MailSend, DraftError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let draft = reload(&mut tx, draft_id).await?;
    own_claim(&draft, claim)?;
    let Some((account_id, action_id)) = claimed_mail(&draft) else {
        return match draft.status {
            Status::Sent | Status::Failed | Status::Uncertain => {
                Ok(MailSend::Settled(Box::new(draft)))
            }
            status => Err(DraftError::Settled(status)),
        };
    };
    let recorded: Option<Option<String>> = sqlx::query_scalar(
        "SELECT message_id FROM draft_mail_sends WHERE account_id = ?1 AND action_id = ?2",
    )
    .bind(account_id)
    .bind(action_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(recorded) = recorded {
        return Ok(MailSend::Dispatch {
            message_id: recorded.unwrap_or_else(|| message_id.to_owned()),
        });
    }
    let succeeded: Option<Uuid> = sqlx::query_scalar(
        "SELECT s.action_id FROM draft_mail_sends s JOIN connector_actions a
           ON a.account_id = s.account_id AND a.action_id = s.action_id
         WHERE s.draft_id = ?1 AND a.status = 'succeeded'
         ORDER BY s.created_at DESC, s.rowid DESC LIMIT 1",
    )
    .bind(draft_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(earlier) = succeeded {
        let settled = sqlx::query(
            "UPDATE outgoing_drafts SET status = 'sent', delivery = ?2,
                 result_detail = 'An earlier send of it went out. It was not sent again.',
                 settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
                 updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE id = ?1 AND status = 'sending'",
        )
        .bind(draft_id)
        .bind(Json(Delivery::Mail {
            account_id,
            action_id: earlier,
        }))
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if settled == 0 {
            return Err(DraftError::Settled(draft.status));
        }
        events::turn_changed(&mut tx, draft.bot_id, draft.turn_id).await?;
        let draft = reload(&mut tx, draft_id).await?;
        tx.commit().await?;
        return Ok(MailSend::Settled(Box::new(draft)));
    }
    let unchecked: Vec<(Uuid, Option<String>)> = sqlx::query_as(
        "SELECT s.action_id, s.message_id
         FROM draft_mail_sends s JOIN connector_actions a
           ON a.account_id = s.account_id AND a.action_id = s.action_id
         WHERE s.draft_id = ?1 AND s.sent_check IS NULL
           AND a.status IN ('uncertain', 'dispatching')
         ORDER BY s.created_at DESC, s.rowid DESC",
    )
    .bind(draft_id)
    .fetch_all(&mut *tx)
    .await?;
    if !unchecked.is_empty() {
        let first = unchecked[0].1.clone();
        let searchable = first.is_some() && unchecked.iter().all(|(_, id)| *id == first);
        return Ok(MailSend::CheckSent {
            message_id: first.filter(|_| searchable),
            earlier: unchecked.into_iter().map(|(id, _)| id).collect(),
        });
    }
    sqlx::query(
        "INSERT INTO draft_mail_sends (account_id, action_id, draft_id, message_id)
         VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(account_id)
    .bind(action_id)
    .bind(draft_id)
    .bind(message_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(MailSend::Dispatch {
        message_id: message_id.to_owned(),
    })
}

/// Settle the claim `claimed` of a connected email after searching Sent for
/// the `earlier` sends [`next_mail_send`] named, without sending it.
/// `Ok(check)` is what the search found; `Err(reason)` says why there was
/// no search.
///
/// Found: the newest earlier action succeeded and the draft is sent,
/// naming it. Absent: the draft is uncertain again, naming it, and the next
/// approval sends. No search: the draft is uncertain and nothing is
/// recorded, so the next approval must search again. A draft another claim
/// owns is refused. A claim already settled, or an earlier send already
/// searched for, changes nothing.
pub async fn record_sent_check(
    pool: &SqlitePool,
    draft_id: Uuid,
    claimed: Uuid,
    earlier: &[Uuid],
    check: Result<SentCheck, String>,
) -> Result<OutgoingDraft, DraftError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let draft = reload(&mut tx, draft_id).await?;
    own_claim(&draft, claimed)?;
    let (Some((account_id, _)), Some(&newest)) = (claimed_mail(&draft), earlier.first()) else {
        return Ok(draft);
    };
    let (status, detail) = match &check {
        Ok(SentCheck::Found) => (
            Status::Sent,
            "Its Sent folder already holds it, so the earlier send went out. It was not sent again."
                .to_owned(),
        ),
        Ok(SentCheck::Absent) => (
            Status::Uncertain,
            "Its Sent folder does not hold it, which does not prove the earlier send failed. \
             Nothing was sent again; it sends only if the owner approves it again."
                .to_owned(),
        ),
        Err(reason) => (
            Status::Uncertain,
            format!(
                "Its Sent folder was not searched: {reason}. Nothing was sent again."
            ),
        ),
    };
    let moved = sqlx::query(
        "UPDATE outgoing_drafts SET status = ?2, delivery = ?3, result_detail = ?4,
             settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 AND status = 'sending' AND json_extract(delivery, '$.via') = 'mail'
           AND claim_action_id = ?5",
    )
    .bind(draft_id)
    .bind(status.as_str())
    .bind(Json(Delivery::Mail {
        account_id,
        action_id: newest,
    }))
    .bind(
        detail
            .chars()
            .take(outgoing::MAX_DETAIL)
            .collect::<String>(),
    )
    .bind(claimed)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if moved == 0 {
        return Ok(draft);
    }
    if let Ok(check) = check {
        let marked = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE draft_mail_sends SET sent_check = ?2,
                 checked_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE draft_id = ?1 AND sent_check IS NULL
               AND {}",
            in_ids("action_id", "?3")
        )))
        .bind(draft_id)
        .bind(check)
        .bind(Json(earlier))
        .execute(&mut *tx)
        .await?
        .rows_affected();
        // Another search already settled one of them, or the newest send
        // failed after all: keep the claim as it was.
        let settled = marked != earlier.len() as u64
            || (check == SentCheck::Found
                && !crate::connectors::store::found_in_sent(&mut tx, account_id, newest).await?);
        if settled {
            tx.rollback().await?;
            return Ok(draft);
        }
    }
    events::turn_changed(&mut tx, draft.bot_id, draft.turn_id).await?;
    let draft = reload(&mut tx, draft_id).await?;
    tx.commit().await?;
    Ok(draft)
}

/// Every mail action that sent, or tried to send, `draft_id`.
pub async fn mail_attempts(
    pool: &SqlitePool,
    draft_id: Uuid,
) -> Result<Vec<MailAttempt>, sqlx::Error> {
    sqlx::query_as(
        "SELECT s.action_id, a.status, json_extract(a.result, '$.phase') AS phase,
                json_extract(a.result, '$.reason') AS reason, s.message_id, s.sent_check,
                s.created_at
         FROM draft_mail_sends s LEFT JOIN connector_actions a
           ON a.account_id = s.account_id AND a.action_id = s.action_id
         WHERE s.draft_id = ?1 ORDER BY s.created_at, s.rowid",
    )
    .bind(draft_id)
    .fetch_all(pool)
    .await
}

/// After the social send of claim `claim` returned. A send the connection
/// refused, or gave up on, before its worker admitted it (`refused`) fails;
/// the worker then never admits a copy of it still queued. One admitted
/// first settles through its own receipt. Announces the draft's current
/// state.
pub async fn after_social_send(
    pool: &SqlitePool,
    id: Uuid,
    claim: Uuid,
    refused: Option<&str>,
) -> Result<OutgoingDraft, DraftError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let draft = reload(&mut tx, id).await?;
    own_claim(&draft, claim)?;
    if let (
        Some(reason),
        Status::Sending,
        Some(Delivery::Social {
            account_id,
            action_id,
        }),
    ) = (refused, draft.status, draft.delivery.as_deref())
    {
        let admitted: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM social_actions WHERE connection_id = ?1 AND action_id = ?2)",
        )
        .bind(account_id)
        .bind(action_id)
        .fetch_one(&mut *tx)
        .await?;
        if !admitted {
            fail_claimed(&mut tx, id, reason).await?;
        }
    }
    events::turn_changed(&mut tx, draft.bot_id, draft.turn_id).await?;
    let updated = reload(&mut tx, id).await?;
    tx.commit().await?;
    Ok(updated)
}
