use super::*;
use session::Socket;
use std::sync::Mutex;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio_rustls::TlsAcceptor;

fn tls() -> (TlsAcceptor, TlsConnector) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.der().clone()).unwrap();
    let client = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
        )
        .unwrap();
    (
        TlsAcceptor::from(Arc::new(server)),
        TlsConnector::from(Arc::new(client)),
    )
}
fn auth() -> Auth {
    Auth {
        username: "owner@example.org".into(),
        secret: zeroize::Zeroizing::new("fixture-password".into()),
        oauth: false,
    }
}
const RAW:&[u8]=b"Date: Mon, 16 Nov 2026 10:00:00 +0100\r\nFrom: Sender <sender@example.org>\r\nTo: owner@example.org\r\nSubject: New message\r\nMessage-ID: <fixture@example.org>\r\n\r\nA body without marking read.\r\n";
struct ImapFixture {
    endpoint: Endpoint,
    connector: TlsConnector,
    changes: mpsc::Sender<()>,
    idle: oneshot::Receiver<()>,
    commands: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}
async fn imap_fixture() -> ImapFixture {
    imap_fixture_options(false, 14).await
}

async fn imap_fixture_options(start_tls: bool, uid_next: u32) -> ImapFixture {
    imap_fixture_serving(start_tls, uid_next, RAW.to_vec()).await
}

async fn read_from(client: &mut Imap, message: &MessageRef) -> Value {
    let (raw, received_at) = client.fetch_message(message).await.unwrap();
    read(
        &Fetched {
            raw: raw.into(),
            received_at,
        },
        message,
    )
    .unwrap()
}

/// The window `<origin.length>` of a `BODY.PEEK[]` fetch, or the whole.
fn partial(command: &str, len: usize) -> (usize, usize) {
    let Some((_, window)) = command.split_once("BODY.PEEK[]<") else {
        return (0, len);
    };
    let (origin, length) = window
        .trim_end_matches(')')
        .trim_end_matches('>')
        .split_once('.')
        .unwrap();
    let origin: usize = origin.parse().unwrap();
    let start = origin.min(len);
    (start, (start + length.parse::<usize>().unwrap()).min(len))
}

async fn imap_fixture_serving(start_tls: bool, uid_next: u32, raw: Vec<u8>) -> ImapFixture {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = Endpoint {
        host: "localhost".into(),
        port: listener.local_addr().unwrap().port(),
        start_tls,
    };
    let (acceptor, connector) = tls();
    let (changes, mut arrivals) = mpsc::channel(1);
    let (ready, idle) = oneshot::channel();
    let commands = Arc::new(Mutex::new(Vec::new()));
    let recorded = commands.clone();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        if start_tls {
            socket
                .write_all(b"* OK [CAPABILITY IMAP4rev1 STARTTLS] fixture\r\n")
                .await
                .unwrap();
            let mut read = BufReader::new(socket);
            let mut line = String::new();
            read.read_line(&mut line).await.unwrap();
            let (tag, command) = line.trim_end().split_once(' ').unwrap();
            assert_eq!(command, "STARTTLS");
            read.get_mut()
                .write_all(format!("{tag} OK upgrade\r\n").as_bytes())
                .await
                .unwrap();
            socket = read.into_inner();
        }
        let Ok(socket) = acceptor.accept(socket).await else {
            return;
        };
        let (read, mut write) = tokio::io::split(socket);
        let mut read = BufReader::new(read);
        if !start_tls {
            write.write_all(b"* OK [CAPABILITY IMAP4rev1 AUTH=PLAIN AUTH=XOAUTH2 SASL-IR UIDPLUS MOVE IDLE] fixture\r\n").await.unwrap();
        }
        let mut line = String::new();
        let mut ready = Some(ready);
        let mut selected = String::new();
        loop {
            line.clear();
            if read.read_line(&mut line).await.unwrap_or(0) == 0 {
                break;
            }
            let (tag, command) = line.trim_end().split_once(' ').unwrap();
            let upper = command.to_ascii_uppercase();
            let response = if upper.starts_with("AUTHENTICATE PLAIN ") {
                assert_eq!(
                    command.split_whitespace().nth(2).unwrap(),
                    STANDARD.encode(b"\0owner@example.org\0fixture-password")
                );
                format!("{tag} OK authenticated\r\n")
            } else if upper.starts_with("AUTHENTICATE XOAUTH2 ") {
                assert_eq!(
                    command.split_whitespace().nth(2).unwrap(),
                    STANDARD
                        .encode(b"user=owner@example.org\x01auth=Bearer fixture-password\x01\x01")
                );
                format!("{tag} OK authenticated\r\n")
            } else if upper == "CAPABILITY" {
                format!(
                    "* CAPABILITY IMAP4rev1 AUTH=PLAIN AUTH=XOAUTH2 SASL-IR UIDPLUS MOVE IDLE\r\n{tag} OK capabilities\r\n"
                )
            } else {
                recorded.lock().unwrap().push(command.into());
                if upper == "EXAMINE BROKEN" {
                    format!("{tag} NO [UNAVAILABLE] try later\r\n")
                } else if upper.starts_with("EXAMINE ") || upper.starts_with("SELECT ") {
                    selected = command.split_once(' ').unwrap().1.to_owned();
                    format!(
                        "* 3 EXISTS\r\n* OK [UIDVALIDITY 77] valid\r\n* OK [UIDNEXT {uid_next}] next\r\n{tag} OK selected\r\n"
                    )
                } else if upper.starts_with("LIST ") {
                    format!(
                        "* LIST (\\HasNoChildren) \"/\" INBOX\r\n\
                         * LIST (\\HasNoChildren) \"/\" Archive\r\n\
                         * LIST (\\HasNoChildren) \"/\" Trash\r\n\
                         * LIST (\\HasNoChildren) \"/\" Broken\r\n\
                         * LIST (\\HasNoChildren \\All) \"/\" \"[Gmail]/All Mail\"\r\n\
                         {tag} OK list\r\n"
                    )
                } else if upper.starts_with("UID SEARCH HEADER MESSAGE-ID ") {
                    let hit = if selected.contains("All Mail") {
                        " 42"
                    } else {
                        ""
                    };
                    format!("* SEARCH{hit}\r\n{tag} OK search\r\n")
                } else if upper.starts_with("UID SEARCH ") {
                    format!("* SEARCH 11 12 13\r\n{tag} OK search\r\n")
                } else if upper.starts_with("UID FETCH 42") && upper.contains("ENVELOPE") {
                    format!(
                        "* 1 FETCH (UID 42 ENVELOPE (NIL \"Moved\" NIL NIL NIL NIL NIL NIL NIL \"<moved@example.org>\"))\r\n\
                         * 2 FETCH (UID 43 ENVELOPE (NIL \"Decoy\" NIL NIL NIL NIL NIL NIL NIL \"<xmoved@example.org>\"))\r\n\
                         {tag} OK fetch\r\n"
                    )
                } else if upper.starts_with("UID FETCH ") && upper.contains("ENVELOPE") {
                    format!(
                        "* 1 FETCH (UID 11 FLAGS (\\Seen) INTERNALDATE \"16-Nov-2026 09:00:00 +0000\" ENVELOPE (\"Mon, 16 Nov 2026 10:00:00 +0100\" \"Morning\" ((\"=?UTF-8?Q?Caf=C3=A9_Bot?=\" NIL \"a\" \"example.org\")) NIL NIL NIL NIL NIL NIL \"<scan-11@example.org>\"))\r\n\
                         * 2 FETCH (UID 12 FLAGS () INTERNALDATE \"16-Nov-2026 18:30:00 -0500\" ENVELOPE (NIL \"No date\" ((NIL NIL \"b\" \"example.org\")) NIL NIL NIL NIL NIL NIL NIL))\r\n\
                         * 3 FETCH (UID 13 FLAGS () INTERNALDATE \"15-Nov-2026 08:00:00 +0000\" ENVELOPE (\"sometime last week\" \"Moved in\" ((NIL NIL \"c\" \"example.org\")) NIL NIL NIL NIL NIL NIL NIL))\r\n\
                         {tag} OK fetch\r\n"
                    )
                } else if upper.starts_with("UID FETCH ") && upper.contains("BODY.PEEK[]") {
                    let mut items = String::new();
                    if upper.contains("RFC822.SIZE") {
                        items.push_str(&format!(" RFC822.SIZE {}", raw.len()));
                    }
                    if upper.contains("INTERNALDATE") {
                        items.push_str(" INTERNALDATE \"16-Nov-2026 09:00:00 +0000\"");
                    }
                    let (start, end) = partial(&upper, raw.len());
                    let origin = if upper.contains("BODY.PEEK[]<") {
                        format!("<{start}>")
                    } else {
                        String::new()
                    };
                    write
                        .write_all(
                            format!(
                                "* 1 FETCH (UID 11{items} BODY[]{origin} {{{}}}\r\n",
                                end - start
                            )
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
                    write.write_all(&raw[start..end]).await.unwrap();
                    format!(")\r\n{tag} OK body\r\n")
                } else if upper.starts_with("UID STORE ") {
                    format!("* 1 FETCH (UID 11 FLAGS (\\Seen))\r\n{tag} OK store\r\n")
                } else if upper.starts_with("UID MOVE ") || upper.starts_with("UID COPY ") {
                    format!("{tag} OK [COPYUID 77 11 21] moved\r\n")
                } else if upper.starts_with("UID EXPUNGE ") {
                    format!("* 1 EXPUNGE\r\n{tag} OK expunged\r\n")
                } else if upper == "IDLE" {
                    write.write_all(b"+ idling\r\n").await.unwrap();
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(());
                    }
                    if arrivals.recv().await.is_none() {
                        break;
                    }
                    "* 4 EXISTS\r\n".into()
                } else {
                    panic!("unexpected fixture command {command}")
                }
            };
            if write.write_all(response.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    ImapFixture {
        endpoint,
        connector,
        changes,
        idle,
        commands,
        task,
    }
}

#[tokio::test]
async fn uid_identity_peek_flags_move_and_bounded_arrivals() {
    let fixture = imap_fixture().await;
    let mut client = Imap::connect(&fixture.endpoint, &auth(), &fixture.connector)
        .await
        .unwrap();
    let baseline = scan(&mut client, None).await.unwrap();
    assert_eq!(
        baseline.cursor,
        Cursor {
            uid_validity: 77,
            last_uid: 13
        }
    );
    assert!(baseline.arrivals.is_empty());
    let next = scan(
        &mut client,
        Some(&Cursor {
            uid_validity: 77,
            last_uid: 10,
        }),
    )
    .await
    .unwrap();
    assert_eq!(next.arrivals.len(), 3);
    assert_eq!(next.cursor.last_uid, 13);
    let envelope = &next.arrivals[0].envelope;
    assert_eq!(
        (
            envelope.message_id.as_deref(),
            envelope.subject.as_deref(),
            envelope.sender.as_deref()
        ),
        (
            Some("scan-11@example.org"),
            Some("Morning"),
            Some("Café Bot")
        )
    );
    assert_eq!(
        next.arrivals[1].envelope.sender.as_deref(),
        Some("b@example.org"),
        "a sender without a name shows its address"
    );
    let message = MessageRef {
        folder: "INBOX".into(),
        uid_validity: 77,
        uid: 11,
    };
    assert_eq!(client.body(&message).await.unwrap(), RAW);
    client.flag(&message, Flag::Seen, true).await.unwrap();
    client.move_message(&message, "Archive").await.unwrap();
    client
        .capabilities
        .retain(|c| !c.to_string().eq_ignore_ascii_case("MOVE"));
    let copied = client.move_message(&message, "Archive").await.unwrap();
    assert_eq!(copied["message"]["uid"], 21);
    client.uid_expunge(sequence(11).unwrap()).await.unwrap();
    let stale = MessageRef {
        uid_validity: 78,
        ..message
    };
    assert_eq!(
        client.body(&stale).await.unwrap_err().code.as_str(),
        "stale_message_reference"
    );
    let commands = fixture.commands.lock().unwrap();
    assert!(commands.iter().any(|c| c.contains("BODY.PEEK[]")));
    assert!(
        commands
            .iter()
            .any(|c| c.starts_with("UID STORE 11 +FLAGS"))
    );
    assert!(
        commands
            .iter()
            .any(|c| c.starts_with("UID MOVE 11 Archive"))
    );
    assert!(!commands.iter().any(|c| c == "EXPUNGE" || c == "CLOSE"));
    fixture.task.abort();
}

#[tokio::test]
async fn a_small_message_arrives_in_one_fetch_and_stays_unread() {
    let fixture = imap_fixture().await;
    let mut client = Imap::connect(&fixture.endpoint, &auth(), &fixture.connector)
        .await
        .unwrap();
    let message = MessageRef {
        folder: "INBOX".into(),
        uid_validity: 77,
        uid: 11,
    };
    let (raw, received_at) = client.fetch_message(&message).await.unwrap();
    assert_eq!(raw, RAW);
    assert_eq!(received_at.as_deref(), Some("2026-11-16T09:00:00Z"));
    assert_eq!(
        *fixture.commands.lock().unwrap(),
        [
            "SELECT INBOX",
            "UID FETCH 11 (UID RFC822.SIZE INTERNALDATE BODY.PEEK[]<0.1048576>)"
        ]
    );
    fixture.task.abort();
}

#[tokio::test]
async fn a_kept_session_reads_again_without_signing_in() {
    let fixture = imap_fixture().await;
    let mut session: Box<dyn MailReader> = Box::new(
        Imap::connect(&fixture.endpoint, &auth(), &fixture.connector)
            .await
            .unwrap(),
    );
    let message = MessageRef {
        folder: "INBOX".into(),
        uid_validity: 77,
        uid: 11,
    };
    for _ in 0..2 {
        assert_eq!(&session.read_message(&message).await.unwrap().raw[..], RAW);
    }
    let fetch = "UID FETCH 11 (UID RFC822.SIZE INTERNALDATE BODY.PEEK[]<0.1048576>)";
    assert_eq!(
        *fixture.commands.lock().unwrap(),
        ["SELECT INBOX", fetch, "SELECT INBOX", fetch],
        "the fixture accepts one connection, so the second read reused it"
    );
    fixture.task.abort();
}

#[tokio::test]
async fn search_and_read_return_sender_date_and_received_time_newest_first() {
    let fixture = imap_fixture().await;
    let mut client = Imap::connect(&fixture.endpoint, &auth(), &fixture.connector)
        .await
        .unwrap();
    let found = client.search_messages("INBOX", None, 50).await.unwrap();
    let dated: Vec<_> = found["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["message"]["uid"].as_u64().unwrap(),
                m["date"].clone(),
                m["received_at"].clone(),
            )
        })
        .collect();
    assert_eq!(
        dated,
        vec![
            (12, Value::Null, json!("2026-11-16T23:30:00Z")),
            (
                11,
                json!("2026-11-16T10:00:00+01:00"),
                json!("2026-11-16T09:00:00Z")
            ),
            (
                13,
                json!("sometime last week"),
                json!("2026-11-15T08:00:00Z")
            ),
        ],
        "newest received first; a missing Date is null and an unparsable one stays raw"
    );
    let read = read_from(
        &mut client,
        &MessageRef {
            folder: "INBOX".into(),
            uid_validity: 77,
            uid: 11,
        },
    )
    .await;
    assert_eq!(read["date"], "2026-11-16T10:00:00+01:00");
    assert_eq!(read["received_at"], "2026-11-16T09:00:00Z");
    fixture.task.abort();
}

/// A message with a PDF and an image attachment, over the old 2 MiB limit.
fn large_message() -> Vec<u8> {
    let raw = MessageBuilder::new()
        .from("sender@example.org")
        .to("owner@example.org")
        .subject("Shareholder documents")
        .text_body("The annual report is attached.")
        .attachment("application/pdf", "report.pdf", &b"%PDF-1.4 report"[..])
        .attachment("image/png", "scan.png", vec![7u8; 2_400_000])
        .write_to_vec()
        .unwrap();
    assert!(raw.len() > 3 * 1024 * 1024, "{}", raw.len());
    raw
}

#[tokio::test]
async fn an_email_over_the_old_limit_reads_in_chunks_and_lists_its_attachments() {
    let fixture = imap_fixture_serving(false, 14, large_message()).await;
    let mut client = Imap::connect(&fixture.endpoint, &auth(), &fixture.connector)
        .await
        .unwrap();
    let message = MessageRef {
        folder: "INBOX".into(),
        uid_validity: 77,
        uid: 11,
    };
    let read = read_from(&mut client, &message).await;
    assert!(read["body"].as_str().unwrap().contains("annual report"));
    let listed: Vec<_> = read["attachments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            (
                a["part"].clone(),
                a["name"].clone(),
                a["media_type"].clone(),
            )
        })
        .collect();
    assert_eq!(
        listed,
        vec![
            (json!(0), json!("report.pdf"), json!("application/pdf")),
            (json!(1), json!("scan.png"), json!("image/png")),
        ]
    );
    let fetches: Vec<String> = fixture
        .commands
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.contains("BODY.PEEK[]<"))
        .cloned()
        .collect();
    assert!(fetches.len() >= 4, "fetched in 1 MiB chunks: {fetches:?}");
    assert!(fetches[0].contains("BODY.PEEK[]<0.1048576>"), "{fetches:?}");
    let opened = client.open_attachments(&message, &[0, 5]).await.unwrap();
    assert_eq!(opened["subject"], "Shareholder documents");
    assert_eq!(
        STANDARD
            .decode(opened["attachments"][0]["data_base64"].as_str().unwrap())
            .unwrap(),
        b"%PDF-1.4 report"
    );
    assert_eq!(opened["attachments"][1]["error"], "attachment_not_found");
    fixture.task.abort();
}

#[tokio::test]
async fn idle_notifies_on_new_mail() {
    let fixture = imap_fixture().await;
    let stop = CancellationToken::new();
    let credentials = auth();
    let waiting = wait_for_mail(
        &fixture.endpoint,
        &credentials,
        &Cursor {
            uid_validity: 77,
            last_uid: 13,
        },
        stop,
        &fixture.connector,
    );
    let trigger = async {
        fixture.idle.await.unwrap();
        fixture.changes.send(()).await.unwrap();
    };
    let (result, ()) = timeout(Duration::from_secs(3), async {
        tokio::join!(waiting, trigger)
    })
    .await
    .unwrap();
    result.unwrap();
    assert!(fixture.commands.lock().unwrap().iter().any(|c| c == "IDLE"));
    fixture.task.abort();
}

#[tokio::test]
async fn idle_cancellation_drops_the_connection_promptly() {
    let fixture = imap_fixture().await;
    let stop = CancellationToken::new();
    let credentials = auth();
    let waiting = wait_for_mail(
        &fixture.endpoint,
        &credentials,
        &Cursor {
            uid_validity: 77,
            last_uid: 13,
        },
        stop.clone(),
        &fixture.connector,
    );
    let cancel = async {
        fixture.idle.await.unwrap();
        stop.cancel();
    };
    let (result, ()) = timeout(Duration::from_secs(3), async {
        tokio::join!(waiting, cancel)
    })
    .await
    .unwrap();
    result.unwrap();
    fixture.task.abort();
}

#[tokio::test]
async fn idle_stays_connected_when_a_minute_passes_without_mail() {
    let fixture = imap_fixture().await;
    let stop = CancellationToken::new();
    let watching_stop = stop.clone();
    let endpoint = fixture.endpoint.clone();
    let connector = fixture.connector.clone();
    let task = tokio::spawn(async move {
        wait_for_mail(
            &endpoint,
            &auth(),
            &Cursor {
                uid_validity: 77,
                last_uid: 13,
            },
            watching_stop,
            &connector,
        )
        .await
    });
    fixture.idle.await.unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    assert!(
        !task.is_finished(),
        "Quiet inboxes retain their IDLE connection."
    );
    stop.cancel();
    task.await.unwrap().unwrap();
    fixture.task.abort();
}

#[tokio::test]
async fn mail_arriving_between_scan_and_idle_is_reconciled_immediately() {
    let fixture = imap_fixture().await;
    wait_for_mail(
        &fixture.endpoint,
        &auth(),
        &Cursor {
            uid_validity: 77,
            last_uid: 10,
        },
        CancellationToken::new(),
        &fixture.connector,
    )
    .await
    .unwrap();
    assert!(
        !fixture
            .commands
            .lock()
            .unwrap()
            .iter()
            .any(|command| command == "IDLE")
    );
    fixture.task.abort();
}

#[tokio::test]
async fn untrusted_tls_certificate_is_rejected() {
    let fixture = imap_fixture().await;
    assert!(Imap::open(&fixture.endpoint, &auth()).await.is_err());
    fixture.task.abort();
}

#[tokio::test]
async fn smtp_tls_sends_blind_recipient_only_in_envelope() {
    smtp_fixture_send(false).await;
}

#[tokio::test]
async fn smtp_starttls_upgrades_before_authentication() {
    smtp_fixture_send(true).await;
}

async fn smtp_fixture_send(start_tls: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = Endpoint {
        host: "localhost".into(),
        port: listener.local_addr().unwrap().port(),
        start_tls,
    };
    let (acceptor, connector) = tls();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        if start_tls {
            socket.write_all(b"220 fixture ESMTP\r\n").await.unwrap();
            let mut read = BufReader::new(socket);
            let mut line = String::new();
            read.read_line(&mut line).await.unwrap();
            assert!(line.starts_with("EHLO "));
            read.get_mut()
                .write_all(b"250-fixture\r\n250 STARTTLS\r\n")
                .await
                .unwrap();
            line.clear();
            read.read_line(&mut line).await.unwrap();
            assert_eq!(line, "STARTTLS\r\n");
            read.get_mut().write_all(b"220 upgrade\r\n").await.unwrap();
            socket = read.into_inner();
        }
        let socket = acceptor.accept(socket).await.unwrap();
        let (read, mut write) = tokio::io::split(socket);
        let mut reader = BufReader::new(read);
        if !start_tls {
            write.write_all(b"220 fixture ESMTP\r\n").await.unwrap();
        }
        let mut line = String::new();
        let mut recipients = Vec::new();
        let mut body = String::new();
        loop {
            line.clear();
            assert_ne!(reader.read_line(&mut line).await.unwrap(), 0);
            let response: &[u8] = if line.starts_with("EHLO ") {
                b"250-fixture\r\n250 AUTH PLAIN XOAUTH2\r\n".as_slice()
            } else if line.starts_with("AUTH PLAIN ") {
                b"235 authenticated\r\n"
            } else if line.starts_with("MAIL FROM:") {
                assert!(line.contains("owner@example.org"));
                b"250 sender accepted\r\n"
            } else if line.starts_with("RCPT TO:") {
                recipients.push(line.clone());
                b"250 recipient accepted\r\n"
            } else if line == "DATA\r\n" {
                write.write_all(b"354 send message\r\n").await.unwrap();
                loop {
                    line.clear();
                    reader.read_line(&mut line).await.unwrap();
                    if line == ".\r\n" {
                        break;
                    }
                    body.push_str(&line);
                }
                write.write_all(b"250 queued\r\n").await.unwrap();
                break;
            } else {
                panic!("unexpected SMTP fixture command");
            };
            write.write_all(response).await.unwrap();
        }
        (recipients, body)
    });
    let mut client = Smtp::connect(&endpoint, &auth(), &connector).await.unwrap();
    let draft = Draft {
        to: vec!["recipient@example.org".into()],
        cc: Vec::new(),
        bcc: vec!["blind@example.org".into()],
        subject: "fixture".into(),
        body: "Hello\n.hidden\n".into(),
        attachments: Vec::new(),
        reply_to: None,
        message_id: None,
    };
    let bytes = compose("owner@example.org", &draft, None).unwrap();
    client
        .send(
            smtp_address("owner@example.org").unwrap().into(),
            vec![
                smtp_address("recipient@example.org").unwrap().into(),
                smtp_address("blind@example.org").unwrap().into(),
            ],
            bytes,
            Default::default(),
        )
        .await
        .unwrap();
    let (recipients, body) = task.await.unwrap();
    assert_eq!(recipients.len(), 2);
    assert!(recipients.iter().any(|r| r.contains("blind@example.org")));
    assert!(!body.to_ascii_lowercase().contains("bcc:"));
    assert!(!body.contains("blind@example.org"));
    assert!(body.contains("..hidden\r\n"));
}

#[test]
fn an_exact_reply_draft_joins_the_thread_without_taking_its_recipients() {
    let raw = b"From: sender@example.org\r\nTo: owner@example.org\r\nSubject: Hello\r\nMessage-ID: <original@example.org>\r\n\r\ntext";
    let message = MessageParser::default().parse(raw).unwrap();
    let draft = Draft {
        to: vec!["ada@example.org".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "Owner's own subject".into(),
        body: "approved text".into(),
        attachments: Vec::new(),
        reply_to: None,
        message_id: None,
    };
    let bytes = compose("owner@example.org", &draft, Some(thread(&message))).unwrap();
    let parsed = MessageParser::default().parse(&bytes).unwrap();
    assert_eq!(parsed.in_reply_to().as_text(), Some("original@example.org"));
    assert_eq!(parsed.subject(), Some("Owner's own subject"));
    assert_eq!(super::mime::addresses(parsed.to()), vec!["ada@example.org"]);
}

#[test]
fn reply_all_keeps_thread_and_excludes_self_and_blind_recipients() {
    let raw=b"From: sender@example.org\r\nReply-To: reply@example.org\r\nTo: owner@example.org, colleague@example.org\r\nCc: colleague@example.org, cc@example.org\r\nBcc: hidden@example.org\r\nSubject: Hello\r\nMessage-ID: <original@example.org>\r\nReferences: <earlier@example.org>\r\n\r\ntext";
    let message = MessageParser::default().parse(raw).unwrap();
    let (draft, headers) = reply(&message, "owner@example.org", "answer", true).unwrap();
    assert_eq!(draft.to, vec!["reply@example.org", "colleague@example.org"]);
    assert_eq!(draft.cc, vec!["cc@example.org"]);
    assert!(draft.bcc.is_empty());
    let bytes = compose("owner@example.org", &draft, Some(headers)).unwrap();
    let parsed = MessageParser::default().parse(&bytes).unwrap();
    assert_eq!(parsed.in_reply_to().as_text(), Some("original@example.org"));
    assert!(
        String::from_utf8(bytes)
            .unwrap()
            .contains("earlier@example.org")
    );
}

#[tokio::test]
async fn imap_starttls_and_xoauth2_authenticate_over_encrypted_socket() {
    let fixture = imap_fixture_options(true, 14).await;
    let mut credentials = auth();
    credentials.oauth = true;
    let mut client = Imap::connect(&fixture.endpoint, &credentials, &fixture.connector)
        .await
        .unwrap();
    assert_eq!(scan(&mut client, None).await.unwrap().cursor.last_uid, 13);
    fixture.task.abort();
}

#[tokio::test]
async fn scan_pages_uid_gaps_and_rebaselines_a_new_uid_validity() {
    let fixture = imap_fixture_options(false, 114).await;
    let mut client = Imap::connect(&fixture.endpoint, &auth(), &fixture.connector)
        .await
        .unwrap();
    let first = scan(
        &mut client,
        Some(&Cursor {
            uid_validity: 77,
            last_uid: 0,
        }),
    )
    .await
    .unwrap();
    assert_eq!(first.cursor.last_uid, 100);
    assert!(first.more);
    assert_eq!(first.arrivals.len(), 3);
    let second = scan(&mut client, Some(&first.cursor)).await.unwrap();
    assert_eq!(second.cursor.last_uid, 113);
    assert!(!second.more);
    assert!(second.arrivals.is_empty());
    let reset = scan(
        &mut client,
        Some(&Cursor {
            uid_validity: 78,
            last_uid: 0,
        }),
    )
    .await
    .unwrap();
    assert_eq!(reset.cursor.last_uid, 113);
    assert!(reset.arrivals.is_empty());
    fixture.task.abort();
}

#[test]
fn attachments_round_trip_and_forward_preserves_the_original_message() {
    let draft = Draft {
        to: vec!["recipient@example.org".into()],
        cc: Vec::new(),
        bcc: vec!["blind@example.org".into()],
        subject: "Files".into(),
        body: "See attachment".into(),
        attachments: vec![OutgoingAttachment {
            name: "document.txt".into(),
            media_type: "text/plain".into(),
            data_base64: STANDARD.encode(b"original attachment"),
        }],
        reply_to: None,
        message_id: None,
    };
    let raw = compose("owner@example.org", &draft, None).unwrap();
    let parsed = MessageParser::default().parse(&raw).unwrap();
    assert_eq!(parsed.attachment_count(), 1);
    assert_eq!(
        parsed.attachment(0).unwrap().contents(),
        b"original attachment"
    );
    assert_eq!(
        parsed.attachment(0).unwrap().attachment_name(),
        Some("document.txt")
    );
    assert!(parsed.bcc().is_none());
    let forwarded = compose_forward("owner@example.org", &draft, &raw).unwrap();
    let forwarded = MessageParser::default().parse(&forwarded).unwrap();
    assert_eq!(forwarded.attachment_count(), 2);
    assert_eq!(
        forwarded.attachment(1).unwrap().attachment_name(),
        Some("forwarded.eml")
    );
    let with_blind = [b"Bcc: blind@example.org\r\n".as_slice(), &raw].concat();
    let parsed = MessageParser::default().parse(&with_blind).unwrap();
    assert_eq!(addresses(parsed.bcc()), vec!["blind@example.org"]);
    let redacted = mime::forward_source(&with_blind).unwrap();
    let redacted = MessageParser::default().parse(&redacted).unwrap();
    assert!(redacted.bcc().is_none());
    assert_eq!(
        redacted.attachment(0).unwrap().contents(),
        b"original attachment"
    );
    let mut invalid = draft.clone();
    invalid.attachments[0].data_base64 = "!".into();
    assert_eq!(
        compose("owner@example.org", &invalid, None)
            .unwrap_err()
            .code
            .as_str(),
        "invalid_mail_attachment"
    );
    let mut oversized = draft;
    oversized.attachments[0].data_base64 =
        STANDARD.encode(vec![0; crate::connectors::types::MAX_ATTACHMENT_TOTAL + 1]);
    assert_eq!(
        compose("owner@example.org", &oversized, None)
            .unwrap_err()
            .code
            .as_str(),
        "mail_attachments_too_large"
    );
}

#[test]
fn sender_date_keeps_offsets_and_falls_back_to_the_header_text() {
    assert_eq!(
        header_date(Some(" Mon, 16 Nov 2026 10:00:00 -0500\r\n")).as_deref(),
        Some("2026-11-16T10:00:00-05:00")
    );
    assert_eq!(
        header_date(Some("31 Feb 2026 10:00:00 +0100")).as_deref(),
        Some("31 Feb 2026 10:00:00 +0100"),
        "an impossible calendar date is not presented as a timestamp"
    );
    assert_eq!(header_date(Some("  ")), None);
    assert_eq!(header_date(None), None);
}

#[tokio::test]
async fn a_tls_write_reaches_the_peer_before_the_reply_is_awaited() {
    const SIZE: usize = 1024 * 1024;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (acceptor, connector) = tls();
    let _server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut tls = acceptor.accept(socket).await.unwrap();
        let mut bytes = vec![0; 16384];
        let mut received = 0;
        while received < SIZE {
            tokio::time::sleep(Duration::from_millis(1)).await;
            let n = tls.read(&mut bytes).await.unwrap();
            assert_ne!(n, 0, "the client closed the connection");
            received += n;
        }
        tls.write_all(b"250 queued\r\n").await.unwrap();
        tls.flush().await.unwrap();
    });
    let tcp = tokio::net::TcpSocket::new_v4().unwrap();
    tcp.set_send_buffer_size(4096).unwrap();
    let tcp = tcp.connect(address).await.unwrap();
    let name = ServerName::try_from("localhost").unwrap();
    let mut socket = Socket::Tls(Box::new(connector.connect(name, tcp).await.unwrap()));
    socket.write(&vec![b'x'; SIZE]).await.unwrap();
    let mut reply = [0; 16];
    timeout(Duration::from_secs(30), socket.read(&mut reply))
        .await
        .expect("the peer never received the end of the write")
        .unwrap();
}

#[derive(Clone, Copy, PartialEq)]
enum Ending {
    RejectRecipient,
    CloseAfterMessage,
    HangAfterMessage,
    RejectMessage,
    Accept,
}

async fn send_until(ending: Ending, draft: &Draft) -> (Result<(), MailError>, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = Endpoint {
        host: "localhost".into(),
        port: listener.local_addr().unwrap().port(),
        start_tls: false,
    };
    let (acceptor, connector) = tls();
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let socket = acceptor.accept(socket).await.unwrap();
        let (read, mut write) = tokio::io::split(socket);
        let mut reader = BufReader::new(read);
        write.write_all(b"220 fixture ESMTP\r\n").await.unwrap();
        let mut line = String::new();
        let mut message = String::new();
        loop {
            line.clear();
            if !matches!(reader.read_line(&mut line).await, Ok(1..)) {
                return message;
            }
            let response: &[u8] = if line.starts_with("EHLO ") {
                b"250-fixture\r\n250 AUTH PLAIN XOAUTH2\r\n"
            } else if line.starts_with("AUTH PLAIN ") {
                b"235 authenticated\r\n"
            } else if line.starts_with("MAIL FROM:") {
                b"250 sender accepted\r\n"
            } else if line.starts_with("RCPT TO:") {
                if ending == Ending::RejectRecipient {
                    b"550 5.1.1 no such mailbox\r\n"
                } else {
                    b"250 recipient accepted\r\n"
                }
            } else if line == "DATA\r\n" {
                write.write_all(b"354 send message\r\n").await.unwrap();
                loop {
                    line.clear();
                    reader.read_line(&mut line).await.unwrap();
                    if line == ".\r\n" {
                        break;
                    }
                    message.push_str(&line);
                }
                match ending {
                    Ending::CloseAfterMessage => return message,
                    Ending::HangAfterMessage => std::future::pending().await,
                    Ending::RejectMessage => b"552 5.3.4 refused: private body text\r\n",
                    _ => b"250 queued\r\n",
                }
            } else {
                panic!("unexpected SMTP fixture command");
            };
            write.write_all(response).await.unwrap();
        }
    });
    let mut client = Smtp::connect(&endpoint, &auth(), &connector).await.unwrap();
    let recipients = draft
        .to
        .iter()
        .map(|a| smtp_address(a).unwrap().into())
        .collect();
    let mut phase = SendPhase::Connect;
    let result = timeout(
        Duration::from_secs(2),
        client.transmit(
            smtp_address("owner@example.org").unwrap().into(),
            recipients,
            compose("owner@example.org", draft, None).unwrap(),
            &mut phase,
        ),
    )
    .await
    .unwrap_or_else(|_| Err(SmtpClientError::Io(io::ErrorKind::TimedOut.into())))
    .map_err(|error| smtp_failure(error).at(phase));
    drop(client);
    if ending == Ending::HangAfterMessage {
        task.abort();
        return (result, String::new());
    }
    (result, task.await.unwrap())
}

fn private_draft() -> Draft {
    Draft {
        to: vec!["recipient@example.org".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "fixture".into(),
        body: "private body text".into(),
        attachments: Vec::new(),
        reply_to: None,
        message_id: Some("draft-1234@example.org".into()),
    }
}

#[tokio::test]
async fn a_send_failing_before_the_whole_message_is_written_sent_nothing() {
    let (result, message) = send_until(Ending::RejectRecipient, &private_draft()).await;
    let error = result.unwrap_err();
    assert_eq!(error.phase, Some(SendPhase::Envelope));
    assert!(!error.may_have_sent());
    assert_eq!(
        error.reason(),
        "the mail server refused a recipient with code 550 while naming the sender and \
         recipients"
    );
    assert!(message.is_empty());

    let (result, message) = send_until(Ending::RejectMessage, &private_draft()).await;
    let error = result.unwrap_err();
    assert_eq!(error.phase, Some(SendPhase::Refused));
    assert!(!error.may_have_sent(), "a refused message was not accepted");
    assert_eq!(
        error.reason(),
        "the mail server refused the message with code 552 after the whole message was sent",
        "the reply text can quote the message, so only its code is kept"
    );
    assert!(message.contains("private body text"));
}

#[tokio::test]
async fn a_send_losing_the_reply_after_the_whole_message_may_have_gone_out() {
    let (result, message) = send_until(Ending::CloseAfterMessage, &private_draft()).await;
    let error = result.unwrap_err();
    assert_eq!(error.phase, Some(SendPhase::Reply));
    assert!(error.may_have_sent());
    assert_eq!(
        error.reason(),
        "the mail server closed the connection while waiting for the server's reply after the \
         whole message was sent"
    );
    assert!(
        message.contains("Message-ID: <draft-1234@example.org>"),
        "{message}"
    );

    let capture = Arc::new(Mutex::new(Vec::new()));
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || LogWriter(writer.clone()))
        .finish();
    let (status, receipt) = tracing::subscriber::with_default(subscriber, || {
        super::super::failure_receipt(uuid::Uuid::nil(), uuid::Uuid::nil(), &error)
    });
    assert_eq!(status, crate::connector_core::actions::Outcome::Uncertain);
    assert_eq!(receipt["phase"], "reply");
    let logged = String::from_utf8(capture.lock().unwrap().clone()).unwrap();
    assert!(logged.contains("WARN"), "{logged}");
    assert!(logged.contains("closed the connection"), "{logged}");
    assert!(logged.contains("phase=\"reply\""), "{logged}");
    for private in [
        "private body text",
        "fixture-password",
        "recipient@example.org",
    ] {
        assert!(!logged.contains(private), "{logged}");
    }
}

#[tokio::test]
async fn a_send_cancelled_while_awaiting_the_reply_may_have_gone_out() {
    let (result, _) = send_until(Ending::HangAfterMessage, &private_draft()).await;
    let error = result.unwrap_err();
    assert_eq!(error.phase, Some(SendPhase::Reply), "{}", error.reason());
    assert!(error.may_have_sent());
}

#[tokio::test]
async fn an_accepted_send_carries_its_message_id() {
    let (result, message) = send_until(Ending::Accept, &private_draft()).await;
    result.unwrap();
    assert!(
        message.contains("Message-ID: <draft-1234@example.org>"),
        "{message}"
    );
}

struct LogWriter(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn reader(raw: &str) -> Value {
    let parsed = MessageParser::default().parse(raw.as_bytes()).unwrap();
    let message = MessageRef {
        folder: "INBOX".into(),
        uid_validity: 1,
        uid: 9,
    };
    serde_json::to_value(mime::view(&parsed, &message, None)).unwrap()
}

#[test]
fn the_reader_gets_the_html_part_with_its_cid_images_and_lists_only_real_attachments() {
    let value = reader(
        "From: Coupon Co <hello@coupons.test>\r\nTo: me@x.test\r\nSubject: Verify\r\n\
         MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
         --m\r\nContent-Type: multipart/related; boundary=\"r\"\r\n\r\n\
         --r\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
         <p>Hi <img src=\"cid:Logo@x\"></p>\r\n\
         --r\r\nContent-Type: image/png\r\nContent-ID: <logo@x>\r\nContent-Transfer-Encoding: base64\r\n\r\n\
         iVBORw0KGgo=\r\n--r--\r\n\
         --m\r\nContent-Type: application/pdf; name=\"terms.pdf\"\r\n\
         Content-Disposition: attachment; filename=\"terms.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\n\
         JVBERi0=\r\n--m--\r\n",
    );
    assert_eq!(value["html"], "<p>Hi <img src=\"cid:Logo@x\"></p>");
    assert_eq!(value["text"], "Hi\n");
    assert_eq!(
        value["from"],
        json!([{"name":"Coupon Co","address":"hello@coupons.test"}])
    );
    assert_eq!(
        value["inline"],
        json!([{"content_id":"logo@x","media_type":"image/png","data_base64":"iVBORw0KGgo="}])
    );
    assert_eq!(
        value["attachments"],
        json!([{"part":1,"name":"terms.pdf","media_type":"application/pdf","size":5}])
    );
}

#[test]
fn a_plain_text_email_has_no_html_for_the_reader() {
    let value = reader(
        "From: a@x.test\r\nTo: me@x.test\r\nSubject: Hi\r\nContent-Type: text/plain\r\n\r\nJust text.\r\n",
    );
    assert_eq!(value["html"], Value::Null);
    assert_eq!(value["text"], "Just text.\r\n");
    assert_eq!(value["attachments"], json!([]));
}

fn bot_read(raw: &str) -> Value {
    let fetched = Fetched {
        raw: raw.as_bytes().into(),
        received_at: None,
    };
    let message = MessageRef {
        folder: "INBOX".into(),
        uid_validity: 1,
        uid: 9,
    };
    read(&fetched, &message).unwrap()
}

const BOOKING_HTML: &str = "<p>Your booking is confirmed.</p>\
    <table><tr><td style=\"background:#06c\"><a href=\"https://hotel.test/reply?id=1&amp;t=2\" \
    style=\"color:#fff\">Reply</a></td></tr></table>\
    <p>Read <a href=\"https://hotel.test/terms\">our terms</a>.</p>\
    <a href=\"https://hotel.test/home\"><img src=\"https://hotel.test/logo.png\" alt=\"Hotel logo\"></a>\
    <p><a href=\"https://evil.test/login\">https://bank.test/login</a></p>\
    <p><a href=\"https://hotel.test/terms\">terms again</a> <a href=\"javascript:alert(1)\">run</a></p>";

#[test]
fn a_bot_reading_html_mail_gets_every_link_url_inline_and_listed() {
    let value = bot_read(&format!(
        "From: Hotel <stay@hotel.test>\r\nTo: me@x.test\r\nSubject: Booking\r\n\
         MIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\n\r\n{BOOKING_HTML}\r\n"
    ));
    let body = value["body"].as_str().unwrap();
    for shown in [
        "Reply <https://hotel.test/reply?id=1&t=2>",
        "our terms <https://hotel.test/terms>",
        "Hotel logo <https://hotel.test/home>",
        "https://bank.test/login <https://evil.test/login>",
    ] {
        assert!(body.contains(shown), "{shown:?} missing from {body:?}");
    }
    assert!(!body.contains("javascript:"), "{body:?}");
    assert_eq!(
        value["links"],
        json!([
            {"text":"Reply","url":"https://hotel.test/reply?id=1&t=2"},
            {"text":"our terms","url":"https://hotel.test/terms"},
            {"text":"Hotel logo","url":"https://hotel.test/home"},
            {"text":"https://bank.test/login","url":"https://evil.test/login"},
        ])
    );
    assert_eq!(value["source"], "external_email");
}

#[test]
fn a_bot_reads_the_html_part_over_a_plain_text_stub() {
    let value = bot_read(&format!(
        "From: Hotel <stay@hotel.test>\r\nTo: me@x.test\r\nSubject: Booking\r\n\
         MIME-Version: 1.0\r\nContent-Type: multipart/alternative; boundary=\"a\"\r\n\r\n\
         --a\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n\
         View this email in your browser.\r\n\
         --a\r\nContent-Type: text/html; charset=utf-8\r\n\r\n{BOOKING_HTML}\r\n--a--\r\n"
    ));
    let body = value["body"].as_str().unwrap();
    assert!(
        body.contains("Reply <https://hotel.test/reply?id=1&t=2>"),
        "{body:?}"
    );
    assert!(!body.contains("in your browser"), "{body:?}");
}

#[test]
fn links_past_the_text_limit_are_still_listed() {
    let filler = "<p>word</p>".repeat(8000);
    let value = bot_read(&format!(
        "From: a@x.test\r\nTo: me@x.test\r\nSubject: Long\r\nContent-Type: text/html\r\n\r\n\
         {filler}<a href=\"https://x.test/end\">End</a>\r\n"
    ));
    assert_eq!(value["truncated"], true);
    assert_eq!(value["body"].as_str().unwrap().chars().count(), 32000);
    assert_eq!(
        value["links"],
        json!([{"text":"End","url":"https://x.test/end"}])
    );
}

#[test]
fn a_forwarded_html_message_quotes_its_link_urls() {
    let raw = format!(
        "From: Hotel <stay@hotel.test>\r\nTo: me@x.test\r\nSubject: Booking\r\n\
         MIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\n\r\n{BOOKING_HTML}\r\n"
    );
    let parsed = MessageParser::default().parse(raw.as_bytes()).unwrap();
    let draft = mime::forward(&parsed, vec!["friend@x.test".into()], "FYI");
    assert_eq!(draft.subject, "Fwd: Booking");
    assert!(draft.body.starts_with("FYI\n\n---------- Forwarded message ----------\nFrom: stay@hotel.test\nSubject: Booking\n\n"), "{}", draft.body);
    assert!(
        draft
            .body
            .contains("Reply <https://hotel.test/reply?id=1&t=2>"),
        "{}",
        draft.body
    );
}

#[test]
fn a_fuller_plain_part_wins_over_an_html_button_and_its_link_is_still_listed() {
    let value = bot_read(
        "From: Shop <orders@shop.test>\r\nTo: me@x.test\r\nSubject: Ready\r\n\
         MIME-Version: 1.0\r\nContent-Type: multipart/alternative; boundary=\"a\"\r\n\r\n\
         --a\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n\
         Your order is ready. Your pickup code is 4821. Show it at the desk.\r\n\
         --a\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
         <img src=\"https://shop.test/banner.png\"><a href=\"https://shop.test/order/7\">Details</a>\r\n--a--\r\n",
    );
    assert_eq!(
        value["body"],
        "Your order is ready. Your pickup code is 4821. Show it at the desk."
    );
    assert_eq!(
        value["links"],
        json!([{"text":"Details","url":"https://shop.test/order/7"}])
    );
}

#[test]
fn an_inline_html_part_after_a_plain_one_still_gives_its_links() {
    let value = bot_read(
        "From: Hotel <stay@hotel.test>\r\nTo: me@x.test\r\nSubject: Booking\r\n\
         MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
         --m\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nHi.\r\n\
         --m\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
         <a href=\"https://hotel.test/reply\">Reply</a>\r\n--m--\r\n",
    );
    assert_eq!(
        value["links"],
        json!([{"text":"Reply","url":"https://hotel.test/reply"}])
    );
}

#[test]
fn every_inline_html_part_gives_its_links() {
    let value = bot_read(
        "From: Hotel <stay@hotel.test>\r\nTo: me@x.test\r\nSubject: Booking\r\n\
         MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
         --m\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
         <a href=\"https://hotel.test/terms\">Terms</a>\r\n\
         --m\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
         <a href=\"https://hotel.test/reply\">Reply</a>\r\n--m--\r\n",
    );
    assert_eq!(
        value["body"],
        "Terms <https://hotel.test/terms>\nReply <https://hotel.test/reply>"
    );
    assert_eq!(
        value["links"],
        json!([
            {"text":"Terms","url":"https://hotel.test/terms"},
            {"text":"Reply","url":"https://hotel.test/reply"},
        ])
    );
}

#[tokio::test]
async fn locate_finds_the_exact_message_id_after_archive_and_trash() {
    let fixture = imap_fixture().await;
    let mut client = Imap::connect(&fixture.endpoint, &auth(), &fixture.connector)
        .await
        .unwrap();
    let config = MailConfig {
        archive_folder: "Archive".into(),
        trash_folder: "Trash".into(),
        ..Provider::Gmail.config("owner@example.org".into()).unwrap()
    };
    let found = client
        .locate(&config, "moved@example.org", Some("Receipts"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        found,
        MessageRef {
            folder: "[Gmail]/All Mail".into(),
            uid_validity: 77,
            uid: 42,
        },
        "43's Message-ID only contains the one sought"
    );
    let examined: Vec<String> = fixture
        .commands
        .lock()
        .unwrap()
        .iter()
        .filter_map(|c| c.strip_prefix("EXAMINE ").map(str::to_owned))
        .collect();
    assert_eq!(
        examined,
        ["Archive", "Trash", "\"[Gmail]/All Mail\""],
        "an unlisted folder is not searched"
    );
    let unreadable = client
        .locate(&config, "moved@example.org", Some("Broken"))
        .await;
    assert!(
        unreadable.is_err(),
        "a folder that fails to open fails the search instead of reading as empty"
    );
    fixture.task.abort();
}
