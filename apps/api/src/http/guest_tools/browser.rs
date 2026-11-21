//! `computer_browser`, and the Vault values it types into pages.

use std::sync::Arc;

use serde_json::{Value, json};

use super::{fit, lease, text};
use crate::computers::ComputerManager;
use crate::db::task_secrets;
use crate::grants::Grant;
use crate::http::AppState;
use crate::http::tool_result::{Done, Refusal, ToolResult, slice};

pub(super) async fn browser(
    state: &AppState,
    grant: &Grant,
    computers: &Arc<ComputerManager>,
    args: &Value,
) -> ToolResult {
    let action = text(args, "action").unwrap_or_default();
    let mut fill: Option<VaultFill> = None;
    let mut argv = match text(args, "dialog").as_deref() {
        Some("accept") => vec!["--accept-dialogs".to_owned()],
        None | Some("dismiss") => Vec::new(),
        Some(_) => return Err(Refusal::text("dialog is dismiss or accept.")),
    };
    argv.push(action.clone());
    match action.as_str() {
        "open" => match text(args, "url") {
            Some(url) => argv.push(url),
            None => return Err(Refusal::text("open needs url.")),
        },
        "click" => match text(args, "selector") {
            Some(sel) => argv.push(sel),
            None => return Err(Refusal::text("click needs selector.")),
        },
        "type" => match (
            text(args, "selector"),
            text(args, "text"),
            text(args, "secret"),
        ) {
            (Some(sel), Some(t), None) => {
                argv.push(sel);
                argv.push(t);
            }
            (Some(sel), None, Some(name)) => {
                let filled = vault_fill(state, grant, &name, text(args, "field")).await?;
                argv.pop();
                argv.extend(["type-secret".to_owned(), sel, filled.site.clone()]);
                fill = Some(filled);
            }
            _ => {
                return Err(Refusal::text(
                    "type needs selector and either text or secret and field.",
                ));
            }
        },
        "text" | "title" | "snapshot" => {}
        _ => return Err(Refusal::text("unknown action.")),
    }
    let lease = lease(state, grant, true).await?;
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let input = fill.as_ref().map(|f| f.value.as_bytes());
    match computers.browser_with(lease, &argv, input).await {
        Ok(out) => {
            let mut parsed: Value = serde_json::from_str(out.stdout.trim())
                .unwrap_or_else(|_| json!({ "error": slice(&out.stderr, 0, 500).1 }));
            let mut redaction = match fill {
                Some(ref fill) => fill.redaction.clone(),
                None => Vec::new(),
            };
            redaction.extend(page_redaction(state, grant, &parsed).await?);
            parsed = task_secrets::redact_json(parsed, &redaction);
            let log = match &fill {
                Some(fill) => format!("browser type {} from the Vault", fill.label),
                None => format!("browser {action}"),
            };
            if parsed.get("error").is_some() {
                Err(Refusal::text(format!("BROWSER_ERROR: {}", parsed["error"])).logged(log))
            } else {
                if let Some(snapshot) = parsed["snapshot"].as_str() {
                    let refs = crate::computers::browser_refs::parse(snapshot);
                    computers.remember_browser_refs(grant.bot_id, refs).await;
                }
                Ok(Done::json(fit(parsed)).logged(log))
            }
        }
        Err(err) => Err(err.into()),
    }
}

/// One Vault value to type into a page of the entry's website.
struct VaultFill {
    value: zeroize::Zeroizing<String>,
    site: String,
    label: String,
    redaction: Vec<task_secrets::Redact>,
}

/// The Vault values a browser result might show (see
/// `task_secrets::browser_redaction`). When the Vault cannot be read, the
/// result is withheld rather than shown unredacted.
async fn page_redaction(
    state: &AppState,
    grant: &Grant,
    result: &Value,
) -> Result<Vec<task_secrets::Redact>, Refusal> {
    let host = result["url"]
        .as_str()
        .and_then(|url| reqwest::Url::parse(url).ok())
        .and_then(|url| url.host_str().map(str::to_owned));
    task_secrets::browser_redaction(
        &state.db,
        Some(&*state.secret_key),
        grant.bot_id,
        host.as_deref(),
    )
    .await
    .map_err(|err| {
        tracing::warn!(error = %err, "vault redaction unavailable");
        Refusal::text(
            "VAULT_UNAVAILABLE: the page may show a Vault value and the Vault could not be read, \
             so the result is withheld. The action itself ran.",
        )
    })
}

/// Look up the value `computer_browser` types for `secret` and `field`.
/// An entry without a website cannot be typed into a page: the guest
/// refuses every page outside the website, so a page that asks for the
/// login cannot redirect it elsewhere.
async fn vault_fill(
    state: &AppState,
    grant: &Grant,
    name: &str,
    field: Option<String>,
) -> Result<VaultFill, Refusal> {
    let refused = |text: String| Err(Refusal::text(text));
    let Some(field) = field else {
        return refused("secret needs field, the variable to type, such as PASSWORD.".into());
    };
    let (website, mut env) =
        match task_secrets::login(&state.db, Some(&*state.secret_key), name).await {
            Ok(found) => found,
            Err(task_secrets::SecretError::Unknown(_)) => {
                return refused(format!(
                    "SECRET_UNKNOWN: there is no Vault entry named {name}; search computer_secrets."
                ));
            }
            Err(err) => {
                tracing::warn!(error = %err, "vault entry unavailable");
                return refused("SECRET_UNAVAILABLE: the Vault could not be read.".into());
            }
        };
    let Some(site) = website else {
        return refused(format!(
            "SECRET_NO_WEBSITE: {name} has no website, so it is typed into no page. Ask the \
             owner to set its website in Settings > Vault."
        ));
    };
    let redaction = env.redaction();
    let Some(at) = env.vars.iter().position(|(var, _)| *var == field) else {
        let fields: Vec<&str> = env.vars.iter().map(|(var, _)| var.as_str()).collect();
        return refused(format!(
            "SECRET_FIELD: {name} has no field {field}; it has {}.",
            fields.join(", ")
        ));
    };
    let (_, value) = env.vars.swap_remove(at);
    if value.len() < task_secrets::MIN_REDACTED {
        return refused(format!(
            "SECRET_TOO_SHORT: {name}.{field} is under {} bytes, so a page could show it \
             unredacted; it is not typed. Ask the owner to sign in with computer_handoff.",
            task_secrets::MIN_REDACTED
        ));
    }
    task_secrets::record_use(
        &state.db,
        &[name.to_owned()],
        grant.bot_id,
        Some(grant.turn_id),
        task_secrets::Use::Browser,
    )
    .await;
    Ok(VaultFill {
        value,
        site: site.as_str().to_owned(),
        label: format!("{name}.{field}"),
        redaction,
    })
}
