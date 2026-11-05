//! A minimal RFB 3.8 client for the owner's in-app desktop viewer.
//!
//! The API connects to the guest's noVNC WebSocket (published to host
//! loopback) and speaks RFB itself, so the owner's client never sees the
//! loopback address or the VNC password. The WebSocket `Host` picks the VNC
//! server (see infra/guest/vnc): `else-watch` reaches the view-only server
//! that the bot's input guard ignores; anything else reaches the controlling
//! one. A watch connection asks for 32-bit pixels in Raw and CopyRect plus
//! DesktopSize; a control connection only sends input.

use des::cipher::{BlockEncrypt, KeyInit};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// The view-only server's desktop name (`x11vnc -desktop`), which tells a
/// guest image with a watch server apart from an older one.
pub const WATCH_NAME: &str = "else-watch";
const MAX_SIDE: u16 = 4096;
const MAX_PIXELS: u32 = 8 * 1024 * 1024;

fn valid_size(width: u16, height: u16) -> bool {
    width > 0
        && height > 0
        && width <= MAX_SIDE
        && height <= MAX_SIDE
        && u32::from(width) * u32::from(height) <= MAX_PIXELS
}
const MAX_CUT_TEXT: u32 = 1 << 20;
const BRIDGE_BUFFER: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Watch,
    Control,
}

#[derive(Debug, thiserror::Error)]
pub enum ViewerError {
    #[error("the desktop connection failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("the desktop refused the connection: {0}")]
    Refused(String),
    #[error("the desktop sent something this viewer does not support: {0}")]
    Protocol(String),
}

/// One RFB connection after the handshake.
pub struct Connection {
    pub reader: tokio::io::ReadHalf<DuplexStream>,
    pub writer: tokio::io::WriteHalf<DuplexStream>,
    pub width: u16,
    pub height: u16,
    pub name: String,
    /// Ends when the WebSocket closes; awaited after the halves are dropped
    /// so a close is complete before it is acknowledged.
    pub bridge: tokio::task::JoinHandle<()>,
}

/// Open an RFB connection through the guest's noVNC WebSocket on host
/// loopback `port`.
pub async fn connect(port: u16, password: &str, target: Target) -> Result<Connection, ViewerError> {
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    let mut request = format!("ws://127.0.0.1:{port}/websockify")
        .into_client_request()
        .map_err(|e| ViewerError::Protocol(e.to_string()))?;
    let host = match target {
        Target::Watch => WATCH_NAME.to_owned(),
        Target::Control => format!("127.0.0.1:{port}"),
    };
    let headers = request.headers_mut();
    headers.insert("host", host.parse().expect("valid header"));
    headers.insert(
        "sec-websocket-protocol",
        "binary".parse().expect("valid header"),
    );
    let (ws, _) = tokio_tungstenite::client_async(request, tcp)
        .await
        .map_err(|e| ViewerError::Refused(e.to_string()))?;
    let (ours, theirs) = tokio::io::duplex(BRIDGE_BUFFER);
    let bridge = tokio::spawn(bridge(ws, theirs));
    let (mut reader, mut writer) = tokio::io::split(ours);
    let (width, height, name) = handshake(&mut reader, &mut writer, password).await?;
    // SetPixelFormat: 32 bpp, depth 24, little endian, true colour,
    // red/green/blue at bytes 0/1/2.
    const SET_PIXEL_FORMAT_RGBA: [u8; 20] = [
        0, 0, 0, 0, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0,
    ];
    let mut negotiation = SET_PIXEL_FORMAT_RGBA.to_vec();
    // SetEncodings: Raw, CopyRect, DesktopSize.
    negotiation.extend_from_slice(&[2, 0, 0, 3]);
    for encoding in [0i32, 1, -223] {
        negotiation.extend_from_slice(&encoding.to_be_bytes());
    }
    writer.write_all(&negotiation).await?;
    Ok(Connection {
        reader,
        writer,
        width,
        height,
        name,
        bridge,
    })
}

async fn bridge<S>(ws: tokio_tungstenite::WebSocketStream<S>, mut rfb: DuplexStream)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut sink, mut stream) = ws.split();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        tokio::select! {
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Binary(data))) => {
                    if rfb.write_all(&data).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                Some(Ok(Message::Text(_))) => break,
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
            },
            n = rfb.read(&mut buffer) => match n {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if sink.send(Message::Binary(buffer[..n].to_vec().into())).await.is_err() {
                        break;
                    }
                }
            },
        }
    }
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), sink.close()).await;
}

async fn handshake<R, W>(
    reader: &mut R,
    writer: &mut W,
    password: &str,
) -> Result<(u16, u16, String), ViewerError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut version = [0u8; 12];
    reader.read_exact(&mut version).await?;
    if !version.starts_with(b"RFB 003.") || &version[8..] < b"008\n".as_slice() {
        return Err(ViewerError::Protocol("RFB 3.8 or later is required".into()));
    }
    writer.write_all(b"RFB 003.008\n").await?;
    let count = reader.read_u8().await?;
    if count == 0 {
        return Err(ViewerError::Refused(read_reason(reader).await?));
    }
    let mut types = vec![0u8; count as usize];
    reader.read_exact(&mut types).await?;
    if types.contains(&2) {
        writer.write_all(&[2]).await?;
        let mut challenge = [0u8; 16];
        reader.read_exact(&mut challenge).await?;
        writer
            .write_all(&vnc_auth_response(password, challenge))
            .await?;
    } else if types.contains(&1) {
        writer.write_all(&[1]).await?;
    } else {
        return Err(ViewerError::Protocol("no supported security type".into()));
    }
    if reader.read_u32().await? != 0 {
        return Err(ViewerError::Refused(
            read_reason(reader).await.unwrap_or_default(),
        ));
    }
    // ClientInit: shared, so the bot's screenshots and other viewers stay.
    writer.write_all(&[1]).await?;
    let width = reader.read_u16().await?;
    let height = reader.read_u16().await?;
    let mut format = [0u8; 16];
    reader.read_exact(&mut format).await?;
    let name_len = reader.read_u32().await?;
    if name_len > 4096 {
        return Err(ViewerError::Protocol("desktop name too long".into()));
    }
    let mut name = vec![0u8; name_len as usize];
    reader.read_exact(&mut name).await?;
    if !valid_size(width, height) {
        return Err(ViewerError::Protocol("unsupported screen size".into()));
    }
    Ok((width, height, String::from_utf8_lossy(&name).into_owned()))
}

async fn read_reason<R: AsyncRead + Unpin>(reader: &mut R) -> Result<String, ViewerError> {
    let len = reader.read_u32().await?.min(1024);
    let mut reason = vec![0u8; len as usize];
    reader.read_exact(&mut reason).await?;
    Ok(String::from_utf8_lossy(&reason).into_owned())
}

/// VNC authentication: the challenge DES-encrypted with the password (at
/// most 8 bytes, zero padded) as the key, each key byte bit-reversed.
pub fn vnc_auth_response(password: &str, challenge: [u8; 16]) -> [u8; 16] {
    let mut key = [0u8; 8];
    for (k, b) in key.iter_mut().zip(password.bytes()) {
        *k = b.reverse_bits();
    }
    let cipher = des::Des::new_from_slice(&key).expect("an 8-byte key");
    let mut out = challenge;
    for block in out.as_chunks_mut::<8>().0 {
        cipher.encrypt_block(block.into());
    }
    out
}

/// What the server sent, in order.
#[derive(Debug)]
pub enum Event {
    /// Raw pixels, 4 bytes each (R, G, B, unused).
    Pixels {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        data: Vec<u8>,
    },
    Copy {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        from_x: u16,
        from_y: u16,
    },
    Resize {
        width: u16,
        height: u16,
    },
    /// The end of one framebuffer update.
    UpdateEnd,
}

/// Parse server messages until the connection ends. Events go to `events`
/// (bounded, so a slow consumer slows the socket); with `None` everything is
/// read and dropped (a control connection requests no updates).
pub async fn read_events<R: AsyncRead + Unpin>(
    mut reader: R,
    mut width: u16,
    mut height: u16,
    events: Option<mpsc::Sender<Event>>,
) -> Result<(), ViewerError> {
    let send = |event: Event| {
        let events = events.clone();
        async move {
            match events {
                Some(tx) => tx.send(event).await.is_ok(),
                None => true,
            }
        }
    };
    loop {
        let kind = match reader.read_u8().await {
            Ok(kind) => kind,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        match kind {
            0 => {
                reader.read_u8().await?;
                let rects = reader.read_u16().await?;
                for _ in 0..rects {
                    let x = reader.read_u16().await?;
                    let y = reader.read_u16().await?;
                    let w = reader.read_u16().await?;
                    let h = reader.read_u16().await?;
                    let encoding = reader.read_i32().await?;
                    if encoding == -223 {
                        if !valid_size(w, h) {
                            return Err(ViewerError::Protocol("unsupported screen size".into()));
                        }
                        (width, height) = (w, h);
                        if !send(Event::Resize { width, height }).await {
                            return Ok(());
                        }
                        continue;
                    }
                    if u32::from(x) + u32::from(w) > u32::from(width)
                        || u32::from(y) + u32::from(h) > u32::from(height)
                    {
                        return Err(ViewerError::Protocol(
                            "a rectangle outside the screen".into(),
                        ));
                    }
                    let event = match encoding {
                        0 => {
                            let mut data = vec![0u8; w as usize * h as usize * 4];
                            reader.read_exact(&mut data).await?;
                            Event::Pixels {
                                x,
                                y,
                                width: w,
                                height: h,
                                data,
                            }
                        }
                        1 => {
                            let from_x = reader.read_u16().await?;
                            let from_y = reader.read_u16().await?;
                            if u32::from(from_x) + u32::from(w) > u32::from(width)
                                || u32::from(from_y) + u32::from(h) > u32::from(height)
                            {
                                return Err(ViewerError::Protocol(
                                    "a copy from outside the screen".into(),
                                ));
                            }
                            Event::Copy {
                                x,
                                y,
                                width: w,
                                height: h,
                                from_x,
                                from_y,
                            }
                        }
                        other => {
                            return Err(ViewerError::Protocol(format!("encoding {other}")));
                        }
                    };
                    if !send(event).await {
                        return Ok(());
                    }
                }
                if !send(Event::UpdateEnd).await {
                    return Ok(());
                }
            }
            1 => {
                reader.read_u8().await?;
                reader.read_u16().await?;
                let colours = reader.read_u16().await?;
                skip(&mut reader, u64::from(colours) * 6).await?;
            }
            2 => {}
            3 => {
                let mut pad = [0u8; 3];
                reader.read_exact(&mut pad).await?;
                let len = reader.read_u32().await?;
                if len > MAX_CUT_TEXT {
                    return Err(ViewerError::Protocol("clipboard text too long".into()));
                }
                skip(&mut reader, u64::from(len)).await?;
            }
            other => return Err(ViewerError::Protocol(format!("message type {other}"))),
        }
    }
}

async fn skip<R: AsyncRead + Unpin>(reader: &mut R, n: u64) -> Result<(), ViewerError> {
    let copied = tokio::io::copy(&mut reader.take(n), &mut tokio::io::sink()).await?;
    if copied != n {
        return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
    }
    Ok(())
}

pub fn update_request(incremental: bool, width: u16, height: u16) -> [u8; 10] {
    let [w0, w1] = width.to_be_bytes();
    let [h0, h1] = height.to_be_bytes();
    [3, incremental as u8, 0, 0, 0, 0, w0, w1, h0, h1]
}

pub fn key_event(keysym: u32, down: bool) -> [u8; 8] {
    let [a, b, c, d] = keysym.to_be_bytes();
    [4, down as u8, 0, 0, a, b, c, d]
}

pub fn pointer_event(buttons: u8, x: u16, y: u16) -> [u8; 6] {
    let [x0, x1] = x.to_be_bytes();
    let [y0, y1] = y.to_be_bytes();
    [5, buttons, x0, x1, y0, y1]
}

/// The screen as the viewer last saw it, 4 bytes per pixel.
pub struct Framebuffer {
    pub width: u16,
    pub height: u16,
    pixels: Vec<u8>,
}

impl Framebuffer {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            pixels: vec![0; width as usize * height as usize * 4],
        }
    }

    /// Apply one event. Events were bounds-checked when parsed, against
    /// the size in force at the time, which this framebuffer tracks too.
    pub fn apply(&mut self, event: &Event) {
        let stride = self.width as usize * 4;
        match *event {
            Event::Pixels {
                x,
                y,
                width,
                height,
                ref data,
            } => {
                let row = width as usize * 4;
                for r in 0..height as usize {
                    let at = (y as usize + r) * stride + x as usize * 4;
                    self.pixels[at..at + row].copy_from_slice(&data[r * row..(r + 1) * row]);
                }
            }
            Event::Copy {
                x,
                y,
                width,
                height,
                from_x,
                from_y,
            } => {
                let row = width as usize * 4;
                let rows: Vec<usize> = if from_y < y {
                    (0..height as usize).rev().collect()
                } else {
                    (0..height as usize).collect()
                };
                for r in rows {
                    let from = (from_y as usize + r) * stride + from_x as usize * 4;
                    let to = (y as usize + r) * stride + x as usize * 4;
                    self.pixels.copy_within(from..from + row, to);
                }
            }
            Event::Resize { width, height } => *self = Self::new(width, height),
            Event::UpdateEnd => {}
        }
    }

    /// The whole screen as an RGB PNG.
    pub fn png(&self) -> Vec<u8> {
        let mut rgb = Vec::with_capacity(self.width as usize * self.height as usize * 3);
        for px in self.pixels.as_chunks::<4>().0 {
            rgb.extend_from_slice(&px[..3]);
        }
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, self.width.into(), self.height.into());
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder.write_header().expect("PNG header into memory");
        writer
            .write_image_data(&rgb)
            .expect("PNG data of the declared size into memory");
        writer.finish().expect("PNG end into memory");
        out
    }

    pub fn snapshot(&self) -> Self {
        Self {
            width: self.width,
            height: self.height,
            pixels: self.pixels.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vnc_auth_matches_a_reference_response() {
        let challenge: [u8; 16] = *b"0123456789abcdef";
        assert_eq!(
            hex::encode(vnc_auth_response("secret12", challenge)),
            REFERENCE
        );
    }

    const REFERENCE: &str = "5f15f4f0e1684cdc260ea962ab82fa3b";

    #[test]
    fn copies_overlapping_rows_without_smearing() {
        let mut fb = Framebuffer::new(1, 3);
        fb.apply(&Event::Pixels {
            x: 0,
            y: 0,
            width: 1,
            height: 3,
            data: vec![1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0],
        });
        fb.apply(&Event::Copy {
            x: 0,
            y: 1,
            width: 1,
            height: 2,
            from_x: 0,
            from_y: 0,
        });
        assert_eq!(fb.pixels, vec![1, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0]);
    }
}
