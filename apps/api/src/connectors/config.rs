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

impl MailCommand {
    pub fn validate(&self) -> Result<(), MailError> {
        fn msg(m: &MessageRef) -> bool {
            folder(&m.folder) && m.uid > 0 && m.uid_validity > 0
        }
        fn draft(d: &Draft) -> bool {
            use base64::Engine;
            let mut bytes = 0;
            if d.attachments.len() > 10
                || d.attachments.iter().any(|a| {
                    if !text(&a.name, 255)
                        || a.name.contains(['/', '\\'])
                        || !text(&a.media_type, 127)
                        || !a.media_type.is_ascii()
                        || !a.media_type.contains('/')
                        || a.media_type.contains([' ', ';'])
                        || a.data_base64.len() > 1_400_000
                    {
                        return true;
                    }
                    match base64::engine::general_purpose::STANDARD.decode(&a.data_base64) {
                        Ok(data) => {
                            bytes += data.len();
                            bytes > 1_048_576
                        }
                        Err(_) => true,
                    }
                })
            {
                return false;
            }
            d.to.len() + d.cc.len() + d.bcc.len() <= 50
                && !d.to.is_empty()
                && d.to.iter().chain(&d.cc).chain(&d.bcc).all(|s| address(s))
                && d.subject.len() <= 998
                && !d.subject.contains(['\r', '\n'])
                && d.body.len() <= 32_000
        }
        if let Self::Search { limit, .. } = self
            && !(1..=100).contains(limit)
        {
            return Err(MailError::new("invalid_mail_command").detail("limit must be 1 to 100"));
        }
        let valid = match self {
            Self::Folders => true,
            Self::Search {
                folder: f, query, ..
            } => folder(f) && query.as_ref().is_none_or(|s| text(s, 1024)),
            Self::Read { message }
            | Self::SetRead { message, .. }
            | Self::SetFlagged { message, .. }
            | Self::Archive { message }
            | Self::Trash { message }
            | Self::Delete { message } => msg(message),
            Self::Attachment { message, part } => msg(message) && *part < 1000,
            Self::Move { message, folder: f } => msg(message) && folder(f),
            Self::Draft { draft: d } | Self::Send { draft: d } => {
                draft(d) && d.reply_to.as_ref().is_none_or(msg)
            }
            Self::Reply { message, body, .. } => msg(message) && body.len() <= 32_000,
            Self::Forward {
                message, to, body, ..
            } => {
                msg(message)
                    && (1..=50).contains(&to.len())
                    && to.iter().all(|s| address(s))
                    && body.len() <= 32_000
            }
            Self::CreateFolder { folder: f } | Self::DeleteFolder { folder: f } => folder(f),
            Self::RenameFolder { folder: f, name } => folder(f) && folder(name),
        };
        if valid {
            Ok(())
        } else {
            Err(MailError::new("invalid_mail_command"))
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
        assert!(MailCommand::Send { draft }.validate().is_err());
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
