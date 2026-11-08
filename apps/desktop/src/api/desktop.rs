use super::{Client, ClientError, ErrorBody};
use futures_util::{SinkExt as _, StreamExt as _};
use gpui_kit::RenderImage;
use image::{ImageDecoder as _, ImageReader};
use reqwest::header::AUTHORIZATION;
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use std::io::Cursor;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use uuid::Uuid;

const MAX_PNG_BYTES: usize = 8 * 1024 * 1024;
const MAX_DIMENSION: u32 = 4096;
const MAX_PIXELS: u64 = 8 * 1024 * 1024;
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest single text commit the viewer types; longer ones are refused
/// with a notice instead of being truncated.
#[cfg(any(target_os = "macos", test))]
pub const MAX_TEXT_CHARS: usize = 4096;

#[derive(Debug, Clone, Deserialize)]
pub struct ComputerState {
    pub state: String,
    pub desired_state: String,
    #[serde(default)]
    pub desktop_available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopMode {
    Watch,
    Control,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DesktopCommand {
    Mode { mode: DesktopMode },
    Pointer { x: u16, y: u16, buttons: u8 },
    Key { key: u32, down: bool },
    Release,
}

#[derive(Debug)]
pub enum DesktopEvent {
    Size { width: u16, height: u16 },
    Frame(Arc<RenderImage>),
    Mode(DesktopMode),
    Error(String),
    Asleep,
    Notice(String),
}

enum Input {
    Command(DesktopCommand),
    #[cfg(any(target_os = "macos", test))]
    Text(Box<str>),
}

#[derive(Clone)]
pub struct DesktopCommands {
    queue: mpsc::Sender<(u64, Input)>,
    motion: watch::Sender<Option<(u64, DesktopCommand)>>,
    sequence: Arc<AtomicU64>,
    cancel: watch::Sender<bool>,
}

impl DesktopCommands {
    pub fn send_or_disconnect(&self, command: DesktopCommand) -> bool {
        self.enqueue(Input::Command(command))
    }

    /// Queues committed text, which is dropped unless the desktop is in control
    /// mode when its turn comes. Callers bound it by [`MAX_TEXT_CHARS`].
    #[cfg(any(target_os = "macos", test))]
    pub fn type_text(&self, text: &str) -> bool {
        debug_assert!(text.chars().count() <= MAX_TEXT_CHARS);
        text.is_empty() || self.enqueue(Input::Text(text.into()))
    }

    fn enqueue(&self, input: Input) -> bool {
        if *self.cancel.borrow() {
            return false;
        }
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        if self.queue.try_send((sequence, input)).is_err() {
            self.cancel.send_replace(true);
            return false;
        }
        true
    }

    pub fn move_pointer(&self, x: u16, y: u16, buttons: u8) {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        self.motion
            .send_replace(Some((sequence, DesktopCommand::Pointer { x, y, buttons })));
    }
}

pub struct DesktopStream {
    pub commands: DesktopCommands,
    pub events: mpsc::Receiver<DesktopEvent>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerEvent {
    Size {
        width: u16,
        height: u16,
    },
    Mode {
        mode: DesktopMode,
    },
    Error {
        #[serde(default)]
        code: String,
        message: String,
    },
}

/// The X11 keysym that types `ch`, or `None` for control characters the
/// guest cannot receive as text.
#[cfg(any(target_os = "macos", test))]
fn text_keysym(ch: char) -> Option<u32> {
    Some(match ch {
        '\n' | '\r' => 0xff0d,
        '\t' => 0xff09,
        ch if ch.is_control() => return None,
        ch if (ch as u32) <= 0xff => ch as u32,
        ch => 0x0100_0000 | ch as u32,
    })
}

fn valid_dimensions(width: u32, height: u32) -> bool {
    width > 0
        && height > 0
        && width <= MAX_DIMENSION
        && height <= MAX_DIMENSION
        && u64::from(width) * u64::from(height) <= MAX_PIXELS
}

fn decode_frame(bytes: &[u8], expected: Option<(u16, u16)>) -> Result<Arc<RenderImage>, String> {
    if bytes.len() > MAX_PNG_BYTES {
        return Err("The desktop frame exceeds the image limit.".into());
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_PIXELS * 8);
    reader.limits(limits);
    let decoder = reader
        .into_decoder()
        .map_err(|_| "The desktop sent an invalid PNG frame.")?;
    let dimensions = decoder.dimensions();
    if !valid_dimensions(dimensions.0, dimensions.1)
        || expected.map(|(w, h)| (u32::from(w), u32::from(h))) != Some(dimensions)
    {
        return Err("The desktop frame has unexpected dimensions.".into());
    }
    let mut pixels = image::DynamicImage::from_decoder(decoder)
        .map_err(|_| "The desktop sent an invalid PNG frame.")?
        .into_rgba8();
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Ok(Arc::new(RenderImage::new(vec![image::Frame::new(pixels)])))
}

type Decoding = tokio::task::JoinHandle<Result<Arc<RenderImage>, String>>;

fn decode_later(
    bytes: tokio_tungstenite::tungstenite::Bytes,
    expected: Option<(u16, u16)>,
) -> Decoding {
    tokio::task::spawn_blocking(move || decode_frame(&bytes, expected))
}

async fn decoded(task: &mut Option<Decoding>) -> Result<Arc<RenderImage>, String> {
    let Some(handle) = task else {
        return std::future::pending().await;
    };
    let frame = handle
        .await
        .map_err(|_| "Cannot decode the desktop frame.".to_owned());
    *task = None;
    frame?
}

impl Client {
    pub async fn computer(self: Arc<Self>, bot: Uuid) -> Result<ComputerState, ClientError> {
        let this = self.clone();
        self.run(async move {
            let request = this.request(Method::GET, &format!("/v1/bots/{bot}/computer"));
            this.send_json(request).await.map(|(_, state)| state)
        })
        .await
    }

    pub async fn resume_computer(self: Arc<Self>, bot: Uuid) -> Result<(), ClientError> {
        let this = self.clone();
        self.run(async move {
            let request = this
                .request(Method::POST, &format!("/v1/bots/{bot}/computer/resume"))
                .timeout(Duration::from_secs(15));
            let response = this.send_request(request).await?;
            let status = response.status();
            if status == StatusCode::UNAUTHORIZED {
                return Err(ClientError::Unauthorized);
            }
            if status.is_success() {
                return Ok(());
            }
            if status.is_redirection() {
                return Err(ClientError::Protocol(format!(
                    "redirect ({status}) refused"
                )));
            }
            let bytes = response
                .bytes()
                .await
                .map_err(|err| ClientError::Protocol(err.without_url().to_string()))?;
            Err(match serde_json::from_slice::<ErrorBody>(&bytes) {
                Ok(body) => ClientError::Api {
                    status: status.as_u16(),
                    code: body.error.code,
                    message: body.error.message,
                    retryable: body.error.retryable,
                },
                Err(_) => ClientError::Protocol(format!("HTTP {status}")),
            })
        })
        .await
    }

    pub fn desktop(self: Arc<Self>, bot: Uuid, wake: bool) -> DesktopStream {
        let (queue, command_rx) = mpsc::channel(64);
        let (motion, motion_rx) = watch::channel(None);
        let sequence = Arc::new(AtomicU64::new(1));
        let (cancel, cancel_rx) = watch::channel(false);
        let (event_tx, events) = mpsc::channel(2);
        self.runtime.spawn({
            let this = self.clone();
            async move {
                let result = this
                    .run_desktop(bot, wake, command_rx, motion_rx, cancel_rx, &event_tx)
                    .await;
                if let Err(error) = result {
                    let _ = tokio::time::timeout(
                        WRITE_TIMEOUT,
                        event_tx.send(DesktopEvent::Error(error)),
                    )
                    .await;
                }
            }
        });
        DesktopStream {
            commands: DesktopCommands {
                queue,
                motion,
                sequence,
                cancel,
            },
            events,
        }
    }

    async fn run_desktop(
        &self,
        bot: Uuid,
        wake: bool,
        mut commands: mpsc::Receiver<(u64, Input)>,
        mut motion: watch::Receiver<Option<(u64, DesktopCommand)>>,
        mut cancel: watch::Receiver<bool>,
        events: &mpsc::Sender<DesktopEvent>,
    ) -> Result<(), String> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
        self.verify_transport()
            .await
            .map_err(|err| err.to_string())?;
        let mut url = url::Url::parse(&self.origin.ws_url()).expect("validated API origin");
        url.set_path(&format!("/v1/bots/{bot}/desktop/stream"));
        url.set_query(Some(if wake {
            "mode=watch&wake=true"
        } else {
            "mode=watch"
        }));
        let mut request = url.as_str().into_client_request().expect("valid WS URL");
        request
            .headers_mut()
            .insert(AUTHORIZATION, self.auth.clone());
        let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(MAX_PNG_BYTES))
            .max_frame_size(Some(MAX_PNG_BYTES));
        let connected = tokio::select! {
            biased;
            _ = events.closed() => return Ok(()),
            _ = cancel.changed() => return Ok(()),
            result = tokio::time::timeout(
                Duration::from_secs(90),
                tokio_tungstenite::connect_async_tls_with_config(
                    request,
                    Some(config),
                    false,
                    Some(tokio_tungstenite::Connector::Rustls(self.tls.clone())),
                ),
            ) => result,
        };
        let mut socket = match connected {
            Ok(Ok((socket, _))) => socket,
            Ok(Err(WsError::Http(response))) => {
                if response.status() == StatusCode::UNAUTHORIZED {
                    return Err(ClientError::Unauthorized.to_string());
                }
                return Err(format!(
                    "Cannot open the desktop (HTTP {}).",
                    response.status()
                ));
            }
            Ok(Err(_)) => return Err("Cannot connect to the bot's desktop.".into()),
            Err(_) => return Err("The bot's desktop did not wake in time. Try again.".into()),
        };
        let mut dimensions = None;
        let mut mode = DesktopMode::Watch;
        macro_rules! deliver {
            ($event:expr) => {
                tokio::select! {
                    biased;
                    _ = events.closed() => return Ok(()),
                    _ = cancel.changed() => return Err("Desktop input stopped because its queue was full.".into()),
                    result = events.send($event) => if result.is_err() { return Ok(()) },
                }
            };
        }
        macro_rules! send {
            ($message:expr) => {
                tokio::select! {
                    biased;
                    _ = events.closed() => return Ok(()),
                    _ = cancel.changed() => return Err("Desktop input stopped because its queue was full.".into()),
                    result = tokio::time::timeout(WRITE_TIMEOUT, socket.send($message)) => {
                        if !matches!(result, Ok(Ok(()))) {
                            return Err("The desktop connection stopped responding.".into());
                        }
                    },
                }
            };
        }
        let mut last_sequence = 0;
        let mut typing: Option<std::vec::IntoIter<u32>> = None;
        // The socket keeps being read while a frame decodes; only the newest
        // frame that arrived meanwhile is decoded next, so a slow decode skips
        // stale frames instead of falling behind the screen.
        let mut decoding: Option<Decoding> = None;
        let mut newest = None;
        loop {
            tokio::select! {
                biased;
                _ = events.closed() => return Ok(()),
                _ = cancel.changed() => return Err("Desktop input stopped because its queue was full.".into()),
                command = commands.recv(), if typing.is_none() => {
                    let Some((sequence, input)) = command else { return Ok(()) };
                    last_sequence = sequence;
                    #[cfg(not(any(target_os = "macos", test)))]
                    let Input::Command(command) = input;
                    #[cfg(any(target_os = "macos", test))]
                    let command = match input {
                        Input::Command(command) => command,
                        Input::Text(text) => {
                            if mode == DesktopMode::Control {
                                let keys: Vec<u32> = text.chars().filter_map(text_keysym).collect();
                                typing = Some(keys.into_iter());
                            }
                            continue;
                        },
                    };
                    match &command {
                        DesktopCommand::Pointer { .. } | DesktopCommand::Key { .. }
                            if mode != DesktopMode::Control => continue,
                        _ => {},
                    }
                    let text = serde_json::to_string(&command).expect("desktop command serializes");
                    send!(Message::Text(text.into()));
                },
                changed = motion.changed(), if typing.is_none() => {
                    if changed.is_err() { return Ok(()); }
                    let pending = motion.borrow_and_update().clone();
                    if let Some((sequence, command)) = pending.filter(|(sequence, _)| *sequence > last_sequence) {
                        last_sequence = sequence;
                        if mode == DesktopMode::Control {
                            let text = serde_json::to_string(&command).expect("desktop command serializes");
                            send!(Message::Text(text.into()));
                        }
                    }
                },
                message = tokio::time::timeout(if dimensions.is_none() { Duration::from_secs(210) } else { Duration::from_secs(45) }, socket.next()) => {
                    match message {
                        Ok(Some(Ok(Message::Text(text)))) => {
                            if text.len() > 64 * 1024 {
                                return Err("The desktop sent an oversized status message.".into());
                            }
                            let event: ServerEvent = serde_json::from_str(&text)
                                .map_err(|_| "The desktop sent an invalid status message.")?;
                            match event {
                                ServerEvent::Size { width, height } => {
                                    if !valid_dimensions(u32::from(width), u32::from(height)) {
                                        return Err("The desktop size exceeds the image limit.".into());
                                    }
                                    if dimensions != Some((width, height)) {
                                        if let Some(stale) = decoding.take() {
                                            stale.abort();
                                        }
                                        newest = None;
                                    }
                                    dimensions = Some((width, height));
                                    deliver!(DesktopEvent::Size { width, height });
                                },
                                ServerEvent::Mode { mode: acknowledged } => {
                                    mode = acknowledged;
                                    deliver!(DesktopEvent::Mode(mode));
                                },
                                ServerEvent::Error { code, message } => {
                                    if matches!(code.as_str(), "computer_stopped" | "computer_not_running") {
                                        deliver!(DesktopEvent::Asleep);
                                        return Ok(());
                                    }
                                    if matches!(code.as_str(), "control_in_use" | "control_failed" | "invalid_message") {
                                        deliver!(DesktopEvent::Notice(message));
                                    } else {
                                        return Err(message);
                                    }
                                },
                            }
                        },
                        Ok(Some(Ok(Message::Binary(bytes)))) => {
                            if decoding.is_some() {
                                newest = Some((bytes, dimensions));
                            } else {
                                decoding = Some(decode_later(bytes, dimensions));
                            }
                        },
                        Ok(Some(Ok(Message::Ping(bytes)))) => send!(Message::Pong(bytes)),
                        Ok(Some(Ok(Message::Pong(_)))) => {},
                        Ok(Some(Ok(Message::Close(_)))) | Ok(None) => {
                            return Err("The desktop connection closed. Click to reconnect.".into());
                        },
                        Ok(Some(Err(_))) | Err(_) => {
                            return Err("The desktop connection stopped responding.".into());
                        },
                        Ok(Some(Ok(_))) => {},
                    }
                },
                frame = decoded(&mut decoding) => {
                    let frame = frame?;
                    decoding = newest.take().map(|(bytes, expected)| decode_later(bytes, expected));
                    deliver!(DesktopEvent::Frame(frame));
                },
                () = std::future::ready(()), if typing.is_some() => {
                    let next = typing.as_mut().and_then(Iterator::next);
                    let Some(key) = next.filter(|_| mode == DesktopMode::Control) else {
                        typing = None;
                        continue;
                    };
                    if typing.as_ref().is_some_and(|keys| keys.len() == 0) {
                        typing = None;
                    }
                    for down in [true, false] {
                        let text = serde_json::to_string(&DesktopCommand::Key { key, down })
                            .expect("desktop command serializes");
                        send!(Message::Text(text.into()));
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Origin, OwnerToken};

    fn png(width: u32, height: u32, pixels: Vec<u8>) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        image::RgbaImage::from_raw(width, height, pixels)
            .unwrap()
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    fn client(origin: Origin) -> Arc<Client> {
        let token = OwnerToken::parse(&"T".repeat(43)).unwrap();
        Arc::new(Client::new(origin, &token, tokio::runtime::Handle::current()).unwrap())
    }

    #[test]
    fn desktop_frame_has_the_announced_size_and_bgra_pixels() {
        let bytes = png(1, 1, vec![10, 20, 30, 255]);
        let image = decode_frame(&bytes, Some((1, 1))).unwrap();
        assert_eq!(image.as_bytes(0).unwrap(), &[30, 20, 10, 255]);
        assert!(decode_frame(&bytes, Some((2, 1))).is_err());
        assert!(decode_frame(&bytes, None).is_err());
        assert!(decode_frame(b"invalid PNG", Some((1, 1))).is_err());
        assert!(decode_frame(&vec![0; MAX_PNG_BYTES + 1], Some((1, 1))).is_err());
    }

    #[tokio::test]
    async fn resume_accepts_no_content_and_keeps_the_token_in_its_header() {
        let (origin, server) =
            super::super::tests::serve_once("HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .await;
        let bot = Uuid::new_v4();
        client(origin).resume_computer(bot).await.unwrap();
        let request = server.await.unwrap();
        assert!(request.starts_with(&format!("POST /v1/bots/{bot}/computer/resume ")));
        assert!(request.contains(&format!("Bearer {}", "T".repeat(43))));
        assert!(!request.lines().next().unwrap().contains(&"T".repeat(43)));
    }

    #[tokio::test]
    async fn saturated_input_cancels_instead_of_dropping_a_release() {
        let (queue, mut rx) = mpsc::channel(1);
        let (cancel, cancel_rx) = watch::channel(false);
        let (motion, _motion_rx) = watch::channel(None);
        let sequence = Arc::new(AtomicU64::new(1));
        let commands = DesktopCommands {
            queue,
            motion,
            sequence,
            cancel,
        };
        assert!(commands.send_or_disconnect(DesktopCommand::Key {
            key: 65,
            down: true
        }));
        assert!(!commands.send_or_disconnect(DesktopCommand::Key {
            key: 65,
            down: false
        }));
        assert!(*cancel_rx.borrow());
        rx.recv().await.unwrap();
        assert!(!commands.send_or_disconnect(DesktopCommand::Release));
    }

    #[tokio::test]
    async fn pointer_motion_leaves_capacity_for_button_and_key_releases() {
        let (queue, mut rx) = mpsc::channel(2);
        let (cancel, cancel_rx) = watch::channel(false);
        let (motion, _motion_rx) = watch::channel(None);
        let commands = DesktopCommands {
            queue,
            motion,
            sequence: Arc::new(AtomicU64::new(1)),
            cancel,
        };
        for x in 0..10000 {
            commands.move_pointer(x, 10, 1);
        }
        assert!(commands.send_or_disconnect(DesktopCommand::Pointer {
            x: 10,
            y: 10,
            buttons: 0
        }));
        assert!(commands.send_or_disconnect(DesktopCommand::Key {
            key: 65,
            down: false
        }));
        assert!(!*cancel_rx.borrow());
        assert!(matches!(
            rx.recv().await,
            Some((
                _,
                Input::Command(DesktopCommand::Pointer { buttons: 0, .. })
            ))
        ));
        assert!(matches!(
            rx.recv().await,
            Some((_, Input::Command(DesktopCommand::Key { down: false, .. })))
        ));
    }

    #[tokio::test]
    async fn a_long_text_commit_is_typed_in_order_before_later_input() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = Origin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let mut received = Vec::new();
            loop {
                let text = ws.next().await.unwrap().unwrap().into_text().unwrap();
                if text.contains(r#""type":"mode""#) {
                    ws.send(Message::Text(text.clone())).await.unwrap();
                }
                let done = received.len() > 1 && text == r#"{"type":"mode","mode":"control"}"#;
                received.push(text.to_string());
                if done {
                    return received;
                }
            }
        });
        let mut stream = client(origin).desktop(Uuid::new_v4(), false);
        let control = || DesktopCommand::Mode {
            mode: DesktopMode::Control,
        };
        let watch = DesktopCommand::Mode {
            mode: DesktopMode::Watch,
        };
        let commit = format!("{}é漢\n", "ab".repeat(60));
        assert!(stream.commands.send_or_disconnect(control()));
        assert!(matches!(
            stream.events.recv().await,
            Some(DesktopEvent::Mode(DesktopMode::Control))
        ));
        assert!(stream.commands.type_text(&commit));
        assert!(stream.commands.send_or_disconnect(DesktopCommand::Release));
        assert!(stream.commands.send_or_disconnect(watch));
        assert!(matches!(
            stream.events.recv().await,
            Some(DesktopEvent::Mode(DesktopMode::Watch))
        ));
        assert!(stream.commands.type_text("ignored while watching"));
        assert!(stream.commands.send_or_disconnect(control()));
        let received = server.await.unwrap();

        let tap = |key: u32| {
            [true, false].map(|down| format!(r#"{{"type":"key","key":{key},"down":{down}}}"#))
        };
        let mut expected = vec![r#"{"type":"mode","mode":"control"}"#.to_owned()];
        for _ in 0..60 {
            expected.extend(tap(0x61));
            expected.extend(tap(0x62));
        }
        expected.extend(tap(0xe9));
        expected.extend(tap(0x0100_6f22));
        expected.extend(tap(0xff0d));
        expected.push(r#"{"type":"release"}"#.to_owned());
        expected.push(r#"{"type":"mode","mode":"watch"}"#.to_owned());
        expected.push(r#"{"type":"mode","mode":"control"}"#.to_owned());
        assert_eq!(received, expected);
    }

    #[tokio::test]
    async fn explicit_wake_is_opted_in_and_preserves_the_server_failure() {
        use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = Origin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            #[expect(
                clippy::result_large_err,
                reason = "Tungstenite fixes the callback error type to ErrorResponse."
            )]
            let mut ws = tokio_tungstenite::accept_hdr_async(
                tcp,
                |request: &Request, response: Response| {
                    assert_eq!(request.uri().query(), Some("mode=watch&wake=true"));
                    Ok(response)
                },
            )
            .await
            .unwrap();
            ws.send(Message::Text(
                r#"{"type":"error","code":"computer_unavailable","message":"The runtime is unavailable."}"#.into(),
            ))
            .await
            .unwrap();
            let _ = ws.close(None).await;
        });
        let mut stream = client(origin).desktop(Uuid::new_v4(), true);
        assert!(matches!(
            stream.events.recv().await,
            Some(DesktopEvent::Error(message)) if message == "The runtime is unavailable."
        ));
        assert!(stream.events.recv().await.is_none());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_refused_takeover_keeps_the_preview_connected() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = Origin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            for text in [
                r#"{"type":"size","width":1,"height":1}"#,
                r#"{"type":"mode","mode":"watch"}"#,
                r#"{"type":"error","code":"control_failed","message":"The browser is busy."}"#,
            ] {
                ws.send(Message::Text(text.into())).await.unwrap();
            }
            ws.send(Message::Binary(png(1, 1, vec![1, 2, 3, 255]).into()))
                .await
                .unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(5), ws.next()).await;
        });
        let mut stream = client(origin).desktop(Uuid::new_v4(), false);
        assert!(matches!(
            stream.events.recv().await,
            Some(DesktopEvent::Size { .. })
        ));
        assert!(matches!(
            stream.events.recv().await,
            Some(DesktopEvent::Mode(DesktopMode::Watch))
        ));
        assert!(
            matches!(stream.events.recv().await,Some(DesktopEvent::Notice(message)) if message == "The browser is busy.")
        );
        assert!(matches!(
            stream.events.recv().await,
            Some(DesktopEvent::Frame(_))
        ));
        drop(stream);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_burst_of_frames_ends_on_the_newest_without_going_back() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = Origin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            ws.send(Message::Text(
                r#"{"type":"size","width":1,"height":1}"#.into(),
            ))
            .await
            .unwrap();
            for shade in 0..40 {
                ws.send(Message::Binary(png(1, 1, vec![shade, 0, 0, 255]).into()))
                    .await
                    .unwrap();
            }
            ws.send(Message::Text(r#"{"type":"mode","mode":"watch"}"#.into()))
                .await
                .unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(5), ws.next()).await;
        });
        let mut stream = client(origin).desktop(Uuid::new_v4(), false);
        let mut shades = Vec::new();
        let mut mode = false;
        while !(mode && shades.last() == Some(&39)) {
            match stream.events.recv().await.unwrap() {
                DesktopEvent::Frame(image) => shades.push(image.as_bytes(0).unwrap()[2]),
                DesktopEvent::Mode(DesktopMode::Watch) => mode = true,
                _ => {}
            }
        }
        assert!(shades.is_sorted_by(|a, b| a < b), "{shades:?}");
        drop(stream);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn desktop_stream_authenticates_delivers_pixels_and_stops_on_drop() {
        use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = Origin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let bot = Uuid::new_v4();
        let (key_seen, key_received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            #[expect(
                clippy::result_large_err,
                reason = "Tungstenite fixes the callback error type to ErrorResponse."
            )]
            let mut ws = tokio_tungstenite::accept_hdr_async(
                tcp,
                move |request: &Request, response: Response| {
                    assert_eq!(
                        request.uri().path(),
                        format!("/v1/bots/{bot}/desktop/stream")
                    );
                    assert_eq!(request.uri().query(), Some("mode=watch"));
                    assert_eq!(
                        request.headers()[AUTHORIZATION],
                        format!("Bearer {}", "T".repeat(43))
                    );
                    assert!(request.headers().get("origin").is_none());
                    Ok(response)
                },
            )
            .await
            .unwrap();
            ws.send(Message::Text(
                r#"{"type":"size","width":1,"height":1}"#.into(),
            ))
            .await
            .unwrap();
            ws.send(Message::Binary(png(1, 1, vec![40, 50, 60, 255]).into()))
                .await
                .unwrap();
            let mode = ws.next().await.unwrap().unwrap();
            assert_eq!(
                mode.into_text().unwrap(),
                r#"{"type":"mode","mode":"control"}"#
            );
            ws.send(Message::Text(r#"{"type":"mode","mode":"control"}"#.into()))
                .await
                .unwrap();
            let key = ws.next().await.unwrap().unwrap();
            assert_eq!(
                key.into_text().unwrap(),
                r#"{"type":"key","key":65,"down":true}"#
            );
            key_seen.send(()).unwrap();
            assert!(
                tokio::time::timeout(Duration::from_secs(5), ws.next())
                    .await
                    .is_ok()
            );
        });
        let mut stream = client(origin).desktop(bot, false);
        assert!(matches!(
            stream.events.recv().await,
            Some(DesktopEvent::Size {
                width: 1,
                height: 1
            })
        ));
        let Some(DesktopEvent::Frame(image)) = stream.events.recv().await else {
            panic!("expected desktop pixels");
        };
        assert_eq!(image.as_bytes(0).unwrap(), &[60, 50, 40, 255]);
        assert!(stream.commands.send_or_disconnect(DesktopCommand::Pointer {
            x: 0,
            y: 0,
            buttons: 1,
        }));
        assert!(stream.commands.send_or_disconnect(DesktopCommand::Mode {
            mode: DesktopMode::Control
        }));
        assert!(matches!(
            stream.events.recv().await,
            Some(DesktopEvent::Mode(DesktopMode::Control))
        ));
        assert!(stream.commands.send_or_disconnect(DesktopCommand::Key {
            key: 65,
            down: true
        }));
        key_received.await.unwrap();
        drop(stream);
        server.await.unwrap();
    }
}
