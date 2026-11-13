use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use oauth2::{AccessToken, RefreshToken, TokenResponse, basic::BasicTokenType};
use rmcp::transport::auth::{
    AuthorizationMetadata, OAuthTokenResponse, StoredCredentials, VendorExtraTokenFields,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::Error;
use crate::credentials::SecretKey;

const MAX_BODY: usize = 64 * 1024;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Credential {
    pub(super) resource: String,
    pub(super) metadata: AuthorizationMetadata,
    pub(super) client_id: String,
    /// The secret dynamic registration issued, if any. A registered app's
    /// secret stays in the server configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) client_secret: Option<String>,
    pub(super) token: OAuthTokenResponse,
    pub(super) scopes: Vec<String>,
    pub(super) received_at: u64,
}

impl Credential {
    pub(super) fn from_stored(
        resource: String,
        metadata: AuthorizationMetadata,
        client_secret: Option<String>,
        stored: StoredCredentials,
    ) -> Option<Self> {
        Some(Self {
            resource,
            metadata,
            client_id: stored.client_id,
            client_secret,
            token: stored.token_response?,
            scopes: stored.granted_scopes,
            received_at: stored.token_received_at.unwrap_or_else(now),
        })
    }

    pub(super) fn stored(&self) -> StoredCredentials {
        StoredCredentials::new(
            self.client_id.clone(),
            Some(self.token.clone()),
            self.scopes.clone(),
            Some(self.received_at),
        )
        .with_issuer(self.metadata.issuer.clone())
    }

    pub(super) fn expires_at(&self) -> Option<u64> {
        self.token
            .expires_in()
            .map(|ttl| self.received_at.saturating_add(ttl.as_secs()))
    }
}

pub(super) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Sign-ins saved before the move to rmcp's OAuth client.
#[derive(Deserialize)]
struct Legacy {
    discovery: LegacyDiscovery,
    client_id: String,
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<i64>,
}

#[derive(Deserialize)]
struct LegacyDiscovery {
    resource: String,
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    scopes: Vec<String>,
    require_iss: bool,
}

impl From<Legacy> for Credential {
    fn from(legacy: Legacy) -> Self {
        let received_at = now();
        let mut token = OAuthTokenResponse::new(
            AccessToken::new(legacy.access_token),
            BasicTokenType::Bearer,
            VendorExtraTokenFields::default(),
        );
        token.set_refresh_token(legacy.refresh_token.map(RefreshToken::new));
        token.set_expires_in(
            legacy
                .expires_at
                .map(|at| Duration::from_secs(at.saturating_sub(received_at as i64).max(0) as u64))
                .as_ref(),
        );
        let discovery = legacy.discovery;
        let metadata = serde_json::from_value(serde_json::json!({
            "issuer": discovery.issuer,
            "authorization_endpoint": discovery.authorization_endpoint,
            "token_endpoint": discovery.token_endpoint,
            "registration_endpoint": discovery.registration_endpoint,
            "authorization_response_iss_parameter_supported": discovery.require_iss,
        }))
        .expect("legacy OAuth metadata");
        Self {
            resource: discovery.resource,
            metadata,
            client_id: legacy.client_id,
            client_secret: None,
            token,
            scopes: discovery.scopes,
            received_at,
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Saved {
    Current(Box<Credential>),
    Legacy(Box<Legacy>),
}

/// The package's one saved sign-in, shared by every bot. It keeps the file
/// name and encryption purpose of the nil bot, which stdio packages always
/// used. Sign-ins saved per bot by older versions are adopted on load.
#[derive(Clone)]
pub(in crate::mcp) struct Store {
    path: PathBuf,
    bot: Uuid,
    key: Option<Arc<SecretKey>>,
}

impl Store {
    pub(in crate::mcp) fn new(path: PathBuf, key: Option<Arc<SecretKey>>) -> Self {
        Self {
            path,
            bot: Uuid::nil(),
            key,
        }
    }

    #[cfg(test)]
    pub(in crate::mcp) fn for_bot(path: PathBuf, bot: Uuid, key: Option<Arc<SecretKey>>) -> Self {
        Self { path, bot, key }
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn require_key(&self) -> Result<(), Error> {
        self.key.as_ref().map(|_| ()).ok_or(Error::Invalid(
            "MCP sign-in needs the installation's SECRET_KEY.",
        ))
    }

    pub(super) async fn load(&self) -> Result<Option<Credential>, Error> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            if let Some(key) = store.key.as_deref()
                && store.bot.is_nil()
            {
                adopt_per_bot(&store.path, store.bot, key)?;
            }
            load(&store.path, store.bot, store.key.as_deref())
        })
        .await
        .map_err(|_| Error::Connection)?
    }

    pub(super) async fn save(&self, credential: &Credential) -> Result<(), Error> {
        let key = self.key.clone().ok_or(Error::Invalid(
            "MCP sign-in needs the installation's SECRET_KEY.",
        ))?;
        let path = self.path.clone();
        let bot = self.bot;
        let credential = credential.clone();
        tokio::task::spawn_blocking(move || save(&path, bot, &key, &credential))
            .await
            .map_err(|_| Error::Connection)?
    }

    pub(super) async fn remove(&self) -> Result<(), Error> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || remove(&store.path, store.bot))
            .await
            .map_err(|_| Error::Connection)?
    }
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    salt: String,
    ciphertext: String,
}

fn identity(
    key: &SecretKey,
    salt: &[u8; 32],
    path: &Path,
    bot: Uuid,
) -> Result<age::x25519::Identity, Error> {
    let package = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(Error::Connection)?;
    crate::backup::identity_for_purpose(
        key,
        salt,
        &format!("else.bot/mcp-oauth/v1/{package}/{bot}"),
    )
    .map_err(|_| Error::Invalid("The MCP credentials could not be encrypted or decrypted."))
}

fn filename(bot: Uuid) -> String {
    format!("oauth-{bot}.json")
}

pub(super) fn save(
    path: &Path,
    bot: Uuid,
    key: &SecretKey,
    credential: &impl Serialize,
) -> Result<(), Error> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut salt = [0u8; 32];
    getrandom::fill(&mut salt).expect("OS entropy");
    let identity = identity(key, &salt, path, bot)?;
    let recipient = identity.to_public();
    let encryptor =
        age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
            .map_err(|_| Error::Connection)?;
    let plain = Zeroizing::new(serde_json::to_vec(credential).map_err(|_| Error::Connection)?);
    let mut ciphertext = Vec::new();
    let mut writer = encryptor
        .wrap_output(&mut ciphertext)
        .map_err(|_| Error::Connection)?;
    writer.write_all(&plain)?;
    writer.finish()?;
    let bytes = serde_json::to_vec(&Envelope {
        salt: URL_SAFE_NO_PAD.encode(salt),
        ciphertext: URL_SAFE_NO_PAD.encode(ciphertext),
    })
    .map_err(|_| Error::Connection)?;
    if bytes.len() > MAX_BODY {
        return Err(Error::Invalid(
            "The encrypted MCP credentials exceed 64 KiB.",
        ));
    }
    super::super::require_dir(path)?;
    let staging = path.join(format!(".oauth-{}", Uuid::new_v4()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staging)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&staging, path.join(filename(bot)))?;
        std::fs::File::open(path)?.sync_all()?;
        Ok::<_, Error>(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(staging);
    }
    result
}

fn load(path: &Path, bot: Uuid, key: Option<&SecretKey>) -> Result<Option<Credential>, Error> {
    let text = match super::super::read_file(&path.join(filename(bot))) {
        Ok(text) => text,
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let key = key.ok_or(Error::Invalid(
        "MCP credentials need the installation's SECRET_KEY.",
    ))?;
    let envelope: Envelope = serde_json::from_str(&text)
        .map_err(|_| Error::Invalid("Invalid encrypted MCP credentials."))?;
    let salt: [u8; 32] = URL_SAFE_NO_PAD
        .decode(envelope.salt)
        .map_err(|_| Error::Connection)?
        .try_into()
        .map_err(|_| Error::Connection)?;
    let ciphertext = URL_SAFE_NO_PAD
        .decode(envelope.ciphertext)
        .map_err(|_| Error::Connection)?;
    let identity = identity(key, &salt, path, bot)?;
    let decryptor = age::Decryptor::new(ciphertext.as_slice()).map_err(|_| Error::Connection)?;
    let mut reader = decryptor
        .decrypt(std::iter::once(&identity as &dyn age::Identity))
        .map_err(|_| Error::Invalid("The MCP credentials could not be decrypted."))?;
    let mut plain = Zeroizing::new(Vec::new());
    reader.read_to_end(&mut plain)?;
    let credential = match serde_json::from_slice(&plain)
        .map_err(|_| Error::Invalid("Invalid decrypted MCP credentials."))?
    {
        Saved::Current(credential) => *credential,
        Saved::Legacy(legacy) => (*legacy).into(),
    };
    Ok(Some(credential))
}

/// Move the most recent sign-in saved for one bot to the shared file, then
/// delete every per-bot file, so a disconnect cannot leave one behind.
fn adopt_per_bot(path: &Path, shared: Uuid, key: &SecretKey) -> Result<(), Error> {
    let mut per_bot = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(bot) = name
            .to_str()
            .and_then(|n| n.strip_prefix("oauth-")?.strip_suffix(".json"))
            .and_then(|id| Uuid::parse_str(id).ok())
            .filter(|bot| *bot != shared)
        else {
            continue;
        };
        if entry.file_type()?.is_file() {
            per_bot.push((entry.metadata()?.modified()?, bot));
        }
    }
    if per_bot.is_empty() {
        return Ok(());
    }
    if !path.join(filename(shared)).exists() {
        // Restores do not keep file times, so the latest token expiry decides
        // which sign-in is newest; the file time only breaks ties.
        let newest = per_bot
            .iter()
            .filter_map(|(modified, bot)| match load(path, *bot, Some(key)) {
                Ok(credential) => credential.map(|c| (c.expires_at(), *modified, c)),
                Err(_) => {
                    tracing::warn!("Skipping an unreadable per-bot MCP sign-in");
                    None
                }
            })
            .max_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        if let Some((_, _, credential)) = newest {
            save(path, shared, key, &credential)?;
        }
    }
    for (_, bot) in per_bot {
        remove(path, bot)?;
    }
    Ok(())
}

fn remove(path: &Path, bot: Uuid) -> Result<(), Error> {
    match std::fs::remove_file(path.join(filename(bot))) {
        Ok(()) => {
            std::fs::File::open(path)?.sync_all()?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests;
