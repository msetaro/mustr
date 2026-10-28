//! A dock panel with the file tree of the active review: files grouped by directory, their
//! change and line counts, and viewed marks. Clicking a file scrolls the review to it.

use crate::{
    ReviewView,
    file_tree::{FileChange, FileTreeFile, FileTreeRow, FileViewedState},
};
use gpui::{
    Action, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement,
    ParentElement as _, Pixels, Render, SharedString, Styled as _, Subscription,
    UniformListScrollHandle, WeakEntity, Window, actions, px, uniform_list,
};
use mustr_ui::{tokens, tokens::scaled};
use std::ops::Range;
use ui::{
    Checkbox, Color, DiffStat, Icon, IconName, IconSize, Label, LabelCommon as _, LabelSize,
    ToggleState, Tooltip, prelude::*,
};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    review,
    [
        /// Shows or hides the review's file tree.
        ToggleFileTree
    ]
);

const ROW_HEIGHT: Pixels = px(24.);

/// A file's viewed checkbox, and whether it can be clicked.
struct ViewedToggle {
    enabled: bool,
    review: WeakEntity<ReviewView>,
}
const INDENT: Pixels = px(12.);
const DEFAULT_WIDTH: Pixels = px(280.);

/// The file tree of the review that was last active in the workspace.
pub struct FileTreePanel {
    focus_handle: FocusHandle,
    review: Option<WeakEntity<ReviewView>>,
    position: DockPosition,
    selected_file: Option<usize>,
    scroll_handle: UniformListScrollHandle,
    _review_subscription: Option<Subscription>,
    _workspace_subscription: Subscription,
}

impl FileTreePanel {
    /// Doesn't read `workspace`, so it can be called while the workspace is being updated (for
    /// example, from `observe_new`).
    pub fn new(workspace: &Entity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace_subscription = cx.subscribe_in(
            workspace,
            window,
            |this, workspace, event: &workspace::Event, _, cx| match event {
                workspace::Event::ActiveItemChanged => {
                    let review = workspace
                        .read(cx)
                        .active_item(cx)
                        .and_then(|item| item.downcast::<ReviewView>());
                    if let Some(review) = review {
                        this.set_review(&review, cx);
                    }
                }
                workspace::Event::ItemRemoved { .. } => cx.notify(),
                _ => {}
            },
        );
        Self {
            focus_handle: cx.focus_handle(),
            review: None,
            position: DockPosition::Left,
            selected_file: None,
            scroll_handle: UniformListScrollHandle::new(),
            _review_subscription: None,
            _workspace_subscription: workspace_subscription,
        }
    }

    /// Shows `review`'s files.
    pub fn set_review(&mut self, review: &Entity<ReviewView>, cx: &mut Context<Self>) {
        if self
            .review
            .as_ref()
            .is_some_and(|current| current == &review.downgrade())
        {
            return;
        }
        self.review = Some(review.downgrade());
        self.selected_file = None;
        self._review_subscription = Some(cx.observe(review, |_, _, cx| cx.notify()));
        cx.notify();
    }

    pub fn review(&self) -> Option<Entity<ReviewView>> {
        self.review.as_ref()?.upgrade()
    }

    fn open_file(&mut self, file: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(review) = self.review() else {
            return;
        };
        self.selected_file = Some(file);
        let scrolled = review.update(cx, |review, cx| review.scroll_to_file(file, window, cx));
        if scrolled {
            window.focus(&review.focus_handle(cx), cx);
        }
        cx.notify();
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(review) = self.review() else {
            return Vec::new();
        };
        let (can_mark_viewed, viewed_busy) = {
            let review = review.read(cx);
            let busy = (0..review.file_tree().file_count())
                .filter(|file| review.is_marking_viewed(*file))
                .collect::<Vec<_>>();
            (review.can_mark_viewed(), busy)
        };
        let (tracks_viewed, rows) = {
            let tree = review.read(cx).file_tree();
            let rows = tree
                .rows()
                .get(range.clone())
                .unwrap_or_default()
                .iter()
                .map(|row| {
                    let file = match row {
                        FileTreeRow::File { file, .. } => tree.files().get(*file).cloned(),
                        FileTreeRow::Directory { .. } => None,
                    };
                    (row.clone(), file)
                })
                .collect::<Vec<_>>();
            (tree.tracks_viewed(), rows)
        };
        rows.into_iter()
            .zip(range)
            .map(|((row, entry), index)| match (row, entry) {
                (FileTreeRow::Directory { name, depth }, _) => {
                    render_directory(index, name, depth).into_any_element()
                }
                (FileTreeRow::File { file, name, depth }, Some(entry)) => {
                    let viewed = tracks_viewed.then(|| ViewedToggle {
                        enabled: can_mark_viewed && !viewed_busy.contains(&file),
                        review: review.downgrade(),
                    });
                    self.render_file(index, file, &entry, name, depth, viewed, cx)
                        .into_any_element()
                }
                (FileTreeRow::File { .. }, None) => div().into_any_element(),
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_file(
        &self,
        row: usize,
        file: usize,
        entry: &FileTreeFile,
        name: SharedString,
        depth: usize,
        viewed: Option<ViewedToggle>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let status = cx.theme().status();
        let change_color = match entry.change {
            FileChange::Added => status.created,
            FileChange::Deleted => status.deleted,
            FileChange::Modified => status.modified,
            FileChange::Renamed => status.renamed,
        };
        let selected = self.selected_file == Some(file);
        let tooltip: SharedString = match (&entry.unavailable, &entry.old_path) {
            (Some(reason), _) => format!("{}\nNot shown: {reason}", entry.path).into(),
            (None, Some(old_path)) => format!("{}\nRenamed from {old_path}", entry.path).into(),
            (None, None) => format!("{} · {}", entry.path, entry.change.description()).into(),
        };
        let viewed_state = match entry.viewed {
            FileViewedState::Viewed => ToggleState::Selected,
            FileViewedState::Unviewed | FileViewedState::Dismissed => ToggleState::Unselected,
        };

        h_flex()
            .id(("review-file", row))
            .h(scaled(ROW_HEIGHT))
            .w_full()
            .pl(scaled(px(8.) + INDENT * depth as f32))
            .pr_2()
            .gap_1p5()
            .rounded(scaled(tokens::CORNER_RADIUS / 2.))
            .when(selected, |this| this.bg(colors.element_selected))
            .when(entry.unavailable.is_some(), |this| this.opacity(0.5))
            .when(entry.unavailable.is_none(), |this| {
                this.cursor_pointer()
                    .hover(|style| style.bg(colors.element_hover))
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.open_file(file, window, cx)),
                    )
            })
            .tooltip(Tooltip::text(tooltip))
            .when_some(viewed, |this, viewed| {
                let checkbox = Checkbox::new(("review-file-viewed", row), viewed_state);
                this.child(if viewed.enabled {
                    let review = viewed.review;
                    checkbox
                        .tooltip(Tooltip::text(if entry.viewed == FileViewedState::Viewed {
                            "Mark as not viewed"
                        } else {
                            "Mark as viewed"
                        }))
                        .on_click(move |_, _, cx| {
                            review
                                .update(cx, |review, cx| review.toggle_viewed(file, cx))
                                .ok();
                        })
                } else {
                    checkbox.visualization_only(true)
                })
            })
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(name)
                        .size(LabelSize::Small)
                        .truncate()
                        .when(entry.change == FileChange::Deleted, |label| {
                            label.strikethrough()
                        })
                        .when(entry.viewed == FileViewedState::Viewed, |label| {
                            label.color(Color::Muted)
                        }),
                ),
            )
            .when(entry.viewed == FileViewedState::Dismissed, |this| {
                this.child(
                    Icon::new(IconName::Circle)
                        .size(IconSize::XSmall)
                        .color(Color::Warning),
                )
            })
            .when_some(entry.line_counts, |this, (added, removed)| {
                this.child(
                    DiffStat::new(
                        ("review-file-diff-stat", row),
                        added as usize,
                        removed as usize,
                    )
                    .label_size(LabelSize::XSmall),
                )
            })
            .child(
                div().w(scaled(px(10.))).flex_none().child(
                    Label::new(entry.change.letter())
                        .size(LabelSize::XSmall)
                        .weight(FontWeight::SEMIBOLD)
                        .color(Color::Custom(change_color)),
                ),
            )
    }
}

fn render_directory(row: usize, name: SharedString, depth: usize) -> impl IntoElement {
    h_flex()
        .id(("review-directory", row))
        .h(scaled(ROW_HEIGHT))
        .w_full()
        .pl(scaled(px(8.) + INDENT * depth as f32))
        .pr_2()
        .gap_1p5()
        .child(
            Icon::new(IconName::Folder)
                .size(IconSize::Small)
                .color(Color::Muted),
        )
        .child(
            div().flex_1().min_w_0().child(
                Label::new(name)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(),
            ),
        )
}

impl EventEmitter<PanelEvent> for FileTreePanel {}

impl Focusable for FileTreePanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for FileTreePanel {
    fn persistent_name() -> &'static str {
        "Review Files"
    }

    fn panel_key() -> &'static str {
        "MustrReviewFiles"
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, window: &Window, _: &App) -> Pixels {
        scaled(DEFAULT_WIDTH).to_pixels(window.rem_size())
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::ListTree)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Review Files")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFileTree)
    }

    fn activation_priority(&self) -> u32 {
        0
    }
}

impl Render for FileTreePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let review = self.review();
        let (summary, row_count, viewed_error) = match &review {
            Some(review) => {
                let review = review.read(cx);
                let tree = review.file_tree();
                (
                    Some(tree.summary()),
                    tree.rows().len(),
                    review.viewed_error().cloned(),
                )
            }
            None => (None, 0, None),
        };

        v_flex()
            .key_context("MustrReviewFiles")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.panel_background)
            .child(
                h_flex()
                    .flex_none()
                    .h(scaled(px(32.)))
                    .px_3()
                    .gap_2()
                    .border_b(tokens::BORDER_WIDTH)
                    .border_color(colors.border)
                    .child(
                        Label::new("Files")
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .child(div().flex_1())
                    .when_some(summary, |this, summary| {
                        this.child(
                            Label::new(summary)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    }),
            )
            .when_some(viewed_error, |this, error| {
                this.child(
                    h_flex()
                        .px_3()
                        .py_1()
                        .gap_1()
                        .border_b(tokens::BORDER_WIDTH)
                        .border_color(colors.border_variant)
                        .child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::XSmall)
                                .color(Color::Error),
                        )
                        .child(
                            Label::new(error)
                                .size(LabelSize::XSmall)
                                .color(Color::Error),
                        ),
                )
            })
            .map(|this| {
                if review.is_some() {
                    this.child(
                        uniform_list(
                            "review-file-tree",
                            row_count,
                            cx.processor(Self::render_rows),
                        )
                        .flex_1()
                        .px_1()
                        .py_1()
                        .track_scroll(&self.scroll_handle),
                    )
                } else {
                    this.child(
                        div().p_3().child(
                            Label::new("Open a pull request to see its files.")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                    )
                }
            })
    }
}
