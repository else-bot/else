//! Outbound HTTP clients use the ring Rustls provider, including in tests.

/// Initialize Rustls before reqwest constructs its TLS configuration. The
/// process-wide provider is immutable once installed; concurrent client
/// constructors keep the already-installed provider.
pub fn builder() -> reqwest::ClientBuilder {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder().tls_backend_rustls()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose,
        IsCa, KeyPair, KeyUsagePurpose,
    };
    use rustls::pki_types::PrivatePkcs8KeyDer;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn connect(host: &str, trust_test_ca: bool) -> (bool, bool) {
        // Short-lived, fresh certificates satisfy platform validity limits
        // without fixtures that eventually expire or trust-store changes.
        let now = time::OffsetDateTime::now_utc();
        let mut ca_params = CertificateParams::default();
        ca_params.not_before = now - time::Duration::days(1);
        ca_params.not_after = now + time::Duration::days(7);
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "else test CA");
        let ca = CertifiedIssuer::self_signed(ca_params, KeyPair::generate().unwrap()).unwrap();
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
        params.not_before = now - time::Duration::days(1);
        params.not_after = now + time::Duration::days(7);
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        let cert = params.signed_by(&key, &ca).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        // This deliberately cannot fall back to TLS 1.2.
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(5), async move {
                let (socket, _) = listener.accept().await.unwrap();
                let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
                let Ok(mut stream) = acceptor.accept(socket).await else {
                    return false;
                };
                assert_eq!(
                    stream.get_ref().1.protocol_version(),
                    Some(rustls::ProtocolVersion::TLSv1_3)
                );
                let mut request = Vec::new();
                let mut chunk = [0; 1024];
                while !request.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => return false,
                        Ok(n) => request.extend_from_slice(&chunk[..n]),
                    }
                    assert!(request.len() <= 8192);
                }
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await
                    .unwrap();
                true
            })
            .await
            .expect("local TLS server finished")
        });
        let mut client = super::builder()
            .no_proxy()
            .resolve("localhost", addr)
            .timeout(Duration::from_secs(5));
        if trust_test_ca {
            client = client.tls_certs_merge([reqwest::Certificate::from_der(ca.der()).unwrap()]);
        }
        let result = client
            .build()
            .unwrap()
            .get(format!("https://{host}:{}/", addr.port()))
            .send()
            .await;
        let accepted = match result {
            Ok(response) => response.status().is_success(),
            Err(error) => {
                eprintln!("local HTTPS request rejected: {error:?}");
                false
            }
        };
        (accepted, server.await.unwrap())
    }

    #[tokio::test]
    async fn trusted_https_negotiates_tls_13() {
        assert_eq!(connect("localhost", true).await, (true, true));
    }

    #[tokio::test]
    async fn https_rejects_an_untrusted_certificate() {
        assert_eq!(connect("localhost", false).await, (false, false));
    }

    #[tokio::test]
    async fn https_rejects_the_wrong_hostname_even_with_a_trusted_ca() {
        assert_eq!(connect("127.0.0.1", true).await, (false, false));
    }
}
