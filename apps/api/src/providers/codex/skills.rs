use std::path::Path;

use serde_json::{Value, json};

use super::{AppServer, RPC_TIMEOUT};

/// CODEX_HOME does not isolate skills: Codex also scans HOME and repository ancestors.
pub(super) async fn config(server: &AppServer) -> Result<Value, String> {
    let project = Path::new(&server.cwd)
        .canonicalize()
        .map_err(|err| format!("Cannot resolve the Codex project directory: {err}"))?;
    let catalog = server
        .request(
            "skills/list",
            json!({ "cwds": [&server.cwd], "forceReload": true }),
            RPC_TIMEOUT,
        )
        .await
        .map_err(|err| err.to_string())?;
    let entries = catalog["data"]
        .as_array()
        .ok_or("Codex returned an invalid skills catalog.")?;
    let mut disabled = Vec::new();
    for entry in entries {
        let skills = entry["skills"]
            .as_array()
            .ok_or("Codex returned an invalid skills catalog.")?;
        for skill in skills {
            let path = skill["path"]
                .as_str()
                .ok_or("Codex returned a skill without a path.")?;
            if skill["scope"] != "repo" || !Path::new(path).starts_with(&project) {
                disabled.push(json!({ "path": path, "enabled": false }));
            }
        }
    }
    Ok(json!({ "skills.config": disabled }))
}

#[cfg(test)]
mod tests;
