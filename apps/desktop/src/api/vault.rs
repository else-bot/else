use super::{Client, ClientError};
use chrono::{DateTime, Utc};
use else_wire::secrets::{List, RevealedSecret, SavedSecret, VaultEntry, VaultUse};
use reqwest::Method;
use serde::Serialize;
use std::{collections::BTreeMap, sync::Arc};

// Value-bearing payloads deliberately do not implement Debug.
#[derive(Serialize)]
pub struct SaveSecret {
    pub env: BTreeMap<String, String>,
    pub description: String,
    pub website: Option<String>,
}

#[derive(Serialize)]
pub struct PatchSecret {
    pub description: String,
    pub website: Option<String>,
    pub set: BTreeMap<String, String>,
    pub remove: Vec<String>,
}

impl Client {
    pub async fn vault(self: Arc<Self>) -> Result<Vec<VaultEntry>, ClientError> {
        self.call::<List>(Method::GET, "/v1/task-secrets".into(), None)
            .await
            .map(|(_, list)| list.secrets)
    }

    /// Create an entry; the server refuses a taken name with a conflict.
    pub async fn create_secret(
        self: Arc<Self>,
        name: String,
        body: SaveSecret,
    ) -> Result<SavedSecret, ClientError> {
        self.call(
            Method::POST,
            format!("/v1/task-secrets/{name}"),
            Some(serde_json::json!(body)),
        )
        .await
        .map(|(_, result)| result)
    }

    pub async fn patch_secret(
        self: Arc<Self>,
        name: String,
        body: PatchSecret,
    ) -> Result<VaultEntry, ClientError> {
        self.call(
            Method::PATCH,
            format!("/v1/task-secrets/{name}"),
            Some(serde_json::json!(body)),
        )
        .await
        .map(|(_, result)| result)
    }

    pub async fn reveal_secret(
        self: Arc<Self>,
        name: String,
    ) -> Result<RevealedSecret, ClientError> {
        self.call(
            Method::POST,
            format!("/v1/task-secrets/{name}/reveal"),
            None,
        )
        .await
        .map(|(_, result)| result)
    }

    pub async fn delete_secret(self: Arc<Self>, name: String) -> Result<(), ClientError> {
        self.call::<()>(Method::DELETE, format!("/v1/task-secrets/{name}"), None)
            .await
            .map(|_| ())
    }
}

pub fn valid_secret_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('a'..='z'))
        && name.len() <= 40
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '-'))
}

pub fn valid_secret_var(name: &str) -> bool {
    const RESERVED: &[&str] = &[
        "PATH", "HOME", "USER", "LOGNAME", "SHELL", "PWD", "OLDPWD", "IFS", "ENV", "BASH_ENV",
        "DISPLAY", "TERM", "TMPDIR", "LANG", "PS4",
    ];
    let mut chars = name.chars();
    matches!(chars.next(), Some('A'..='Z' | '_'))
        && name.len() <= 64
        && chars.all(|c| matches!(c, 'A'..='Z' | '0'..='9' | '_'))
        && !RESERVED.contains(&name)
        && !name.starts_with("LD_")
        && !name.starts_with("ELSE_")
}

pub fn validate_secret_metadata(
    description: &str,
    website: &str,
) -> Result<Option<String>, String> {
    if description.len() > 200 {
        return Err("Keep the description within 200 bytes.".into());
    }
    let website = website.trim();
    if website.is_empty() {
        return Ok(None);
    }
    let host =
        url::Host::parse(website).map_err(|_| "Enter a website host, such as x.com.".to_owned())?;
    if website.contains(['/', ':', '@', '?', '#']) || host.to_string() != website.to_lowercase() {
        return Err("Enter a website host, such as x.com, without a URL path.".into());
    }
    Ok(Some(website.to_lowercase()))
}

pub fn vault_last_used(last: Option<&VaultUse>, now: DateTime<Utc>) -> String {
    let Some(last) = last else {
        return "Never used".into();
    };
    let seconds = (now - last.at).num_seconds().max(0);
    let ago = match seconds {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86400),
    };
    format!(
        "by {} {ago}",
        last.bot_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| last.bot_id.to_string())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use else_wire::secrets::VaultUseKind;
    use uuid::Uuid;

    #[test]
    fn vault_names_and_fields_reject_reserved_or_unsafe_names() {
        for name in ["x", "x-login_2", &"a".repeat(40)] {
            assert!(valid_secret_name(name));
        }
        for name in ["", "1x", "X", "a/b", &"a".repeat(41)] {
            assert!(!valid_secret_name(name));
        }
        for var in [
            "USERNAME",
            "PASSWORD",
            "TOTP_SECRET",
            "_KEY2",
            &"A".repeat(64),
        ] {
            assert!(valid_secret_var(var));
        }
        for var in [
            "",
            "password",
            "2KEY",
            "A-B",
            "PATH",
            "HOME",
            "BASH_ENV",
            "PS4",
            "LD_PRELOAD",
            "ELSE_TOKEN",
            &"A".repeat(65),
        ] {
            assert!(!valid_secret_var(var));
        }
    }

    #[test]
    fn vault_metadata_accepts_hosts_and_counts_description_bytes() {
        assert_eq!(
            validate_secret_metadata("X", " X.COM ").unwrap(),
            Some("x.com".into())
        );
        assert_eq!(validate_secret_metadata("", "").unwrap(), None);
        for host in [
            "https://x.com",
            "x.com/login",
            "a@x.com",
            "x.com:443",
            "a b",
        ] {
            assert!(validate_secret_metadata("", host).is_err());
        }
        assert!(validate_secret_metadata(&"é".repeat(100), "").is_ok());
        assert!(validate_secret_metadata(&"é".repeat(101), "").is_err());
    }

    #[test]
    fn a_future_vault_use_keeps_its_label() {
        let usage: VaultUse = serde_json::from_value(serde_json::json!({
            "bot_id": "00000000-0000-0000-0000-000000000000",
            "bot_name": "Helper",
            "use": "connector",
            "at": "2026-11-09T01:00:00Z"
        }))
        .unwrap();
        assert_eq!(usage.r#use.as_str(), "connector");
    }

    #[test]
    fn vault_last_use_names_the_bot_and_elapsed_time() {
        let now = Utc::now();
        let mut last = VaultUse {
            bot_id: Uuid::nil(),
            bot_name: Some("Helper".into()),
            r#use: VaultUseKind::Browser,
            at: now - chrono::Duration::hours(3),
        };
        assert_eq!(vault_last_used(None, now), "Never used");
        assert_eq!(vault_last_used(Some(&last), now), "by Helper 3h ago");
        last.at = now - chrono::Duration::minutes(2);
        assert_eq!(vault_last_used(Some(&last), now), "by Helper 2m ago");
        last.at = now - chrono::Duration::days(2);
        assert_eq!(vault_last_used(Some(&last), now), "by Helper 2d ago");
        last.at = now + chrono::Duration::seconds(10);
        assert_eq!(vault_last_used(Some(&last), now), "by Helper just now");
        last.bot_name = None;
        assert!(vault_last_used(Some(&last), now).contains(&Uuid::nil().to_string()));
    }
}
