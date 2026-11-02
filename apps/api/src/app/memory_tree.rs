//! Deterministic rules of the memory summary tree and saved view (POC-014),
//! adapted from the pinned UniiChat design (docs/research/memory.md).
//!
//! A node `start+span` covers entries `start .. start+span` (zero-based);
//! spans are powers of two and starts are aligned. A view is an ordered list
//! of nodes covering a contiguous prefix of the log. Code decides everything
//! here: labels, exact copies and joins, byte sizes, and which adjacent
//! siblings merge next. Only text that cannot be copied or joined within the
//! node limit needs a model (POC-015).

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

/// Byte budgets of one generation, recorded with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recipe {
    pub version: u32,
    /// Largest node body in UTF-8 bytes.
    pub node_bytes: usize,
    /// Main view: compaction starts above `view_high` and merges toward
    /// `view_low`, counting labels, separators, and the wrapper.
    pub view_high: usize,
    pub view_low: usize,
}

impl Default for Recipe {
    fn default() -> Self {
        Self {
            version: 1,
            node_bytes: 512,
            view_high: 128_000,
            view_low: 64_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeId {
    pub start: i64,
    pub span: i64,
}

impl NodeId {
    pub fn leaf(seq: i64) -> Self {
        Self {
            start: seq,
            span: 1,
        }
    }

    pub fn is_valid(self) -> bool {
        self.start >= 0
            && self.span > 0
            && self.span & (self.span - 1) == 0
            && self.start % self.span == 0
    }

    pub fn end(self) -> i64 {
        self.start + self.span
    }

    /// The two children of a parent node.
    pub fn children(self) -> Option<(NodeId, NodeId)> {
        (self.span > 1).then(|| {
            let half = self.span / 2;
            (
                NodeId {
                    start: self.start,
                    span: half,
                },
                NodeId {
                    start: self.start + half,
                    span: half,
                },
            )
        })
    }

    /// The parent, and whether this node is its left child.
    pub fn parent(self) -> (NodeId, bool) {
        let span = self.span * 2;
        let start = self.start - self.start % span;
        (NodeId { start, span }, start == self.start)
    }

    pub fn sibling(self) -> NodeId {
        let (parent, left) = self.parent();
        NodeId {
            start: if left {
                self.start + self.span
            } else {
                parent.start
            },
            span: self.span,
        }
    }
}

/// The label that precedes an original entry inside a leaf.
pub fn leaf_text(kind: &str, seq: i64, supersedes: Option<i64>, body: &str) -> String {
    let label = match kind {
        "owner_message" => "Owner".to_owned(),
        "assistant_message" => "Assistant".to_owned(),
        "owner_correction" => format!(
            "Owner correction of #{}",
            supersedes.map_or("?".into(), |s| s.to_string())
        ),
        "tool_activity" => "Tool".to_owned(),
        other => other.to_owned(),
    };
    let _ = seq;
    format!("{label}: {body}")
}

/// How a node is built without a model, if it can be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Build {
    /// The input fits: saved unchanged (`verbatim`) or two children joined.
    Exact { method: &'static str, body: String },
    /// Too long for an exact node: needs a summary.
    NeedsSummary { input: String },
}

pub fn build_leaf(text: String, recipe: &Recipe) -> Build {
    if !text.is_empty() && text.len() <= recipe.node_bytes {
        Build::Exact {
            method: "verbatim",
            body: text,
        }
    } else {
        Build::NeedsSummary { input: text }
    }
}

pub fn build_parent(left: &str, right: &str, recipe: &Recipe) -> Build {
    let joined = format!("{left}\n{right}");
    if joined.len() <= recipe.node_bytes {
        Build::Exact {
            method: "join",
            body: joined,
        }
    } else {
        Build::NeedsSummary { input: joined }
    }
}

/// Bytes of one rendered view line: `start+span|text` with newlines flattened.
pub fn line_bytes(node: NodeId, body: &str) -> usize {
    format!("{}+{}|", node.start, node.span).len() + body.len() + 1
}

/// The `<chat>` wrapper around the lines.
pub const WRAPPER_BYTES: usize = "<chat>\n</chat>".len();

pub fn render(view: &[(NodeId, &str)]) -> String {
    let mut text = String::from("<chat>\n");
    for (node, body) in view {
        text.push_str(&format!(
            "{}+{}|{}\n",
            node.start,
            node.span,
            body.replace('\n', " ")
        ));
    }
    text.push_str("</chat>");
    text
}

/// Whether `view` covers exactly entries `0..count` with aligned nodes.
pub fn covers(view: &[NodeId], count: i64) -> bool {
    let mut cursor = 0;
    for node in view {
        if node.start != cursor || !node.is_valid() {
            return false;
        }
        cursor = node.end();
    }
    cursor == count
}

/// The adjacent sibling pair to merge next, if any: among pairs whose
/// parent is built, the greatest `due = (count - last) / span` where `last`
/// is the pair's final entry; ties go to the oldest range. Compared exactly
/// by cross multiplication.
pub fn next_merge(view: &[NodeId], count: i64, built: impl Fn(NodeId) -> bool) -> Option<usize> {
    let mut best: Option<(i128, i128, i64, usize)> = None; // (numerator, denominator, start, pos)
    for pos in 0..view.len().saturating_sub(1) {
        let left = view[pos];
        let right = view[pos + 1];
        let (parent, is_left) = left.parent();
        if !is_left || right.start != left.end() || right.span != left.span || !built(parent) {
            continue;
        }
        let last = left.start + 2 * left.span - 1;
        let numerator = (count - last) as i128;
        let denominator = left.span as i128;
        let better = match best {
            None => true,
            Some((n, d, start, _)) => match (numerator * d).cmp(&(n * denominator)) {
                Ordering::Greater => true,
                Ordering::Equal => left.start < start,
                Ordering::Less => false,
            },
        };
        if better {
            best = Some((numerator, denominator, left.start, pos));
        }
    }
    best.map(|(_, _, _, pos)| pos)
}

/// A saved view with its byte size, updated incrementally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub nodes: Vec<NodeId>,
    pub compacting: bool,
}

/// Append newly published leaves, then compact if due. `size_of` gives the
/// rendered line bytes of a node and `built` whether a node exists. Returns
/// whether any merge happened.
pub fn publish(
    view: &mut View,
    leaves: &[NodeId],
    count: i64,
    recipe: &Recipe,
    size_of: &dyn Fn(NodeId) -> usize,
    built: &dyn Fn(NodeId) -> bool,
) -> bool {
    view.nodes.extend_from_slice(leaves);
    let mut size = WRAPPER_BYTES + view.nodes.iter().map(|n| size_of(*n)).sum::<usize>();
    if size > recipe.view_high {
        view.compacting = true;
    }
    let mut merged = false;
    if view.compacting {
        while size > recipe.view_low {
            let Some(pos) = next_merge(&view.nodes, count, built) else {
                break;
            };
            let (left, right) = (view.nodes[pos], view.nodes[pos + 1]);
            let parent = left.parent().0;
            size = size - size_of(left) - size_of(right) + size_of(parent);
            view.nodes.splice(pos..pos + 2, [parent]);
            merged = true;
        }
        // Without a built parent, coverage stays and compaction resumes later.
        view.compacting = size > recipe.view_low;
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(start: i64, span: i64) -> NodeId {
        NodeId { start, span }
    }

    #[test]
    fn reference_merge_order_regression() {
        // At 10 entries the newest pair merges, not the older left pair:
        // due is measured from the pair's last entry.
        let view = [n(0, 4), n(4, 4), n(8, 1), n(9, 1)];
        assert_eq!(next_merge(&view, 10, |_| true), Some(2));
        // Without that parent built, the older pair is next; without any
        // built parent nothing merges and coverage stays.
        assert_eq!(next_merge(&view, 10, |p| p != n(8, 2)), Some(0));
        assert_eq!(next_merge(&view, 10, |_| false), None);
    }

    /// The reference's rollback push (docs/experiments/memory/run.py,
    /// `rollback_equivalence`): states as (keep, start) pairs, newest first.
    fn push(state: i64, states: &mut Vec<(u8, i64)>, at: usize) {
        if at == states.len() {
            states.push((0, state));
            return;
        }
        let (keep, old) = states[at];
        if keep == 0 {
            states[at] = (1, old);
        } else {
            states[at] = (0, state);
            push(old, states, at + 1);
        }
    }

    #[test]
    fn last_based_merging_matches_rollback_push_for_20001_lengths() {
        let mut states: Vec<(u8, i64)> = Vec::new();
        let mut view: Vec<NodeId> = Vec::new();
        for count in 1..=20_001i64 {
            push(count - 1, &mut states, 0);
            let mut starts: Vec<i64> = states.iter().map(|(_, start)| *start).collect();
            starts.reverse();
            let expected: Vec<NodeId> = starts
                .iter()
                .enumerate()
                .map(|(i, s)| NodeId {
                    start: *s,
                    span: starts.get(i + 1).copied().unwrap_or(count) - s,
                })
                .collect();
            view.push(NodeId::leaf(count - 1));
            // Every aligned parent counts as built for this algorithm check.
            while view.len() > expected.len() {
                let pos = next_merge(&view, count, |_| true).expect("a merge");
                let parent = view[pos].parent().0;
                view.splice(pos..pos + 2, [parent]);
            }
            assert_eq!(view, expected, "count {count}");
            assert!(covers(&view, count));
        }
    }

    #[test]
    fn exact_copies_and_joins_respect_the_node_limit() {
        let recipe = Recipe {
            node_bytes: 16,
            ..Recipe::default()
        };
        assert_eq!(
            build_leaf("Owner: hi".into(), &recipe),
            Build::Exact {
                method: "verbatim",
                body: "Owner: hi".into()
            }
        );
        assert!(matches!(
            build_leaf("Owner: a much longer text".into(), &recipe),
            Build::NeedsSummary { .. }
        ));
        assert_eq!(
            build_parent("a", "b", &recipe),
            Build::Exact {
                method: "join",
                body: "a\nb".into()
            }
        );
        assert!(matches!(
            build_parent("0123456789", "0123456789", &recipe),
            Build::NeedsSummary { .. }
        ));
    }

    #[test]
    fn publish_compacts_toward_the_low_threshold_and_keeps_coverage() {
        let recipe = Recipe {
            node_bytes: 512,
            view_high: 400,
            view_low: 200,
            version: 1,
        };
        let size = |node: NodeId| line_bytes(node, "x".repeat(20).as_str());
        let mut view = View {
            nodes: vec![],
            compacting: false,
        };
        for count in 1..=200 {
            publish(
                &mut view,
                &[NodeId::leaf(count - 1)],
                count,
                &recipe,
                &size,
                &|_| true,
            );
            assert!(covers(&view.nodes, count));
            let bytes = WRAPPER_BYTES + view.nodes.iter().map(|n| size(*n)).sum::<usize>();
            assert!(bytes <= recipe.view_high, "count {count}: {bytes}");
        }
        // With no parents built, nothing merges and nothing is lost.
        let mut blocked = View {
            nodes: vec![],
            compacting: false,
        };
        let leaves: Vec<NodeId> = (0..64).map(NodeId::leaf).collect();
        publish(&mut blocked, &leaves, 64, &recipe, &size, &|_| false);
        assert_eq!(blocked.nodes.len(), 64);
        assert!(blocked.compacting, "stays due until parents exist");
    }

    #[test]
    fn node_ids_are_aligned_powers_of_two() {
        assert!(n(8, 4).is_valid());
        assert!(!n(6, 4).is_valid());
        assert!(!n(0, 3).is_valid());
        assert_eq!(n(4, 4).parent(), (n(0, 8), false));
        assert_eq!(n(8, 4).sibling(), n(12, 4));
        assert_eq!(n(0, 8).children(), Some((n(0, 4), n(4, 4))));
    }
}
