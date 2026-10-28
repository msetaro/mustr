//! Review threads (a forge's line comments) and the cards that show them in the diff.
//!
//! A thread's card goes in an editor block below its line (`mustr-docs/ZED-INTEGRATION.md`, Q3).
//! Threads the diff can't place (the forge dropped their line after later pushes, or the line is
//! out of range) go in a collapsible list at the top of their file instead.

use anyhow::Result;
use buffer_diff::BufferDiff;
use editor::{
    Editor,
    display_map::{BlockPlacement, BlockProperties, BlockStyle},
};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FontWeight, IntoElement,
    MouseButton, ParentElement as _, Pixels, Render, SharedString, Styled as _, Subscription, Task,
    TextStyleRefinement, Window, px, relative,
};
use language::{Buffer, LanguageRegistry, Point};
use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use multi_buffer::Anchor;
use mustr_ui::{tokens, tokens::scaled};
use serde::{Deserialize, Serialize};
use settings::Settings as _;
use std::sync::Arc;
use theme_settings::ThemeSettings;
use time::{OffsetDateTime, UtcOffset};
use time_format::TimestampFormat;
use ui::{
    Button, ButtonSize, ButtonStyle, Color, Icon, IconName, IconSize, Label, LabelCommon as _,
    LabelSize, Tooltip, prelude::*,
};

use crate::{
    ReviewBackend, ReviewPermissions,
    comment_html::comment_markdown,
    composer::{CancelComment, Composer, ConfirmComment, cancel_button, error_label},
    header::pill,
};

/// Cards are capped at this many columns of the editor's font, so long comments stay readable
/// on wide screens.
const CARD_MAX_COLUMNS: f32 = 110.;

/// Which side of the diff a thread's line is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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
    /// Whether the viewer may edit it (it's theirs).
    pub can_edit: bool,
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
    /// Whether the viewer may resolve or unresolve it.
    pub can_resolve: bool,
    pub comments: Vec<ThreadComment>,
    /// How many comments the thread has, which can be more than `comments` holds when not all of
    /// them could be loaded.
    pub comment_count: usize,
    /// A local comment on an agent's diff that was sent to the agent, rather than a forge's
    /// thread.
    pub sent_to_agent: bool,
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
    line_anchor(
        thread.side,
        thread.line?,
        buffer,
        diff,
        rhs_editor,
        lhs_editor,
        cx,
    )
}

/// Where a card below the 1-based `line` of `side` goes. `None` when the line isn't in the diff.
pub(crate) fn line_anchor(
    side: DiffSide,
    line: u32,
    buffer: &Entity<Buffer>,
    diff: &Entity<BufferDiff>,
    rhs_editor: &Entity<Editor>,
    lhs_editor: Option<&Entity<Editor>>,
    cx: &App,
) -> Option<(EditorSide, Anchor)> {
    let row = line.checked_sub(1)?;
    let buffer_snapshot = buffer.read(cx).snapshot();
    let rhs_snapshot = rhs_editor.read(cx).buffer().read(cx).snapshot(cx);
    match side {
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

/// A length from mustr's designs in pixels at the user's `ui_font_size`, for the cards and what
/// they hold.
///
/// Zed's editor sets the rem size of what it draws, cards included, from the buffer font size
/// (`EditorElement::rem_size` in `crates/editor/src/element.rs`), so [`scaled`] would be bigger
/// in a card than in the rest of the UI. This is the same in a card and out of one.
pub(crate) fn ui_scaled(length: Pixels, cx: &App) -> Pixels {
    scaled(length).to_pixels(ThemeSettings::get_global(cx).ui_font_size(cx))
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
                .pr(block_cx.margins.right + ui_scaled(px(8.), block_cx.app))
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

pub enum ThreadCardEvent {
    /// Text being written in the card changed, so the pending review should be saved.
    DraftChanged,
    /// A write to the forge succeeded, so the threads should be loaded again.
    Posted,
}

/// Lets a card write to the forge.
#[derive(Clone)]
pub(crate) struct ThreadWrites {
    pub backend: Arc<dyn ReviewBackend>,
    pub permissions: Arc<ReviewPermissions>,
}

/// A write the card sends to the forge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThreadWrite {
    Reply,
    Resolve,
    Edit,
}

struct EditedComment {
    comment_id: String,
    composer: Composer,
}

/// A thread's card: the first author and a summary when collapsed, every comment when expanded.
/// Resolved threads start collapsed. With writes allowed, the card can reply, resolve, and edit
/// the viewer's own comments; errors show next to the action, and the text written is kept.
pub struct ThreadCard {
    thread: Arc<ReviewThread>,
    bodies: Vec<Entity<Markdown>>,
    /// Shown in the card's title, for cards listed away from their line.
    location: Option<SharedString>,
    expanded: bool,
    language_registry: Option<Arc<LanguageRegistry>>,
    writes: Option<ThreadWrites>,
    reply: Option<Composer>,
    edited: Option<EditedComment>,
    in_flight: Option<(ThreadWrite, Task<()>)>,
    error: Option<(ThreadWrite, SharedString)>,
    _reply_subscription: Option<Subscription>,
    _edit_subscription: Option<Subscription>,
}

impl EventEmitter<ThreadCardEvent> for ThreadCard {}

impl ThreadCard {
    pub(crate) fn new(
        thread: Arc<ReviewThread>,
        location: Option<SharedString>,
        language_registry: Option<Arc<LanguageRegistry>>,
        writes: Option<ThreadWrites>,
        cx: &mut Context<Self>,
    ) -> Self {
        let bodies = comment_bodies(&thread, language_registry.clone(), cx);
        Self {
            expanded: !thread.is_resolved,
            thread,
            bodies,
            location,
            language_registry,
            writes,
            reply: None,
            edited: None,
            in_flight: None,
            error: None,
            _reply_subscription: None,
            _edit_subscription: None,
        }
    }

    pub fn thread(&self) -> &Arc<ReviewThread> {
        &self.thread
    }

    pub fn is_expanded(&self) -> bool {
        self.expanded
    }

    /// The reply being written, if any.
    pub fn unsent_reply(&self, cx: &App) -> Option<String> {
        self.reply.as_ref().map(|composer| composer.text(cx))
    }

    /// The comment being edited and its new text.
    pub fn unsaved_edit(&self, cx: &App) -> Option<(String, String)> {
        self.edited
            .as_ref()
            .map(|edited| (edited.comment_id.clone(), edited.composer.text(cx)))
    }

    pub fn reply_editor(&self) -> Option<&Entity<Editor>> {
        self.reply.as_ref().map(Composer::editor)
    }

    pub fn edit_editor(&self) -> Option<&Entity<Editor>> {
        self.edited.as_ref().map(|edited| edited.composer.editor())
    }

    pub fn error(&self) -> Option<&SharedString> {
        self.error.as_ref().map(|(_, error)| error)
    }

    /// Reopens text that was being written when the review was last closed.
    pub(crate) fn restore(
        &mut self,
        reply: Option<&str>,
        edit: Option<(&str, &str)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(reply) = reply {
            self.open_reply(reply, window, cx);
        }
        if let Some((comment_id, text)) = edit
            && self
                .thread
                .comments
                .iter()
                .any(|comment| comment.id == comment_id)
        {
            self.open_edit(comment_id.to_string(), text, window, cx);
        }
    }

    /// Shows the thread as the forge now reports it, keeping any text being written.
    pub(crate) fn set_thread(&mut self, thread: Arc<ReviewThread>, cx: &mut Context<Self>) {
        if thread == self.thread {
            return;
        }
        self.bodies = comment_bodies(&thread, self.language_registry.clone(), cx);
        if self
            .edited
            .as_ref()
            .is_some_and(|edited| !thread.comments.iter().any(|c| c.id == edited.comment_id))
        {
            self.edited = None;
            self._edit_subscription = None;
        }
        self.thread = thread;
        cx.notify();
    }

    /// A rough height in lines, before the editor measures the card.
    pub(crate) fn estimated_lines(&self) -> u32 {
        let mut lines = estimated_card_lines(&self.thread, self.expanded);
        if self.expanded && self.writes.is_some() {
            lines += 2;
        }
        if self.reply.is_some() {
            lines += 6;
        }
        lines
    }

    pub(crate) fn start_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.expanded = true;
        match &self.reply {
            Some(reply) => reply.focus(window, cx),
            None => {
                self.open_reply("", window, cx);
                if let Some(reply) = &self.reply {
                    reply.focus(window, cx);
                }
            }
        }
        cx.notify();
    }

    fn open_reply(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let composer = Composer::new(text, "Reply…", window, cx);
        self._reply_subscription = Some(composer.subscribe(cx, |_: &mut Self, cx| {
            cx.emit(ThreadCardEvent::DraftChanged)
        }));
        self.reply = Some(composer);
    }

    fn open_edit(
        &mut self,
        comment_id: String,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let composer = Composer::new(text, "Edit the comment…", window, cx);
        self._edit_subscription = Some(composer.subscribe(cx, |_: &mut Self, cx| {
            cx.emit(ThreadCardEvent::DraftChanged)
        }));
        self.edited = Some(EditedComment {
            comment_id,
            composer,
        });
    }

    pub(crate) fn start_edit(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(comment) = self.thread.comments.get(index) else {
            return;
        };
        if self
            .edited
            .as_ref()
            .is_none_or(|edited| edited.comment_id != comment.id)
        {
            let (id, body) = (comment.id.clone(), comment.body.clone());
            self.open_edit(id, &body, window, cx);
            cx.emit(ThreadCardEvent::DraftChanged);
        }
        if let Some(edited) = &self.edited {
            edited.composer.focus(window, cx);
        }
        cx.notify();
    }

    /// Why a write can't be sent now, if it can't.
    fn write_blocked(&self) -> Option<SharedString> {
        let writes = self.writes.as_ref()?;
        writes.permissions.blocked.clone()
    }

    fn send(
        &mut self,
        write: ThreadWrite,
        task: Task<Result<()>>,
        on_success: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        self.error = None;
        let task = cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.in_flight = None;
                match result {
                    Ok(()) => {
                        on_success(this, cx);
                        cx.emit(ThreadCardEvent::Posted);
                    }
                    Err(error) => {
                        log::error!("{write:?} on a review thread failed: {error:#}");
                        this.error = Some((write, format!("{error:#}").into()));
                    }
                }
                cx.notify();
            })
            .ok();
        });
        self.in_flight = Some((write, task));
        cx.notify();
    }

    pub(crate) fn send_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(writes), Some(reply)) = (self.writes.clone(), &self.reply) else {
            return;
        };
        if self.in_flight.is_some() || self.write_blocked().is_some() {
            return;
        }
        if reply.is_blank(cx) {
            reply.focus(window, cx);
            return;
        }
        let body = reply.text(cx);
        let task = writes
            .backend
            .reply_to_thread(self.thread.id.clone(), body, cx);
        self.send(
            ThreadWrite::Reply,
            task,
            |this, cx| {
                this.reply = None;
                this._reply_subscription = None;
                cx.emit(ThreadCardEvent::DraftChanged);
            },
            cx,
        );
    }

    fn cancel_reply(&mut self, cx: &mut Context<Self>) {
        self.reply = None;
        self._reply_subscription = None;
        if matches!(self.error, Some((ThreadWrite::Reply, _))) {
            self.error = None;
        }
        cx.emit(ThreadCardEvent::DraftChanged);
        cx.notify();
    }

    pub(crate) fn set_resolved(&mut self, resolved: bool, cx: &mut Context<Self>) {
        let Some(writes) = self.writes.clone() else {
            return;
        };
        if self.in_flight.is_some() || self.write_blocked().is_some() {
            return;
        }
        let task = writes
            .backend
            .set_thread_resolution(self.thread.id.clone(), resolved, cx);
        self.send(
            ThreadWrite::Resolve,
            task,
            move |this, _| {
                let mut thread = (*this.thread).clone();
                thread.is_resolved = resolved;
                this.thread = Arc::new(thread);
            },
            cx,
        );
    }

    pub(crate) fn save_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(writes), Some(edited)) = (self.writes.clone(), &self.edited) else {
            return;
        };
        if self.in_flight.is_some() || self.write_blocked().is_some() {
            return;
        }
        if edited.composer.is_blank(cx) {
            edited.composer.focus(window, cx);
            return;
        }
        let task =
            writes
                .backend
                .update_comment(edited.comment_id.clone(), edited.composer.text(cx), cx);
        self.send(
            ThreadWrite::Edit,
            task,
            |this, cx| {
                this.edited = None;
                this._edit_subscription = None;
                cx.emit(ThreadCardEvent::DraftChanged);
            },
            cx,
        );
    }

    fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        self.edited = None;
        self._edit_subscription = None;
        if matches!(self.error, Some((ThreadWrite::Edit, _))) {
            self.error = None;
        }
        cx.emit(ThreadCardEvent::DraftChanged);
        cx.notify();
    }

    fn error_for(&self, write: ThreadWrite) -> Option<&SharedString> {
        self.error
            .as_ref()
            .filter(|(failed, _)| *failed == write)
            .map(|(_, error)| error)
    }

    fn is_sending(&self, write: ThreadWrite) -> bool {
        self.in_flight
            .as_ref()
            .is_some_and(|(sending, _)| *sending == write)
    }

    fn render_comment(
        &self,
        index: usize,
        comment: &ThreadComment,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let colors = cx.theme().colors();
        let time = comment
            .created_at
            .map(|created_at| relative_time(created_at, OffsetDateTime::now_utc()));
        let body = self.bodies.get(index).cloned();
        let edited = self
            .edited
            .as_ref()
            .filter(|edited| edited.comment_id == comment.id);
        let can_edit = comment.can_edit && self.writes.is_some() && edited.is_none();
        let blocked = self.write_blocked();
        let editor = edited.map(|edited| {
            v_flex()
                .gap_1p5()
                .on_action(
                    cx.listener(|this, _: &ConfirmComment, window, cx| this.save_edit(window, cx)),
                )
                .on_action(cx.listener(|this, _: &CancelComment, _, cx| this.cancel_edit(cx)))
                .child(edited.composer.render(cx))
                .child(
                    h_flex()
                        .gap_1()
                        .children(self.error_for(ThreadWrite::Edit).map(error_label))
                        .child(div().flex_1())
                        .child(
                            cancel_button("cancel-edit")
                                .on_click(cx.listener(|this, _, _, cx| this.cancel_edit(cx))),
                        )
                        .child(
                            edited
                                .composer
                                .confirm_button("save-edit", "Save", cx)
                                .loading(self.is_sending(ThreadWrite::Edit))
                                .disabled(blocked.is_some() || self.in_flight.is_some())
                                .when_some(blocked.clone(), |this, blocked| {
                                    this.tooltip(Tooltip::text(blocked))
                                })
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.save_edit(window, cx)),
                                ),
                        ),
                )
        });
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
                    })
                    .child(div().flex_1())
                    .when(can_edit, |this| {
                        this.child(
                            Button::new(("edit-comment", index), "Edit")
                                .style(ButtonStyle::Subtle)
                                .size(ButtonSize::Compact)
                                .label_size(LabelSize::XSmall)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.start_edit(index, window, cx)
                                })),
                        )
                    }),
            )
            .map(|this| match editor {
                Some(editor) => this.child(editor),
                None => this.when_some(body, |this, body| {
                    this.child(
                        MarkdownElement::new(body, comment_markdown_style(window, cx))
                            .on_url_click(|url, _, cx| cx.open_url(&url)),
                    )
                }),
            })
    }

    fn render_actions(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let writes = self.writes.as_ref()?;
        let colors = cx.theme().colors();
        let permissions = &writes.permissions;
        let blocked = permissions.blocked.clone();
        let busy = self.in_flight.is_some();
        let can_resolve = permissions.resolve && self.thread.can_resolve;
        let resolved = self.thread.is_resolved;

        if let Some(reply) = &self.reply {
            return Some(
                v_flex()
                    .px_3()
                    .py_2()
                    .gap_1p5()
                    .border_t(tokens::BORDER_WIDTH)
                    .border_color(colors.border_variant)
                    .on_action(cx.listener(|this, _: &ConfirmComment, window, cx| {
                        this.send_reply(window, cx)
                    }))
                    .on_action(cx.listener(|this, _: &CancelComment, _, cx| this.cancel_reply(cx)))
                    .child(reply.render(cx))
                    .child(
                        h_flex()
                            .gap_1()
                            .children(
                                self.error_for(ThreadWrite::Reply)
                                    .or(self.error_for(ThreadWrite::Resolve))
                                    .map(error_label),
                            )
                            .child(div().flex_1())
                            .child(
                                cancel_button("cancel-reply")
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel_reply(cx))),
                            )
                            .child(
                                reply
                                    .confirm_button("send-reply", "Reply", cx)
                                    .loading(self.is_sending(ThreadWrite::Reply))
                                    .disabled(blocked.is_some() || busy)
                                    .when_some(blocked, |this, blocked| {
                                        this.tooltip(Tooltip::text(blocked))
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.send_reply(window, cx)
                                    })),
                            ),
                    )
                    .into_any_element(),
            );
        }

        if !permissions.reply && !can_resolve {
            return None;
        }
        Some(
            h_flex()
                .px_3()
                .py_1()
                .gap_1()
                .border_t(tokens::BORDER_WIDTH)
                .border_color(colors.border_variant)
                .children(
                    self.error_for(ThreadWrite::Resolve)
                        .or(self.error_for(ThreadWrite::Reply))
                        .map(error_label),
                )
                .child(div().flex_1())
                .when(can_resolve, |this| {
                    this.child(
                        Button::new(
                            "resolve-thread",
                            if resolved { "Unresolve" } else { "Resolve" },
                        )
                        .style(ButtonStyle::Subtle)
                        .size(ButtonSize::Compact)
                        .label_size(LabelSize::Small)
                        .loading(self.is_sending(ThreadWrite::Resolve))
                        .disabled(blocked.is_some() || busy)
                        .when_some(blocked.clone(), |this, blocked| {
                            this.tooltip(Tooltip::text(blocked))
                        })
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.set_resolved(!resolved, cx)),
                        ),
                    )
                })
                .when(permissions.reply, |this| {
                    this.child(
                        Button::new("reply-thread", "Reply")
                            .style(ButtonStyle::Outlined)
                            .size(ButtonSize::Compact)
                            .label_size(LabelSize::Small)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.start_reply(window, cx)),
                            ),
                    )
                })
                .into_any_element(),
        )
    }
}

fn comment_bodies(
    thread: &ReviewThread,
    language_registry: Option<Arc<LanguageRegistry>>,
    cx: &mut App,
) -> Vec<Entity<Markdown>> {
    thread
        .comments
        .iter()
        .map(|comment| {
            let body = SharedString::from(comment_markdown(&comment.body));
            let language_registry = language_registry.clone();
            cx.new(|cx| Markdown::new(body, language_registry, None, cx))
        })
        .collect()
}

impl Render for ThreadCard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let thread = self.thread.clone();
        let expanded = self.expanded;
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
        let actions = if expanded {
            self.render_actions(cx)
        } else {
            None
        };
        let colors = cx.theme().colors();
        let status = cx.theme().status();
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
                this.child(pill("Outdated", status.warning, cx))
            })
            .when(thread.is_resolved, |this| {
                this.child(pill("Resolved", status.success, cx))
            })
            .when(thread.sent_to_agent, |this| {
                this.child(pill(
                    "Sent to agent",
                    mustr_ui::palette(cx).agent.hsla(),
                    cx,
                ))
            })
            .when(!expanded && self.reply.is_some(), |this| {
                this.child(pill("Reply in progress", colors.text_accent, cx))
            })
            .child(
                Label::new(count_label)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );

        v_flex()
            .w_full()
            .rounded(ui_scaled(tokens::CORNER_RADIUS, cx))
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
            .children(actions)
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
            .rounded(ui_scaled(tokens::CORNER_RADIUS, cx))
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

pub(crate) fn comment_markdown_style(window: &Window, cx: &App) -> MarkdownStyle {
    let mut style = editor::hover_markdown_style(window, cx);
    style.base_text_style.refine(&TextStyleRefinement {
        font_size: Some(ui_scaled(tokens::TEXT_SIZE_LARGE, cx).into()),
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
            can_resolve: false,
            comments: vec![ThreadComment {
                id: "c".to_string(),
                author: None,
                body: "one\ntwo".to_string(),
                created_at: None,
                edited: false,
                can_edit: false,
            }],
            sent_to_agent: false,
            comment_count: 1,
        };
        assert_eq!(estimated_card_lines(&thread, false), 2);
        assert_eq!(estimated_card_lines(&thread, true), 6);
    }
}
