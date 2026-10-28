//! Review threads (a forge's line comments) and the cards that show them in the diff.
//!
//! A thread's card goes in an editor block below its line (`mustr-docs/ZED-INTEGRATION.md`, Q3).
//! Threads the diff can't place (the forge dropped their line after later pushes, or the line is
//! out of range) go in a collapsible list at the top of their file instead.

use buffer_diff::BufferDiff;
use editor::{
    Editor,
    display_map::{BlockPlacement, BlockProperties, BlockStyle},
};
use gpui::{
    App, AppContext as _, Context, Entity, FontWeight, IntoElement, MouseButton,
    ParentElement as _, Render, SharedString, Styled as _, TextStyleRefinement, Window, px,
    relative,
};
use language::{Buffer, LanguageRegistry, Point};
use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use multi_buffer::Anchor;
use mustr_ui::tokens;
use std::sync::Arc;
use time::{OffsetDateTime, UtcOffset};
use time_format::TimestampFormat;
use ui::{Color, Icon, IconName, IconSize, Label, LabelCommon as _, LabelSize, prelude::*};

use crate::{comment_html::comment_markdown, header::pill};

/// Cards are capped at this many columns of the editor's font, so long comments stay readable
/// on wide screens.
const CARD_MAX_COLUMNS: f32 = 110.;

/// Which side of the diff a thread's line is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffSide {
    /// The base revision (deleted and unchanged lines).
    Left,
    /// The head revision (added and unchanged lines).
    Right,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadComment {
    pub id: String,
    /// `None` for deleted accounts.
    pub author: Option<String>,
    /// Markdown, as written on the forge.
    pub body: String,
    pub created_at: Option<OffsetDateTime>,
    pub edited: bool,
}

/// A thread of line comments on one file, as the forge reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewThread {
    pub id: String,
    pub path: String,
    /// The 1-based line on `side`, or `None` when the forge no longer places the thread in the
    /// diff (GitHub drops the line of outdated threads).
    pub line: Option<u32>,
    pub side: DiffSide,
    pub is_resolved: bool,
    pub is_outdated: bool,
    pub comments: Vec<ThreadComment>,
    /// How many comments the thread has, which can be more than `comments` holds when not all of
    /// them could be loaded.
    pub comment_count: usize,
}

/// Which editor of the diff a card is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorSide {
    /// The head side in split mode, and the only editor in unified mode.
    Right,
    /// The base side in split mode.
    Left,
}

/// Where a thread's card goes: below this anchor in this editor. `None` when the thread's line
/// isn't in the diff, so it belongs in its file's outdated list.
pub(crate) fn thread_anchor(
    thread: &ReviewThread,
    buffer: &Entity<Buffer>,
    diff: &Entity<BufferDiff>,
    rhs_editor: &Entity<Editor>,
    lhs_editor: Option<&Entity<Editor>>,
    cx: &App,
) -> Option<(EditorSide, Anchor)> {
    let row = thread.line?.checked_sub(1)?;
    let buffer_snapshot = buffer.read(cx).snapshot();
    let rhs_snapshot = rhs_editor.read(cx).buffer().read(cx).snapshot(cx);
    match thread.side {
        DiffSide::Right => {
            if row > buffer_snapshot.max_point().row {
                return None;
            }
            // Biased right, so in a unified diff the anchor lands after any deleted lines shown
            // at the same position.
            let anchor = buffer_snapshot.anchor_after(Point::new(row, 0));
            Some((EditorSide::Right, rhs_snapshot.anchor_in_excerpt(anchor)?))
        }
        DiffSide::Left => {
            let diff_snapshot = diff.read(cx).snapshot(cx);
            let base = diff_snapshot.base_text();
            if row > base.max_point().row {
                return None;
            }
            let base_point = Point::new(row, 0);
            let base_anchor = base.anchor_before(base_point);
            // In split mode the base text is a buffer of the left-hand editor.
            if let Some(lhs_editor) = lhs_editor {
                let lhs_snapshot = lhs_editor.read(cx).buffer().read(cx).snapshot(cx);
                return Some((
                    EditorSide::Left,
                    lhs_snapshot.anchor_in_excerpt(base_anchor)?,
                ));
            }
            // In unified mode a deleted line shows inside its hunk, before the hunk's head text,
            // which is where `MultiBufferSnapshot::anchor_at` puts anchors into deleted text.
            let base_offset = base.point_to_offset(base_point);
            if let Some(hunk) = diff_snapshot
                .hunks(&buffer_snapshot)
                .find(|hunk| hunk.diff_base_byte_range.contains(&base_offset))
            {
                let anchor = rhs_snapshot.anchor_in_excerpt(hunk.buffer_range.start)?;
                return Some((EditorSide::Right, anchor.with_diff_base_anchor(base_anchor)));
            }
            // An unchanged line shows once, in the head text.
            let head_point =
                diff_snapshot.base_text_point_to_buffer_point(base_point, &buffer_snapshot);
            let anchor = buffer_snapshot.anchor_after(Point::new(head_point.row, 0));
            Some((EditorSide::Right, rhs_snapshot.anchor_in_excerpt(anchor)?))
        }
    }
}

/// The head-text row a thread's line shows on, so the review can make sure it's in an excerpt
/// even when it's an unchanged line further from a change than the excerpts' context reaches.
pub(crate) fn thread_head_row(
    thread: &ReviewThread,
    buffer: &Entity<Buffer>,
    diff: &Entity<BufferDiff>,
    cx: &App,
) -> Option<u32> {
    let row = thread.line?.checked_sub(1)?;
    let buffer_snapshot = buffer.read(cx).snapshot();
    match thread.side {
        DiffSide::Right => (row <= buffer_snapshot.max_point().row).then_some(row),
        DiffSide::Left => {
            let diff_snapshot = diff.read(cx).snapshot(cx);
            if row > diff_snapshot.base_text().max_point().row {
                return None;
            }
            Some(
                diff_snapshot
                    .base_text_point_to_buffer_point(Point::new(row, 0), &buffer_snapshot)
                    .row,
            )
        }
    }
}

/// A block holding `view`, aligned with the code after the gutter.
pub(crate) fn card_block<V: Render>(
    placement: BlockPlacement<Anchor>,
    view: Entity<V>,
    estimated_lines: u32,
) -> BlockProperties<Anchor> {
    BlockProperties {
        placement,
        height: Some(estimated_lines),
        // Sticky blocks don't scroll sideways with long lines, so a card stays in view.
        style: BlockStyle::Sticky,
        priority: 0,
        render: Arc::new(move |block_cx| {
            div()
                .w_full()
                .pl(block_cx.margins.gutter.full_width())
                .pr(block_cx.margins.right + px(8.))
                .py_1()
                .child(
                    div()
                        .max_w(block_cx.em_width * CARD_MAX_COLUMNS)
                        .child(view.clone()),
                )
                .into_any_element()
        }),
    }
}

/// A rough height in lines, before the editor measures the card.
pub(crate) fn estimated_card_lines(thread: &ReviewThread, expanded: bool) -> u32 {
    if !expanded {
        return 2;
    }
    let body_lines = thread
        .comments
        .iter()
        .map(|comment| comment.body.lines().count().max(1) as u32 + 2)
        .sum::<u32>();
    2 + body_lines
}

/// A thread's card: the first author and a summary when collapsed, every comment when expanded.
/// Resolved threads start collapsed.
pub struct ThreadCard {
    thread: Arc<ReviewThread>,
    bodies: Vec<Entity<Markdown>>,
    /// Shown in the card's title, for cards listed away from their line.
    location: Option<SharedString>,
    expanded: bool,
}

impl ThreadCard {
    pub(crate) fn new(
        thread: Arc<ReviewThread>,
        location: Option<SharedString>,
        language_registry: Option<Arc<LanguageRegistry>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let bodies = thread
            .comments
            .iter()
            .map(|comment| {
                let body = SharedString::from(comment_markdown(&comment.body));
                let language_registry = language_registry.clone();
                cx.new(|cx| Markdown::new(body, language_registry, None, cx))
            })
            .collect();
        Self {
            expanded: !thread.is_resolved,
            thread,
            bodies,
            location,
        }
    }

    pub fn thread(&self) -> &Arc<ReviewThread> {
        &self.thread
    }

    pub fn is_expanded(&self) -> bool {
        self.expanded
    }

    fn render_comment(
        &self,
        index: usize,
        comment: &ThreadComment,
        window: &Window,
        cx: &App,
    ) -> impl IntoElement + use<> {
        let colors = cx.theme().colors();
        let time = comment
            .created_at
            .map(|created_at| relative_time(created_at, OffsetDateTime::now_utc()));
        let body = self.bodies.get(index).cloned();
        v_flex()
            .px_3()
            .py_2()
            .gap_1()
            .border_t(tokens::BORDER_WIDTH)
            .border_color(colors.border_variant)
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        Label::new(author_name(comment))
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .when_some(time, |this, time| {
                        this.child(Label::new(time).size(LabelSize::Small).color(Color::Muted))
                    })
                    .when(comment.edited, |this| {
                        this.child(
                            Label::new("edited")
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    }),
            )
            .when_some(body, |this, body| {
                this.child(
                    MarkdownElement::new(body, comment_markdown_style(window, cx))
                        .on_url_click(|url, _, cx| cx.open_url(&url)),
                )
            })
    }
}

impl Render for ThreadCard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let status = cx.theme().status();
        let thread = self.thread.clone();
        let expanded = self.expanded;
        let first = thread.comments.first();
        let summary = first
            .map(|comment| plain_summary(&comment.body))
            .unwrap_or_default();
        let count = thread.comment_count.max(thread.comments.len());
        let count_label = if count == 1 {
            "1 comment".to_string()
        } else {
            format!("{count} comments")
        };
        let not_loaded = count.saturating_sub(thread.comments.len());

        let title = h_flex()
            .id("thread-card-title")
            .w_full()
            .px_2()
            .py_1()
            .gap_1p5()
            .cursor_pointer()
            .hover(|style| style.bg(colors.element_hover))
            .on_click(cx.listener(|this, _, _, cx| {
                this.expanded = !this.expanded;
                cx.notify();
            }))
            .child(
                Icon::new(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(IconSize::XSmall)
                .color(Color::Muted),
            )
            .child(
                Icon::new(IconName::Chat)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .when_some(self.location.clone(), |this, location| {
                this.child(
                    Label::new(location)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .buffer_font(cx),
                )
            })
            .when_some(first, |this, first| {
                this.child(
                    Label::new(author_name(first))
                        .size(LabelSize::Small)
                        .weight(FontWeight::SEMIBOLD),
                )
            })
            .child(div().flex_1().min_w_0().when(!expanded, |this| {
                this.child(
                    Label::new(summary)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                )
            }))
            .when(thread.is_outdated, |this| {
                this.child(pill("Outdated", status.warning))
            })
            .when(thread.is_resolved, |this| {
                this.child(pill("Resolved", status.success))
            })
            .child(
                Label::new(count_label)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );

        let comments = if expanded {
            thread
                .comments
                .iter()
                .enumerate()
                .map(|(index, comment)| {
                    self.render_comment(index, comment, window, cx)
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        v_flex()
            .w_full()
            .rounded(tokens::CORNER_RADIUS)
            .border(tokens::BORDER_WIDTH)
            .border_color(colors.border)
            .bg(colors.surface_background)
            .overflow_hidden()
            // Clicks on a card are the card's, not the editor's: they shouldn't move the cursor.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(title)
            .children(comments)
            .when(expanded && not_loaded > 0, |this| {
                this.child(
                    div()
                        .px_3()
                        .py_1()
                        .border_t(tokens::BORDER_WIDTH)
                        .border_color(colors.border_variant)
                        .child(
                            Label::new(format!(
                                "{not_loaded} more {} not loaded",
                                if not_loaded == 1 {
                                    "comment"
                                } else {
                                    "comments"
                                }
                            ))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        ),
                )
            })
    }
}

/// The threads of one file that the diff can't place, listed at the top of the file.
pub struct OutdatedThreads {
    cards: Vec<Entity<ThreadCard>>,
    expanded: bool,
}

impl OutdatedThreads {
    pub(crate) fn new() -> Self {
        Self {
            cards: Vec::new(),
            expanded: false,
        }
    }

    pub(crate) fn set_cards(&mut self, cards: Vec<Entity<ThreadCard>>, cx: &mut Context<Self>) {
        self.cards = cards;
        cx.notify();
    }

    pub fn thread_ids(&self, cx: &App) -> Vec<String> {
        self.cards
            .iter()
            .map(|card| card.read(cx).thread.id.clone())
            .collect()
    }
}

impl Render for OutdatedThreads {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let expanded = self.expanded;
        let count = self.cards.len();
        v_flex()
            .w_full()
            .rounded(tokens::CORNER_RADIUS)
            .border(tokens::BORDER_WIDTH)
            .border_color(colors.border)
            .bg(colors.surface_background)
            .overflow_hidden()
            // Clicks on a card are the card's, not the editor's: they shouldn't move the cursor.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                h_flex()
                    .id("outdated-threads-title")
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_1p5()
                    .cursor_pointer()
                    .hover(|style| style.bg(colors.element_hover))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.expanded = !this.expanded;
                        cx.notify();
                    }))
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                    )
                    .child(
                        Icon::new(IconName::Chat)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(if count == 1 {
                            "1 outdated thread".to_string()
                        } else {
                            format!("{count} outdated threads")
                        })
                        .size(LabelSize::Small),
                    )
                    .child(
                        Label::new("on lines no longer in the diff")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .when(expanded, |this| {
                this.child(
                    v_flex()
                        .p_2()
                        .gap_2()
                        .border_t(tokens::BORDER_WIDTH)
                        .border_color(colors.border_variant)
                        .children(self.cards.iter().cloned()),
                )
            })
    }
}

/// The first line of a comment with the most common Markdown markers removed, for a one-line
/// summary.
fn plain_summary(body: &str) -> String {
    let body = comment_markdown(body);
    let line = body
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let line = line.trim_start_matches(['#', '>', ' ']);
    line.replace("**", "")
        .replace("__", "")
        .replace('`', "")
        .trim()
        .to_string()
}

fn author_name(comment: &ThreadComment) -> String {
    comment
        .author
        .clone()
        .unwrap_or_else(|| "Deleted user".to_string())
}

fn comment_markdown_style(window: &Window, cx: &App) -> MarkdownStyle {
    let mut style = editor::hover_markdown_style(window, cx);
    style.base_text_style.refine(&TextStyleRefinement {
        font_size: Some(tokens::TEXT_SIZE_LARGE.into()),
        line_height: Some(relative(1.5)),
        ..Default::default()
    });
    style
}

/// "5 minutes ago", "Yesterday", "3 weeks ago", in the local time zone.
pub(crate) fn relative_time(timestamp: OffsetDateTime, now: OffsetDateTime) -> String {
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    time_format::format_localized_timestamp(timestamp, now, offset, TimestampFormat::Relative)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn summaries_drop_markdown_markers() {
        assert_eq!(
            plain_summary("\n## **Critical** `src/main.rs:3`\nMore."),
            "Critical src/main.rs:3"
        );
        assert_eq!(plain_summary("> quoted"), "quoted");
        assert_eq!(
            plain_summary("<!-- bot -->\n<details><summary>Review <b>status</b></summary>"),
            "Review status"
        );
        assert_eq!(plain_summary(""), "");
    }

    #[test]
    fn relative_times() {
        let now = datetime!(2026-10-27 12:00 UTC);
        assert_eq!(
            relative_time(datetime!(2026-10-27 11:55 UTC), now),
            "5 minutes ago"
        );
        assert_eq!(
            relative_time(datetime!(2026-10-27 09:00 UTC), now),
            "3 hours ago"
        );
        assert_eq!(
            relative_time(datetime!(2026-10-06 12:00 UTC), now),
            "3 weeks ago"
        );
    }

    #[test]
    fn card_heights_are_estimated_from_the_comments() {
        let thread = ReviewThread {
            id: "t".to_string(),
            path: "a.rs".to_string(),
            line: Some(1),
            side: DiffSide::Right,
            is_resolved: false,
            is_outdated: false,
            comments: vec![ThreadComment {
                id: "c".to_string(),
                author: None,
                body: "one\ntwo".to_string(),
                created_at: None,
                edited: false,
            }],
            comment_count: 1,
        };
        assert_eq!(estimated_card_lines(&thread, false), 2);
        assert_eq!(estimated_card_lines(&thread, true), 6);
    }
}
