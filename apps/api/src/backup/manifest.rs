//! The records a set publishes: the unencrypted completion and the
//! encrypted manifest it names.

use serde::{Deserialize, Serialize};

/// Recorded with each set so recovery can select the key; holds no key
/// bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Encryption {
    pub format: String,
    pub key_id: String,
    pub kdf: String,
    pub info: String,
    pub salt_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectRef {
    pub object: String,
    pub ciphertext_bytes: u64,
    pub ciphertext_sha256: String,
}

/// The unencrypted bootstrap, published last. Untrusted until the manifest
/// it names decrypts and matches it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Completion {
    pub schema_version: u32,
    pub set_id: String,
    pub encryption: Encryption,
    pub manifest: ObjectRef,
    /// A hint for retention only (the manifest is authoritative): the set
    /// recovers the whole assistant.
    #[serde(default)]
    pub whole_assistant: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactKind {
    /// The database image, always first.
    Sqlite,
    /// One bot's disk archive, see [`ComputerEntry::carries_image`].
    Vm,
    /// A host artifact a bot fetched, `file-<artifact id>.age`.
    File,
    /// A bot's project directory, see [`projects::Format`].
    Project,
    /// The shared MCP library, `mcp.tar.age`.
    Mcp,
}

/// What a set holds of one bot's computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VmState {
    Included,
    /// The bot never used a computer.
    None,
    /// Gone from the runtime.
    Missing,
    OmittedDeletedBot,
    /// Not captured, as in a database-only set.
    Omitted,
}

/// What a set holds of one bot's project directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectState {
    Included,
    /// Some entries could not be read.
    Partial,
    /// The bot has no project directory.
    None,
    OmittedDeletedBot,
    /// Not captured, as in sets made before project directories.
    #[default]
    Omitted,
}

impl ProjectState {
    /// The set holds an archive of the directory.
    pub fn archived(self) -> bool {
        matches!(self, Self::Included | Self::Partial)
    }
}

/// How much of one kind of state a set covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Covered {
    Included,
    Partial,
    #[default]
    Omitted,
}

/// Native provider sessions are never in a set; providers need a fresh
/// login check after a restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderState {
    OmittedReauthRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub kind: ArtifactKind,
    pub object: String,
    /// For a `vm` artifact, its bot.
    pub bot_id: Option<uuid::Uuid>,
    pub ciphertext_bytes: u64,
    pub ciphertext_sha256: String,
    pub plaintext_bytes: u64,
    pub plaintext_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Application {
    pub version: String,
    /// The newest applied migration.
    pub migration_version: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sqlite {
    pub server_version: String,
    pub dump_version: String,
    pub format: String,
    /// Extensions the restore target needs (name and version).
    pub extensions: Vec<(String, String)>,
}

/// Row count and content digest of one table, taken in the dump's snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableDigest {
    pub table: String,
    pub rows: i64,
    /// SHA-256 over the table's sorted row hashes.
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BotEntry {
    pub id: uuid::Uuid,
    pub name: String,
    pub status: crate::db::bots::BotStatus,
    pub vm: VmState,
    pub computer: Option<ComputerEntry>,
    /// Conversation context is rebuilt from SQLite.
    pub provider_state: ProviderState,
    /// The native sessions the bot had (not included), for the record.
    pub provider_sessions: Vec<SessionEntry>,
    #[serde(default)]
    pub project: ProjectState,
}

/// A bot computer captured as a disk archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputerEntry {
    pub object: String,
    pub runtime_name: String,
    pub runtime_version: String,
    pub image: String,
    pub cpus: u32,
    pub memory_mib: u32,
    /// The owner's intent (`auto` or `stopped`), restored as recorded.
    pub desired_state: crate::computers::Desired,
    /// Network profile; restore applies its policy explicitly.
    pub network: String,
    pub desktop_port: Option<u16>,
    /// The desktop's VNC password (the guest holds the same in its disk).
    pub desktop_password: Option<String>,
    /// It was running before the backup stopped it for capture.
    pub was_running: bool,
    /// For a desktop: `ok` when its browser was closed cleanly before the
    /// disk was captured.
    pub browser_checkpoint: Option<String>,
    /// Where the computer mounts the bot's project directory, if it was
    /// created with that mount; a restore binds the restored directory
    /// there. Absent in sets made before project mounts.
    #[serde(default)]
    pub project_mount: Option<ProjectMount>,
    /// The archive holds the computer's image. A set stores each image in
    /// the first archive that uses it only, and a restore imports that one
    /// first; sets before schema 3 hold it in every archive.
    #[serde(default = "carries_image")]
    pub carries_image: bool,
}

fn carries_image() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectMount {
    pub guest: String,
    /// The guest owner of the mounted files.
    pub uid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionEntry {
    pub provider: String,
    pub native_session_id: String,
    pub status: String,
}

/// What a set contains, stated explicitly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Coverage {
    pub database: Covered,
    pub vm: Covered,
    /// Bot project directories.
    #[serde(default)]
    pub projects: Covered,
    /// Shared MCP skills, documentation, and connection config: never
    /// partial.
    #[serde(default)]
    pub mcp: Covered,
    pub provider_state: ProviderState,
    /// True only when every state the assistant needs is included.
    pub whole_assistant: bool,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub set_id: String,
    pub created_at: String,
    pub capture_started_at: String,
    pub capture_finished_at: String,
    pub encryption: Encryption,
    pub application: Application,
    pub sqlite: Sqlite,
    pub artifacts: Vec<Artifact>,
    pub tables: Vec<TableDigest>,
    pub bots: Vec<BotEntry>,
    pub coverage: Coverage,
}

/// The coverage of a database-only set.
pub fn database_only_coverage() -> Coverage {
    Coverage {
        database: Covered::Included,
        vm: Covered::Omitted,
        projects: Covered::Omitted,
        mcp: Covered::Omitted,
        provider_state: ProviderState::OmittedReauthRequired,
        whole_assistant: false,
        note: "Database only: bot computers (files, browser profiles, running jobs), the shared \
               MCP library and native provider sessions are not included. Restoring this set does not recover the \
               whole assistant."
            .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A schema 4 manifest with every artifact kind, bot entry state and
    /// coverage value, as the capture writes it (compact JSON).
    const GOLDEN_MANIFEST: &str = concat!(
        r#"{"schema_version":4,"set_id":"20261121T000000000Z-0a1b2c3d","#,
        r#""created_at":"2026-11-21T00:00:02.000Z","capture_started_at":"2026-11-21T00:00:00.000Z","#,
        r#""capture_finished_at":"2026-11-21T00:00:02.000Z","#,
        r#""encryption":{"format":"age-encryption.org/v1","key_id":"sk1-0011223344556677","kdf":"HKDF-SHA-256","info":"else.bot/backups/v1","salt_hex":"00"},"#,
        r#""application":{"version":"0.1.0","migration_version":48},"#,
        r#""sqlite":{"server_version":"3.50.0","dump_version":"3.50.0","format":"sqlite3","extensions":[]},"#,
        r#""artifacts":["#,
        r#"{"kind":"sqlite","object":"database.dump.age","bot_id":null,"ciphertext_bytes":10,"ciphertext_sha256":"c0","plaintext_bytes":8,"plaintext_sha256":"p0"},"#,
        r#"{"kind":"vm","object":"vm-00000000-0000-0000-0000-00000000000a.msb.age","bot_id":"00000000-0000-0000-0000-00000000000a","ciphertext_bytes":11,"ciphertext_sha256":"c1","plaintext_bytes":9,"plaintext_sha256":"p1"},"#,
        r#"{"kind":"file","object":"file-00000000-0000-0000-0000-0000000000f1.age","bot_id":"00000000-0000-0000-0000-00000000000a","ciphertext_bytes":12,"ciphertext_sha256":"c2","plaintext_bytes":10,"plaintext_sha256":"p2"},"#,
        r#"{"kind":"project","object":"project-00000000-0000-0000-0000-00000000000a.tar.zst.age","bot_id":"00000000-0000-0000-0000-00000000000a","ciphertext_bytes":13,"ciphertext_sha256":"c3","plaintext_bytes":11,"plaintext_sha256":"p3"},"#,
        r#"{"kind":"mcp","object":"mcp.tar.age","bot_id":null,"ciphertext_bytes":14,"ciphertext_sha256":"c4","plaintext_bytes":12,"plaintext_sha256":"p4"}],"#,
        r#""tables":[{"table":"bots","rows":5,"sha256":"t0"}],"#,
        r#""bots":["#,
        r#"{"id":"00000000-0000-0000-0000-00000000000a","name":"Alpha","status":"active","vm":"included","#,
        r#""computer":{"object":"vm-00000000-0000-0000-0000-00000000000a.msb.age","runtime_name":"else-a","runtime_version":"0.4.0","image":"else-guest:2","cpus":2,"memory_mib":2048,"desired_state":"auto","network":"public","desktop_port":6080,"desktop_password":"secret","was_running":true,"browser_checkpoint":"ok","project_mount":{"guest":"/home/else/project","uid":1000},"carries_image":true},"#,
        r#""provider_state":"omitted-reauth-required","provider_sessions":[{"provider":"codex","native_session_id":"n1","status":"active"}],"project":"partial"},"#,
        r#"{"id":"00000000-0000-0000-0000-00000000000b","name":"Beta","status":"deleted","vm":"omitted-deleted-bot","computer":null,"provider_state":"omitted-reauth-required","provider_sessions":[],"project":"omitted-deleted-bot"},"#,
        r#"{"id":"00000000-0000-0000-0000-00000000000c","name":"Gamma","status":"active","vm":"missing","computer":null,"provider_state":"omitted-reauth-required","provider_sessions":[{"provider":"claude","native_session_id":"n2","status":"retired"}],"project":"none"},"#,
        r#"{"id":"00000000-0000-0000-0000-00000000000d","name":"Delta","status":"active","vm":"none","computer":null,"provider_state":"omitted-reauth-required","provider_sessions":[],"project":"included"},"#,
        r#"{"id":"00000000-0000-0000-0000-00000000000e","name":"Epsilon","status":"active","vm":"omitted","computer":null,"provider_state":"omitted-reauth-required","provider_sessions":[],"project":"omitted"}],"#,
        r#""coverage":{"database":"included","vm":"partial","projects":"partial","mcp":"included","provider_state":"omitted-reauth-required","whole_assistant":false,"note":"Partial."}}"#,
    );

    #[test]
    fn a_manifest_keeps_its_bytes_through_a_round_trip() {
        let manifest: Manifest = serde_json::from_str(GOLDEN_MANIFEST).unwrap();
        assert_eq!(serde_json::to_string(&manifest).unwrap(), GOLDEN_MANIFEST);
    }

    #[test]
    fn manifests_from_schemas_2_and_3_still_parse() {
        let old = concat!(
            r#"{"schema_version":2,"set_id":"20251121T000000000Z-0a1b2c3d","#,
            r#""created_at":"2025-11-21T00:00:02.000Z","capture_started_at":"2025-11-21T00:00:00.000Z","#,
            r#""capture_finished_at":"2025-11-21T00:00:02.000Z","#,
            r#""encryption":{"format":"age-encryption.org/v1","key_id":"sk1-0011223344556677","kdf":"HKDF-SHA-256","info":"else.bot/backups/v1","salt_hex":"00"},"#,
            r#""application":{"version":"0.0.9","migration_version":20},"#,
            r#""sqlite":{"server_version":"3.45.0","dump_version":"3.45.0","format":"sqlite3","extensions":[]},"#,
            r#""artifacts":[{"kind":"sqlite","object":"database.dump.age","bot_id":null,"ciphertext_bytes":10,"ciphertext_sha256":"c0","plaintext_bytes":8,"plaintext_sha256":"p0"}],"#,
            r#""tables":[],"#,
            r#""bots":[{"id":"00000000-0000-0000-0000-00000000000a","name":"Alpha","status":"active","vm":"included","#,
            r#""computer":{"object":"vm-00000000-0000-0000-0000-00000000000a.msb.age","runtime_name":"else-a","runtime_version":"0.3.0","image":"else-guest:1","cpus":1,"memory_mib":1024,"desired_state":"stopped","network":"public","desktop_port":null,"desktop_password":null,"was_running":false,"browser_checkpoint":null},"#,
            r#""provider_state":"omitted-reauth-required","provider_sessions":[]}],"#,
            r#""coverage":{"database":"included","vm":"omitted","provider_state":"omitted-reauth-required","whole_assistant":false,"note":"Database only."}}"#,
        );
        for schema in [2, 3] {
            let text = old.replace(
                r#""schema_version":2"#,
                &format!(r#""schema_version":{schema}"#),
            );
            let manifest: Manifest = serde_json::from_str(&text).unwrap();
            let json = serde_json::to_string(&manifest).unwrap();
            for default in [
                r#""project_mount":null,"carries_image":true}"#,
                r#""provider_sessions":[],"project":"omitted"}"#,
                r#""projects":"omitted","mcp":"omitted""#,
            ] {
                assert!(
                    json.contains(default),
                    "schema {schema} defaults {default}: {json}"
                );
            }
        }
    }
}
