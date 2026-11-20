//! The owner's email reader: the message behind a mail-triggered turn,
//! fetched with the saved credential and never through a bot.

use super::*;

const RAW: &str = "From: Coupon Co <hello@coupons.test>\r\nSubject: Verify your email\r\n\
    MIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Hi</p>";

/// Holds messages 9 and 11 in any folder, counting the sessions it signs
/// in and the messages it serves. `trash` moves a message to Trash as UID
/// 21, `gone` lists messages that left their folder, and `locate` finds
/// the message in `found` unless it is `unsearchable`. `folders` says it is `holding` and waits for
/// `release`, standing in for a long bot command, and `reject` refuses new
/// sign-ins and drops open sessions.
#[derive(Default, Clone)]
struct Mailbox(Arc<Inbox>);

#[derive(Default)]
struct Inbox {
    commands: std::sync::Mutex<Vec<Value>>,
    gone: std::sync::Mutex<Vec<MessageRef>>,
    found: std::sync::Mutex<Option<MessageRef>>,
    unsearchable: std::sync::atomic::AtomicBool,
    fetches: std::sync::atomic::AtomicUsize,
    sessions: std::sync::atomic::AtomicUsize,
    reject: std::sync::atomic::AtomicBool,
    holding: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl std::ops::Deref for Mailbox {
    type Target = Inbox;
    fn deref(&self) -> &Inbox {
        &self.0
    }
}

impl Inbox {
    fn serve(&self, message: &MessageRef) -> Result<Fetched, MailError> {
        self.fetches
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let found = self.found.lock().unwrap().as_ref() == Some(message);
        if !found
            && (![9, 11].contains(&message.uid) || self.gone.lock().unwrap().contains(message))
        {
            return Err(MailError::new("message_not_found"));
        }
        Ok(Fetched {
            raw: RAW.as_bytes().into(),
            received_at: None,
        })
    }
}

impl Inbox {
    /// Moves `message` to `to`, or out of every folder when `to` is `None`.
    fn leave(&self, message: &MessageRef, to: impl Into<Option<MessageRef>>) {
        self.gone.lock().unwrap().push(message.clone());
        *self.found.lock().unwrap() = to.into();
    }

    fn locates(&self) -> usize {
        self.commands
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c["operation"] == "locate")
            .count()
    }
}

#[async_trait::async_trait]
impl MailReader for Mailbox {
    async fn read_message(&mut self, message: &MessageRef) -> Result<Fetched, MailError> {
        if self.reject.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(MailError::new("mail_transport_failed"));
        }
        self.serve(message)
    }
}

#[async_trait::async_trait]
impl MailTransport for Mailbox {
    async fn verify(&self, _: &MailConfig, _: &Auth) -> Result<(), MailError> {
        Ok(())
    }
    async fn execute(
        &self,
        _: &MailConfig,
        _: &Auth,
        command: &MailCommand,
    ) -> Result<Value, MailError> {
        self.commands
            .lock()
            .unwrap()
            .push(serde_json::to_value(command).unwrap());
        match command {
            MailCommand::Trash { message } => {
                let trashed = MessageRef {
                    folder: "Trash".into(),
                    uid_validity: 1,
                    uid: 21,
                };
                self.leave(message, trashed.clone());
                Ok(json!({"moved":true,"destination":"Trash","message":trashed}))
            }
            MailCommand::Locate { .. }
                if self.unsearchable.load(std::sync::atomic::Ordering::SeqCst) =>
            {
                Err(MailError::new("mail_transport_failed"))
            }
            MailCommand::Locate { .. } => Ok(json!({"message":*self.found.lock().unwrap()})),
            MailCommand::Folders => {
                self.holding.notify_one();
                self.release.notified().await;
                Ok(json!({"folders":[]}))
            }
            _ => Err(MailError::new("message_not_found")),
        }
    }
    async fn fetch(
        &self,
        _: &MailConfig,
        _: &Auth,
        message: &MessageRef,
    ) -> Result<Fetched, MailError> {
        self.serve(message)
    }
    async fn reader(&self, _: &MailConfig, _: &Auth) -> Result<Box<dyn MailReader>, MailError> {
        if self.reject.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(MailError::new(AUTHENTICATION_FAILED));
        }
        self.sessions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Box::new(self.clone()))
    }
    async fn scan(&self, _: &MailConfig, _: &Auth, _: Option<&Cursor>) -> Result<Scan, MailError> {
        Ok(Scan {
            cursor: Cursor {
                uid_validity: 1,
                last_uid: 0,
            },
            arrivals: vec![],
            more: false,
        })
    }
    async fn wait(
        &self,
        _: &MailConfig,
        _: &Auth,
        _: &Cursor,
        stop: CancellationToken,
    ) -> Result<(), MailError> {
        stop.cancelled().await;
        Ok(())
    }
}

fn messages_path(account: Uuid, uid: u32) -> String {
    format!(
        "/v1/connectors/email/accounts/{account}/messages?folder=INBOX%20%5Bgate%5D&uid_validity=1&uid={uid}"
    )
}

#[tokio::test]
async fn the_owner_reads_the_email_behind_a_mail_turn_and_its_bot_cannot() {
    let mailbox = Arc::new(Mailbox::default());
    let h = recorder::harness_with(4, Some(mailbox.clone())).await;
    let bot: Uuid = h.app.main_bot().await.parse().unwrap();
    let account = account(&h.app).await;
    store::set_permissions(&h.app.db.pool, account.id, bot, &permissions())
        .await
        .unwrap();
    let turn = mail_event(&h, &account, &format!("INBOX {GATE}"), "Summarize it.").await;
    let message = json!({"folder":"INBOX [gate]","uid_validity":1,"uid":9});

    let conversation = h
        .app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    let shown = conversation.body["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == turn.to_string())
        .unwrap_or_else(|| panic!("the mail turn is listed: {}", conversation.body));
    assert_eq!(
        shown["mail"],
        json!([{"account_id":account.id,"message":message,"subject":"Verify your email",
                "sender":"Coupon Co","location":{"state":"arrived"}}])
    );

    let read = h
        .app
        .owner("GET", &messages_path(account.id, 9), None)
        .await;
    assert_eq!(read.status, StatusCode::OK, "{}", read.body);
    assert_eq!(read.body["subject"], "Verify your email");
    assert_eq!(read.body["html"], "<p>Hi</p>");
    assert_eq!(fetches(&mailbox), 1);
    let again = h
        .app
        .owner("GET", &messages_path(account.id, 9), None)
        .await;
    assert_eq!(again.body, read.body);
    assert_eq!(
        fetches(&mailbox),
        1,
        "a reopened email comes from the cache"
    );

    let (_, token) = h.started(turn).await;
    let refused = tool(
        &h.app,
        &token,
        "mail_action",
        json!({"account_id":account.id,"command":{"operation":"view","message":message}}),
    )
    .await;
    assert!(
        refused.error && refused.body.to_string().contains("invalid_mail_request"),
        "{}",
        refused.body
    );
    assert!(mailbox.commands.lock().unwrap().is_empty());
    assert_eq!(fetches(&mailbox), 1);
}

fn fetches(mailbox: &Mailbox) -> usize {
    mailbox.fetches.load(std::sync::atomic::Ordering::SeqCst)
}

fn sessions(mailbox: &Mailbox) -> usize {
    mailbox.sessions.load(std::sync::atomic::Ordering::SeqCst)
}

async fn set_active(h: &recorder::Harness, account: Uuid, active: bool) -> common::Reply {
    h.app
        .owner(
            "PATCH",
            &format!("/v1/connectors/email/accounts/{account}"),
            Some(json!({ "active": active })),
        )
        .await
}

#[tokio::test]
async fn emails_the_bot_never_read_share_one_signed_in_session() {
    let mailbox = Arc::new(Mailbox::default());
    let h = recorder::harness_with(4, Some(mailbox.clone())).await;
    let account = account(&h.app).await;
    for uid in [9, 11] {
        let view = h
            .app
            .owner("GET", &messages_path(account.id, uid), None)
            .await;
        assert_eq!(view.status, StatusCode::OK, "{}", view.body);
    }
    assert_eq!(fetches(&mailbox), 2);
    assert_eq!(sessions(&mailbox), 1, "the second miss reused the session");
}

#[tokio::test]
async fn the_owner_reads_while_a_bot_command_holds_the_account() {
    let mailbox = Arc::new(Mailbox::default());
    let h = recorder::harness_with(4, Some(mailbox.clone())).await;
    let bot: Uuid = h.app.main_bot().await.parse().unwrap();
    let account = account(&h.app).await;
    store::set_permissions(&h.app.db.pool, account.id, bot, &permissions())
        .await
        .unwrap();
    let turn = mail_event(&h, &account, &format!("INBOX {GATE}"), "Summarize it.").await;
    let (_, token) = h.started(turn).await;
    let held = tool(
        &h.app,
        &token,
        "mail_action",
        json!({"account_id":account.id,"command":{"operation":"folders"}}),
    );
    let owner = async {
        tokio::time::timeout(common::SETTLE, mailbox.holding.notified())
            .await
            .expect("the bot command reached the mail server");
        let view = tokio::time::timeout(
            common::SETTLE,
            h.app.owner("GET", &messages_path(account.id, 11), None),
        )
        .await
        .expect("the owner's read did not wait for the bot's command");
        mailbox.release.notify_one();
        view
    };
    let (held, view) = tokio::join!(held, owner);
    assert_eq!(view.status, StatusCode::OK, "{}", view.body);
    assert!(!held.error, "{}", held.body);
}

#[tokio::test]
async fn pausing_an_inbox_drops_its_cached_email_and_session() {
    let mailbox = Arc::new(Mailbox::default());
    let h = recorder::harness_with(4, Some(mailbox.clone())).await;
    let account = account(&h.app).await;
    h.app
        .owner("GET", &messages_path(account.id, 9), None)
        .await;
    assert_eq!(
        set_active(&h, account.id, false).await.status,
        StatusCode::OK
    );
    assert_eq!(
        set_active(&h, account.id, true).await.status,
        StatusCode::OK
    );
    let view = h
        .app
        .owner("GET", &messages_path(account.id, 9), None)
        .await;
    assert_eq!(view.status, StatusCode::OK, "{}", view.body);
    assert_eq!((fetches(&mailbox), sessions(&mailbox)), (2, 2));
}

#[tokio::test]
async fn a_sign_in_rejected_while_reading_marks_the_inbox_and_drops_its_mail() {
    let mailbox = Arc::new(Mailbox::default());
    let h = recorder::harness_with(4, Some(mailbox.clone())).await;
    let account = account(&h.app).await;
    h.app
        .owner("GET", &messages_path(account.id, 9), None)
        .await;
    mailbox
        .reject
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let refused = h
        .app
        .owner("GET", &messages_path(account.id, 11), None)
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    let health = store::account(&h.app.db.pool, account.id)
        .await
        .unwrap()
        .unwrap()
        .health;
    assert_eq!(health, Health::CredentialRejected);

    mailbox
        .reject
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(
        store::set_health(
            &h.app.db.pool,
            account.id,
            account.generation,
            &Health::Connected
        )
        .await
        .unwrap()
    );
    let fetched = fetches(&mailbox);
    h.app
        .owner("GET", &messages_path(account.id, 9), None)
        .await;
    assert_eq!(
        fetches(&mailbox),
        fetched + 1,
        "the cached email was dropped"
    );
}

#[tokio::test]
async fn the_owner_opens_an_email_the_bot_already_read_without_fetching_it_again() {
    let mailbox = Arc::new(Mailbox::default());
    let h = recorder::harness_with(4, Some(mailbox.clone())).await;
    let bot: Uuid = h.app.main_bot().await.parse().unwrap();
    let account = account(&h.app).await;
    store::set_permissions(&h.app.db.pool, account.id, bot, &permissions())
        .await
        .unwrap();
    let turn = mail_event(&h, &account, &format!("INBOX {GATE}"), "Summarize it.").await;
    let message = json!({"folder":"INBOX [gate]","uid_validity":1,"uid":9});
    let (_, token) = h.started(turn).await;
    let read = tool(
        &h.app,
        &token,
        "mail_action",
        json!({"account_id":account.id,"command":{"operation":"read","message":message}}),
    )
    .await;
    assert!(!read.error, "{}", read.body);
    assert_eq!(fetches(&mailbox), 1);

    let view = h
        .app
        .owner("GET", &messages_path(account.id, 9), None)
        .await;
    assert_eq!(view.status, StatusCode::OK, "{}", view.body);
    assert_eq!(view.body["html"], "<p>Hi</p>");
    assert_eq!(fetches(&mailbox), 1, "the bot's read filled the cache");

    let trashed = tool(
        &h.app,
        &token,
        "mail_action",
        json!({"account_id":account.id,"action_id":Uuid::new_v4(),
               "command":{"operation":"trash","message":message}}),
    )
    .await;
    assert!(!trashed.error, "{}", trashed.body);
    h.app
        .owner("GET", &messages_path(account.id, 9), None)
        .await;
    assert_eq!(
        fetches(&mailbox),
        2,
        "a trashed email is read from the server again"
    );
}

#[tokio::test]
async fn a_rejected_sign_in_is_reported_even_for_a_cached_email() {
    let mailbox = Arc::new(Mailbox::default());
    let h = recorder::harness_with(4, Some(mailbox.clone())).await;
    let account = account(&h.app).await;
    let first = h
        .app
        .owner("GET", &messages_path(account.id, 9), None)
        .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    assert!(
        store::set_health(
            &h.app.db.pool,
            account.id,
            account.generation,
            &Health::CredentialRejected,
        )
        .await
        .unwrap()
    );
    h.app
        .owner("GET", &messages_path(account.id, 9), None)
        .await;
    assert_eq!(fetches(&mailbox), 2, "the cache is skipped");
}

#[tokio::test]
async fn the_reader_says_why_a_message_cannot_be_shown() {
    let mailbox = Arc::new(Mailbox::default());
    let h = recorder::harness_with(4, Some(mailbox.clone())).await;
    let account = account(&h.app).await;

    let gone = h
        .app
        .owner("GET", &messages_path(account.id, 10), None)
        .await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND, "{}", gone.body);
    assert_eq!(gone.body["error"]["code"], "message_not_found");

    let unknown = h
        .app
        .owner("GET", &messages_path(Uuid::new_v4(), 9), None)
        .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND, "{}", unknown.body);
    assert_eq!(unknown.body["error"]["code"], "mail_forbidden");

    let cached = h
        .app
        .owner("GET", &messages_path(account.id, 9), None)
        .await;
    assert_eq!(cached.status, StatusCode::OK, "{}", cached.body);
    let paused = set_active(&h, account.id, false).await;
    assert_eq!(paused.status, StatusCode::OK, "{}", paused.body);
    let refused = h
        .app
        .owner("GET", &messages_path(account.id, 9), None)
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert_eq!(refused.body["error"]["code"], "mail_account_paused");
    assert_eq!(
        fetches(&mailbox),
        2,
        "the unknown account and the paused inbox reached no server"
    );
}

/// The first email of `turn` as the owner's conversation lists it.
async fn turn_mail(h: &recorder::Harness, bot: Uuid, turn: Uuid) -> Value {
    let conversation = h
        .app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    conversation.body["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == turn.to_string())
        .unwrap_or_else(|| panic!("the mail turn is listed: {}", conversation.body))["mail"][0]
        .clone()
}

fn arrived() -> MessageRef {
    MessageRef {
        folder: "INBOX [gate]".into(),
        uid_validity: 1,
        uid: 9,
    }
}

/// A mail turn on an inbox the main bot can read and change.
async fn mail_turn(mailbox: &Arc<Mailbox>) -> (recorder::Harness, Uuid, Account, Uuid) {
    let h = recorder::harness_with(4, Some(mailbox.clone())).await;
    let bot: Uuid = h.app.main_bot().await.parse().unwrap();
    let account = account(&h.app).await;
    store::set_permissions(&h.app.db.pool, account.id, bot, &permissions())
        .await
        .unwrap();
    let turn = mail_event(&h, &account, &format!("INBOX {GATE}"), "Triage it.").await;
    (h, bot, account, turn)
}

#[tokio::test]
async fn a_bot_trashes_the_email_and_the_reader_opens_it_in_trash() {
    let mailbox = Arc::new(Mailbox::default());
    let (h, bot, account, turn) = mail_turn(&mailbox).await;
    let (_, token) = h.started(turn).await;
    let trashed = tool(
        &h.app,
        &token,
        "mail_action",
        json!({"account_id":account.id,"action_id":Uuid::new_v4(),
               "command":{"operation":"trash","message":arrived()}}),
    )
    .await;
    assert!(!trashed.error, "{}", trashed.body);

    let mail = turn_mail(&h, bot, turn).await;
    assert_eq!(mail["subject"], "Verify your email");
    assert_eq!(mail["location"]["state"], "trashed");
    assert_eq!(mail["location"]["folder"], "Trash");
    assert_eq!(mail["location"]["by_bot_id"], bot.to_string());
    assert_eq!(mail["location"]["by_turn_id"], turn.to_string());

    let view = h
        .app
        .owner("GET", &messages_path(account.id, 9), None)
        .await;
    assert_eq!(view.status, StatusCode::OK, "{}", view.body);
    assert_eq!(view.body["subject"], "Verify your email");
    assert_eq!(
        view.body["message"],
        json!({"folder":"Trash","uid_validity":1,"uid":21})
    );
    assert_eq!(mailbox.locates(), 0, "the bot's move said where it went");
}

#[tokio::test]
async fn an_email_moved_by_another_client_is_found_by_its_message_id() {
    let mailbox = Arc::new(Mailbox::default());
    let (h, bot, account, turn) = mail_turn(&mailbox).await;
    let archived = MessageRef {
        folder: "Archive".into(),
        uid_validity: 1,
        uid: 31,
    };
    mailbox.leave(&arrived(), archived.clone());

    let view = h
        .app
        .owner("GET", &messages_path(account.id, 9), None)
        .await;
    assert_eq!(view.status, StatusCode::OK, "{}", view.body);
    assert_eq!(
        view.body["message"],
        json!({"folder":"Archive","uid_validity":1,"uid":31})
    );
    let mail = turn_mail(&h, bot, turn).await;
    assert_eq!(mail["location"]["state"], "archived");
    assert_eq!(mail["location"]["by_bot_id"], Value::Null);
    assert_eq!(
        mailbox.commands.lock().unwrap()[0],
        json!({"operation":"locate","message_id":"fixture@coupons.test"})
    );

    let whereabouts = store::whereabouts(&h.app.db.pool, account.id, &arrived())
        .await
        .unwrap()
        .unwrap();
    assert!(
        !store::found(
            &h.app.db.pool,
            whereabouts.event_id,
            &Place::Arrived,
            &Place::Deleted
        )
        .await
        .unwrap(),
        "a reader that saw it before it moved records nothing"
    );
    let again = h
        .app
        .owner("GET", &messages_path(account.id, 9), None)
        .await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
    assert_eq!(mailbox.locates(), 1, "the found place is kept");
}

#[tokio::test]
async fn an_email_deleted_elsewhere_reads_as_deleted_after_one_open() {
    let mailbox = Arc::new(Mailbox::default());
    let (h, bot, account, turn) = mail_turn(&mailbox).await;
    mailbox.leave(&arrived(), None);

    let first = h
        .app
        .owner("GET", &messages_path(account.id, 9), None)
        .await;
    assert_eq!(first.status, StatusCode::NOT_FOUND, "{}", first.body);
    assert_eq!(first.body["error"]["code"], "message_deleted");
    let mail = turn_mail(&h, bot, turn).await;
    assert_eq!(mail["location"]["state"], "deleted");
    assert_eq!(mail["location"]["by_bot_id"], Value::Null);
    assert_eq!(
        mail["subject"], "Verify your email",
        "the row keeps what it was"
    );

    let (fetched, located) = (fetches(&mailbox), mailbox.locates());
    let second = h
        .app
        .owner("GET", &messages_path(account.id, 9), None)
        .await;
    assert_eq!(second.body["error"]["code"], "message_deleted");
    assert_eq!(
        (fetches(&mailbox), mailbox.locates()),
        (fetched, located),
        "a deleted email asks the server nothing"
    );
}

#[tokio::test]
async fn a_failed_search_leaves_the_email_where_it_was() {
    let mailbox = Arc::new(Mailbox::default());
    let (h, bot, account, turn) = mail_turn(&mailbox).await;
    mailbox.leave(&arrived(), None);
    mailbox
        .unsearchable
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let view = h
        .app
        .owner("GET", &messages_path(account.id, 9), None)
        .await;
    assert_eq!(
        view.body["error"]["code"], "mail_transport_failed",
        "{}",
        view.body
    );
    assert_eq!(
        turn_mail(&h, bot, turn).await["location"]["state"],
        "arrived"
    );
}
