//! Linking a new device by QR code.

use super::{client::start, inbound::Refuse, *};

/// How long a scanned code may take to finish linking.
const LINK_TIMEOUT: Duration = Duration::from_secs(60);
/// How long to wait for WhatsApp to send a code, or the next one.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const UNREACHABLE: &str = "WhatsApp could not be reached. Check this computer's internet connection, then show a new code.";

/// Where a pairing is: waiting for WhatsApp, showing a code that expires
/// after its timeout, or linking the phone that scanned it.
enum Stage {
    Connecting,
    Showing(Duration),
    Linking(Jid),
}
impl Stage {
    /// How long the stage may last before the pairing gives up.
    fn limit(&self) -> Duration {
        match self {
            Self::Connecting => CONNECT_TIMEOUT,
            Self::Showing(code) => *code + CONNECT_TIMEOUT,
            Self::Linking(_) => LINK_TIMEOUT,
        }
    }
    fn timed_out(&self) -> Error {
        match self {
            Self::Connecting => Error::Invalid(UNREACHABLE),
            Self::Showing(_) => Error::Expired,
            Self::Linking(_) => Error::Platform,
        }
    }
}

/// Link a new device into `vault`, reporting codes on `progress`.
pub async fn pair(
    db: SqlitePool,
    vault: Vault,
    progress: &Progress,
    stop: CancellationToken,
) -> Result<Identity, Error> {
    let link = Link::new(db, Uuid::nil(), vault);
    let (handle, mut events) = start(
        link.backend.clone(),
        Arc::new(Refuse),
        Arc::new(Notify::new()),
    )
    .await?;
    let mut stage = Stage::Connecting;
    let deadline = tokio::time::sleep(stage.limit());
    tokio::pin!(deadline);
    let result = loop {
        let event = tokio::select! {
            _ = stop.cancelled() => break Err(Error::Inactive),
            () = &mut deadline => break Err(stage.timed_out()),
            event = events.recv() => match event {
                Some(event) => event,
                None => break Err(Error::Platform),
            },
        };
        match &*event {
            Event::PairingQrCode(qr) if !matches!(stage, Stage::Linking(_)) => {
                stage = Stage::Showing(qr.timeout);
                progress.code(qr.code.clone(), qr.timeout.as_secs());
                deadline
                    .as_mut()
                    .reset(tokio::time::Instant::now() + stage.limit());
            }
            Event::PairingQrCodesExhausted(_) => break Err(Error::Expired),
            Event::PairSuccess(success) => {
                stage = Stage::Linking(success.id.to_non_ad());
                progress.linking();
                deadline
                    .as_mut()
                    .reset(tokio::time::Instant::now() + stage.limit());
            }
            Event::PairError(_) => break Err(Error::Platform),
            Event::Connected(_) => {
                let Stage::Linking(number) = &stage else {
                    continue;
                };
                let name = handle.client().push_name();
                break Ok(Identity {
                    id: number.user.to_string(),
                    display_name: if name.is_empty() {
                        format!("+{}", number.user)
                    } else {
                        name
                    },
                    username: Some(format!("+{}", number.user)),
                });
            }
            Event::LoggedOut(_) => break Err(Error::Platform),
            _ => {}
        }
    };
    let _ = handle.shutdown().await;
    result
}
