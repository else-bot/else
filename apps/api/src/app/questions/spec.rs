use serde_json::Value;

use super::{
    APPROVE, APPROVE_FOREVER, APPROVE_ROUTINE, APPROVE_TASK, APPROVE_TOOL, Action, ApprovalKind,
    BotDeletion, Choice, Covers, DENY, EmailConnection, EmailRemoval, Kind, MAX_CHOICE_LABEL,
    MAX_CHOICES, MAX_CONTEXT, MAX_TITLE, MIN_CHOICES, MailAttachments, McpConnection,
    PermissionTarget, Standing, TaskScope, hash,
};

/// The options of an approval card, including the standing scope offered by the gate.
fn approval_choices(kind: ApprovalKind, offer: Option<Standing>) -> Vec<Choice> {
    let mut choices = vec![
        Choice {
            id: APPROVE.into(),
            label: "Approve once".into(),
        },
        Choice {
            id: DENY.into(),
            label: "Deny".into(),
        },
    ];
    match (kind, offer) {
        (ApprovalKind::Destructive, Some(Standing::Bot)) => choices.push(Choice {
            id: APPROVE_FOREVER.into(),
            label: "Always allow for this bot".into(),
        }),
        (_, Some(Standing::Routine)) => choices.push(Choice {
            id: APPROVE_ROUTINE.into(),
            label: "Always allow for this routine".into(),
        }),
        (_, Some(Standing::Tool)) => choices.push(Choice {
            id: APPROVE_TOOL.into(),
            label: "Always allow this tool".into(),
        }),
        (_, Some(Standing::Task)) => choices.push(Choice {
            id: APPROVE_TASK.into(),
            label: "Always allow".into(),
        }),
        (ApprovalKind::External, Some(Standing::Bot)) | (_, None) => {}
    }
    choices
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    pub action: Action,
    pub hash: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub title: String,
    pub context: Option<String>,
    pub ask: Ask,
}

/// What the bot asks: a question with options, or an action to approve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    /// No choices means the owner answers through the chat composer.
    Choice {
        choices: Vec<Choice>,
        multiple: bool,
    },
    /// `offer` is the standing grant of an owner request; the gate replaces
    /// it with the one the asking turn may receive.
    Approval {
        kind: ApprovalKind,
        offer: Option<Standing>,
        approval: Approval,
    },
}

fn text_arg<'a>(args: &'a Value, key: &str, max: usize) -> Result<Option<&'a str>, ()> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            let text = text.trim();
            if text.is_empty() || text.len() > max || text.contains('\0') {
                Err(())
            } else {
                Ok(Some(text))
            }
        }
        Some(_) => Err(()),
    }
}

/// `text` cut to at most `max` bytes on a character boundary.
fn clip(text: &str, max: usize) -> String {
    text[..text.floor_char_boundary(max)].to_owned()
}

/// The owner's exact words in this request that ask for the host action,
/// quoted by the bot in `ask_permission`. The gate checks it against the
/// request's own owner message; it never grants standing permission.
pub fn permission_authorization(args: &Value) -> Result<Option<&str>, &'static str> {
    text_arg(args, "authorization", MAX_CONTEXT)
        .map_err(|_| "authorization is 1 to 4000 bytes: the owner's exact words")
}

impl Spec {
    pub fn kind(&self) -> Kind {
        match &self.ask {
            Ask::Choice { .. } => Kind::Choice,
            Ask::Approval { kind, .. } => (*kind).into(),
        }
    }

    pub fn choices(&self) -> Vec<Choice> {
        match &self.ask {
            Ask::Choice { choices, .. } => choices.clone(),
            Ask::Approval { kind, offer, .. } => approval_choices(*kind, *offer),
        }
    }

    pub fn multiple(&self) -> bool {
        matches!(self.ask, Ask::Choice { multiple: true, .. })
    }

    pub fn approval(&self) -> Option<&Approval> {
        match &self.ask {
            Ask::Choice { .. } => None,
            Ask::Approval { approval, .. } => Some(approval),
        }
    }

    fn approving(
        kind: ApprovalKind,
        offer: Option<Standing>,
        title: String,
        context: String,
        action: Action,
        hash: [u8; 32],
    ) -> Self {
        Self {
            title,
            context: Some(context),
            ask: Ask::Approval {
                kind,
                offer,
                approval: Approval { action, hash },
            },
        }
    }

    pub fn mcp_connection(connection: McpConnection, title: String, context: String) -> Self {
        let encoded = serde_json::to_vec(&connection).expect("MCP consent serialization");
        Self::approving(
            ApprovalKind::External,
            None,
            title,
            context,
            Action::McpConnection {
                mcp_connection: connection,
            },
            hash(&[b"mcp_connection", &encoded]),
        )
    }

    pub fn email_connection(connection: EmailConnection, title: String, context: String) -> Self {
        let encoded = serde_json::to_vec(&connection).expect("email consent serialization");
        Self::approving(
            ApprovalKind::External,
            None,
            title,
            context,
            Action::EmailConnection {
                email_connection: connection,
            },
            hash(&[b"email_connection", &encoded]),
        )
    }

    /// A card approving one deletion of `deletion.bot_id`. It offers no
    /// standing grant, and its identity is the target alone, so a retry in
    /// the same request finds it even after the listed work changed.
    pub fn bot_deletion(deletion: BotDeletion, context: String) -> Self {
        let hash = hash(&[b"bot_deletion", deletion.bot_id.as_bytes()]);
        Self::approving(
            ApprovalKind::Destructive,
            None,
            format!("Delete {}", deletion.name),
            context,
            Action::BotDeletion {
                bot_deletion: deletion,
            },
            hash,
        )
    }

    /// A card approving one removal of an inbox. Like a bot deletion it
    /// offers no standing grant and is identified by its target alone.
    pub fn email_removal(removal: EmailRemoval, context: String) -> Self {
        let hash = hash(&[b"email_removal", removal.account_id.as_bytes()]);
        Self::approving(
            ApprovalKind::Destructive,
            None,
            format!("Remove {}", removal.address),
            context,
            Action::EmailRemoval {
                email_removal: removal,
            },
            hash,
        )
    }

    /// A card approving one read of these attachments. Its identity is the
    /// message and the set of parts, so a retry with the parts in another
    /// order finds the same card.
    pub fn mail_attachments(attachments: MailAttachments) -> Self {
        let mut parts: Vec<_> = attachments.files.iter().map(|f| f.part).collect();
        parts.sort_unstable();
        let message = &attachments.message;
        let identity = format!(
            "{}:{}:{}:{}:{parts:?}",
            attachments.account_id, message.uid_validity, message.uid, message.folder
        );
        let title = match attachments.files.as_slice() {
            [file] => format!("Open {}", file.name),
            files => format!("Open {} attachments", files.len()),
        };
        let mut context = format!(
            "The bot reads and searches the text of these files. Nothing else in {} changes.",
            attachments.address
        );
        if let Some(subject) = &attachments.subject {
            context.push_str(&format!("\nEmail: {subject}"));
        }
        if !attachments.from.is_empty() {
            context.push_str(&format!("\nFrom: {}", attachments.from.join(", ")));
        }
        for file in &attachments.files {
            context.push_str(&format!(
                "\n- {} ({}, {} bytes)",
                file.name, file.media_type, file.size
            ));
        }
        Self::approving(
            ApprovalKind::External,
            None,
            clip(&title, MAX_TITLE),
            clip(&context, MAX_CONTEXT),
            Action::MailAttachments {
                mail_attachments: attachments,
            },
            hash(&[b"mail_attachments", identity.as_bytes()]),
        )
    }

    /// A card saving `target` as a standing grant, or `None` for a target
    /// that [`PermissionTarget::canonical`] refuses: one of kind `choice`,
    /// or a task target whose scope is not normalized.
    pub fn standing_permission(target: PermissionTarget, authorization: &str) -> Option<Self> {
        const DOMAIN: &[u8] = b"standing_permission";
        let kind = ApprovalKind::try_from(target.kind()).ok()?;
        let scoped =
            |target: &PermissionTarget| Some(hash(&[DOMAIN, target.bot_scope()?.as_bytes()]));
        let (title, hash) = match &target {
            PermissionTarget::Bot => (
                "Always allow destructive actions for this bot".to_owned(),
                hash(&[DOMAIN, b"bot"]),
            ),
            PermissionTarget::Routine {
                routine_id, kind, ..
            } => (
                format!("Allow {} actions for this routine", kind.as_str()),
                match kind {
                    Kind::Destructive => hash(&[DOMAIN, b"routine", routine_id.as_bytes()]),
                    _ => hash(&[
                        DOMAIN,
                        b"routine",
                        routine_id.as_bytes(),
                        kind.as_str().as_bytes(),
                    ]),
                },
            ),
            PermissionTarget::McpTool { mcp_id, tool, kind } => (
                format!(
                    "Always allow {} actions with {}/{tool}",
                    kind.as_str(),
                    String::from(mcp_id.clone()),
                ),
                scoped(&target)?,
            ),
            PermissionTarget::ComputerTool { tool, kind } => (
                format!("Always allow {} actions with {tool}", kind.as_str()),
                scoped(&target)?,
            ),
            PermissionTarget::Task {
                task,
                target: on,
                kind,
                covers,
            } => {
                let calls = match covers {
                    Covers::Host => " host",
                    Covers::Computer => " computer",
                    Covers::Both => "",
                };
                (
                    format!(
                        "Always allow {}{calls} actions: {task} on {on}",
                        kind.as_str()
                    ),
                    scoped(&target)?,
                )
            }
        };
        let offer = Some(target.standing());
        Some(Self::approving(
            kind,
            offer,
            title,
            authorization.into(),
            Action::Permission { permission: target },
            hash,
        ))
    }

    /// Omit choices for a question answered through the chat composer.
    pub fn choice(args: &Value) -> Result<Self, &'static str> {
        if !crate::app::args::only(args, &["question", "choices", "context", "multiple"]) {
            return Err("use question, choices, and optionally context and multiple");
        }
        let multiple = match args.get("multiple") {
            None => false,
            Some(Value::Bool(multiple)) => *multiple,
            _ => return Err("multiple must be true or false"),
        };
        let title = text_arg(args, "question", MAX_TITLE)
            .ok()
            .flatten()
            .ok_or("question is 1 to 500 bytes of text")?;
        let context = text_arg(args, "context", MAX_CONTEXT)
            .map_err(|_| "context is at most 4000 bytes of text")?;
        let list = match args.get("choices") {
            None => &[][..],
            Some(Value::Array(list))
                if list.is_empty() || (MIN_CHOICES..=MAX_CHOICES).contains(&list.len()) =>
            {
                list.as_slice()
            }
            _ => return Err("choices lists 2 to 5 short labels, or omit it for a chat answer"),
        };
        if multiple && list.is_empty() {
            return Err("multiple requires 2 to 5 choices");
        }
        let mut choices = Vec::new();
        for (index, label) in list.iter().enumerate() {
            let label = label
                .as_str()
                .map(str::trim)
                .filter(|l| !l.is_empty() && l.len() <= MAX_CHOICE_LABEL && !l.contains('\0'))
                .ok_or("each choice is 1 to 120 bytes of text")?;
            if choices.iter().any(|c: &Choice| c.label == label) {
                return Err("choices must be distinct");
            }
            choices.push(Choice {
                id: ((b'a' + index as u8) as char).to_string(),
                label: label.to_owned(),
            });
        }
        Ok(Self {
            title: title.to_owned(),
            context: context.map(str::to_owned),
            ask: Ask::Choice { choices, multiple },
        })
    }

    /// Parse `ask_permission` arguments: the kind, the exact action the bot
    /// will take on the host, why, and optionally the recurring task and
    /// target it belongs to, which a card may offer to always allow. The
    /// optional authorization quote is read by [`permission_authorization`]
    /// and is not part of the action's identity, so a denial still matches.
    pub fn permission(args: &Value) -> Result<Self, &'static str> {
        if !crate::app::args::only(
            args,
            &["kind", "action", "reason", "task_scope", "authorization"],
        ) {
            return Err("use kind, action, and optionally reason, task_scope, and authorization");
        }
        let kind = args["kind"]
            .as_str()
            .and_then(ApprovalKind::parse)
            .ok_or("kind is destructive or external")?;
        let action = text_arg(args, "action", MAX_CONTEXT)
            .ok()
            .flatten()
            .ok_or("action is 1 to 4000 bytes: exactly what will be done")?;
        let reason = text_arg(args, "reason", MAX_TITLE)
            .map_err(|_| "reason is at most 500 bytes of text")?;
        let task_scope = TaskScope::from_args(args)?;
        let action_hash = match &task_scope {
            None => hash(&[b"host", kind.as_str().as_bytes(), action.as_bytes()]),
            Some(scope) => hash(&[
                b"host",
                kind.as_str().as_bytes(),
                action.as_bytes(),
                b"task",
                scope.task.as_bytes(),
                scope.target.as_bytes(),
            ]),
        };
        let offer = task_scope.as_ref().map(|_| Standing::Task);
        let title = reason.map(str::to_owned).unwrap_or_else(|| {
            match kind {
                ApprovalKind::External => "Allow this action that reaches outside your machine?",
                ApprovalKind::Destructive => "Allow this action on your machine?",
            }
            .to_owned()
        });
        Ok(Self::approving(
            kind,
            offer,
            title,
            action.to_owned(),
            Action::Host {
                host: action.to_owned(),
                task_scope,
            },
            action_hash,
        ))
    }

    /// An approval bound to one computer tool call: the tool and its exact
    /// arguments. `title` and `context` are what the card shows. A call
    /// declared with a task scope offers that task's standing grant instead
    /// of the bot-wide destructive one.
    pub fn tool_approval(
        kind: ApprovalKind,
        tool: &str,
        args: &Value,
        task_scope: Option<TaskScope>,
        title: String,
        context: String,
    ) -> Self {
        let offer = if task_scope.is_some() {
            Standing::Task
        } else {
            Standing::Bot
        };
        let mut canonical = args.clone();
        canonical.sort_all_objects();
        let canonical = canonical.to_string();
        Self::approving(
            kind,
            Some(offer),
            title,
            context,
            Action::Tool {
                tool: tool.to_owned(),
                args: crate::http::tool_defs::loggable_args(tool, args),
                task_scope,
            },
            hash(&[b"tool", tool.as_bytes(), canonical.as_bytes()]),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn a_choice_question_parses_only_its_fields_and_letters_its_options() {
        let spec = Spec::choice(&json!({
            "question": " Which newsletter template? ",
            "choices": ["Plain text", "HTML digest", "Both"],
            "context": "Draft: Hello everyone...",
        }))
        .unwrap();
        assert_eq!(spec.kind(), Kind::Choice);
        assert_eq!(spec.title, "Which newsletter template?");
        let ids: Vec<String> = spec.choices().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert_eq!(spec.approval(), None);
        assert!(!spec.multiple());
        let multiple = Spec::choice(&json!({
            "question": "Which tasks?", "choices": ["one", "two", "three", "four", "five"],
            "multiple": true,
        }))
        .unwrap();
        assert!(multiple.multiple());
        assert_eq!(multiple.choices().last().unwrap().id, "e");
        for bad in [
            json!({ "question": "q", "choices": ["only one"] }),
            json!({ "question": "q", "choices": ["a", "a"] }),
            json!({ "question": "q", "choices": ["a", "b", "c", "d", "e", "f"] }),
            json!({ "question": "q", "multiple": true }),
            json!({ "question": "q", "choices": ["a", "b"], "multiple": "yes" }),
            json!({ "question": " ", "choices": ["a", "b"] }),
            json!({ "question": "q", "choices": ["a", "b"], "value": "x" }),
            json!({ "question": "q", "choices": [1, 2] }),
            json!(["q"]),
        ] {
            assert!(Spec::choice(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_permission_binds_the_described_action_and_offers_no_blanket_grant() {
        let delete = Spec::permission(&json!({
            "kind": "destructive", "action": "rm -rf ~/old-site", "reason": "Remove the old site",
        }))
        .unwrap();
        assert_eq!(delete.kind(), Kind::Destructive);
        assert_eq!(delete.title, "Remove the old site");
        assert_eq!(delete.context.as_deref(), Some("rm -rf ~/old-site"));
        let ids: Vec<String> = delete.choices().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, [APPROVE, DENY]);

        let send =
            Spec::permission(&json!({ "kind": "external", "action": "mail -s hi bob@x" })).unwrap();
        let ids: Vec<String> = send.choices().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, [APPROVE, DENY]);
        assert_ne!(
            send.approval().map(|a| a.hash),
            delete.approval().map(|a| a.hash)
        );
        assert!(Spec::permission(&json!({ "kind": "local", "action": "ls" })).is_err());
        assert!(Spec::permission(&json!({ "kind": "external" })).is_err());
    }

    fn tailscale(task: &str, target: &str) -> Spec {
        Spec::permission(&json!({
            "kind": "destructive", "action": "ssh omarchy-m1 sudo tailscale up --ssh",
            "task_scope": { "task": task, "target": target },
        }))
        .unwrap()
    }

    #[test]
    fn a_task_scoped_permission_offers_always_allow_and_binds_its_scope() {
        let spec = tailscale("  Manage   Tailscale via SSH ", "omarchy-m1");
        let ids: Vec<String> = spec.choices().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, [APPROVE, DENY, APPROVE_TASK]);
        assert_eq!(spec.choices()[2].label, "Always allow");
        let approval = spec.approval().unwrap();
        let scope = TaskScope {
            task: "Manage Tailscale via SSH".into(),
            target: "omarchy-m1".into(),
        };
        assert_eq!(
            approval.action,
            Action::Host {
                host: "ssh omarchy-m1 sudo tailscale up --ssh".into(),
                task_scope: Some(scope),
            }
        );
        let target = PermissionTarget::for_action(spec.kind(), &approval.action).unwrap();
        assert_eq!(target.standing(), Standing::Task);
        let serialized = serde_json::to_value(&target).unwrap();
        assert_eq!(
            serialized,
            json!({ "scope": "task", "task": "Manage Tailscale via SSH",
                    "target": "omarchy-m1", "kind": "destructive", "covers": "host" })
        );
        assert_eq!(
            serde_json::from_value::<PermissionTarget>(serialized).unwrap(),
            target
        );

        let unscoped = Spec::permission(&json!({
            "kind": "destructive", "action": "ssh omarchy-m1 sudo tailscale up --ssh",
        }))
        .unwrap();
        let other_target = tailscale("Manage Tailscale via SSH", "omarchy-m2");
        let hashes = [
            unscoped.approval().unwrap().hash,
            approval.hash,
            other_target.approval().unwrap().hash,
        ];
        assert_ne!(
            hashes[0], hashes[1],
            "the scope is part of the bound action"
        );
        assert_ne!(hashes[1], hashes[2]);
        assert_eq!(
            serde_json::to_value(&unscoped.approval().unwrap().action).unwrap(),
            json!({ "host": "ssh omarchy-m1 sudo tailscale up --ssh" }),
            "unscoped host actions keep their stored shape"
        );
    }

    /// The columns a card stores, so pending cards and their retries keep
    /// matching across upgrades.
    fn stored(spec: &Spec) -> Value {
        json!({
            "kind": spec.kind().as_str(),
            "title": spec.title,
            "context": spec.context,
            "choices": spec.choices(),
            "multiple": spec.multiple(),
            "action": spec.approval().map(|a| &a.action),
            "hash": spec.approval().map(|a| hex::encode(a.hash)),
        })
    }

    #[test]
    fn every_card_keeps_its_stored_row() {
        let routine = Uuid::from_u128(7);
        let target = |value: Value| serde_json::from_value::<PermissionTarget>(value).unwrap();
        let specs = [
            Spec::mcp_connection(
                serde_json::from_value(json!({"id": "github", "provider": "github", "scopes": ["repo"]}))
                    .unwrap(),
                "Connect GitHub".into(),
                "Reads repos".into(),
            ),
            Spec::email_connection(
                serde_json::from_value(json!({"provider": "gmail", "address": "a@b.c", "replaces": 3}))
                    .unwrap(),
                "Connect a@b.c".into(),
                "Reads mail".into(),
            ),
            Spec::bot_deletion(
                serde_json::from_value(json!({"bot_id": Uuid::from_u128(1), "name": "Scout",
                    "avatar": {"shape": "star", "color": "red"}, "model": null, "category": null}))
                .unwrap(),
                "Stops 2 routines".into(),
            ),
            Spec::email_removal(
                serde_json::from_value(json!({"account_id": Uuid::from_u128(2), "address": "a@b.c"}))
                    .unwrap(),
                "Stops triggers".into(),
            ),
            Spec::mail_attachments(
                serde_json::from_value(json!({"account_id": Uuid::from_u128(2), "address": "a@b.c",
                    "message": {"folder": "INBOX", "uid_validity": 9, "uid": 4},
                    "subject": "Invoice", "from": ["x@y.z"],
                    "files": [{"part": 3, "name": "b.pdf", "media_type": "application/pdf", "size": 10},
                              {"part": 1, "name": "a.txt", "media_type": "text/plain", "size": 5}]}))
                .unwrap(),
            ),
            Spec::standing_permission(PermissionTarget::Bot, "always allow").unwrap(),
            Spec::standing_permission(
                target(json!({"scope": "routine", "routine_id": routine, "prompt_revision": 2})),
                "allow it",
            ).unwrap(),
            Spec::standing_permission(
                target(json!({"scope": "routine", "routine_id": routine, "prompt_revision": 2,
                    "kind": "external"})),
                "allow it",
            ).unwrap(),
            Spec::standing_permission(
                target(json!({"scope": "mcp_tool", "mcp_id": "github", "tool": "create_issue",
                    "kind": "external"})),
                "allow it",
            ).unwrap(),
            Spec::standing_permission(
                target(json!({"scope": "computer_tool", "tool": "computer_run", "kind": "external"})),
                "allow it",
            ).unwrap(),
            Spec::standing_permission(
                target(json!({"scope": "task", "task": "Deploy", "target": "web", "kind": "destructive",
                    "covers": "host"})),
                "allow it",
            ).unwrap(),
            Spec::standing_permission(
                target(json!({"scope": "task", "task": "Deploy", "target": "web", "kind": "external",
                    "covers": "both"})),
                "allow it",
            ).unwrap(),
            Spec::permission(&json!({"kind": "external", "action": "curl -X POST x"})).unwrap(),
            Spec::permission(&json!({"kind": "destructive", "action": "rm a", "reason": "Clean up",
                "task_scope": {"task": "Clean", "target": "/tmp"}}))
            .unwrap(),
            Spec::tool_approval(
                ApprovalKind::Destructive,
                "computer_run",
                &json!({"command": "rm a", "effect": "destructive"}),
                None,
                "Run rm".into(),
                "rm a".into(),
            ),
            Spec::tool_approval(
                ApprovalKind::External,
                "computer_run",
                &json!({"command": "./send", "effect": "external"}),
                TaskScope::new("Send", "list"),
                "Run send".into(),
                "./send".into(),
            ),
            Spec::choice(&json!({"question": "Which?", "choices": ["x", "y"], "multiple": true}))
                .unwrap(),
        ];
        let expected: Vec<Value> = STORED_ROWS
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(specs.iter().map(stored).collect::<Vec<_>>(), expected);
    }

    const STORED_ROWS: &str = r#"{"kind":"external","title":"Connect GitHub","context":"Reads repos","choices":[{"id":"approve","label":"Approve once"},{"id":"deny","label":"Deny"}],"multiple":false,"action":{"mcp_connection":{"id":"github","provider":"github","scopes":["repo"]}},"hash":"bc0db640bb7ab0532d06ec44671a7e7bf5d6c167b2910474b0df68b13b6a83d0"}
{"kind":"external","title":"Connect a@b.c","context":"Reads mail","choices":[{"id":"approve","label":"Approve once"},{"id":"deny","label":"Deny"}],"multiple":false,"action":{"email_connection":{"provider":"gmail","address":"a@b.c","replaces":3}},"hash":"c43c6b03fd0806f4e80e376a699eb73c147465a0fb9a9972d81f70b4e8e0189e"}
{"kind":"destructive","title":"Delete Scout","context":"Stops 2 routines","choices":[{"id":"approve","label":"Approve once"},{"id":"deny","label":"Deny"}],"multiple":false,"action":{"bot_deletion":{"bot_id":"00000000-0000-0000-0000-000000000001","name":"Scout","avatar":{"shape":"star","color":"red"},"model":null,"category":null}},"hash":"9d48149230629ac9fb3f72143cef81d9e508810248fc93d3223e68ecc9458499"}
{"kind":"destructive","title":"Remove a@b.c","context":"Stops triggers","choices":[{"id":"approve","label":"Approve once"},{"id":"deny","label":"Deny"}],"multiple":false,"action":{"email_removal":{"account_id":"00000000-0000-0000-0000-000000000002","address":"a@b.c"}},"hash":"6eed2abb4a6680e14ae6143f6920f2e29a2dbac50aa6cadabbca7c8cc9c9985f"}
{"kind":"external","title":"Open 2 attachments","context":"The bot reads and searches the text of these files. Nothing else in a@b.c changes.\nEmail: Invoice\nFrom: x@y.z\n- b.pdf (application/pdf, 10 bytes)\n- a.txt (text/plain, 5 bytes)","choices":[{"id":"approve","label":"Approve once"},{"id":"deny","label":"Deny"}],"multiple":false,"action":{"mail_attachments":{"account_id":"00000000-0000-0000-0000-000000000002","address":"a@b.c","message":{"folder":"INBOX","uid_validity":9,"uid":4},"subject":"Invoice","from":["x@y.z"],"files":[{"part":3,"name":"b.pdf","media_type":"application/pdf","size":10},{"part":1,"name":"a.txt","media_type":"text/plain","size":5}]}},"hash":"294ce1fe5c57995fef186f31e8906cb7c35546623b677a22cf38de36ea62b56c"}
{"kind":"destructive","title":"Always allow destructive actions for this bot","context":"always allow","choices":[{"id":"approve","label":"Approve once"},{"id":"deny","label":"Deny"},{"id":"approve_forever","label":"Always allow for this bot"}],"multiple":false,"action":{"permission":{"scope":"bot"}},"hash":"96f00f24602555c2960d9ce7bfd6631284016f7e71eafb4b1152b52e323c310b"}
{"kind":"destructive","title":"Allow destructive actions for this routine","context":"allow it","choices":[{"id":"approve","label":"Approve once"},{"id":"deny","label":"Deny"},{"id":"approve_routine","label":"Always allow for this routine"}],"multiple":false,"action":{"permission":{"scope":"routine","routine_id":"00000000-0000-0000-0000-000000000007","prompt_revision":2,"kind":"destructive"}},"hash":"96742b5daf10cee886d1725c8bc3ca63522b5126bd4fe9bc8c20774d19867323"}
{"kind":"external","title":"Allow external actions for this routine","context":"allow it","choices":[{"id":"approve","label":"Approve once"},{"id":"deny","label":"Deny"},{"id":"approve_routine","label":"Always allow for this routine"}],"multiple":false,"action":{"permission":{"scope":"routine","routine_id":"00000000-0000-0000-0000-000000000007","prompt_revision":2,"kind":"external"}},"hash":"6e21e61eda03132e64acd5dc8dc0daa7d3b105fe72dfef7b0b62890f7a5dc42d"}
{"kind":"external","title":"Always allow external actions with github/create_issue","context":"allow it","choices":[{"id":"approve","label":"Approve once"},{"id":"deny","label":"Deny"},{"id":"approve_tool","label":"Always allow this tool"}],"multiple":false,"action":{"permission":{"scope":"mcp_tool","mcp_id":"github","tool":"create_issue","kind":"external"}},"hash":"0090c9125814f2719824db382c2b71e85947e5b0cd0d7a685c250fd5db661edd"}
{"kind":"external","title":"Always allow external actions with computer_run","context":"allow it","choices":[{"id":"approve","label":"Approve once"},{"id":"deny","label":"Deny"},{"id":"approve_tool","label":"Always allow this tool"}],"multiple":false,"action":{"permission":{"scope":"computer_tool","tool":"computer_run","kind":"external"}},"hash":"f7853895683b008dcf71bec1ae911979741742893374ef4db2d03d1c9f8268db"}
{"kind":"destructive","title":"Always allow destructive host actions: Deploy on web","context":"allow it","choices":[{"id":"approve","label":"Approve once"},{"id":"deny","label":"Deny"},{"id":"approve_task","label":"Always allow"}],"multiple":false,"action":{"permission":{"scope":"task","task":"Deploy","target":"web","kind":"destructive","covers":"host"}},"hash":"768100d3a2831aa311993036c16ef40714cb3a0d0ba711aecdbb410df937c7b2"}
{"kind":"external","title":"Always allow external actions: Deploy on web","context":"allow it","choices":[{"id":"approve","label":"Approve once"},{"id":"deny","label":"Deny"},{"id":"approve_task","label":"Always allow"}],"multiple":false,"action":{"permission":{"scope":"task","task":"Deploy","target":"web","kind":"external","covers":"both"}},"hash":"d5630360d03f60b2c6af701e98a67c4b1f2f135efb3433b57f663db31077623f"}
{"kind":"external","title":"Allow this action that reaches outside your machine?","context":"curl -X POST x","choices":[{"id":"approve","label":"Approve once"},{"id":"deny","label":"Deny"}],"multiple":false,"action":{"host":"curl -X POST x"},"hash":"98d17570b68e6c87a6a76a8e2c0200d10cc487c00e3ef55dad70d69e0def3c6e"}
{"kind":"destructive","title":"Clean up","context":"rm a","choices":[{"id":"approve","label":"Approve once"},{"id":"deny","label":"Deny"},{"id":"approve_task","label":"Always allow"}],"multiple":false,"action":{"host":"rm a","task_scope":{"task":"Clean","target":"/tmp"}},"hash":"1cea4a109501402ef7146a703ee05d7d10a6345d18c6519e4de19c9913cae5cc"}
{"kind":"destructive","title":"Run rm","context":"rm a","choices":[{"id":"approve","label":"Approve once"},{"id":"deny","label":"Deny"},{"id":"approve_forever","label":"Always allow for this bot"}],"multiple":false,"action":{"tool":"computer_run","args":{"command":"rm a","effect":"destructive"}},"hash":"0c9da9dfc6569accc0b96242c2c8dae45fcdf8d791e46ec3e09bb507fbd40b92"}
{"kind":"external","title":"Run send","context":"./send","choices":[{"id":"approve","label":"Approve once"},{"id":"deny","label":"Deny"},{"id":"approve_task","label":"Always allow"}],"multiple":false,"action":{"tool":"computer_run","args":{"command":"./send","effect":"external"},"task_scope":{"task":"Send","target":"list"}},"hash":"529720a3ed624055c8c0a2f722a23c62ce14f86f646813a4a3ea34baa23182c6"}
{"kind":"choice","title":"Which?","context":null,"choices":[{"id":"a","label":"x"},{"id":"b","label":"y"}],"multiple":true,"action":null,"hash":null}"#;

    #[test]
    fn a_tool_approval_hashes_the_exact_arguments_regardless_of_key_order() {
        let a = Spec::tool_approval(
            ApprovalKind::External,
            "computer_run",
            &json!({ "command": "./send.sh", "effect": "external" }),
            None,
            "t".into(),
            "c".into(),
        );
        let b = Spec::tool_approval(
            ApprovalKind::External,
            "computer_run",
            &json!({ "effect": "external", "command": "./send.sh" }),
            None,
            "t".into(),
            "c".into(),
        );
        let c = Spec::tool_approval(
            ApprovalKind::External,
            "computer_run",
            &json!({ "command": "./send.sh --all", "effect": "external" }),
            None,
            "t".into(),
            "c".into(),
        );
        assert_eq!(a.approval().map(|a| a.hash), b.approval().map(|a| a.hash));
        assert_ne!(a.approval().map(|a| a.hash), c.approval().map(|a| a.hash));
        assert_ne!(
            a.approval().map(|a| a.hash),
            Spec::tool_approval(
                ApprovalKind::External,
                "computer_job_start",
                &json!({ "command": "./send.sh", "effect": "external" }),
                None,
                "t".into(),
                "c".into()
            )
            .approval()
            .map(|a| a.hash)
        );
    }
}
