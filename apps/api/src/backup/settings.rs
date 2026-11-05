use std::io::{Read, Write};

use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use zeroize::Zeroizing;

use super::s3::S3Config;
use crate::credentials::SecretKey;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("Backup settings changed. Reload and try again.")]
    Conflict,
    #[error("A backup is running. Wait for it to finish before changing settings.")]
    Busy,
    #[error("Backup service is unavailable.")]
    Unavailable,
    #[error("Backup credentials could not be encrypted or decrypted.")]
    Crypto,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DestinationView {
    pub service: String,
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub access_key_id: String,
    pub path_style: bool,
    pub has_secret: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DestinationInput {
    pub service: String,
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub access_key_id: String,
    pub path_style: bool,
    pub secret_access_key: Option<String>,
}

#[derive(Clone)]
pub struct Destination {
    pub view: DestinationView,
    secret: Zeroizing<String>,
}

impl Destination {
    pub fn s3_config(&self) -> S3Config {
        let v = &self.view;
        S3Config {
            endpoint: v.endpoint.clone(),
            region: v.region.clone(),
            bucket: v.bucket.clone(),
            prefix: v.prefix.clone(),
            access_key_id: v.access_key_id.clone(),
            secret_access_key: self.secret.clone(),
            path_style: v.path_style,
        }
    }

    pub fn resolve(input: DestinationInput, saved: Option<&Self>) -> Result<Self, Error> {
        let secret = input
            .secret_access_key
            .map(Zeroizing::new)
            .or_else(|| saved.map(|d| d.secret.clone()))
            .ok_or(Error::Invalid("Enter the secret access key."))?;
        let endpoint = input.endpoint.trim().trim_end_matches('/').to_owned();
        let url = reqwest::Url::parse(&endpoint)
            .map_err(|_| Error::Invalid("Enter a valid endpoint URL."))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            return Err(Error::Invalid(
                "The endpoint must be an HTTP or HTTPS origin without a path or credentials.",
            ));
        }
        if !matches!(input.service.as_str(), "r2" | "s3" | "compatible") {
            return Err(Error::Invalid("Choose R2, S3, or S3 compatible storage."));
        }
        let region = input.region.trim().to_owned();
        let bucket = input.bucket.trim().to_owned();
        let access_key_id = input.access_key_id.trim().to_owned();
        if [&region, &bucket, &access_key_id]
            .iter()
            .any(|v| v.is_empty() || v.len() > 255 || v.chars().any(char::is_control))
            || bucket.contains('/')
            || bucket.contains('\\')
            || secret.is_empty()
            || secret.len() > 8192
        {
            return Err(Error::Invalid(
                "Enter a region, bucket, access key ID, and secret access key.",
            ));
        }
        let mut prefix = input.prefix.trim().trim_start_matches('/').to_owned();
        if prefix.len() > 512
            || prefix.chars().any(char::is_control)
            || prefix.contains('\\')
            || prefix.split('/').any(|p| p == "." || p == "..")
        {
            return Err(Error::Invalid(
                "Enter a folder without dot segments or control characters.",
            ));
        }
        if !prefix.is_empty() && !prefix.ends_with('/') {
            prefix.push('/');
        }
        Ok(Self {
            view: DestinationView {
                service: input.service,
                endpoint,
                region,
                bucket,
                prefix,
                access_key_id,
                path_style: input.path_style,
                has_secret: true,
            },
            secret,
        })
    }

    fn from_env(config: S3Config) -> Result<Self, Error> {
        let service = if config.endpoint.contains(".r2.cloudflarestorage.com") {
            "r2"
        } else if config.endpoint.contains(".amazonaws.com") {
            "s3"
        } else {
            "compatible"
        };
        Self::resolve(
            DestinationInput {
                service: service.into(),
                endpoint: config.endpoint,
                region: config.region,
                bucket: config.bucket,
                prefix: config.prefix,
                access_key_id: config.access_key_id,
                path_style: config.path_style,
                secret_access_key: Some(config.secret_access_key.to_string()),
            },
            None,
        )
    }
}

#[derive(Clone)]
pub struct Settings {
    pub revision: i64,
    pub enabled: bool,
    pub interval_secs: u64,
    pub keep: usize,
    pub destination: Option<Destination>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    pub expected_revision: i64,
    pub enabled: bool,
    pub interval_secs: u64,
    pub keep: usize,
    pub destination: Option<DestinationInput>,
}

impl Settings {
    pub fn updated(&self, input: Update) -> Result<Self, Error> {
        if input.expected_revision != self.revision {
            return Err(Error::Conflict);
        }
        if input.interval_secs > 30 * 86400
            || (input.interval_secs != 0 && input.interval_secs < 60)
        {
            return Err(Error::Invalid(
                "The interval must be manual or between one minute and 30 days.",
            ));
        }
        if !(1..=1000).contains(&input.keep) {
            return Err(Error::Invalid("Keep between 1 and 1000 backups."));
        }
        let destination = input
            .destination
            .map(|d| Destination::resolve(d, self.destination.as_ref()))
            .transpose()?;
        if input.enabled && destination.is_none() {
            return Err(Error::Invalid(
                "Configure a backup destination before enabling backups.",
            ));
        }
        Ok(Self {
            revision: self.revision + 1,
            enabled: input.enabled,
            interval_secs: input.interval_secs,
            keep: input.keep,
            destination,
        })
    }
}

fn identity(key: &SecretKey, salt: &[u8], revision: i64) -> Result<age::x25519::Identity, Error> {
    let salt = salt.try_into().map_err(|_| Error::Crypto)?;
    super::identity_for_purpose(
        key,
        &salt,
        &format!("else.bot/remote-backup-settings/v1/{revision}"),
    )
    .map_err(|_| Error::Crypto)
}

pub async fn save(pool: &SqlitePool, key: &SecretKey, settings: &Settings) -> Result<(), Error> {
    let (destination, salt, cipher) = match &settings.destination {
        None => (None, None, None),
        Some(d) => {
            let mut salt = [0u8; 32];
            getrandom::fill(&mut salt).map_err(|_| Error::Crypto)?;
            let recipient = identity(key, &salt, settings.revision)?.to_public();
            let encryptor =
                age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
                    .map_err(|_| Error::Crypto)?;
            let mut cipher = Vec::new();
            let mut writer = encryptor
                .wrap_output(&mut cipher)
                .map_err(|_| Error::Crypto)?;
            writer
                .write_all(d.secret.as_bytes())
                .and_then(|()| writer.finish().map(drop))
                .map_err(|_| Error::Crypto)?;
            (
                Some(serde_json::to_string(&d.view).map_err(|_| Error::Crypto)?),
                Some(salt.to_vec()),
                Some(cipher),
            )
        }
    };
    let result = sqlx::query("INSERT INTO backup_settings (id, revision, enabled, interval_secs, keep_count, destination, secret_salt, secret_cipher) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT(id) DO UPDATE SET revision = excluded.revision, enabled = excluded.enabled, interval_secs = excluded.interval_secs, keep_count = excluded.keep_count, destination = excluded.destination, secret_salt = excluded.secret_salt, secret_cipher = excluded.secret_cipher WHERE backup_settings.revision = excluded.revision - 1")
        .bind(settings.revision).bind(settings.enabled).bind(settings.interval_secs as i64).bind(settings.keep as i64)
        .bind(destination).bind(salt).bind(cipher).execute(pool).await?;
    if result.rows_affected() == 0 {
        return Err(Error::Conflict);
    }
    Ok(())
}

pub async fn load(pool: &SqlitePool, key: &SecretKey) -> Result<Option<Settings>, Error> {
    let Some(row) = sqlx::query("SELECT * FROM backup_settings WHERE id = 1")
        .fetch_optional(pool)
        .await?
    else {
        return Ok(None);
    };
    let revision = row.get("revision");
    let destination = match row.get::<Option<String>, _>("destination") {
        None => None,
        Some(value) => {
            let view = serde_json::from_str(&value).map_err(|_| Error::Crypto)?;
            let salt: Vec<u8> = row.get("secret_salt");
            let cipher: Vec<u8> = row.get("secret_cipher");
            let identity = identity(key, &salt, revision)?;
            let decryptor = age::Decryptor::new(cipher.as_slice()).map_err(|_| Error::Crypto)?;
            let mut reader = decryptor
                .decrypt(std::iter::once(&identity as &dyn age::Identity))
                .map_err(|_| Error::Crypto)?;
            let mut plain = Zeroizing::new(Vec::new());
            reader.read_to_end(&mut plain).map_err(|_| Error::Crypto)?;
            let secret =
                Zeroizing::new(String::from_utf8(plain.to_vec()).map_err(|_| Error::Crypto)?);
            Some(Destination { view, secret })
        }
    };
    Ok(Some(Settings {
        revision,
        enabled: row.get("enabled"),
        interval_secs: row.get::<i64, _>("interval_secs") as u64,
        keep: row.get::<i64, _>("keep_count") as usize,
        destination,
    }))
}

pub async fn load_or_seed(
    pool: &SqlitePool,
    key: &SecretKey,
    config: &crate::config::Config,
) -> Result<Settings, Error> {
    if let Some(settings) = load(pool, key).await? {
        return Ok(settings);
    }
    let (destination, valid) = match config.s3.clone().map(Destination::from_env).transpose() {
        Ok(destination) => (destination, true),
        Err(_) => {
            tracing::warn!(
                "remote backup environment configuration is invalid; local backups remain enabled"
            );
            (None, false)
        }
    };
    let settings = Settings {
        revision: 0,
        enabled: destination.is_some(),
        interval_secs: config.backup.interval.as_secs(),
        keep: config.backup.keep,
        destination,
    };
    if valid {
        save(pool, key, &settings).await?;
    }
    Ok(settings)
}
