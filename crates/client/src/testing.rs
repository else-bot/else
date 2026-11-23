//! Helpers for tests of code that talks to the server, here and in the apps
//! (feature `testing`).

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::origin::Origin;

/// A local server that answers one request with `response` and hands back
/// the request it received.
pub async fn serve_once(response: &'static str) -> (Origin, tokio::task::JoinHandle<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = Origin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = socket.read(&mut buf).await.unwrap();
        socket.write_all(response.as_bytes()).await.unwrap();
        String::from_utf8_lossy(&buf[..n]).into_owned()
    });
    (origin, task)
}
