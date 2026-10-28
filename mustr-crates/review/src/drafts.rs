//! A pending review: line comments drafted locally, the review's summary, and unsent replies and
//! edits, all submitted to the forge only when the reviewer says so.
//!
//! Drafts are plain data so the app can persist them (`mustr-docs/PLAN.md` §7: drafts are never
//! lost). Forges accept line comments only on lines of their diff's hunks (§14), so a draft is
//! anchored to a [`CommentPosition`] the forge's hunks allow, found with [`CommentableLines`].

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::DiffSide;

/// One line of a forge's diff hunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HunkLine {
    Context,
    Added,
    Deleted,
}

/// Where a line comment goes in the forge's diff, as T3's `PullRequestReviewPosition`. Lines
/// are 1-based.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum CommentPosition {
    #[serde(rename_all = "camelCase")]
    Added { new_line: u32 },
    #[serde(rename_all = "camelCase")]
    Deleted { old_line: u32 },
    /// An unchanged line, which shows on both sides; `side` is the one the comment is on.
    #[serde(rename_all = "camelCase")]
    Context {
        old_line: u32,
        new_line: u32,
        side: DiffSide,
    },
}

impl CommentPosition {
    pub fn side(&self) -> DiffSide {
        match self {
            CommentPosition::Added { .. } => DiffSide::Right,
            CommentPosition::Deleted { .. } => DiffSide::Left,
            CommentPosition::Context { side, .. } => *side,
        }
    }

    /// The line on [`Self::side`].
    pub fn line(&self) -> u32 {
        match *self {
            CommentPosition::Added { new_line } => new_line,
            CommentPosition::Deleted { old_line } => old_line,
            CommentPosition::Context {
                old_line,
                new_line,
                side,
            } => match side {
                DiffSide::Left => old_line,
                DiffSide::Right => new_line,
            },
        }
    }
}

/// The lines of one file that a forge accepts line comments on: the lines of the hunks in the
/// patch it serves. GitHub rejects a whole review if one of its comments is anywhere else.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommentableLines {
    /// Head lines in a hunk, with the base line of each context line.
    head: BTreeMap<u32, Option<u32>>,
    /// Base lines in a hunk, with the head line of each context line.
    base: BTreeMap<u32, Option<u32>>,
}

impl CommentableLines {
    /// From the hunks of the forge's patch: each hunk's first base line, first head line, and
    /// lines in order.
    pub fn from_hunks<L: IntoIterator<Item = HunkLine>>(
        hunks: impl IntoIterator<Item = (u32, u32, L)>,
    ) -> Self {
        let mut lines = Self::default();
        for (old_start, new_start, hunk_lines) in hunks {
            // A hunk that starts at line 0 has no lines on that side.
            let (mut old_line, mut new_line) = (old_start.max(1), new_start.max(1));
            for line in hunk_lines {
                match line {
                    HunkLine::Context => {
                        lines.head.insert(new_line, Some(old_line));
                        lines.base.insert(old_line, Some(new_line));
                        old_line += 1;
                        new_line += 1;
                    }
                    HunkLine::Added => {
                        lines.head.insert(new_line, None);
                        new_line += 1;
                    }
                    HunkLine::Deleted => {
                        lines.base.insert(old_line, None);
                        old_line += 1;
                    }
                }
            }
        }
        lines
    }

    pub fn is_empty(&self) -> bool {
        self.head.is_empty() && self.base.is_empty()
    }

    /// Where a comment on `line` of `side` goes, or `None` when the forge doesn't accept one there.
    pub fn position(&self, side: DiffSide, line: u32) -> Option<CommentPosition> {
        match side {
            DiffSide::Right => Some(match *self.head.get(&line)? {
                None => CommentPosition::Added { new_line: line },
                Some(old_line) => CommentPosition::Context {
                    old_line,
                    new_line: line,
                    side,
                },
            }),
            DiffSide::Left => Some(match *self.base.get(&line)? {
                None => CommentPosition::Deleted { old_line: line },
                Some(new_line) => CommentPosition::Context {
                    old_line: line,
                    new_line,
                    side,
                },
            }),
        }
    }

    /// How far `line` is from the nearest line that takes comments, on the same side.
    pub fn distance_to_nearest(&self, side: DiffSide, line: u32) -> Option<u32> {
        let lines = match side {
            DiffSide::Right => &self.head,
            DiffSide::Left => &self.base,
        };
        let before = lines
            .range(..=line)
            .next_back()
            .map(|(other, _)| line - other);
        let after = lines.range(line..).next().map(|(other, _)| other - line);
        match (before, after) {
            (Some(before), Some(after)) => Some(before.min(after)),
            (before, after) => before.or(after),
        }
    }
}

/// The reviewer's verdict, as T3's `PullRequestReviewVerdict`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewVerdict {
    #[default]
    Comment,
    Approve,
    RequestChanges,
}

impl ReviewVerdict {
    pub const ALL: [ReviewVerdict; 3] = [
        ReviewVerdict::Comment,
        ReviewVerdict::Approve,
        ReviewVerdict::RequestChanges,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ReviewVerdict::Comment => "Comment",
            ReviewVerdict::Approve => "Approve",
            ReviewVerdict::RequestChanges => "Request changes",
        }
    }
}

/// A line comment drafted for the pending review.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftComment {
    /// A local id, unique within the review.
    pub id: String,
    /// The file's path in the head revision, or in the base revision for deleted files.
    pub path: String,
    /// The path before a rename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub position: CommentPosition,
    /// The first line of the selection the comment was written on, when it spanned several
    /// lines on the same side. The comment itself goes on [`Self::position`], its last line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    /// Markdown.
    pub body: String,
    /// `false` while the comment is still being written: it reopens in its composer and isn't
    /// part of the submitted review until it's added.
    #[serde(default)]
    pub added: bool,
}

impl DraftComment {
    /// "Line 14", "Lines 10–14", with "(base)" for the base side.
    pub fn line_label(&self) -> String {
        let line = self.position.line();
        let lines = match self.start_line {
            Some(start) if start < line => format!("Lines {start}–{line}"),
            _ => format!("Line {line}"),
        };
        match self.position.side() {
            DiffSide::Left => format!("{lines} (base)"),
            DiffSide::Right => lines,
        }
    }
}

/// Everything about a review that isn't on the forge yet.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingReview {
    #[serde(default)]
    pub comments: Vec<DraftComment>,
    /// The review's summary body.
    #[serde(default)]
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<ReviewVerdict>,
    /// Unsent replies, by thread id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub replies: BTreeMap<String, String>,
    /// Unsaved edits of the reviewer's own comments, by comment id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub edits: BTreeMap<String, String>,
}

impl PendingReview {
    pub fn is_empty(&self) -> bool {
        self.comments
            .iter()
            .all(|comment| comment.body.trim().is_empty())
            && self.summary.trim().is_empty()
            && self.replies.values().all(|reply| reply.trim().is_empty())
            && self.edits.is_empty()
    }

    /// The comments that go with the review when it's submitted.
    pub fn added_comments(&self) -> impl Iterator<Item = &DraftComment> {
        self.comments.iter().filter(|comment| comment.added)
    }
}

/// The lines a comment was written on, as the diff showed them when the review was submitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuotedCode {
    /// The language's name for code fences, such as `rust`.
    pub language: Option<String>,
    pub text: String,
}

/// What's sent when the reviewer submits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewSubmission {
    pub verdict: ReviewVerdict,
    pub body: String,
    pub comments: Vec<DraftComment>,
    /// The code each comment is on, by draft id. A forge has the diff already; an agent gets the
    /// code quoted in its message.
    pub quotes: BTreeMap<String, QuotedCode>,
}

/// T3's rule (`PullRequestReviewForm.tsx`): approving needs nothing, the other verdicts need a
/// summary or at least one comment.
pub fn can_submit(verdict: ReviewVerdict, summary: &str, comment_count: usize) -> bool {
    verdict == ReviewVerdict::Approve || !summary.trim().is_empty() || comment_count > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use HunkLine::{Added, Context, Deleted};
    use pretty_assertions::assert_eq;

    fn lines() -> CommentableLines {
        // @@ -3,4 +3,5 @@: context, deleted, added, added, context, context
        // @@ -20,0 +22,1 @@: added
        CommentableLines::from_hunks([
            (3, 3, vec![Context, Deleted, Added, Added, Context, Context]),
            (20, 22, vec![Added]),
        ])
    }

    #[test]
    fn positions_follow_the_forges_hunks() {
        let lines = lines();
        assert_eq!(
            lines.position(DiffSide::Right, 3),
            Some(CommentPosition::Context {
                old_line: 3,
                new_line: 3,
                side: DiffSide::Right
            })
        );
        assert_eq!(
            lines.position(DiffSide::Left, 4),
            Some(CommentPosition::Deleted { old_line: 4 })
        );
        assert_eq!(
            lines.position(DiffSide::Right, 4),
            Some(CommentPosition::Added { new_line: 4 })
        );
        assert_eq!(
            lines.position(DiffSide::Right, 5),
            Some(CommentPosition::Added { new_line: 5 })
        );
        // Context after the change: base line 5 is head line 6.
        assert_eq!(
            lines.position(DiffSide::Left, 5),
            Some(CommentPosition::Context {
                old_line: 5,
                new_line: 6,
                side: DiffSide::Left
            })
        );
        assert_eq!(
            lines.position(DiffSide::Right, 7),
            Some(CommentPosition::Context {
                old_line: 6,
                new_line: 7,
                side: DiffSide::Right
            })
        );
        assert_eq!(lines.position(DiffSide::Right, 8), None);
        assert_eq!(lines.position(DiffSide::Right, 2), None);
        assert_eq!(
            lines.position(DiffSide::Right, 22),
            Some(CommentPosition::Added { new_line: 22 })
        );
        assert_eq!(lines.position(DiffSide::Left, 20), None);

        assert_eq!(lines.distance_to_nearest(DiffSide::Right, 12), Some(5));
        assert_eq!(lines.distance_to_nearest(DiffSide::Right, 1), Some(2));
        assert_eq!(lines.distance_to_nearest(DiffSide::Left, 40), Some(34));
        assert_eq!(
            CommentableLines::default().distance_to_nearest(DiffSide::Left, 1),
            None
        );

        // A new file's hunk starts at base line 0.
        let added = CommentableLines::from_hunks([(0, 1, vec![Added, Added])]);
        assert_eq!(
            added.position(DiffSide::Right, 2),
            Some(CommentPosition::Added { new_line: 2 })
        );
        assert_eq!(added.position(DiffSide::Left, 1), None);
    }

    #[test]
    fn drafts_round_trip_through_json_in_t3s_shapes() {
        let review = PendingReview {
            comments: vec![DraftComment {
                id: "d1".to_string(),
                path: "src/main.rs".to_string(),
                old_path: None,
                position: CommentPosition::Context {
                    old_line: 5,
                    new_line: 6,
                    side: DiffSide::Left,
                },
                start_line: Some(3),
                body: "Why?".to_string(),
                added: true,
            }],
            summary: "Looks good.".to_string(),
            verdict: Some(ReviewVerdict::RequestChanges),
            replies: [("PRRT_1".to_string(), "On it".to_string())]
                .into_iter()
                .collect(),
            edits: BTreeMap::new(),
        };
        let json = serde_json::to_value(&review).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "comments": [{
                    "id": "d1",
                    "path": "src/main.rs",
                    "position": { "kind": "context", "oldLine": 5, "newLine": 6, "side": "left" },
                    "startLine": 3,
                    "body": "Why?",
                    "added": true
                }],
                "summary": "Looks good.",
                "verdict": "request-changes",
                "replies": { "PRRT_1": "On it" }
            })
        );
        assert_eq!(
            serde_json::from_value::<PendingReview>(json).unwrap(),
            review
        );
        assert_eq!(
            serde_json::from_str::<PendingReview>("{}").unwrap(),
            PendingReview::default()
        );
    }

    #[test]
    fn labels_and_submit_rules() {
        let mut draft = DraftComment {
            id: "d".to_string(),
            path: "a.rs".to_string(),
            old_path: None,
            position: CommentPosition::Added { new_line: 14 },
            start_line: None,
            body: String::new(),
            added: false,
        };
        assert_eq!(draft.line_label(), "Line 14");
        draft.start_line = Some(10);
        assert_eq!(draft.line_label(), "Lines 10–14");
        draft.position = CommentPosition::Deleted { old_line: 14 };
        assert_eq!(draft.line_label(), "Lines 10–14 (base)");

        assert!(can_submit(ReviewVerdict::Approve, "", 0));
        assert!(!can_submit(ReviewVerdict::Comment, "  ", 0));
        assert!(!can_submit(ReviewVerdict::RequestChanges, "", 0));
        assert!(can_submit(ReviewVerdict::RequestChanges, "Fix it", 0));
        assert!(can_submit(ReviewVerdict::Comment, "", 1));

        let mut pending = PendingReview::default();
        assert!(pending.is_empty());
        pending.replies.insert("t".to_string(), " ".to_string());
        assert!(pending.is_empty());
        pending.comments.push(DraftComment {
            body: "Hm".to_string(),
            ..draft
        });
        assert!(!pending.is_empty());
        assert_eq!(pending.added_comments().count(), 0);
    }
}
