pub struct Registration {
    pub client_id: String,
    pub client_secret: String,
}

pub fn parse(input: &str) -> Result<Registration, &'static str> {
    let json: serde_json::Value = serde_json::from_str(input)
        .map_err(|_| "GitHub OAuth configuration must be valid JSON.")?;
    let credential = |name| {
        json.get(name)
            .and_then(|value| value.as_str())
            .filter(|value| {
                !value.is_empty()
                    && value.len() <= 2048
                    && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
            })
            .map(str::to_owned)
            .ok_or("GitHub OAuth configuration requires a client_id and client_secret with at most 2048 printable ASCII characters each.")
    };
    Ok(Registration {
        client_id: credential("client_id")?,
        client_secret: credential("client_secret")?,
    })
}
