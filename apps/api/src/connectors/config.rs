use super::types::*;

impl Provider {
    pub fn config(self, address: String) -> Result<MailConfig, MailError> {
        let (imap, smtp, port, archive, trash, drafts, sent) = match self {
            Self::Gmail => (
                "imap.gmail.com",
                "smtp.gmail.com",
                465,
                "[Gmail]/All Mail",
                "[Gmail]/Trash",
                "[Gmail]/Drafts",
                "[Gmail]/Sent Mail",
            ),
            Self::Microsoft => (
                "outlook.office365.com",
                "smtp.office365.com",
                587,
                "Archive",
                "Deleted Items",
                "Drafts",
                "Sent Items",
            ),
            Self::Yahoo => (
                "imap.mail.yahoo.com",
                "smtp.mail.yahoo.com",
                465,
                "Archive",
                "Trash",
                "Draft",
                "Sent",
            ),
            Self::Icloud => (
                "imap.mail.me.com",
                "smtp.mail.me.com",
                587,
                "Archive",
                "Deleted Messages",
                "Drafts",
                "Sent Messages",
            ),
            Self::Imap => return Err(MailError::new("custom_endpoints_required")),
        };
        Ok(MailConfig {
            provider: self,
            username: address.clone(),
            address,
            imap: Endpoint {
                host: imap.into(),
                port: 993,
                start_tls: false,
            },
            smtp: Endpoint {
                host: smtp.into(),
                port,
                start_tls: port == 587,
            },
            archive_folder: archive.into(),
            trash_folder: trash.into(),
            drafts_folder: drafts.into(),
            sent_folder: sent.into(),
        })
    }

    pub fn oauth_endpoints(self) -> Result<(&'static str, &'static str, &'static str), MailError> {
        match self {
            Self::Gmail => Ok((
                "https://accounts.google.com/o/oauth2/v2/auth",
                "https://oauth2.googleapis.com/token",
                "https://mail.google.com/",
            )),
            Self::Microsoft => Ok((
                "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
                "https://login.microsoftonline.com/common/oauth2/v2.0/token",
                "offline_access https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send",
            )),
            Self::Yahoo => Ok((
                "https://api.login.yahoo.com/oauth2/request_auth",
                "https://api.login.yahoo.com/oauth2/get_token",
                "mail-r mail-w",
            )),
            _ => Err(MailError::new("oauth_unavailable")),
        }
    }
}

fn text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}
fn address(value: &str) -> bool {
    text(value, 254) && value.contains('@') && !value.contains([' ', '<', '>'])
}
fn folder(value: &str) -> bool {
    text(value, 512)
}

impl MailConfig {
    pub fn validate(&self) -> Result<(), MailError> {
        if !address(&self.address)
            || !text(&self.username, 254)
            || [&self.imap, &self.smtp].iter().any(|e| {
                e.port == 0
                    || !text(&e.host, 253)
                    || !e.host.is_ascii()
                    || e.host.contains(['/', ':', ' ', '@'])
            })
            || [
                &self.archive_folder,
                &self.trash_folder,
                &self.drafts_folder,
                &self.sent_folder,
            ]
            .iter()
            .any(|s| !folder(s))
        {
            return Err(MailError::new("invalid_mail_config"));
        }
        if self.provider != Provider::Imap {
            let preset = self.provider.config(self.address.clone())?;
            if self.imap.host != preset.imap.host
                || self.imap.port != preset.imap.port
                || self.imap.start_tls != preset.imap.start_tls
                || self.smtp.host != preset.smtp.host
                || self.smtp.port != preset.smtp.port
                || self.smtp.start_tls != preset.smtp.start_tls
            {
                return Err(MailError::new("provider_endpoint_mismatch"));
            }
        }
        Ok(())
    }
}

impl Credential {
    pub fn validate(&self, provider: Provider) -> Result<(), MailError> {
        let valid = match self {
            Self::Password { password } => {
                !password.is_empty()
                    && password.len() <= 4096
                    && !matches!(provider, Provider::Gmail | Provider::Microsoft)
            }
            Self::OAuth2 {
                client_id,
                refresh_token,
                client_secret,
                ..
            } => {
                provider.oauth_endpoints().is_ok()
                    && text(client_id, 1024)
                    && !refresh_token.is_empty()
                    && refresh_token.len() <= 8192
                    && client_secret.as_ref().is_none_or(|s| s.len() <= 4096)
            }
        };
        if valid {
            Ok(())
        } else {
            Err(MailError::new("invalid_mail_credential"))
        }
    }
}

/// A rule a mail command broke, named for the bot: which field and its range.
type Rule = Result<(), &'static str>;

fn ensure(ok: bool, rule: &'static str) -> Rule {
    if ok { Ok(()) } else { Err(rule) }
}

fn message_ref(m: &MessageRef) -> Rule {
    ensure(folder(&m.folder), "folder must be 1 to 512 bytes of text")?;
    ensure(
        m.uid > 0 && m.uid_validity > 0,
        "message needs its uid and uid_validity from a search",
    )
}

fn body(text: &str) -> Rule {
    ensure(text.len() <= 32_000, "body must be at most 32000 bytes")
}

fn recipients<'a>(mut all: impl Iterator<Item = &'a String>) -> Rule {
    ensure(
        all.all(|s| address(s)),
        "each recipient must be one email address",
    )
}

fn draft(d: &Draft) -> Rule {
    use base64::Engine;
    ensure(
        d.attachments.len() <= 10,
        "a draft takes at most 10 attachments",
    )?;
    let mut bytes = 0;
    for a in &d.attachments {
        ensure(
            text(&a.name, 255) && !a.name.contains(['/', '\\']),
            "an attachment name must be 1 to 255 bytes without slashes",
        )?;
        ensure(
            text(&a.media_type, 127)
                && a.media_type.is_ascii()
                && a.media_type.contains('/')
                && !a.media_type.contains([' ', ';']),
            "an attachment media_type must look like text/plain",
        )?;
        let data = (a.data_base64.len() <= 1_400_000)
            .then(|| {
                base64::engine::general_purpose::STANDARD
                    .decode(&a.data_base64)
                    .ok()
            })
            .flatten();
        let Some(data) = data else {
            return Err("an attachment must be base64 of at most 1 MiB");
        };
        bytes += data.len();
    }
    ensure(bytes <= 1_048_576, "attachments must total at most 1 MiB")?;
    ensure(!d.to.is_empty(), "to needs at least one address")?;
    ensure(
        d.to.len() + d.cc.len() + d.bcc.len() <= 50,
        "to, cc and bcc take at most 50 addresses together",
    )?;
    recipients(d.to.iter().chain(&d.cc).chain(&d.bcc))?;
    ensure(
        d.subject.len() <= 998 && !d.subject.contains(['\r', '\n']),
        "subject must be one line of at most 998 bytes",
    )?;
    body(&d.body)
}

impl MailCommand {
    pub fn validate(&self) -> Result<(), MailError> {
        self.rule()
            .map_err(|rule| MailError::new("invalid_mail_command").detail(rule))
    }

    fn rule(&self) -> Rule {
        let folder_name = |f: &str| ensure(folder(f), "folder must be 1 to 512 bytes of text");
        match self {
            Self::Folders => Ok(()),
            Self::Search {
                folder: f,
                query,
                limit,
                ..
            } => {
                folder_name(f)?;
                ensure(
                    query.as_ref().is_none_or(|s| text(s, 1024)),
                    "query must be 1 to 1024 bytes of text",
                )?;
                ensure((1..=100).contains(limit), "limit must be 1 to 100")
            }
            Self::Read { message }
            | Self::SetRead { message, .. }
            | Self::SetFlagged { message, .. }
            | Self::Archive { message }
            | Self::Trash { message }
            | Self::Delete { message } => message_ref(message),
            Self::Attachment { message, part } => {
                message_ref(message)?;
                ensure(*part < 1000, "part must be below 1000")
            }
            Self::Move { message, folder: f } => {
                message_ref(message)?;
                folder_name(f)
            }
            Self::Send { draft: d } => {
                draft(d)?;
                d.reply_to.as_ref().map_or(Ok(()), message_ref)
            }
            Self::Reply {
                message, body: b, ..
            } => {
                message_ref(message)?;
                body(b)
            }
            Self::Forward {
                message,
                to,
                body: b,
                ..
            } => {
                message_ref(message)?;
                ensure((1..=50).contains(&to.len()), "to takes 1 to 50 addresses")?;
                recipients(to.iter())?;
                body(b)
            }
            Self::CreateFolder { folder: f } | Self::DeleteFolder { folder: f } => folder_name(f),
            Self::RenameFolder { folder: f, name } => {
                folder_name(f)?;
                ensure(folder(name), "name must be 1 to 512 bytes of text")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_endpoints_and_mail_headers_are_validated_at_input() {
        for provider in [
            Provider::Gmail,
            Provider::Microsoft,
            Provider::Yahoo,
            Provider::Icloud,
        ] {
            let mut config = provider.config("owner@example.com".into()).unwrap();
            assert!(config.validate().is_ok());
            config.imap.host = "other.example.com".into();
            assert!(config.validate().is_err());
        }
        let draft = Draft {
            to: vec!["owner@example.com".into()],
            cc: vec![],
            bcc: vec![],
            subject: "hello\r\nBcc: attacker@example.com".into(),
            body: String::new(),
            attachments: vec![],
            reply_to: None,
        };
        assert_eq!(
            MailCommand::Send { draft }
                .validate()
                .unwrap_err()
                .to_string(),
            "invalid_mail_command: subject must be one line of at most 998 bytes"
        );
        let message = MessageRef {
            folder: "INBOX".into(),
            uid_validity: 1,
            uid: 0,
        };
        for (command, rule) in [
            (
                MailCommand::Search {
                    folder: String::new(),
                    query: None,
                    limit: 10,
                },
                "folder must be 1 to 512 bytes of text",
            ),
            (
                MailCommand::Read {
                    message: message.clone(),
                },
                "message needs its uid and uid_validity from a search",
            ),
            (
                MailCommand::RenameFolder {
                    folder: "INBOX".into(),
                    name: String::new(),
                },
                "name must be 1 to 512 bytes of text",
            ),
        ] {
            assert_eq!(
                command.validate().unwrap_err().to_string(),
                format!("invalid_mail_command: {rule}"),
                "each rule names its field"
            );
        }
        let search = |limit| MailCommand::Search {
            folder: "INBOX".into(),
            query: None,
            limit,
        };
        assert!(search(100).validate().is_ok());
        for limit in [0, 200] {
            assert_eq!(
                search(limit).validate().unwrap_err().to_string(),
                "invalid_mail_command: limit must be 1 to 100"
            );
        }
        assert!(
            !MailCommand::Send {
                draft: Draft {
                    to: vec!["owner@example.com".into()],
                    cc: vec![],
                    bcc: vec![],
                    subject: "hello".into(),
                    body: String::new(),
                    attachments: vec![],
                    reply_to: None
                }
            }
            .allowed(&Permissions {
                read: true,
                write: true,
                send: false,
                wake: false
            })
        );
    }
}
