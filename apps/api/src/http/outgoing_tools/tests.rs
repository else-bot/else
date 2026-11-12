use super::*;
use crate::app::outgoing::Edits;
use sqlx::types::Json;

#[tokio::test]
async fn connected_send_uses_the_owner_edit_approved_after_the_initial_read() {
    let db = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::migrate(&db).await.unwrap();
    for platform in [Platform::Telegram, Platform::Discord] {
        let bot = Uuid::new_v4();
        let conversation = Uuid::new_v4();
        let turn = Uuid::new_v4();
        let item = Uuid::new_v4();
        sqlx::query("INSERT INTO bots(id,name) VALUES(?1,'Approved send')")
            .bind(bot)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO conversations(id,bot_id) VALUES(?1,?2)")
            .bind(conversation)
            .bind(bot)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO turns(id,bot_id,conversation_id,request_id,request_hash,turn_no,owner_item_id,provider_id,status,attempt) VALUES(?1,?2,?3,?4,zeroblob(32),1,?5,'synthetic','running',1)")
            .bind(turn)
            .bind(bot)
            .bind(conversation)
            .bind(Uuid::new_v4())
            .bind(item)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO items(id,conversation_id,seq,turn_id,kind,text) VALUES(?1,?2,1,?3,'user','Send an update')")
            .bind(item)
            .bind(conversation)
            .bind(turn)
            .execute(&db)
            .await
            .unwrap();
        let account_id = Uuid::new_v4();
        let content = Content::Message(Message {
            platform,
            account_id: Some(account_id),
            account: None,
            destination: Some("100".into()),
            task: None,
            reply_to: Some("42".into()),
            body: "Provisional text A".into(),
        });
        let draft = outgoing_drafts::propose(&db, turn, &content, None)
            .await
            .unwrap();
        let initial = outgoing_drafts::load_for(&db, bot, draft.id).await.unwrap();
        assert_eq!(initial.content, Json(content));
        outgoing_drafts::approve(
            &db,
            bot,
            draft.id,
            initial.version,
            Edits {
                subject: None,
                body: Some("Owner-approved text B"),
            },
            false,
        )
        .await
        .unwrap();
        let action_id = Uuid::new_v4();
        let claimed = outgoing_drafts::claim(
            &db,
            turn,
            draft.id,
            &Delivery::Social {
                account_id,
                action_id,
            },
        )
        .await
        .unwrap();
        let ConnectedPayload::Social {
            account_id: sent_account,
            action_id: sent_action,
            message,
            destination,
        } = connected_payload(&claimed).unwrap()
        else {
            panic!("a native message must use its social transport");
        };
        assert_eq!(sent_account, account_id);
        assert_eq!(sent_action, action_id);
        assert_eq!(message.platform, platform);
        assert_eq!(destination, "100");
        assert_eq!(message.reply_to.as_deref(), Some("42"));
        assert_eq!(message.body, "Owner-approved text B");
        assert_eq!(claimed.version, claimed.approved_version.unwrap());
    }
}
