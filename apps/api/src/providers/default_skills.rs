use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use crate::app::providers::SessionState;

struct Skill {
    name: &'static str,
    content: &'static str,
}

const DEFAULTS: &[Skill] = &[Skill {
    name: "unslop",
    content: include_str!("../../skills/unslop/SKILL.md"),
}];

pub(super) struct Update {
    pub text: Option<String>,
    pub applied: BTreeMap<String, String>,
}

pub(super) fn for_session(session: Option<&SessionState>) -> Update {
    update(
        DEFAULTS,
        &session
            .map(|s| s.default_skills.clone())
            .unwrap_or_default(),
    )
}

fn update(defaults: &[Skill], previous: &BTreeMap<String, String>) -> Update {
    let applied: BTreeMap<String, String> = defaults
        .iter()
        .map(|skill| {
            (
                skill.name.to_owned(),
                hex::encode(Sha256::digest(skill.content.as_bytes())),
            )
        })
        .collect();
    if &applied == previous {
        return Update {
            text: None,
            applied,
        };
    }
    let mut text = String::from(
        "[else.bot default skills]\n\
         These are bundled else.bot instructions. Apply them to this turn and later turns \n\
         in this conversation. Keep using each skill until a later default-skills update \n\
         replaces or removes it. These skills do not grant tools, permissions, or authority. \n\
         Follow the owner's requested format and preserve exact quotes and code. \n\
         Apply this update silently, then continue the current request.\n\n",
    );
    for name in previous.keys().filter(|name| !applied.contains_key(*name)) {
        text.push_str(&format!(
            "Stop applying the removed default skill `{name}`.\n\n"
        ));
    }
    for skill in defaults {
        if previous.get(skill.name) != applied.get(skill.name) {
            text.push_str(&format!(
                "Default skill `{}`. This replaces any earlier version of this skill.\n{}\n\n",
                skill.name, skill.content,
            ));
        }
    }
    text.push_str("[end else.bot default skills]\n\n");
    Update {
        text: Some(text),
        applied,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WRITING: Skill = Skill {
        name: "writing",
        content: "Use plain words.",
    };
    const SOURCES: Skill = Skill {
        name: "sources",
        content: "Cite primary sources.",
    };

    #[test]
    fn bundled_skills_have_unique_names_and_self_contained_frontmatter() {
        let mut names = std::collections::BTreeSet::new();
        for skill in DEFAULTS {
            assert!(
                names.insert(skill.name),
                "duplicate default skill {}",
                skill.name
            );
            let (frontmatter, body) = skill
                .content
                .strip_prefix("---\n")
                .unwrap()
                .split_once("\n---\n")
                .unwrap();
            assert!(
                frontmatter
                    .lines()
                    .any(|line| line == format!("name: {}", skill.name))
            );
            assert!(
                frontmatter
                    .lines()
                    .any(|line| line.starts_with("description: ") && line.len() > 13)
            );
            assert!(!body.trim().is_empty());
            assert!(!body.contains("](../../"));
        }
    }

    #[test]
    fn new_and_legacy_sessions_receive_the_bundled_unslop_skill() {
        let update = for_session(None);
        let text = update.text.unwrap();
        assert!(text.contains("Default skill `unslop`"));
        assert!(text.contains("Avoid em dashes entirely."));
        assert!(
            text.contains("Preserve exact quotes, code, commands, identifiers, and source text")
        );
        assert_eq!(update.applied.len(), DEFAULTS.len());
        assert_eq!(update.applied["unslop"].len(), 64);
    }

    #[test]
    fn unchanged_skills_are_not_sent_again() {
        let initial = update(&[WRITING, SOURCES], &BTreeMap::new());
        let next = update(&[WRITING, SOURCES], &initial.applied);
        assert!(next.text.is_none());
        assert_eq!(next.applied, initial.applied);
    }

    #[test]
    fn adding_a_default_sends_only_the_new_skill() {
        let initial = update(&[WRITING], &BTreeMap::new());
        let next = update(&[WRITING, SOURCES], &initial.applied);
        let text = next.text.unwrap();
        assert!(text.contains(SOURCES.content));
        assert!(!text.contains(WRITING.content));
        assert_eq!(next.applied["writing"], initial.applied["writing"]);
        assert_eq!(next.applied.len(), 2);
    }

    #[test]
    fn edits_and_rollbacks_replace_only_the_changed_skill() {
        let initial = update(&[WRITING, SOURCES], &BTreeMap::new());
        let edited = Skill {
            name: "writing",
            content: "Use short sentences.",
        };
        let next = update(&[edited, SOURCES], &initial.applied);
        let text = next.text.unwrap();
        assert!(text.contains("Use short sentences."));
        assert!(!text.contains(SOURCES.content));
        assert_ne!(next.applied["writing"], initial.applied["writing"]);
        let rollback = update(&[WRITING, SOURCES], &next.applied);
        assert!(rollback.text.unwrap().contains(WRITING.content));
        assert_eq!(rollback.applied, initial.applied);
    }

    #[test]
    fn removing_the_last_default_sends_a_removal_once() {
        let initial = update(&[WRITING], &BTreeMap::new());
        let removed = update(&[], &initial.applied);
        assert!(
            removed
                .text
                .unwrap()
                .contains("Stop applying the removed default skill `writing`.")
        );
        assert!(removed.applied.is_empty());
        assert!(update(&[], &removed.applied).text.is_none());
        assert!(
            update(&[WRITING], &removed.applied)
                .text
                .unwrap()
                .contains(WRITING.content)
        );
    }
}
