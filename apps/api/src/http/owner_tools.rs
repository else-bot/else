use serde_json::{Value, json};

use super::AppState;
use super::mcp::{Outcome, tool_error, tool_result};
use crate::grants::Grant;

pub(super) fn secret_schema() -> Value {
    json!({
        "name": "request_secret",
        "description": "Ask the owner for credentials a task needs (an API key, a password, \
            a private key) when computer_secrets does not list them. The owner sees a private \
            form in the conversation for exactly these variables and types the values there; \
            they are stored as the task secret `name`, never shown to you, and never belong \
            in chat. Returns status pending at once. Then tell the owner briefly what you \
            need and why, and end this attempt; you continue automatically once they answer. \
            Never ask the owner to paste credentials into the chat. At most one per attempt.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "name": { "type": "string", "pattern": "^[a-z][a-z0-9_-]{0,39}$",
                          "description": "The task secret to store, e.g. smtp or github." },
                "reason": { "type": "string", "minLength": 1,
                            "maxLength": crate::db::secret_requests::MAX_REASON,
                            "description": "Shown to the owner: what it is for." },
                "env": { "type": "array", "minItems": 1, "maxItems": 16, "uniqueItems": true,
                         "items": { "type": "string", "pattern": "^[A-Z_][A-Z0-9_]{0,63}$" },
                         "description": "Environment variable names, e.g. SMTP_PASSWORD." },
            },
            "required": ["name", "reason", "env"],
            "additionalProperties": false,
        },
        "annotations": { "readOnlyHint": false, "destructiveHint": false,
                         "idempotentHint": true, "openWorldHint": false },
    })
}

pub(super) fn choice_schema() -> Value {
    json!({
        "name": "ask_owner",
        "description": "Ask the owner a question in a native card. Offer 2 to 5 options \
            when a pick settles it, or omit choices for an open question answered in chat. The owner \
            sees a card with the question, the optional context (for example a draft), and \
            the choices, and picks one in the app. Set multiple to true to let them select \
            several actions and confirm with Done. Returns status pending at once: tell the \
            owner briefly what you are asking, end this attempt, and you continue \
            automatically with their choice. A new owner message instead skips the card. \
            At most one per attempt.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "question": { "type": "string", "minLength": 1,
                              "maxLength": crate::app::questions::MAX_TITLE,
                              "description": "One short question." },
                "choices": { "type": "array", "minItems": 0,
                             "maxItems": crate::app::questions::MAX_CHOICES, "uniqueItems": true,
                             "items": { "type": "string", "minLength": 1,
                                        "maxLength": crate::app::questions::MAX_CHOICE_LABEL },
                             "description": "2 to 5 short labels, in the order shown. Omit or leave empty for a chat answer." },
                "multiple": { "type": "boolean", "default": false,
                              "description": "Allow multiple selections, confirmed with Done. Requires choices. This never approves destructive or external actions." },
                "context": { "type": "string", "maxLength": crate::app::questions::MAX_CONTEXT,
                             "description": "Optional: what the choice is about, such as a draft." },
            },
            "required": ["question"],
            "additionalProperties": false,
        },
        "annotations": { "readOnlyHint": false, "destructiveHint": false,
                         "idempotentHint": true, "openWorldHint": false },
    })
}

pub(super) fn permission_schema() -> Value {
    json!({
        "name": "ask_permission",
        "description": "Ask the owner to approve one destructive or external action you will \
            take with your HOST shell or file tools (the owner's machine), which the API \
            cannot gate itself. Computer tools do not need this: declare their effect \
            argument instead. destructive: deletes, overwrites, or changes something hard to \
            undo, here or on a remote service (deleting a remote git branch, repository, file, \
            or record is destructive, not external). external: sends a message or email, posts \
            or edits anything others can see, pays, or otherwise reaches people or services \
            outside. Returns approved \
            at once when a standing permission for destructive actions applies (the owner's \
            Always allow for this bot, or for this routine in its scheduled runs); \
            otherwise status pending: tell the owner briefly what and why, end this attempt, \
            and act only after the approval arrives in the continuation, on exactly that \
            action, once. External actions are approved one at a time, always. At most one \
            per attempt.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "kind": { "type": "string", "enum": ["destructive", "external"] },
                "action": { "type": "string", "minLength": 1,
                            "maxLength": crate::app::questions::MAX_CONTEXT,
                            "description": "Exactly what will be done: the command, or the message and its recipients." },
                "reason": { "type": "string", "maxLength": crate::app::questions::MAX_TITLE,
                            "description": "Shown as the card's title: what it is for." },
            },
            "required": ["kind", "action"],
            "additionalProperties": false,
        },
        "annotations": { "readOnlyHint": false, "destructiveHint": false,
                         "idempotentHint": true, "openWorldHint": false },
    })
}

pub(super) async fn request_secret(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    use crate::db::secret_requests::{self, RequestError, Spec, Status};
    let spec = match Spec::parse(args) {
        Ok(spec) => spec,
        Err(message) => return (tool_error(&format!("INVALID_REQUEST: {message}")), None),
    };
    match secret_requests::create(&state.db, grant.turn_id, &spec).await {
        Ok(request) if request.status != Status::Pending => {
            let stored = request.status == Status::Stored;
            let log = format!("already {:?} {}", request.status, request.id).to_lowercase();
            let result = json!({
                "status": request.status,
                "request_id": request.id,
                "secret": request.name,
                "env": request.env,
                "next": if stored {
                    "The owner stored this secret; name it in the secrets argument of computer_run or computer_job_start."
                } else {
                    "The owner declined this request; continue without it and do not ask again unless they do."
                },
            });
            (tool_result(result), Some(log))
        }
        Ok(request) => {
            state.scheduler.grants().park(grant.turn_id).await;
            let log = format!("pending {} for {}", request.id, request.name);
            let result = json!({
                "status": "pending",
                "parked": true,
                "request_id": request.id,
                "secret": request.name,
                "env": request.env,
                "next": "Tell the owner briefly what you need and end this attempt now. Do not \
                         wait, poll, or call more tools; the answer arrives in a continuation.",
            });
            (tool_result(result), Some(log))
        }
        Err(err) => {
            let code = match err {
                RequestError::NotAllowed => "NOT_ALLOWED",
                RequestError::AttemptParked => "ATTEMPT_PARKED",
                RequestError::BudgetExhausted => "BUDGET_EXHAUSTED",
                RequestError::Conflict => "CONFLICT",
                RequestError::NotRunning => "NOT_RUNNING",
                RequestError::Db => "UNAVAILABLE",
            };
            let log = format!("rejected {code}");
            (tool_error(&format!("{code}: {err}")), Some(log))
        }
    }
}

pub(super) fn question_refused(err: crate::db::owner_questions::RequestError) -> Outcome {
    use crate::db::owner_questions::RequestError;
    let code = match err {
        RequestError::NotAllowed => "NOT_ALLOWED",
        RequestError::AttemptParked => "ATTEMPT_PARKED",
        RequestError::BudgetExhausted => "BUDGET_EXHAUSTED",
        RequestError::Conflict => "CONFLICT",
        RequestError::NotRunning => "NOT_RUNNING",
        RequestError::Db => "UNAVAILABLE",
    };
    (
        tool_error(&format!("{code}: {err}")),
        Some(format!("rejected {code}")),
    )
}

pub(super) async fn ask_owner(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    use crate::app::questions::{Spec, Status};
    use crate::db::owner_questions;
    let spec = match Spec::choice(args) {
        Ok(spec) => spec,
        Err(message) => return (tool_error(&format!("INVALID_QUESTION: {message}")), None),
    };
    match owner_questions::create(&state.db, grant.turn_id, &spec).await {
        Ok(question) if question.status != Status::Pending => {
            let log = format!("already {} {}", question.status.as_str(), question.id);
            let result = json!({
                "status": question.status,
                "question_id": question.id,
                "answer": question.chosen(),
                "answers": question.selected(),
                "next": match question.status {
                    Status::Answered => "The owner already chose; continue with that choice.",
                    _ => "The owner did not answer this; a newer message of theirs governs. Do not treat it as a choice.",
                },
            });
            (tool_result(result), Some(log))
        }
        Ok(question) => {
            state.scheduler.grants().park(grant.turn_id).await;
            let log = format!("pending {} {:?}", question.id, question.title);
            let result = json!({
                "status": "pending",
                "parked": true,
                "question_id": question.id,
                "choices": question.choices,
                "multiple": question.multiple,
                "next": "Tell the owner briefly what you are asking and end this attempt now. \
                         Do not wait, poll, or call more tools; their choice arrives in a \
                         continuation.",
            });
            (tool_result(result), Some(log))
        }
        Err(err) => question_refused(err),
    }
}

pub(super) async fn ask_permission(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    use crate::app::questions::{Spec, Standing};
    use crate::db::owner_questions::{self, Gate};
    let spec = match Spec::permission(args) {
        Ok(spec) => spec,
        Err(message) => return (tool_error(&format!("INVALID_REQUEST: {message}")), None),
    };
    match owner_questions::gate(&state.db, grant.turn_id, &spec).await {
        Ok(Gate::Run { standing }) => (
            tool_result(
                json!({ "status": "approved", "standing": standing.is_some(),
                "scope": standing, "next": match standing {
                    Some(Standing::Routine) => "Standing permission for destructive actions in \
                        this routine's runs applies. Do exactly this host action now, then \
                        report what you did.",
                    _ => "Do exactly this host action now, once, then report what you did.",
                } }),
            ),
            Some(format!("approved host action: {:?}", spec.context)),
        ),
        Ok(Gate::Pending(question)) => {
            pending_approval(
                state,
                grant,
                &question,
                "Tell the owner briefly what this will do and why, then end this attempt now. \
             Do not act, wait, poll, or call more tools. The decision arrives in a continuation: \
             approved means exactly this action, once; otherwise do not do it.",
            )
            .await
        }
        Ok(Gate::Denied(question)) => (
            tool_result(json!({ "status": "denied", "question_id": question.id,
                "next": "The owner denied this action. Do not do it or an equivalent." })),
            Some(format!("denied {} by the owner", question.id)),
        ),
        Err(err) => question_refused(err),
    }
}

pub(super) async fn pending_approval(
    state: &AppState,
    grant: &Grant,
    question: &crate::db::owner_questions::QuestionInfo,
    next: &str,
) -> Outcome {
    state.scheduler.grants().park(grant.turn_id).await;
    (
        tool_result(
            json!({ "status": "pending", "parked": true, "question_id": question.id,
        "kind": question.kind, "next": next }),
        ),
        Some(format!("awaiting approval {}", question.id)),
    )
}
