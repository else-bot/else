use super::*;
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
const RAW:&[u8]=b"From: Sender <sender@example.org>\r\nTo: owner@example.org\r\nSubject: New message\r\nMessage-ID: <fixture@example.org>\r\n\r\nA body without marking read.\r\n";
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
                if upper.starts_with("EXAMINE ") || upper.starts_with("SELECT ") {
                    format!(
                        "* 3 EXISTS\r\n* OK [UIDVALIDITY 77] valid\r\n* OK [UIDNEXT {uid_next}] next\r\n{tag} OK selected\r\n"
                    )
                } else if upper.starts_with("UID SEARCH ") {
                    format!("* SEARCH 11 12 13\r\n{tag} OK search\r\n")
                } else if upper.starts_with("UID FETCH ") && upper.contains("RFC822.SIZE") {
                    format!(
                        "* 1 FETCH (UID 11 RFC822.SIZE {})\r\n{tag} OK size\r\n",
                        RAW.len()
                    )
                } else if upper.starts_with("UID FETCH ") && upper.contains("BODY.PEEK[]") {
                    write
                        .write_all(
                            format!("* 1 FETCH (UID 11 BODY[] {{{}}}\r\n", RAW.len()).as_bytes(),
                        )
                        .await
                        .unwrap();
                    write.write_all(RAW).await.unwrap();
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
        client.body(&stale).await.unwrap_err().code,
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
            .code,
        "invalid_mail_attachment"
    );
    let mut oversized = draft;
    oversized.attachments[0].data_base64 = STANDARD.encode(vec![0; 1024 * 1024 + 1]);
    assert_eq!(
        compose("owner@example.org", &oversized, None)
            .unwrap_err()
            .code,
        "mail_attachments_too_large"
    );
}
