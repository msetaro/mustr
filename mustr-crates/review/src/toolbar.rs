//! The toolbar above the review's diff: the diff style (unified or split) and collapsing or
//! expanding every file. It replaces the small icon buttons that Zed's buffer search bar shows
//! for diff editors (see `ReviewView::act_as_type`).

use editor::{
    EditorEvent, SplittableEditor, ToggleSplitDiff,
    actions::{FoldAll, UnfoldAll},
};
use gpui::{
    App, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _, Subscription,
    WeakEntity, Window, div, px,
};
use mustr_ui::{Chip, SegmentedControl, tokens};
use settings::DiffViewStyle;
use ui::h_flex;
use workspace::AppState;

pub(crate) struct ReviewToolbar {
    editor: Entity<SplittableEditor>,
    _subscriptions: [Subscription; 2],
}

impl ReviewToolbar {
    pub(crate) fn new(editor: &Entity<SplittableEditor>, cx: &mut Context<Self>) -> Self {
        let rhs_editor = editor.read(cx).rhs_editor().clone();
        Self {
            editor: editor.clone(),
            _subscriptions: [
                cx.observe(editor, |_, _, cx| cx.notify()),
                cx.subscribe(&rhs_editor, |_, _, event: &EditorEvent, cx| {
                    if let EditorEvent::BufferFoldToggled { .. } = event {
                        cx.notify();
                    }
                }),
            ],
        }
    }
}

impl Render for ReviewToolbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = mustr_ui::palette(cx);
        let editor = self.editor.read(cx);
        let is_split = editor.diff_view_style() == DiffViewStyle::Split;
        let any_folded = editor.rhs_editor().read(cx).has_any_buffer_folded(cx);
        let weak_editor = self.editor.downgrade();

        h_flex()
            .flex_none()
            .w_full()
            .gap(px(6.))
            .px(px(10.))
            .py(px(5.))
            .bg(palette.background.hsla())
            .border_b(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .child(
                Chip::new("review-range", "All changes")
                    .accent(true)
                    .tooltip("Showing every change in the pull request"),
            )
            .child(div().flex_1())
            .child(
                SegmentedControl::new("review-diff-style")
                    .segment_with_tooltip("Unified", "One column, with deleted lines inline")
                    .segment_with_tooltip("Split", "The old and new files side by side")
                    .selected(if is_split { 1 } else { 0 })
                    .on_select({
                        let weak_editor = weak_editor.clone();
                        move |index, window, cx| {
                            let style = if index == 1 {
                                DiffViewStyle::Split
                            } else {
                                DiffViewStyle::Unified
                            };
                            set_diff_view_style(&weak_editor, style, window, cx);
                        }
                    }),
            )
            .child(
                Chip::new(
                    "review-fold-all",
                    if any_folded {
                        "Expand all"
                    } else {
                        "Collapse all"
                    },
                )
                .tooltip(if any_folded {
                    "Expand every file"
                } else {
                    "Collapse every file"
                })
                .on_click(move |_, window, cx| {
                    let Some(editor) = weak_editor.upgrade() else {
                        return;
                    };
                    let rhs_editor = editor.read(cx).rhs_editor().clone();
                    rhs_editor.update(cx, |editor, cx| {
                        if editor.has_any_buffer_folded(cx) {
                            editor.unfold_all(&UnfoldAll, window, cx);
                        } else {
                            editor.fold_all(&FoldAll, window, cx);
                        }
                    });
                }),
            )
    }
}

/// Switches the diff style and remembers it, as Zed's diff views do (`DiffStyleControls` in
/// crates/editor/src/split.rs).
fn set_diff_view_style(
    editor: &WeakEntity<SplittableEditor>,
    style: DiffViewStyle,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(editor) = editor.upgrade() else {
        return;
    };
    if let Some(app_state) = AppState::try_global(cx) {
        settings::update_settings_file(app_state.fs.clone(), cx, move |settings, _| {
            settings.editor.diff_view_style = Some(style);
        });
    }
    editor.update(cx, |editor, cx| {
        if editor.diff_view_style() != style {
            editor.toggle_split(&ToggleSplitDiff, window, cx);
        }
    });
}
