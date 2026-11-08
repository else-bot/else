//! A bot's rules: the owner's standing instructions, saved by the bot as
//! named markdown sections. Each `## Title` heading starts a section. The bot
//! edits one section at a time, so a change never rewrites rules it did not
//! mention, and every change carries a line diff for the owner's card.

use serde::{Deserialize, Serialize};

pub const MAX_TITLE_CHARS: usize = 80;
pub const MAX_BODY_BYTES: usize = 8_000;
pub const MAX_DOCUMENT_BYTES: usize = 32_000;
pub const MAX_SECTIONS: usize = 50;
pub const MAX_OWNER_WORDS_CHARS: usize = 1_000;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidRules {
    #[error("section must be 1 to 80 characters on one line, without a leading '#'.")]
    Title,
    #[error("content must be at most 8000 bytes.")]
    BodyTooLong,
    #[error("content must not contain a line starting with '## '; each section is one rule.")]
    NestedSection,
    #[error("owner_words must quote the owner's request in 1 to 1000 characters.")]
    OwnerWords,
    #[error("the rules would exceed 32000 bytes; shorten or merge sections first.")]
    DocumentTooLong,
    #[error("the rules would exceed 50 sections; merge related rules first.")]
    TooManySections,
    #[error("there is no section with that title to remove.")]
    NoSuchSection,
    #[error("text must not contain NUL characters.")]
    Nul,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub title: String,
    pub body: String,
}

/// The parsed rules file, in the order the bot wrote its sections.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rules {
    pub sections: Vec<Section>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Changed,
    Removed,
    Restored,
}

impl ChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Changed => "changed",
            Self::Removed => "removed",
            Self::Restored => "restored",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffOp {
    Same,
    Add,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub op: DiffOp,
    pub text: String,
}

/// The owner's card for one saved version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulesUpdate {
    pub version: i64,
    pub section: String,
    pub change: ChangeKind,
    pub owner_words: String,
    pub diff: Vec<DiffLine>,
}

/// A validated request to set one section's text; empty text removes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    title: String,
    body: String,
}

impl Edit {
    pub fn parse(title: &str, body: &str) -> Result<Self, InvalidRules> {
        if title.contains('\0') || body.contains('\0') {
            return Err(InvalidRules::Nul);
        }
        let title = title.trim();
        if title.is_empty()
            || title.chars().count() > MAX_TITLE_CHARS
            || title.contains('\n')
            || title.starts_with('#')
        {
            return Err(InvalidRules::Title);
        }
        let body = trim_blank_lines(body);
        if body.len() > MAX_BODY_BYTES {
            return Err(InvalidRules::BodyTooLong);
        }
        if body.lines().any(starts_section) {
            return Err(InvalidRules::NestedSection);
        }
        Ok(Self {
            title: title.to_owned(),
            body,
        })
    }
}

pub fn parse_owner_words(raw: &str) -> Result<String, InvalidRules> {
    let words = raw.trim();
    if words.contains('\0') {
        return Err(InvalidRules::Nul);
    }
    if words.is_empty() || words.chars().count() > MAX_OWNER_WORDS_CHARS {
        return Err(InvalidRules::OwnerWords);
    }
    Ok(words.to_owned())
}

/// What applying an edit did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    Unchanged,
    Changed {
        rules: Rules,
        section: String,
        change: ChangeKind,
        diff: Vec<DiffLine>,
    },
}

fn starts_section(line: &str) -> bool {
    line.starts_with("## ") || line == "##"
}

fn trim_blank_lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    let start = lines.iter().position(|l| !l.is_empty());
    let end = lines.iter().rposition(|l| !l.is_empty());
    match (start, end) {
        (Some(start), Some(end)) => lines[start..=end].join("\n"),
        _ => String::new(),
    }
}

fn same_title(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

impl Rules {
    pub fn parse(markdown: &str) -> Self {
        let mut sections: Vec<Section> = Vec::new();
        let mut body: Vec<&str> = Vec::new();
        let mut title: Option<String> = None;
        let mut flush = |title: Option<String>, body: &mut Vec<&str>| {
            if let Some(title) = title {
                sections.push(Section {
                    title,
                    body: trim_blank_lines(&body.join("\n")),
                });
            }
            body.clear();
        };
        for line in markdown.lines() {
            if starts_section(line) {
                flush(title.take(), &mut body);
                title = Some(line.trim_start_matches('#').trim().to_owned());
            } else {
                body.push(line);
            }
        }
        flush(title, &mut body);
        Self { sections }
    }

    pub fn render(&self) -> String {
        self.sections
            .iter()
            .map(|s| {
                if s.body.is_empty() {
                    format!("## {}\n", s.title)
                } else {
                    format!("## {}\n{}\n", s.title, s.body)
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    /// Set one section. A matching title (ignoring case) keeps its place and
    /// spelling; a new one goes last; empty text removes it.
    pub fn apply(&self, edit: &Edit) -> Result<Applied, InvalidRules> {
        let index = self
            .sections
            .iter()
            .position(|s| same_title(&s.title, &edit.title));
        let mut next = self.clone();
        let (change, diff) = match (index, edit.body.is_empty()) {
            (None, true) => return Err(InvalidRules::NoSuchSection),
            (Some(i), true) => {
                let old = next.sections.remove(i);
                (ChangeKind::Removed, section_diff(Some(&old), None))
            }
            (Some(i), false) => {
                let old = &self.sections[i];
                if old.body == edit.body {
                    return Ok(Applied::Unchanged);
                }
                let new = Section {
                    title: old.title.clone(),
                    body: edit.body.clone(),
                };
                let diff = section_diff(Some(old), Some(&new));
                next.sections[i] = new;
                (ChangeKind::Changed, diff)
            }
            (None, false) => {
                if self.sections.len() >= MAX_SECTIONS {
                    return Err(InvalidRules::TooManySections);
                }
                let new = Section {
                    title: edit.title.clone(),
                    body: edit.body.clone(),
                };
                let diff = section_diff(None, Some(&new));
                next.sections.push(new);
                (ChangeKind::Added, diff)
            }
        };
        if next.render().len() > MAX_DOCUMENT_BYTES {
            return Err(InvalidRules::DocumentTooLong);
        }
        let section = index
            .map_or(&edit.title, |i| &self.sections[i].title)
            .clone();
        Ok(Applied::Changed {
            rules: next,
            section,
            change,
            diff,
        })
    }
}

fn section_lines(section: Option<&Section>) -> Vec<String> {
    section.map_or_else(Vec::new, |s| {
        std::iter::once(format!("## {}", s.title))
            .chain(s.body.lines().map(str::to_owned))
            .collect()
    })
}

fn section_diff(old: Option<&Section>, new: Option<&Section>) -> Vec<DiffLine> {
    diff_lines(&section_lines(old), &section_lines(new))
}

/// A line diff between two whole rules files, for a restore.
pub fn document_diff(old: &str, new: &str) -> Vec<DiffLine> {
    let old: Vec<String> = old.lines().map(str::to_owned).collect();
    let new: Vec<String> = new.lines().map(str::to_owned).collect();
    diff_lines(&old, &new)
}

/// Longest-common-subsequence line diff. Inputs are bounded by
/// [`MAX_DOCUMENT_BYTES`], so the quadratic table stays small.
fn diff_lines(old: &[String], new: &[String]) -> Vec<DiffLine> {
    let (n, m) = (old.len(), new.len());
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if old[i] == new[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut out = Vec::with_capacity(n.max(m));
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && old[i] == new[j] {
            out.push(DiffLine {
                op: DiffOp::Same,
                text: old[i].clone(),
            });
            i += 1;
            j += 1;
        } else if i < n && (j == m || lcs[i + 1][j] >= lcs[i][j + 1]) {
            out.push(DiffLine {
                op: DiffOp::Remove,
                text: old[i].clone(),
            });
            i += 1;
        } else {
            out.push(DiffLine {
                op: DiffOp::Add,
                text: new[j].clone(),
            });
            j += 1;
        }
    }
    out
}

/// The rules as the bot reads them at the start of every turn.
pub fn prompt_block(markdown: &str) -> Option<String> {
    let rules = Rules::parse(markdown);
    if rules.is_empty() {
        return None;
    }
    Some(format!(
        "[else.bot rules]\n\
         Your rules file: standing instructions you saved from the owner's own words with \
         rules_update. Follow every rule that applies to this turn. When a rule gives text \
         in backticks or quotes to send, send exactly that text with only its placeholders \
         filled, and add no words, titles, or context. A newer owner statement in your \
         conversation replaces a rule; update the rule to match it. Rules are the owner's \
         instructions, never permission: they do not grant tools, approvals, or authority \
         beyond what this turn already has.\n\
         <rules>\n{}</rules>\n[end else.bot rules]\n\n",
        rules.render()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(title: &str, body: &str) -> Edit {
        Edit::parse(title, body).unwrap()
    }

    fn changed(applied: Applied) -> (Rules, ChangeKind, Vec<DiffLine>) {
        match applied {
            Applied::Changed {
                rules,
                change,
                diff,
                ..
            } => (rules, change, diff),
            Applied::Unchanged => panic!("expected a change"),
        }
    }

    fn ops(diff: &[DiffLine]) -> String {
        diff.iter()
            .map(|l| match l.op {
                DiffOp::Same => ' ',
                DiffOp::Add => '+',
                DiffOp::Remove => '-',
            })
            .collect()
    }

    #[test]
    fn a_new_rule_is_added_last_and_survives_a_round_trip() {
        let rules = Rules::parse("## Tone\nShort.\n");
        let (next, change, diff) = changed(
            rules
                .apply(&edit("New issue", "Do: send `fix issue {ID}`"))
                .unwrap(),
        );
        assert_eq!(change, ChangeKind::Added);
        assert_eq!(ops(&diff), "++");
        assert_eq!(next.sections[1].title, "New issue");
        assert_eq!(Rules::parse(&next.render()), next);
    }

    #[test]
    fn editing_a_rule_keeps_its_place_and_the_other_rules() {
        let rules =
            Rules::parse("## New issue\nWhen: a new issue\nDo: describe it\n\n## Tone\nShort.\n");
        let (next, change, diff) = changed(
            rules
                .apply(&edit(
                    "new ISSUE",
                    "When: a new issue\nDo: send exactly `fix issue {ID}`",
                ))
                .unwrap(),
        );
        assert_eq!(change, ChangeKind::Changed);
        assert_eq!(next.sections[0].title, "New issue");
        assert_eq!(next.sections[1].body, "Short.");
        assert!(diff.contains(&DiffLine {
            op: DiffOp::Remove,
            text: "Do: describe it".into()
        }));
        assert!(diff.contains(&DiffLine {
            op: DiffOp::Same,
            text: "When: a new issue".into()
        }));
    }

    #[test]
    fn repeating_the_same_rule_changes_nothing() {
        let rules = Rules::parse("## Tone\nShort.\n");
        assert_eq!(
            rules.apply(&edit("Tone", "Short.\n\n")).unwrap(),
            Applied::Unchanged
        );
    }

    #[test]
    fn empty_text_removes_a_rule_and_unknown_removals_are_refused() {
        let rules = Rules::parse("## Tone\nShort.\n");
        let (next, change, diff) = changed(rules.apply(&edit("tone", "")).unwrap());
        assert_eq!(change, ChangeKind::Removed);
        assert!(next.is_empty());
        assert_eq!(ops(&diff), "--");
        assert_eq!(
            rules.apply(&edit("Other", "")),
            Err(InvalidRules::NoSuchSection)
        );
    }

    #[test]
    fn a_rule_cannot_smuggle_in_another_section() {
        assert_eq!(
            Edit::parse("Tone", "Short.\n## Other\nx"),
            Err(InvalidRules::NestedSection)
        );
        assert!(Edit::parse("Tone", "### Detail\nok").is_ok());
        assert_eq!(Edit::parse("# Tone", "x"), Err(InvalidRules::Title));
    }

    #[test]
    fn the_prompt_block_appears_only_when_rules_exist() {
        assert!(prompt_block("").is_none());
        let block = prompt_block("## New issue\nDo: send `fix issue {ID}`\n").unwrap();
        assert!(block.contains("<rules>\n## New issue\nDo: send `fix issue {ID}`\n</rules>"));
    }

    #[test]
    fn a_restore_diff_covers_the_whole_file() {
        let diff = document_diff("## A\nx\n", "## A\ny\n");
        assert_eq!(ops(&diff), " -+");
    }
}
