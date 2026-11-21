use super::*;

pub(super) async fn wait_for_mail(
    endpoint: &Endpoint,
    auth: &Auth,
    cursor: &Cursor,
    stop: CancellationToken,
    connector: &TlsConnector,
) -> Result<(), MailError> {
    tokio::select! {
        _=stop.cancelled()=>Ok(()),
        result=async {
            let mut client=timeout(COMMAND_TIMEOUT, Imap::connect(endpoint,auth,connector)).await.map_err(|_|MailError::new(MailErrorCode::MailTimeout))??;
            if !client.has("IDLE") {return Err(MailError::new(MailErrorCode::MailIdleNotSupported));}
            let selected = client.examine(mailbox("INBOX")?,Default::default()).await.map_err(failure)?;
            let validity = selected.uid_validity.ok_or_else(|| MailError::new(MailErrorCode::MailMissingUidValidity))?.get();
            let next = selected.uid_next.ok_or_else(|| MailError::new(MailErrorCode::MailMissingUidNext))?.get();
            if validity != cursor.uid_validity || next.saturating_sub(1) > cursor.last_uid { return Ok(()); }
            client.idle().await
        }=>result,
    }
}

impl Imap {
    async fn idle(&mut self) -> Result<(), MailError> {
        let mut coroutine = ImapIdle::new(
            Arc::new(AtomicBool::new(false)),
            ImapIdleOptions {
                timeout: Some(IDLE_TIMEOUT),
            },
        );
        let mut arg: Option<Vec<u8>> = None;
        let mut bytes = [0; 16384];
        let mut budget = RESPONSE_LIMIT;
        let idle = async {
            loop {
                let state = coroutine.resume(&mut self.fragmentizer, arg.as_deref());
                arg = None;
                match state {
                    ImapCoroutineState::Complete(result) => return result.map_err(failure),
                    ImapCoroutineState::Yielded(ImapIdleYield::Event(_)) => return Ok(()),
                    ImapCoroutineState::Yielded(ImapIdleYield::WantsTag) => {
                        arg = Some(self.tags.generate().as_ref().as_bytes().to_vec())
                    }
                    ImapCoroutineState::Yielded(ImapIdleYield::WantsWrite(bytes)) => {
                        self.socket.write(&bytes).await.map_err(io_failure)?
                    }
                    ImapCoroutineState::Yielded(ImapIdleYield::WantsRead) => {
                        arg = Some(self.socket.exchange(&mut budget, &mut bytes).await?);
                    }
                }
            }
        };
        timeout(IDLE_TIMEOUT, idle).await.unwrap_or(Ok(()))
    }
}

impl Imap {
    /// Each listed message's Message-ID, subject and sender, by UID, in
    /// the selected folder.
    async fn envelopes(
        &mut self,
        uids: &[u32],
    ) -> Result<std::collections::HashMap<u32, Envelope>, MailError> {
        if uids.is_empty() {
            return Ok(Default::default());
        }
        let set = SequenceSet::try_from(
            uids.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
                .as_str(),
        )
        .map_err(invalid)?;
        let rows = self
            .fetch(
                set,
                items(vec![
                    MessageDataItemName::Uid,
                    MessageDataItemName::Envelope,
                ]),
                ImapMessageFetchOptions {
                    uid: true,
                    ..Default::default()
                },
            )
            .await
            .map_err(failure)?;
        Ok(rows
            .values()
            .filter_map(|row| {
                let mut uid = None;
                let mut envelope = None;
                for item in row.as_ref().iter() {
                    match item {
                        MessageDataItem::Uid(id) => uid = Some(id.get()),
                        MessageDataItem::Envelope(e) => envelope = Some(envelope_of(e)),
                        _ => (),
                    }
                }
                Some((uid?, envelope?))
            })
            .collect())
    }

    /// The first folder holding a message whose Message-ID is exactly
    /// `message_id`, searched in `hint` first, then where a moved message
    /// most likely sits. Only folders the server lists are searched, and
    /// one that fails to open fails the search: `None` means it is in
    /// none of them.
    pub(super) async fn locate(
        &mut self,
        config: &MailConfig,
        message_id: &str,
        hint: Option<&str>,
    ) -> Result<Option<MessageRef>, MailError> {
        let listed = self
            .list(mailbox("")?, ListMailbox::try_from("*").map_err(invalid)?)
            .await
            .map_err(failure)?;
        let special = |attribute: &str| {
            listed
                .iter()
                .filter(|(_, _, attributes)| {
                    attributes
                        .iter()
                        .any(|a| a.to_string().eq_ignore_ascii_case(attribute))
                })
                .map(|(name, _, _)| mailbox_name(name))
                .collect::<Vec<_>>()
        };
        let names: std::collections::HashSet<String> = listed
            .iter()
            .map(|(name, _, _)| mailbox_name(name))
            .collect();
        let mut folders: Vec<String> = hint.map(str::to_owned).into_iter().collect();
        folders.extend([config.archive_folder.clone(), config.trash_folder.clone()]);
        folders.extend(special("\\All"));
        folders.extend(special("\\Archive"));
        folders.extend(special("\\Trash"));
        folders.extend(special("\\Junk"));
        folders.push("INBOX".into());
        let mut searched = std::collections::HashSet::new();
        for folder in folders {
            if !(names.contains(&folder) || folder == "INBOX") || !searched.insert(folder.clone()) {
                continue;
            }
            let selected = self
                .examine(mailbox(&folder)?, Default::default())
                .await
                .map_err(failure)?;
            let uid_validity = selected
                .uid_validity
                .ok_or_else(|| MailError::new(MailErrorCode::MailMissingUidValidity))?
                .get();
            let key = SearchKey::Header(
                "Message-ID".try_into().map_err(invalid)?,
                message_id.to_owned().try_into().map_err(invalid)?,
            );
            let found: Vec<u32> = self
                .search(
                    Vec1::try_from(vec![key]).expect("one search key"),
                    ImapMessageSearchOptions { uid: true },
                )
                .await
                .map_err(failure)?
                .into_iter()
                .map(NonZeroU32::get)
                .take(100)
                .collect();
            // SEARCH HEADER matches substrings, so each hit's own
            // Message-ID must be the one sought.
            let exact = self
                .envelopes(&found)
                .await?
                .into_iter()
                .filter(|(_, e)| e.message_id.as_deref() == Some(message_id))
                .map(|(uid, _)| uid)
                .max();
            if let Some(uid) = exact {
                return Ok(Some(MessageRef {
                    folder,
                    uid_validity,
                    uid,
                }));
            }
        }
        Ok(None)
    }
}

pub(super) async fn scan(client: &mut Imap, cursor: Option<&Cursor>) -> Result<Scan, MailError> {
    let data = client
        .examine(mailbox("INBOX")?, Default::default())
        .await
        .map_err(failure)?;
    let uid_validity = data
        .uid_validity
        .ok_or_else(|| MailError::new(MailErrorCode::MailMissingUidValidity))?
        .get();
    let last = data
        .uid_next
        .ok_or_else(|| MailError::new(MailErrorCode::MailMissingUidNext))?
        .get()
        - 1;
    let Some(previous) = cursor.filter(|c| c.uid_validity == uid_validity) else {
        return Ok(Scan {
            cursor: Cursor {
                uid_validity,
                last_uid: last,
            },
            arrivals: Vec::new(),
            more: false,
        });
    };
    if previous.last_uid >= last {
        return Ok(Scan {
            cursor: previous.clone(),
            arrivals: Vec::new(),
            more: false,
        });
    }
    let end = previous.last_uid.saturating_add(100).min(last);
    let range = SequenceSet::from(
        NonZeroU32::new(previous.last_uid + 1).unwrap()..=NonZeroU32::new(end).unwrap(),
    );
    let criteria = Vec1::try_from(vec![SearchKey::Uid(range)]).expect("one search key");
    let found = client
        .search(criteria, ImapMessageSearchOptions { uid: true })
        .await
        .map_err(failure)?;
    let uids: Vec<u32> = found
        .into_iter()
        .map(NonZeroU32::get)
        .filter(|&uid| uid > previous.last_uid && uid <= end)
        .collect();
    let mut envelopes = client.envelopes(&uids).await?;
    let arrivals = uids
        .into_iter()
        .map(|uid| Arrival {
            message: MessageRef {
                folder: "INBOX".into(),
                uid_validity,
                uid,
            },
            envelope: envelopes.remove(&uid).unwrap_or_default(),
        })
        .collect();
    Ok(Scan {
        cursor: Cursor {
            uid_validity,
            last_uid: end,
        },
        arrivals,
        more: end < last,
    })
}
