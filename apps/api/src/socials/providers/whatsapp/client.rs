//! The WhatsApp client: its HTTP, TLS, and how one starts on a device store.

use super::{inbound::Capture, *};

struct Http(reqwest::Client);

#[whatsapp_rust::async_trait]
impl HttpClient for Http {
    async fn execute(&self, request: HttpRequest) -> whatsapp_rust::anyhow::Result<HttpResponse> {
        let method = reqwest::Method::from_bytes(request.method.as_bytes())?;
        let mut builder = self.0.request(method, &request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = request.body {
            builder = builder.body(body.to_vec());
        }
        let response = builder.send().await?;
        let status = response.status().as_u16();
        Ok(HttpResponse::new(status, response.bytes().await?.to_vec()))
    }
}

/// TLS through the explicit `ring` provider, so the WhatsApp socket never
/// depends on which provider the process installed first.
fn connector() -> Result<Connector, Error> {
    use rustls_platform_verifier::BuilderVerifierExt;
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| Error::Crypto)?
    .with_platform_verifier()
    .map_err(|_| Error::Crypto)?
    .with_no_client_auth();
    Ok(Connector::Rustls(tokio_rustls::TlsConnector::from(
        Arc::new(config),
    )))
}

fn http() -> Result<Http, Error> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(60))
        .build()
        .map(Http)
        .map_err(|_| Error::Platform)
}

/// Start a client on `backend`. Messages reach `inbound` before WhatsApp
/// hears they arrived, and history chunks are kept in the device store
/// before their receipt, waking `captured`. The returned events are
/// observations only: the client may drop them under load.
pub(super) async fn start(
    backend: Arc<backend::Backend>,
    inbound: Arc<dyn InboundDurabilityHook>,
    captured: Arc<Notify>,
) -> Result<(BotHandle, mpsc::Receiver<Arc<Event>>), Error> {
    let (tx, rx) = mpsc::channel(64);
    let capture = Arc::new(Capture::new(backend.clone(), captured));
    let bot = Bot::builder()
        .with_backend_arc(backend)
        .with_http_client(http()?)
        .with_transport_factory(TokioWebSocketTransportFactory::new().with_connector(connector()?))
        .with_runtime(TokioRuntime)
        .with_inbound_durability_hook_arc(inbound)
        .with_history_sync_capture_hook_arc(capture)
        .on_event(move |event, _client| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(event).await;
            }
        })
        .build()
        .await
        .map_err(|error| {
            tracing::warn!(%error, "whatsapp client did not start");
            Error::Platform
        })?;
    Ok((bot.spawn(), rx))
}
