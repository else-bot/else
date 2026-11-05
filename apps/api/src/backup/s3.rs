//! A small S3 client for backup replication: AWS Signature V4 over
//! HTTP(S), with only the operations the emulator probe verified so the
//! behavior on R2 stays predictable: `PutObject` (optionally with
//! `If-None-Match: *`), multipart create, upload, complete, list, and abort,
//! `HeadObject`, `GetObject`, `ListObjectsV2`, and `DeleteObject`. The
//! endpoint, region, bucket, prefix, and path-style addressing come from
//! configuration; no AWS endpoint is assumed. Credentials are never logged.

use std::time::Duration;

use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[derive(Debug, thiserror::Error)]
pub enum S3Error {
    #[error("S3 request failed: {0}")]
    Transport(String),
    #[error("S3 answered HTTP {status}")]
    Status { status: u16, code: String },
    #[error("S3 response: {0}")]
    Protocol(String),
}

#[derive(Clone)]
pub struct S3Config {
    /// `http(s)://host[:port]`, without a path.
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    /// Key prefix for every object (normalized to end in `/` when set).
    pub prefix: String,
    pub access_key_id: String,
    pub secret_access_key: Zeroizing<String>,
    /// Path-style addressing (`endpoint/bucket/key`), else virtual-hosted.
    pub path_style: bool,
}

impl std::fmt::Debug for S3Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "S3Config({} bucket {} prefix {:?}, key <redacted>)",
            self.endpoint, self.bucket, self.prefix
        )
    }
}

/// The outcome of a create-only put.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Put {
    Created,
    /// The key existed (`412` to `If-None-Match: *`); nothing was written.
    Exists,
}

#[derive(Clone)]
pub struct S3 {
    config: S3Config,
    http: reqwest::Client,
    scheme: String,
    /// The authority exactly as sent: lowercase host, and a port only when
    /// it is not the scheme's default (what the HTTP client transmits, so
    /// the signed `host` matches).
    authority: String,
}

fn uri_encode(text: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if unreserved || (keep_slash && b == b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = Hmac::<sha2_010::Sha256>::new_from_slice(key).expect("any key length");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

fn xml_unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn tags(xml: &str, name: &str) -> Vec<String> {
    let (open, close) = (format!("<{name}>"), format!("</{name}>"));
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else { break };
        out.push(after[..end].to_owned());
        rest = &after[end + close.len()..];
    }
    out
}

fn tag(xml: &str, name: &str) -> Option<String> {
    tags(xml, name).into_iter().next().map(|t| xml_unescape(&t))
}

impl S3 {
    pub fn new(mut config: S3Config) -> Result<Self, S3Error> {
        let url = reqwest::Url::parse(&config.endpoint)
            .map_err(|_| S3Error::Protocol("the endpoint is not a URL".into()))?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.path().trim_matches('/').is_empty()
            || url.query().is_some()
            || !url.username().is_empty()
        {
            return Err(S3Error::Protocol(
                "the endpoint must be an http(s) URL without a path".into(),
            ));
        }
        let host = url
            .host_str()
            .ok_or_else(|| S3Error::Protocol("the endpoint has no host".into()))?
            .to_ascii_lowercase();
        // `port()` is `None` for the scheme's default port, as sent.
        let authority = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host,
        };
        let scheme = url.scheme().to_owned();
        config.endpoint = format!("{scheme}://{authority}");
        if !config.prefix.is_empty() && !config.prefix.ends_with('/') {
            config.prefix.push('/');
        }
        // An idle timeout rather than a total one: a multi-gigabyte archive
        // streams for as long as data keeps arriving.
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| S3Error::Transport(e.to_string()))?;
        Ok(Self {
            config,
            http,
            scheme,
            authority,
        })
    }

    /// Identifies the destination (endpoint, bucket, and prefix), so state
    /// recorded for one bucket is never taken for another.
    pub fn destination(&self) -> String {
        format!(
            "{}://{}/{}/{}",
            self.scheme, self.authority, self.config.bucket, self.config.prefix
        )
    }

    pub fn config(&self) -> &S3Config {
        &self.config
    }

    /// The full key of `relative` under the configured prefix.
    pub fn key(&self, relative: &str) -> String {
        format!("{}{relative}", self.config.prefix)
    }

    fn target(&self, key: &str) -> (String, String, String) {
        let (scheme, host) = (self.scheme.as_str(), self.authority.as_str());
        let encoded_key = uri_encode(key, true);
        if self.config.path_style {
            let path = if key.is_empty() {
                format!("/{}", self.config.bucket)
            } else {
                format!("/{}/{encoded_key}", self.config.bucket)
            };
            (format!("{scheme}://{host}{path}"), host.to_owned(), path)
        } else {
            let host = format!("{}.{host}", self.config.bucket);
            let path = format!("/{encoded_key}");
            (format!("{scheme}://{host}{path}"), host, path)
        }
    }

    fn request(
        &self,
        method: reqwest::Method,
        key: &str,
        query: &[(&str, &str)],
        headers: &[(&str, String)],
        payload_sha256: &str,
    ) -> reqwest::RequestBuilder {
        let (base, host, path) = self.target(key);
        let mut pairs: Vec<(String, String)> = query
            .iter()
            .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
            .collect();
        pairs.sort();
        let canonical_query = pairs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let now = chrono::Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date = now.format("%Y%m%d").to_string();
        let mut signed: Vec<(String, String)> = vec![
            ("host".into(), host.clone()),
            ("x-amz-content-sha256".into(), payload_sha256.to_owned()),
            ("x-amz-date".into(), amz_date.clone()),
        ];
        for (name, value) in headers {
            signed.push((name.to_ascii_lowercase(), value.trim().to_owned()));
        }
        signed.sort();
        let canonical_headers: String = signed.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
        let signed_headers = signed
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>()
            .join(";");
        let canonical = format!(
            "{method}\n{path}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_sha256}"
        );
        let scope = format!("{date}/{}/s3/aws4_request", self.config.region);
        let to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            hex::encode(Sha256::digest(canonical.as_bytes()))
        );
        let secret = Zeroizing::new(format!("AWS4{}", self.config.secret_access_key.as_str()));
        let k_date = Zeroizing::new(hmac(secret.as_bytes(), &date));
        let k_region = Zeroizing::new(hmac(&k_date, &self.config.region));
        let k_service = Zeroizing::new(hmac(&k_region, "s3"));
        let k_signing = Zeroizing::new(hmac(&k_service, "aws4_request"));
        let signature = hex::encode(hmac(&k_signing, &to_sign));
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.config.access_key_id
        );
        let url = if canonical_query.is_empty() {
            base
        } else {
            format!("{base}?{canonical_query}")
        };
        let mut builder = self
            .http
            .request(method, url)
            .header("x-amz-content-sha256", payload_sha256)
            .header("x-amz-date", amz_date)
            .header("authorization", authorization);
        for (name, value) in headers {
            builder = builder.header(*name, value);
        }
        builder
    }

    async fn send(&self, builder: reqwest::RequestBuilder) -> Result<reqwest::Response, S3Error> {
        let response = builder
            .send()
            .await
            .map_err(|e| S3Error::Transport(e.without_url().to_string()))?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();
        Err(S3Error::Status {
            status,
            code: match tag(&body, "Code").as_deref() {
                Some("BucketAlreadyOwnedByYou") => "BucketAlreadyOwnedByYou",
                Some("BucketAlreadyExists") => "BucketAlreadyExists",
                _ => "RemoteError",
            }
            .to_owned(),
        })
    }

    /// Create the bucket (test and setup use; the worker never creates one).
    pub async fn create_bucket(&self) -> Result<(), S3Error> {
        let builder = self.request(reqwest::Method::PUT, "", &[], &[], EMPTY_SHA256);
        match self.send(builder).await {
            Ok(_) => Ok(()),
            Err(S3Error::Status { code, .. })
                if code == "BucketAlreadyOwnedByYou" || code == "BucketAlreadyExists" =>
            {
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    /// Put a whole object with its SHA-256 checksum (rejected by the store
    /// if the bytes differ). With `create_only`, an existing key is left
    /// unchanged and reported.
    pub async fn put(&self, key: &str, body: Vec<u8>, create_only: bool) -> Result<Put, S3Error> {
        let digest = Sha256::digest(&body);
        let mut headers = vec![(
            "x-amz-checksum-sha256",
            base64::engine::general_purpose::STANDARD.encode(digest),
        )];
        if create_only {
            headers.push(("if-none-match", "*".to_owned()));
        }
        let builder = self
            .request(
                reqwest::Method::PUT,
                key,
                &[],
                &headers,
                &hex::encode(digest),
            )
            .body(body);
        match self.send(builder).await {
            Ok(_) => Ok(Put::Created),
            Err(S3Error::Status { status: 412, .. }) if create_only => Ok(Put::Exists),
            Err(err) => Err(err),
        }
    }

    /// The object's length, or `None` if it does not exist.
    pub async fn head(&self, key: &str) -> Result<Option<u64>, S3Error> {
        let builder = self.request(reqwest::Method::HEAD, key, &[], &[], EMPTY_SHA256);
        match self.send(builder).await {
            // The header itself: a HEAD response has no body to size.
            Ok(response) => Ok(response
                .headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())),
            Err(S3Error::Status { status: 404, .. }) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Stream an object, calling `chunk` for each piece; at most `max`
    /// bytes are accepted. Returns its length and SHA-256.
    pub async fn get_with(
        &self,
        key: &str,
        max: u64,
        mut chunk: impl FnMut(&[u8]) -> Result<(), S3Error>,
    ) -> Result<(u64, String), S3Error> {
        let builder = self.request(reqwest::Method::GET, key, &[], &[], EMPTY_SHA256);
        let mut response = self.send(builder).await?;
        let mut hasher = Sha256::new();
        let mut total = 0u64;
        while let Some(piece) = response
            .chunk()
            .await
            .map_err(|e| S3Error::Transport(e.without_url().to_string()))?
        {
            total += piece.len() as u64;
            if total > max {
                return Err(S3Error::Protocol(format!("{key} is larger than expected")));
            }
            hasher.update(&piece);
            chunk(&piece)?;
        }
        Ok((total, hex::encode(hasher.finalize())))
    }

    /// A small object whole.
    pub async fn get_bytes(&self, key: &str, max: u64) -> Result<Vec<u8>, S3Error> {
        let mut out = Vec::new();
        self.get_with(key, max, |piece| {
            out.extend_from_slice(piece);
            Ok(())
        })
        .await?;
        Ok(out)
    }

    pub async fn create_multipart(&self, key: &str) -> Result<String, S3Error> {
        let builder = self.request(
            reqwest::Method::POST,
            key,
            &[("uploads", "")],
            &[],
            EMPTY_SHA256,
        );
        let body = self
            .send(builder)
            .await?
            .text()
            .await
            .map_err(|e| S3Error::Transport(e.to_string()))?;
        tag(&body, "UploadId").ok_or_else(|| S3Error::Protocol("no UploadId".into()))
    }

    pub async fn upload_part(
        &self,
        key: &str,
        upload_id: &str,
        number: u32,
        body: Vec<u8>,
    ) -> Result<String, S3Error> {
        let digest = hex::encode(Sha256::digest(&body));
        let number = number.to_string();
        let builder = self
            .request(
                reqwest::Method::PUT,
                key,
                &[("partNumber", &number), ("uploadId", upload_id)],
                &[],
                &digest,
            )
            .body(body);
        let response = self.send(builder).await?;
        response
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .ok_or_else(|| S3Error::Protocol("no ETag for the part".into()))
    }

    pub async fn complete_multipart(
        &self,
        key: &str,
        upload_id: &str,
        parts: &[(u32, String)],
    ) -> Result<(), S3Error> {
        let mut xml = String::from("<CompleteMultipartUpload>");
        for (number, etag) in parts {
            xml.push_str(&format!(
                "<Part><PartNumber>{number}</PartNumber><ETag>{}</ETag></Part>",
                etag.replace('&', "&amp;").replace('<', "&lt;")
            ));
        }
        xml.push_str("</CompleteMultipartUpload>");
        let digest = hex::encode(Sha256::digest(xml.as_bytes()));
        let builder = self
            .request(
                reqwest::Method::POST,
                key,
                &[("uploadId", upload_id)],
                &[],
                &digest,
            )
            .body(xml);
        let body = self
            .send(builder)
            .await?
            .text()
            .await
            .map_err(|e| S3Error::Transport(e.to_string()))?;
        // A 200 response can still carry an error document.
        if body.contains("<Error>") {
            return Err(S3Error::Protocol(
                "multipart completion was rejected by remote storage".into(),
            ));
        }
        Ok(())
    }

    pub async fn abort_multipart(&self, key: &str, upload_id: &str) -> Result<(), S3Error> {
        let builder = self.request(
            reqwest::Method::DELETE,
            key,
            &[("uploadId", upload_id)],
            &[],
            EMPTY_SHA256,
        );
        match self.send(builder).await {
            Ok(_) | Err(S3Error::Status { status: 404, .. }) => Ok(()),
            Err(err) => Err(err),
        }
    }

    /// Unfinished multipart uploads under `prefix`: (key, upload ID).
    pub async fn list_multipart(&self, prefix: &str) -> Result<Vec<(String, String)>, S3Error> {
        let builder = self.request(
            reqwest::Method::GET,
            "",
            &[("uploads", ""), ("prefix", prefix)],
            &[],
            EMPTY_SHA256,
        );
        let body = self
            .send(builder)
            .await?
            .text()
            .await
            .map_err(|e| S3Error::Transport(e.to_string()))?;
        Ok(tags(&body, "Upload")
            .iter()
            .filter_map(|upload| Some((tag(upload, "Key")?, tag(upload, "UploadId")?)))
            .collect())
    }

    /// Every object under `prefix`: (key, size), following pagination.
    pub async fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>, S3Error> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![("list-type", "2"), ("prefix", prefix)];
            if let Some(token) = &token {
                query.push(("continuation-token", token));
            }
            let builder = self.request(reqwest::Method::GET, "", &query, &[], EMPTY_SHA256);
            let body = self
                .send(builder)
                .await?
                .text()
                .await
                .map_err(|e| S3Error::Transport(e.to_string()))?;
            for contents in tags(&body, "Contents") {
                if let Some(key) = tag(&contents, "Key") {
                    let size = tag(&contents, "Size")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    out.push((key, size));
                }
            }
            if tag(&body, "IsTruncated").as_deref() != Some("true") {
                return Ok(out);
            }
            token =
                Some(tag(&body, "NextContinuationToken").ok_or_else(|| {
                    S3Error::Protocol("truncated listing without a token".into())
                })?);
        }
    }

    pub async fn delete(&self, key: &str) -> Result<(), S3Error> {
        let builder = self.request(reqwest::Method::DELETE, key, &[], &[], EMPTY_SHA256);
        match self.send(builder).await {
            Ok(_) | Err(S3Error::Status { status: 404, .. }) => Ok(()),
            Err(err) => Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_matches_the_aws_reference_key_derivation() {
        // AWS SigV4 documentation example: the signing key for this date,
        // region, and service.
        let secret = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
        let k_date = hmac(format!("AWS4{secret}").as_bytes(), "20120215");
        let k_region = hmac(&k_date, "us-east-1");
        let k_service = hmac(&k_region, "iam");
        let k_signing = hmac(&k_service, "aws4_request");
        assert_eq!(
            hex::encode(k_signing),
            "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d"
        );
    }

    #[test]
    fn keys_and_queries_are_encoded_for_signing() {
        assert_eq!(uri_encode("sets/a b+c.age", true), "sets/a%20b%2Bc.age");
        assert_eq!(uri_encode("a/b", false), "a%2Fb");
        let xml = "<R><Upload><Key>p/x&amp;y</Key><UploadId>u1</UploadId></Upload><Upload><Key>q</Key><UploadId>u2</UploadId></Upload></R>";
        let uploads: Vec<_> = tags(xml, "Upload")
            .iter()
            .map(|u| (tag(u, "Key").unwrap(), tag(u, "UploadId").unwrap()))
            .collect();
        assert_eq!(
            uploads,
            [("p/x&y".into(), "u1".into()), ("q".into(), "u2".into())]
        );
    }

    #[test]
    fn credentials_are_not_debug_printed() {
        let config = S3Config {
            endpoint: "http://127.0.0.1:1".into(),
            region: "auto".into(),
            bucket: "b".into(),
            prefix: "p".into(),
            access_key_id: "AKID".into(),
            secret_access_key: Zeroizing::new("SECRET".into()),
            path_style: true,
        };
        assert!(!format!("{config:?}").contains("SECRET"));
        let s3 = S3::new(config.clone()).unwrap();
        assert_eq!(s3.key("sets/x"), "p/sets/x");
        let with = |endpoint: &str| {
            S3::new(S3Config {
                endpoint: endpoint.into(),
                ..config.clone()
            })
        };
        assert_eq!(
            with("https://ACCOUNT.R2.example.com:443/")
                .unwrap()
                .target("k")
                .1,
            "account.r2.example.com"
        );
        assert_eq!(
            with("http://127.0.0.1:8333").unwrap().target("k").1,
            "127.0.0.1:8333"
        );
        assert!(with("https://host/path").is_err());
    }
}
