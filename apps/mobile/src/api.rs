//! The phone's connection to the server: the client the desktop uses too
//! (`else_client`), on the phone's own network runtime, with the phone's
//! way of checking Tailscale. Requests run away from NSH's engine thread;
//! the engine awaits their results.

use std::net::IpAddr;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use else_client::api::Tailnet;
pub use else_client::api::{
    Client, ClientError, CreateBotRequest, LiveEvent, SendRequest, StreamMessage,
};
use else_client::model::outgoing::DraftAttachment;
use else_wire::items::{Attachment, MessagePart};
use uuid::Uuid;

use crate::server::Profile;

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("else-net")
            .enable_all()
            .build()
            .expect("the network runtime starts")
    })
}

/// A client for the saved server.
pub fn connect(profile: &Profile) -> Result<Arc<Client>, String> {
    Client::with_tailnet(
        profile.origin.clone(),
        &profile.token,
        runtime().handle().clone(),
        Arc::new(PhoneTailnet),
    )
    .map(Arc::new)
}

/// A Tailscale IP is reachable once the Tailscale app has connected this
/// phone; the phone cannot list the tailnet's devices.
struct PhoneTailnet;

impl Tailnet for PhoneTailnet {
    fn verify(
        &self,
        _: IpAddr,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async {
            if crate::server::tailnet_connected() {
                Ok(())
            } else {
                Err("Connect Tailscale on this iPhone first. The key is only sent over your tailnet.".into())
            }
        })
    }
}

/// A message the owner sends: its text, then its files.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub text: String,
    pub files: Vec<Attachment>,
    /// The draft this message answers, so the bot can send it; the server
    /// closes it if the reply moves on without sending it.
    pub draft: Option<DraftAttachment>,
}

impl Message {
    /// The request that sends it under `request_id`.
    pub fn request(&self, request_id: Uuid) -> SendRequest {
        let parts = self.parts();
        let text = if parts.is_empty() {
            self.text.clone()
        } else {
            MessagePart::flatten(&parts)
        };
        SendRequest {
            request_id,
            text,
            provider_id: None,
            reaction: None,
            parts,
            open_draft: self.draft.clone(),
            answers_notice: None,
        }
    }

    /// Its text then its files; none for a message without files, which is
    /// plain text.
    fn parts(&self) -> Vec<MessagePart> {
        if self.files.is_empty() {
            return Vec::new();
        }
        let text = (!self.text.is_empty()).then(|| MessagePart::Text {
            text: format!("{}\n", self.text),
        });
        text.into_iter()
            .chain(
                self.files
                    .iter()
                    .map(|file| MessagePart::File { file: file.clone() }),
            )
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_with_files_spells_them_after_its_text() {
        let file = Attachment {
            id: Uuid::nil(),
            name: "receipt.pdf".into(),
            media_type: "application/pdf".into(),
            size: 3,
        };
        let message = Message {
            text: "For March".into(),
            files: vec![file.clone()],
            draft: None,
        };
        let request = message.request(Uuid::nil());
        assert_eq!(request.text, "For March\n[File: receipt.pdf]");
        assert_eq!(
            request.parts,
            vec![
                MessagePart::Text {
                    text: "For March\n".into()
                },
                MessagePart::File { file }
            ]
        );
        let plain = Message {
            text: "hi".into(),
            files: vec![],
            draft: None,
        };
        let request = plain.request(Uuid::nil());
        assert_eq!((request.text.as_str(), request.parts.len()), ("hi", 0));
    }
}
