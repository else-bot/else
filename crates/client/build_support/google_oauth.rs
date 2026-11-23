pub struct Registration {
    pub client_id: String,
    pub client_secret: String,
}

pub fn parse(input: &str) -> Result<Registration, &'static str> {
    let json: serde_json::Value = serde_json::from_str(input)
        .map_err(|_| "Google OAuth configuration must be valid JSON.")?;
    let installed = json.get("installed").and_then(|v| v.as_object()).ok_or(
        "Google OAuth configuration must be a Desktop app registration with an installed section.",
    )?;
    let client_id = installed
        .get("client_id")
        .and_then(|v| v.as_str())
        .filter(|v| {
            !v.is_empty()
                && v.len() <= 1024
                && v.ends_with(".apps.googleusercontent.com")
                && v.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
        .ok_or("Google Desktop registration requires a valid client_id.")?;
    let client_secret = installed
        .get("client_secret")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty() && v.len() <= 4096 && !v.chars().any(char::is_control))
        .ok_or("Google Desktop registration requires a nonempty client_secret.")?;
    Ok(Registration {
        client_id: client_id.into(),
        client_secret: client_secret.into(),
    })
}
