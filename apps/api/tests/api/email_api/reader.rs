//! The owner's email reader: the message behind a mail-triggered turn,
//! fetched with the saved credential and never through a bot.

use super::*;

/// Answers only `view`, recording each command it receives.
#[derive(Default)]
struct Mailbox {
    commands: std::sync::Mutex<Vec<Value>>,
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
            MailCommand::View { message } if message.uid == 9 => {
                Ok(json!({"message":message,"subject":"Verify your email","html":"<p>Hi</p>"}))
            }
            _ => Err(MailError::new("message_not_found")),
        }
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
        json!([{"account_id":account.id,"message":message}])
    );

    let read = h
        .app
        .owner("GET", &messages_path(account.id, 9), None)
        .await;
    assert_eq!(read.status, StatusCode::OK, "{}", read.body);
    assert_eq!(read.body["subject"], "Verify your email");
    assert_eq!(read.body["html"], "<p>Hi</p>");
    assert_eq!(
        *mailbox.commands.lock().unwrap(),
        [json!({"operation":"view","message":message})],
        "one view, and nothing that marks it read"
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
    assert_eq!(mailbox.commands.lock().unwrap().len(), 1);
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

    let paused = h
        .app
        .owner(
            "PATCH",
            &format!("/v1/connectors/email/accounts/{}", account.id),
            Some(json!({"active":false})),
        )
        .await;
    assert_eq!(paused.status, StatusCode::OK, "{}", paused.body);
    let refused = h
        .app
        .owner("GET", &messages_path(account.id, 9), None)
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert_eq!(refused.body["error"]["code"], "mail_account_paused");
    assert_eq!(
        mailbox.commands.lock().unwrap().len(),
        1,
        "only the existing account's first read reached the server"
    );
}
