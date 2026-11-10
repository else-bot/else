use super::{Client, ClientError};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupService {
    #[default]
    R2,
    S3,
    Compatible,
}

impl BackupService {
    pub fn label(self) -> &'static str {
        match self {
            Self::R2 => "Cloudflare R2",
            Self::S3 => "Amazon S3",
            Self::Compatible => "S3 compatible",
        }
    }
}

#[derive(Clone, Deserialize)]
pub struct BackupDestination {
    pub service: BackupService,
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub access_key_id: String,
    pub path_style: bool,
    pub has_secret: bool,
}

impl BackupDestination {
    pub fn input(&self) -> BackupDestinationInput {
        BackupDestinationInput {
            service: self.service,
            endpoint: self.endpoint.clone(),
            region: self.region.clone(),
            bucket: self.bucket.clone(),
            prefix: self.prefix.clone(),
            access_key_id: self.access_key_id.clone(),
            path_style: self.path_style,
            secret_access_key: None,
        }
    }
}

#[derive(Clone, Serialize)]
pub struct BackupDestinationInput {
    pub service: BackupService,
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub access_key_id: String,
    pub path_style: bool,
    pub secret_access_key: Option<String>,
}

#[derive(Clone, Deserialize)]
pub struct BackupEstimate {
    pub bytes: u64,
    pub basis: String,
}

#[derive(Clone, Deserialize)]
pub struct BackupRun {
    pub set_id: String,
    pub finished_at: Option<String>,
    pub bytes: Option<u64>,
    pub uploaded_bytes: Option<u64>,
    pub local_status: String,
    pub remote_status: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Deserialize)]
pub struct BackupSnapshot {
    pub revision: i64,
    pub enabled: bool,
    pub interval_secs: u64,
    pub keep: usize,
    pub destination: Option<BackupDestination>,
    pub running: bool,
    pub next_backup_at: Option<String>,
    pub estimate: Option<BackupEstimate>,
    pub recent: Vec<BackupRun>,
    pub remote_count: usize,
    pub remote_bytes: u64,
    pub unavailable_reason: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Serialize)]
pub struct BackupSettingsInput {
    pub expected_revision: i64,
    pub enabled: bool,
    pub interval_secs: u64,
    pub keep: usize,
    pub destination: Option<BackupDestinationInput>,
}

impl BackupSnapshot {
    pub fn input(&self) -> BackupSettingsInput {
        BackupSettingsInput {
            expected_revision: self.revision,
            enabled: self.enabled,
            interval_secs: self.interval_secs,
            keep: self.keep,
            destination: self.destination.as_ref().map(BackupDestination::input),
        }
    }
}

impl Client {
    pub async fn backups(self: Arc<Self>) -> Result<BackupSnapshot, ClientError> {
        self.call(Method::GET, "/v1/settings/backups".into(), None)
            .await
            .map(|(_, snapshot)| snapshot)
    }

    pub async fn save_backups(
        self: Arc<Self>,
        settings: BackupSettingsInput,
    ) -> Result<BackupSnapshot, ClientError> {
        self.call(
            Method::PUT,
            "/v1/settings/backups".into(),
            Some(serde_json::json!(settings)),
        )
        .await
        .map(|(_, snapshot)| snapshot)
    }

    pub async fn test_backup_destination(
        self: Arc<Self>,
        expected_revision: i64,
        destination: BackupDestinationInput,
    ) -> Result<String, ClientError> {
        #[derive(Deserialize)]
        struct Tested {
            message: String,
        }
        let this = self.clone();
        self.run(async move {
            let request = this
                .request(Method::POST, "/v1/settings/backups/test")
                .timeout(std::time::Duration::from_secs(60))
                .json(&serde_json::json!({ "expected_revision": expected_revision, "destination": destination }));
            this.send_json::<Tested>(request)
                .await
                .map(|(_, tested)| tested.message)
        })
        .await
    }

    pub async fn run_backup(self: Arc<Self>) -> Result<(), ClientError> {
        self.call::<serde_json::Value>(Method::POST, "/v1/settings/backups/run".into(), None)
            .await
            .map(|_| ())
    }
}
