use serde::Serialize;
use std::collections::BTreeMap;

// Value-bearing payloads deliberately do not implement Debug or Deserialize.
#[derive(Serialize)]
pub struct SaveSecret {
    pub env: BTreeMap<String, String>,
    pub description: String,
    pub website: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
