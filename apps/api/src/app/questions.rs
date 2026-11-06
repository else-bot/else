use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// One option of an owner question, by its stable id and shown label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Answer {
    Single(String),
    Multiple(Vec<String>),
}

impl Answer {
    pub fn single(&self) -> Option<&str> {
        match self {
            Self::Single(id) => Some(id),
            Self::Multiple(_) => None,
        }
    }

    pub fn contains(&self, id: &str) -> bool {
        match self {
            Self::Single(chosen) => chosen == id,
            Self::Multiple(chosen) => chosen.iter().any(|chosen| chosen == id),
        }
    }
}

/// The owner's answer to a question or approval request: which question
/// (its kind, title, and bound action) and the choices the owner made, or
/// that it ended unanswered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub id: Uuid,
    /// `choice`, `destructive`, or `external`.
    pub kind: Kind,
    pub title: String,
    /// For an approval: the tool call or described host action.
    pub action: Option<Action>,
    /// `answered`, `skipped`, or `cancelled`.
    pub status: Status,
    pub answers: Vec<Choice>,
}

/// Questions per owner request (turn).
pub const MAX_PER_TURN: i64 = 8;
pub const MAX_TITLE: usize = 500;
pub const MAX_CONTEXT: usize = 4000;
pub const MAX_CHOICES: usize = 5;
pub const MIN_CHOICES: usize = 2;
pub const MAX_CHOICE_LABEL: usize = 120;

pub const APPROVE: &str = "approve";
pub const DENY: &str = "deny";
pub const APPROVE_FOREVER: &str = "approve_forever";
pub const APPROVE_ROUTINE: &str = "approve_routine";
pub const APPROVE_TOOL: &str = "approve_tool";
pub const APPROVE_TASK: &str = "approve_task";
/// Bytes of a task scope's task or target, after normalization.
pub const MAX_TASK_SCOPE: usize = 200;
/// The broad destructive grant, separate from permissions for individual tools.
pub const DESTRUCTIVE_SCOPE: &str = "destructive";

/// Where a standing approval applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Standing {
    /// Every request of the bot except routine runs and their tasks.
    Bot,
    /// Runs of one routine admitted with its current prompt.
    Routine,
    /// One MCP tool and effect in the bot's owner requests.
    Tool,
    /// One declared task on one target, with one effect, in the bot's owner requests.
    Task,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum Kind {
    Choice,
    Destructive,
    External,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Choice => "choice",
            Self::Destructive => "destructive",
            Self::External => "external",
        }
    }

    /// The effect a bot may declare for an approval: never `choice`.
    pub fn approval(text: &str) -> Option<Self> {
        match text {
            "destructive" => Some(Self::Destructive),
            "external" => Some(Self::External),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum Status {
    Pending,
    Answered,
    Skipped,
    Cancelled,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Answered => "answered",
            Self::Skipped => "skipped",
            Self::Cancelled => "cancelled",
        }
    }
}

/// The options of an approval card, including the standing scope offered by the gate.
pub fn approval_choices(kind: Kind, offer: Option<Standing>) -> Vec<Choice> {
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
        (Kind::Destructive, Some(Standing::Bot)) => choices.push(Choice {
            id: APPROVE_FOREVER.into(),
            label: "Always allow for this bot".into(),
        }),
        (Kind::Destructive, Some(Standing::Routine)) => choices.push(Choice {
            id: APPROVE_ROUTINE.into(),
            label: "Always allow for this routine".into(),
        }),
        (Kind::Destructive | Kind::External, Some(Standing::Tool)) => choices.push(Choice {
            id: APPROVE_TOOL.into(),
            label: "Always allow this tool".into(),
        }),
        (Kind::Destructive | Kind::External, Some(Standing::Task)) => choices.push(Choice {
            id: APPROVE_TASK.into(),
            label: "Always allow".into(),
        }),
        _ => {}
    }
    choices
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Action {
    McpConnection {
        mcp_connection: McpConnection,
    },
    Tool {
        tool: String,
        args: Value,
    },
    Host {
        host: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_scope: Option<TaskScope>,
    },
    Permission {
        permission: PermissionTarget,
    },
}

/// The recurring work a host action belongs to, as the bot declared it:
/// what kind of task, on which target. A standing grant matches it exactly,
/// after normalization; it never parses the action itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskScope {
    pub task: String,
    pub target: String,
}

fn scope_text(text: &str) -> Option<String> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty() && text.len() <= MAX_TASK_SCOPE && !text.chars().any(char::is_control))
        .then_some(text)
}

impl TaskScope {
    /// The normalized scope, or `None` when either part is empty, too long,
    /// or contains control characters.
    pub fn new(task: &str, target: &str) -> Option<Self> {
        Some(Self {
            task: scope_text(task)?,
            target: {
                let target = target.trim();
                if target.is_empty()
                    || target.len() > MAX_TASK_SCOPE
                    || target.chars().any(char::is_control)
                {
                    return None;
                }
                target.to_owned()
            },
        })
    }

    fn canonical(&self) -> bool {
        Self::new(&self.task, &self.target).as_ref() == Some(self)
    }

    fn key(&self, kind: Kind) -> String {
        format!(
            "task:{}:{}:{}",
            kind.as_str(),
            hex::encode(self.task.to_lowercase()),
            hex::encode(&self.target),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConnection {
    pub id: crate::mcp::PackageId,
    pub provider: Option<String>,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "lowercase", deny_unknown_fields)]
pub enum PermissionTarget {
    Bot,
    Routine {
        routine_id: Uuid,
        prompt_revision: i64,
    },
    #[serde(rename = "mcp_tool")]
    McpTool {
        mcp_id: crate::mcp::PackageId,
        tool: String,
        kind: Kind,
    },
    Task {
        task: String,
        target: String,
        kind: Kind,
    },
}

impl PermissionTarget {
    pub fn standing(&self) -> Standing {
        match self {
            Self::Bot => Standing::Bot,
            Self::Routine { .. } => Standing::Routine,
            Self::McpTool { .. } => Standing::Tool,
            Self::Task { .. } => Standing::Task,
        }
    }

    pub fn kind(&self) -> Kind {
        match self {
            Self::McpTool { kind, .. } | Self::Task { kind, .. } => *kind,
            _ => Kind::Destructive,
        }
    }

    pub fn valid(&self) -> bool {
        match self {
            Self::McpTool { tool, kind, .. } => {
                *kind != Kind::Choice
                    && !tool.trim().is_empty()
                    && tool.len() <= 128
                    && !tool.contains('\0')
            }
            Self::Task { kind, .. } => *kind != Kind::Choice && self.task_scope().is_some(),
            _ => true,
        }
    }

    /// The target with a task scope normalized, or `None` when it is invalid.
    pub fn canonical(self) -> Option<Self> {
        let target = match self {
            Self::Task { task, target, kind } => {
                let scope = TaskScope::new(&task, &target)?;
                Self::Task {
                    task: scope.task,
                    target: scope.target,
                    kind,
                }
            }
            other => other,
        };
        target.valid().then_some(target)
    }

    fn task_scope(&self) -> Option<TaskScope> {
        let Self::Task { task, target, .. } = self else {
            return None;
        };
        let scope = TaskScope {
            task: task.clone(),
            target: target.clone(),
        };
        scope.canonical().then_some(scope)
    }

    pub fn bot_scope(&self) -> Option<String> {
        match self {
            Self::Bot => Some(DESTRUCTIVE_SCOPE.into()),
            Self::Routine { .. } => None,
            Self::McpTool { mcp_id, tool, kind } => Some(format!(
                "mcp:{}:{}:{}",
                String::from(mcp_id.clone()),
                kind.as_str(),
                hex::encode(tool),
            )),
            Self::Task { kind, .. } => Some(self.task_scope()?.key(*kind)),
        }
    }

    /// The standing grant that would cover this approved action: an MCP tool
    /// call, or a host action declared with a task scope.
    pub fn for_action(kind: Kind, action: &Action) -> Option<Self> {
        if let Action::Host {
            task_scope: Some(scope),
            ..
        } = action
        {
            let target = Self::Task {
                task: scope.task.clone(),
                target: scope.target.clone(),
                kind,
            };
            return target.valid().then_some(target);
        }
        let Action::Tool { tool, args } = action else {
            return None;
        };
        if tool != "mcp_request" || args["method"] != "tools/call" {
            return None;
        }
        let target = Self::McpTool {
            mcp_id: crate::mcp::PackageId::try_from(args["id"].as_str()?.to_owned()).ok()?,
            tool: args["tool"].as_str()?.to_owned(),
            kind,
        };
        target.valid().then_some(target)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    pub action: Action,
    pub hash: [u8; 32],
}

/// What the bot asks: a question with options, or an action to approve.
/// An approval's choices are those of an owner request; the gate replaces
/// them with the standing grant the asking turn may receive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub kind: Kind,
    pub title: String,
    pub context: Option<String>,
    pub choices: Vec<Choice>,
    pub multiple: bool,
    pub approval: Option<Approval>,
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

fn only_keys(args: &Value, allowed: &[&str]) -> Result<(), ()> {
    let object = args.as_object().ok_or(())?;
    if object.keys().all(|k| allowed.contains(&k.as_str())) {
        Ok(())
    } else {
        Err(())
    }
}

fn hash(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
        hasher.update(b"\0");
    }
    hasher.finalize().into()
}

impl Spec {
    pub fn mcp_connection(connection: McpConnection, title: String, context: String) -> Self {
        let encoded = serde_json::to_vec(&connection).expect("MCP consent serialization");
        Self {
            kind: Kind::External,
            title,
            context: Some(context),
            choices: approval_choices(Kind::External, None),
            multiple: false,
            approval: Some(Approval {
                hash: hash(&[b"mcp_connection", &encoded]),
                action: Action::McpConnection {
                    mcp_connection: connection,
                },
            }),
        }
    }

    pub fn standing_permission(target: PermissionTarget, authorization: &str) -> Self {
        let standing = target.standing();
        let kind = target.kind();
        let permission_hash = match &target {
            PermissionTarget::Bot => hash(&[b"standing_permission", b"bot"]),
            PermissionTarget::Routine { routine_id, .. } => {
                hash(&[b"standing_permission", b"routine", routine_id.as_bytes()])
            }
            PermissionTarget::McpTool { .. } | PermissionTarget::Task { .. } => hash(&[
                b"standing_permission",
                target.bot_scope().unwrap().as_bytes(),
            ]),
        };
        Self {
            kind,
            title: match standing {
                Standing::Bot => "Always allow destructive actions for this bot".into(),
                Standing::Routine => "Allow destructive actions for this routine".into(),
                Standing::Tool => match &target {
                    PermissionTarget::McpTool { mcp_id, tool, kind } => format!(
                        "Always allow {} actions with {}/{tool}",
                        kind.as_str(),
                        String::from(mcp_id.clone()),
                    ),
                    _ => unreachable!(),
                },
                Standing::Task => match &target {
                    PermissionTarget::Task { task, target, kind } => {
                        format!("Always allow {} actions: {task} on {target}", kind.as_str())
                    }
                    _ => unreachable!(),
                },
            },
            context: Some(authorization.into()),
            choices: approval_choices(kind, Some(standing)),
            multiple: false,
            approval: Some(Approval {
                action: Action::Permission { permission: target },
                hash: permission_hash,
            }),
        }
    }

    /// Omit choices for a question answered through the chat composer.
    pub fn choice(args: &Value) -> Result<Self, &'static str> {
        only_keys(args, &["question", "choices", "context", "multiple"])
            .map_err(|_| "use question, choices, and optionally context and multiple")?;
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
            kind: Kind::Choice,
            title: title.to_owned(),
            context: context.map(str::to_owned),
            choices,
            multiple,
            approval: None,
        })
    }

    /// Parse `ask_permission` arguments: the kind, the exact action the bot
    /// will take on the host, why, and optionally the recurring task and
    /// target it belongs to, which a card may offer to always allow.
    pub fn permission(args: &Value) -> Result<Self, &'static str> {
        only_keys(args, &["kind", "action", "reason", "task_scope"])
            .map_err(|_| "use kind, action, and optionally reason and task_scope")?;
        let kind = args["kind"]
            .as_str()
            .and_then(Kind::approval)
            .ok_or("kind is destructive or external")?;
        let action = text_arg(args, "action", MAX_CONTEXT)
            .ok()
            .flatten()
            .ok_or("action is 1 to 4000 bytes: exactly what will be done")?;
        let reason = text_arg(args, "reason", MAX_TITLE)
            .map_err(|_| "reason is at most 500 bytes of text")?;
        const SCOPE_ERROR: &str =
            "task_scope is {task, target}: each 1 to 200 bytes of text without control characters";
        let task_scope = match args.get("task_scope") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let scope =
                    serde_json::from_value::<TaskScope>(value.clone()).map_err(|_| SCOPE_ERROR)?;
                Some(TaskScope::new(&scope.task, &scope.target).ok_or(SCOPE_ERROR)?)
            }
        };
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
                Kind::External => "Allow this action that reaches outside your machine?",
                _ => "Allow this action on your machine?",
            }
            .to_owned()
        });
        Ok(Self {
            kind,
            title,
            context: Some(action.to_owned()),
            choices: approval_choices(kind, offer),
            multiple: false,
            approval: Some(Approval {
                action: Action::Host {
                    host: action.to_owned(),
                    task_scope,
                },
                hash: action_hash,
            }),
        })
    }

    /// An approval bound to one computer tool call: the tool and its exact
    /// arguments. `title` and `context` are what the card shows.
    pub fn tool_approval(
        kind: Kind,
        tool: &str,
        args: &Value,
        title: String,
        context: String,
    ) -> Self {
        let mut canonical = args.clone();
        canonical.sort_all_objects();
        let canonical = canonical.to_string();
        Self {
            kind,
            title,
            context: Some(context),
            choices: approval_choices(kind, Some(Standing::Bot)),
            multiple: false,
            approval: Some(Approval {
                action: Action::Tool {
                    tool: tool.to_owned(),
                    args: crate::app::tools::loggable_args(tool, args),
                },
                hash: hash(&[b"tool", tool.as_bytes(), canonical.as_bytes()]),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_choice_question_parses_only_its_fields_and_letters_its_options() {
        let spec = Spec::choice(&json!({
            "question": " Which newsletter template? ",
            "choices": ["Plain text", "HTML digest", "Both"],
            "context": "Draft: Hello everyone...",
        }))
        .unwrap();
        assert_eq!(spec.kind, Kind::Choice);
        assert_eq!(spec.title, "Which newsletter template?");
        let ids: Vec<&str> = spec.choices.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert_eq!(spec.approval, None);
        assert!(!spec.multiple);
        let multiple = Spec::choice(&json!({
            "question": "Which tasks?", "choices": ["one", "two", "three", "four", "five"],
            "multiple": true,
        }))
        .unwrap();
        assert!(multiple.multiple);
        assert_eq!(multiple.choices.last().unwrap().id, "e");
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
        assert_eq!(delete.kind, Kind::Destructive);
        assert_eq!(delete.title, "Remove the old site");
        assert_eq!(delete.context.as_deref(), Some("rm -rf ~/old-site"));
        let ids: Vec<&str> = delete.choices.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, [APPROVE, DENY]);

        let send =
            Spec::permission(&json!({ "kind": "external", "action": "mail -s hi bob@x" })).unwrap();
        let ids: Vec<&str> = send.choices.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, [APPROVE, DENY]);
        assert_ne!(
            send.approval.as_ref().map(|a| a.hash),
            delete.approval.as_ref().map(|a| a.hash)
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
        let ids: Vec<&str> = spec.choices.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, [APPROVE, DENY, APPROVE_TASK]);
        assert_eq!(spec.choices[2].label, "Always allow");
        let approval = spec.approval.as_ref().unwrap();
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
        let target = PermissionTarget::for_action(spec.kind, &approval.action).unwrap();
        assert_eq!(target.standing(), Standing::Task);
        let serialized = serde_json::to_value(&target).unwrap();
        assert_eq!(
            serialized,
            json!({ "scope": "task", "task": "Manage Tailscale via SSH",
                    "target": "omarchy-m1", "kind": "destructive" })
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
            unscoped.approval.as_ref().unwrap().hash,
            approval.hash,
            other_target.approval.as_ref().unwrap().hash,
        ];
        assert_ne!(
            hashes[0], hashes[1],
            "the scope is part of the bound action"
        );
        assert_ne!(hashes[1], hashes[2]);
        assert_eq!(
            serde_json::to_value(&unscoped.approval.unwrap().action).unwrap(),
            json!({ "host": "ssh omarchy-m1 sudo tailscale up --ssh" }),
            "unscoped host actions keep their stored shape"
        );
    }

    #[test]
    fn task_grants_preserve_target_identity_and_match_task_names_case_insensitively() {
        let key = |task: &str, target: &str, kind: Kind| {
            PermissionTarget::Task {
                task: task.into(),
                target: target.into(),
                kind,
            }
            .bot_scope()
            .unwrap()
        };
        let base = key("Manage Tailscale via SSH", "omarchy-m1", Kind::Destructive);
        assert_eq!(
            base,
            key("manage tailscale via ssh", "omarchy-m1", Kind::Destructive)
        );
        assert_ne!(
            key("Manage files", "/srv/Prod", Kind::Destructive),
            key("Manage files", "/srv/prod", Kind::Destructive)
        );
        assert_ne!(
            key("Manage files", "/srv/a  b", Kind::Destructive),
            key("Manage files", "/srv/a b", Kind::Destructive)
        );
        assert_eq!(
            TaskScope::new("Manage files", " /srv/a  b ")
                .unwrap()
                .target,
            "/srv/a  b"
        );
        assert_ne!(
            base,
            key("Manage Tailscale via SSH", "omarchy-m2", Kind::Destructive)
        );
        assert_ne!(
            base,
            key("Manage Docker via SSH", "omarchy-m1", Kind::Destructive)
        );
        assert_ne!(
            base,
            key("Manage Tailscale via SSH", "omarchy-m1", Kind::External)
        );
        assert_ne!(
            key("a:b", "c", Kind::Destructive),
            key("a", "b:c", Kind::Destructive),
            "separators inside the text cannot collide"
        );
        assert!(base.starts_with("task:destructive:"));
        assert_ne!(base, DESTRUCTIVE_SCOPE);
    }

    #[test]
    fn malformed_task_scopes_are_refused() {
        let long = "x".repeat(MAX_TASK_SCOPE + 1);
        for bad in [
            json!("Manage Tailscale"),
            json!({ "task": "Manage Tailscale" }),
            json!({ "task": "  ", "target": "omarchy-m1" }),
            json!({ "task": "Manage Tailscale", "target": "" }),
            json!({ "task": long, "target": "omarchy-m1" }),
            json!({ "task": "Manage\u{7}Tailscale", "target": "omarchy-m1" }),
            json!({ "task": "Manage\u{0}Tailscale", "target": "omarchy-m1" }),
            json!({ "task": 1, "target": "omarchy-m1" }),
            json!({ "task": "Manage Tailscale", "target": "omarchy-m1", "kind": "external" }),
        ] {
            assert!(
                Spec::permission(&json!({
                    "kind": "destructive", "action": "tailscale up", "task_scope": bad,
                }))
                .is_err(),
                "{bad}"
            );
        }
        assert!(
            Spec::permission(&json!({
                "kind": "destructive", "action": "tailscale up", "task_scope": null,
            }))
            .unwrap()
            .approval
            .is_some_and(|a| matches!(
                a.action,
                Action::Host {
                    task_scope: None,
                    ..
                }
            ))
        );
        let raw = PermissionTarget::Task {
            task: " Manage  Tailscale ".into(),
            target: "omarchy-m1".into(),
            kind: Kind::External,
        };
        assert!(!raw.valid(), "only normalized targets are valid");
        assert_eq!(
            raw.canonical(),
            Some(PermissionTarget::Task {
                task: "Manage Tailscale".into(),
                target: "omarchy-m1".into(),
                kind: Kind::External,
            })
        );
        for bad in [
            PermissionTarget::Task {
                task: "".into(),
                target: "omarchy-m1".into(),
                kind: Kind::External,
            },
            PermissionTarget::Task {
                task: "Manage".into(),
                target: "omarchy-m1".into(),
                kind: Kind::Choice,
            },
        ] {
            assert_eq!(bad.canonical(), None);
        }
    }

    #[test]
    fn a_tool_approval_hashes_the_exact_arguments_regardless_of_key_order() {
        let a = Spec::tool_approval(
            Kind::External,
            "computer_run",
            &json!({ "command": "./send.sh", "effect": "external" }),
            "t".into(),
            "c".into(),
        );
        let b = Spec::tool_approval(
            Kind::External,
            "computer_run",
            &json!({ "effect": "external", "command": "./send.sh" }),
            "t".into(),
            "c".into(),
        );
        let c = Spec::tool_approval(
            Kind::External,
            "computer_run",
            &json!({ "command": "./send.sh --all", "effect": "external" }),
            "t".into(),
            "c".into(),
        );
        assert_eq!(
            a.approval.as_ref().map(|a| a.hash),
            b.approval.as_ref().map(|a| a.hash)
        );
        assert_ne!(
            a.approval.as_ref().map(|a| a.hash),
            c.approval.as_ref().map(|a| a.hash)
        );
        assert_ne!(
            a.approval.as_ref().map(|a| a.hash),
            Spec::tool_approval(
                Kind::External,
                "computer_job_start",
                &json!({ "command": "./send.sh", "effect": "external" }),
                "t".into(),
                "c".into()
            )
            .approval
            .as_ref()
            .map(|a| a.hash)
        );
    }
}
