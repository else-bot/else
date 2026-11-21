use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{Action, Kind, Standing, computer_tool, hash, wire};

/// Bytes of a task scope's task or target, after normalization.
pub const MAX_TASK_SCOPE: usize = 200;
/// The broad destructive grant, separate from permissions for individual tools.
pub const DESTRUCTIVE_SCOPE: &str = "destructive";

/// The recurring work a host action or computer tool call belongs to, as
/// the bot declared it:
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

    /// The optional `task_scope` argument of a gated call, normalized.
    pub fn from_args(args: &Value) -> Result<Option<Self>, &'static str> {
        const SCOPE_ERROR: &str =
            "task_scope is {task, target}: each 1 to 200 bytes of text without control characters";
        match args.get("task_scope") {
            None | Some(Value::Null) => Ok(None),
            Some(value) => {
                let scope =
                    serde_json::from_value::<Self>(value.clone()).map_err(|_| SCOPE_ERROR)?;
                Self::new(&scope.task, &scope.target)
                    .map(Some)
                    .ok_or(SCOPE_ERROR)
            }
        }
    }

    fn canonical(&self) -> bool {
        Self::new(&self.task, &self.target).as_ref() == Some(self)
    }

    fn key(&self, covers: Covers, kind: Kind) -> String {
        format!(
            "task:{}:{}:{}:{}",
            covers.as_str(),
            kind.as_str(),
            hex::encode(self.task.to_lowercase()),
            hex::encode(&self.target),
        )
    }
}

/// Which gated calls a task grant covers. MCP tools/call is never one of
/// them: a grant for MCP work is an `mcp_tool` grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::IntoStaticStr)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Covers {
    /// Host actions approved through `ask_permission`.
    Host,
    /// Computer tool calls.
    Computer,
    /// A grant saved through `confirm_permission` before grants named what
    /// they cover. A bot can no longer save one.
    Both,
}

impl Covers {
    fn as_str(self) -> &'static str {
        self.into()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "lowercase", deny_unknown_fields)]
pub enum PermissionTarget {
    Bot,
    Routine {
        routine_id: Uuid,
        prompt_revision: i64,
        /// Grants saved before routine grants named their effect allow
        /// destructive actions.
        #[serde(default = "destructive")]
        kind: Kind,
    },
    #[serde(rename = "mcp_tool")]
    McpTool {
        mcp_id: crate::mcp::PackageId,
        tool: String,
        kind: Kind,
    },
    /// One computer tool's external calls without a declared task scope.
    #[serde(rename = "computer_tool")]
    ComputerTool {
        tool: String,
        kind: Kind,
    },
    Task {
        task: String,
        target: String,
        kind: Kind,
        covers: Covers,
    },
}

fn destructive() -> Kind {
    Kind::Destructive
}

impl PermissionTarget {
    pub fn standing(&self) -> Standing {
        match self {
            Self::Bot => Standing::Bot,
            Self::Routine { .. } => Standing::Routine,
            Self::McpTool { .. } | Self::ComputerTool { .. } => Standing::Tool,
            Self::Task { .. } => Standing::Task,
        }
    }

    pub fn kind(&self) -> Kind {
        match self {
            Self::Bot => Kind::Destructive,
            Self::Routine { kind, .. }
            | Self::McpTool { kind, .. }
            | Self::ComputerTool { kind, .. }
            | Self::Task { kind, .. } => *kind,
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
            Self::ComputerTool { tool, kind } => *kind == Kind::External && computer_tool(tool),
            Self::Task { kind, .. } => *kind != Kind::Choice && self.task_scope().is_some(),
            Self::Routine { kind, .. } => *kind != Kind::Choice,
            Self::Bot => true,
        }
    }

    /// The target with a task scope normalized, or `None` when it is invalid.
    pub fn canonical(self) -> Option<Self> {
        let target = match self {
            Self::Task {
                task,
                target,
                kind,
                covers,
            } => {
                let scope = TaskScope::new(&task, &target)?;
                Self::Task {
                    task: scope.task,
                    target: scope.target,
                    kind,
                    covers,
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
            Self::ComputerTool { tool, kind } => {
                Some(format!("computer:{}:{}", kind.as_str(), hex::encode(tool)))
            }
            Self::Task { kind, covers, .. } => Some(self.task_scope()?.key(*covers, *kind)),
        }
    }

    /// The approval hash of a `confirm_permission` question for this task
    /// target saved before task grants named what they cover.
    pub fn legacy_permission_hash(&self) -> Option<[u8; 32]> {
        let Self::Task { kind, .. } = self else {
            return None;
        };
        let scope = self.task_scope()?;
        let key = format!(
            "task:{}:{}:{}",
            kind.as_str(),
            hex::encode(scope.task.to_lowercase()),
            hex::encode(&scope.target),
        );
        Some(hash(&[b"standing_permission", key.as_bytes()]))
    }

    /// The saved scopes that would cover this target: its own, and for a
    /// task, also a grant saved before grants named what they cover.
    pub fn covering_scopes(&self) -> Vec<String> {
        let mut scopes = Vec::from_iter(self.bot_scope());
        if let Self::Task { kind, .. } = self
            && let Some(scope) = self.task_scope()
        {
            scopes.push(scope.key(Covers::Both, *kind));
        }
        scopes.dedup();
        scopes
    }

    /// The standing grant that would cover this approved action: an MCP tool
    /// call, a host action or computer tool call declared with a task scope,
    /// or a computer tool's external call without one.
    pub fn for_action(kind: Kind, action: &Action) -> Option<Self> {
        let declared = match action {
            Action::Host {
                task_scope: Some(scope),
                ..
            } => Some((scope, Covers::Host)),
            Action::Tool {
                task_scope: Some(scope),
                ..
            } => Some((scope, Covers::Computer)),
            _ => None,
        };
        if let Some((scope, covers)) = declared {
            let target = Self::Task {
                task: scope.task.clone(),
                target: scope.target.clone(),
                kind,
                covers,
            };
            return target.valid().then_some(target);
        }
        let Action::Tool { tool, args, .. } = action else {
            return None;
        };
        if kind == Kind::External && computer_tool(tool) {
            return Some(Self::ComputerTool {
                tool: tool.clone(),
                kind,
            });
        }
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

impl From<&TaskScope> for wire::TaskScope {
    fn from(scope: &TaskScope) -> Self {
        Self {
            task: scope.task.clone(),
            target: scope.target.clone(),
        }
    }
}

impl From<&PermissionTarget> for wire::QuestionPermission {
    fn from(target: &PermissionTarget) -> Self {
        match target {
            PermissionTarget::Bot => Self::Bot,
            PermissionTarget::Routine {
                routine_id,
                prompt_revision,
                kind,
            } => Self::Routine {
                routine_id: *routine_id,
                prompt_revision: *prompt_revision,
                kind: (*kind).into(),
            },
            PermissionTarget::McpTool { mcp_id, tool, kind } => Self::McpTool {
                mcp_id: mcp_id.clone().into(),
                tool: tool.clone(),
                kind: Some((*kind).into()),
            },
            PermissionTarget::ComputerTool { tool, kind } => Self::ComputerTool {
                tool: tool.clone(),
                kind: (*kind).into(),
            },
            PermissionTarget::Task {
                task,
                target,
                kind,
                covers,
            } => Self::Task {
                task_scope: wire::TaskScope {
                    task: task.clone(),
                    target: target.clone(),
                },
                kind: Some((*kind).into()),
                covers: Some(match covers {
                    Covers::Host => wire::TaskCoverage::Host,
                    Covers::Computer => wire::TaskCoverage::Computer,
                    Covers::Both => wire::TaskCoverage::Both,
                }),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::questions::Spec;
    use serde_json::json;

    #[test]
    fn task_grants_preserve_target_identity_and_match_task_names_case_insensitively() {
        let key = |task: &str, target: &str, kind: Kind| {
            PermissionTarget::Task {
                task: task.into(),
                target: target.into(),
                kind,
                covers: Covers::Host,
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
        assert!(base.starts_with("task:host:destructive:"));
        assert_ne!(base, DESTRUCTIVE_SCOPE);
        let computer = PermissionTarget::Task {
            task: "Manage Tailscale via SSH".into(),
            target: "omarchy-m1".into(),
            kind: Kind::Destructive,
            covers: Covers::Computer,
        };
        let both = base.replacen("task:host:", "task:both:", 1);
        assert_ne!(computer.bot_scope().unwrap(), base);
        assert_eq!(
            computer.covering_scopes(),
            [computer.bot_scope().unwrap(), both.clone()],
            "a grant saved before covers existed still covers computer calls"
        );
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
            .approval()
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
            covers: Covers::Host,
        };
        assert!(!raw.valid(), "only normalized targets are valid");
        assert_eq!(
            raw.canonical(),
            Some(PermissionTarget::Task {
                task: "Manage Tailscale".into(),
                target: "omarchy-m1".into(),
                kind: Kind::External,
                covers: Covers::Host,
            })
        );
        for bad in [
            PermissionTarget::Task {
                task: "".into(),
                target: "omarchy-m1".into(),
                kind: Kind::External,
                covers: Covers::Host,
            },
            PermissionTarget::Task {
                task: "Manage".into(),
                target: "omarchy-m1".into(),
                kind: Kind::Choice,
                covers: Covers::Host,
            },
        ] {
            assert_eq!(bad.canonical(), None);
        }
    }
}
