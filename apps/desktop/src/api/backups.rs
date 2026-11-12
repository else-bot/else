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
    /// Absent from servers that predate the daily local schedule.
    #[serde(default)]
    pub local: Option<LocalBackupSettings>,
    #[serde(default)]
    pub pause: Option<ServerPause>,
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

#[derive(Clone, Debug, Deserialize)]
pub struct LocalBackupSettings {
    pub enabled: bool,
    pub time: String,
    pub timezone: String,
    pub utc_offset_secs: i32,
    pub keep: usize,
    pub managed_by_env: bool,
    pub next_at: Option<String>,
    pub copies: usize,
    pub bytes: u64,
    pub free_bytes: Option<u64>,
    pub recent: Vec<LocalBackupRun>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalBackupStatus {
    Running,
    Completed,
    Partial,
    Failed,
    /// A status this desktop does not know yet.
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Deserialize)]
pub struct LocalBackupRun {
    pub started_at: String,
    pub finished_at: Option<String>,
    pub status: LocalBackupStatus,
    pub bytes: Option<u64>,
    pub took_secs: Option<u64>,
    pub paused_secs: Option<u64>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    Backup,
    Update,
    /// A reason this desktop does not know yet.
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ServerPause {
    pub reason: PauseReason,
    pub expected_secs: Option<u64>,
    /// How long the pause had run when the server sent this.
    #[serde(default)]
    pub elapsed_secs: u64,
}

#[derive(Serialize)]
pub struct LocalBackupSettingsInput {
    pub expected_revision: i64,
    pub enabled: bool,
    pub time: String,
    pub keep: usize,
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

    pub async fn save_local_backups(
        self: Arc<Self>,
        settings: LocalBackupSettingsInput,
    ) -> Result<BackupSnapshot, ClientError> {
        self.call(
            Method::PUT,
            "/v1/settings/backups/local".into(),
            Some(serde_json::json!(settings)),
        )
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn backup_snapshot_parses_local_schedule_history_and_pause() {
        let mut value = json!({
            "revision": 3, "enabled": false, "interval_secs": 0, "keep": 7,
            "destination": null, "running": false, "next_backup_at": null,
            "estimate": null, "recent": [], "remote_count": 0, "remote_bytes": 0,
            "unavailable_reason": null, "last_error": null,
            "local": {
                "enabled": true, "time": "03:00", "timezone": "Asia/Dubai",
                "utc_offset_secs": 14400, "keep": 7, "managed_by_env": false,
                "next_at": "2026-11-12T23:00:00Z", "copies": 7,
                "bytes": 98400000000u64, "free_bytes": 687000000000u64,
                "recent": [
                    { "set_id": "complete", "started_at": "2026-11-12T06:45:26.950Z",
                      "finished_at": "2026-11-12T06:50:53.914Z", "status": "completed",
                      "bytes": 14590849354u64, "took_secs": 327, "paused_secs": 327, "error": null },
                    { "set_id": "running", "started_at": "2026-11-12T06:45:26.950Z",
                      "finished_at": null, "status": "running", "bytes": null,
                      "took_secs": null, "paused_secs": null, "error": null },
                    { "set_id": "partial", "started_at": "2026-11-11T06:45:26.950Z",
                      "finished_at": "2026-11-11T06:50:53Z", "status": "partial", "bytes": null,
                      "took_secs": 327, "paused_secs": null, "error": "One computer missing" },
                    { "set_id": "failed", "started_at": "2026-11-10T06:45:26.950Z",
                      "finished_at": "2026-11-10T06:50:53Z", "status": "failed", "bytes": null,
                      "took_secs": 327, "paused_secs": null, "error": "Interrupted" }
                ]
            },
            "pause": { "reason": "backup", "started_at": "2026-11-12T06:45:26.950Z",
                "expected_secs": 327, "phase": "computers", "done": 6, "total": 11 }
        });
        let snapshot: BackupSnapshot = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(snapshot.local.as_ref().unwrap().time, "03:00");
        assert_eq!(snapshot.local.as_ref().unwrap().utc_offset_secs, 14400);
        assert_eq!(snapshot.local.as_ref().unwrap().bytes, 98400000000);
        assert_eq!(
            snapshot.local.as_ref().unwrap().recent[0].status,
            LocalBackupStatus::Completed
        );
        assert_eq!(
            snapshot.local.as_ref().unwrap().recent[0].paused_secs,
            Some(327)
        );
        assert_eq!(
            snapshot.local.as_ref().unwrap().recent[1].status,
            LocalBackupStatus::Running
        );
        assert_eq!(
            snapshot.local.as_ref().unwrap().recent[2].status,
            LocalBackupStatus::Partial
        );
        assert_eq!(
            snapshot.local.as_ref().unwrap().recent[3].status,
            LocalBackupStatus::Failed
        );
        let pause = snapshot.pause.unwrap();
        assert_eq!(pause.reason, PauseReason::Backup);
        value["pause"] = json!(null);
        value["local"]["free_bytes"] = json!(null);
        value["local"]["next_at"] = json!(null);
        value["local"]["managed_by_env"] = json!(true);
        let snapshot: BackupSnapshot = serde_json::from_value(value.clone()).unwrap();
        assert!(snapshot.pause.is_none());
        assert!(snapshot.local.as_ref().unwrap().free_bytes.is_none());
        assert!(snapshot.local.as_ref().unwrap().next_at.is_none());
        assert!(snapshot.local.as_ref().unwrap().managed_by_env);
        // Newer servers may add statuses and reasons; older ones omit `local`.
        value["local"]["recent"][0]["status"] = json!("verifying");
        value["pause"] = json!({ "reason": "migration", "expected_secs": null });
        let snapshot: BackupSnapshot = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(
            snapshot.local.unwrap().recent[0].status,
            LocalBackupStatus::Unknown
        );
        assert_eq!(snapshot.pause.unwrap().reason, PauseReason::Other);
        let object = value.as_object_mut().unwrap();
        object.remove("local");
        object.remove("pause");
        let snapshot: BackupSnapshot = serde_json::from_value(value).unwrap();
        assert!(snapshot.local.is_none() && snapshot.pause.is_none());
    }

    #[test]
    fn backup_local_schedule_serializes_the_shared_revision() {
        assert_eq!(
            serde_json::to_value(LocalBackupSettingsInput {
                expected_revision: 3,
                enabled: true,
                time: "03:00".into(),
                keep: 7,
            })
            .unwrap(),
            json!({ "expected_revision": 3, "enabled": true, "time": "03:00", "keep": 7 })
        );
    }
}
