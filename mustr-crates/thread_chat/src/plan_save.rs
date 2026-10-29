//! Save to workspace on a proposed plan, as T3's plan card has it (`chat/ProposedPlanCard.tsx`
//! and `proposedPlan.ts` in T3): the plan goes into a Markdown file in the thread's folder, at a
//! path the user can change, named from the plan's first heading.
//!
//! T3 writes the file on its machine (`projects.writeFile`), which needs `filesystem:write`,
//! a scope mustr doesn't ask for. mustr writes it itself instead, when the thread's folder is on
//! this machine, as it is when T3 runs here; for a folder on T3's machine, Save is off.

use crate::{mentions::Folder, thread_view::ThreadView};
use anyhow::{Context as _, Result, bail};
use editor::Editor;
use gpui::{
    AnyElement, AppContext as _, Context, Entity, IntoElement, ParentElement as _, SharedString,
    Styled as _, Window, div,
};
use mustr_ui::{ButtonStyle, TextButton, palette, tokens, tokens::scaled};
use std::path::{Component, Path, PathBuf};
use ui::{Color, Label, LabelCommon as _, LabelSize, prelude::*};

/// The plan's title: its first heading (`proposedPlanTitle`).
fn plan_title(markdown: &str) -> Option<&str> {
    markdown.lines().find_map(|line| {
        let indent = line.len() - line.trim_start_matches(' ').len();
        if indent > 3 {
            return None;
        }
        let rest = &line[indent..];
        let hashes = rest.len() - rest.trim_start_matches('#').len();
        if !(1..=6).contains(&hashes) {
            return None;
        }
        let after = &rest[hashes..];
        if !after.starts_with(char::is_whitespace) {
            return None;
        }
        Some(after.trim()).filter(|title| !title.is_empty())
    })
}

/// The file a plan is saved as, from its title (`buildProposedPlanMarkdownFilename`).
pub(crate) fn plan_file_name(markdown: &str) -> String {
    let title = plan_title(markdown).unwrap_or("plan").to_lowercase();
    let mut name = String::new();
    let mut dash = false;
    for c in title.chars() {
        if "`'\".,!?()[]{}".contains(c) {
            continue;
        }
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            name.push(c);
            dash = false;
        } else if !dash {
            name.push('-');
            dash = true;
        }
    }
    let name = name.trim_matches('-');
    format!("{}.md", if name.is_empty() { "plan" } else { name })
}

/// Writes a plan into `folder` at `relative`, as T3 writes it (`normalizePlanMarkdownForExport`
/// and `writeFile`): its Markdown with one line break at the end, folders made as needed, and
/// never outside the folder or through a link.
pub(crate) fn save_plan(folder: &Path, relative: &str, markdown: &str) -> Result<PathBuf> {
    let relative = relative.trim();
    if relative.is_empty() {
        bail!("Enter a path in the thread's folder.");
    }
    let relative_path = Path::new(relative);
    if relative_path
        .components()
        .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        bail!("The path has to be inside the thread's folder.");
    }
    let path = folder.join(relative_path);
    if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!("{relative} is a link; save the plan somewhere else.");
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("making the folder for {relative}"))?;
    }
    std::fs::write(&path, format!("{}\n", markdown.trim_end()))
        .with_context(|| format!("writing {relative}"))?;
    Ok(path)
}

/// Saving a plan, on [`crate::composer::ComposerState`].
#[derive(Default)]
pub(crate) struct PlanSaveState {
    /// The plan being saved, by item id, and its path's editor.
    editing: Option<(String, Entity<Editor>)>,
    /// The last path a plan was saved to, which the next save starts from, as T3's does.
    last_path: Option<String>,
    /// Where a plan was saved, by item id, to say so on its card.
    saved: Option<(String, String)>,
}

impl ThreadView {
    /// The thread's folder, when it's on this machine.
    fn local_folder(&self, cx: &mut Context<Self>) -> Option<PathBuf> {
        match self.mention_folder(cx) {
            Folder::Local(folder) => Some(folder),
            _ => None,
        }
    }

    fn start_saving_plan(
        &mut self,
        item_id: String,
        markdown: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = self
            .composer_state
            .plan_save
            .last_path
            .clone()
            .unwrap_or_else(|| plan_file_name(markdown));
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(path, window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
            editor
        });
        window.focus(&gpui::Focusable::focus_handle(&editor, cx), cx);
        self.composer_state.plan_save.editing = Some((item_id, editor));
        self.composer_state.plan_save.saved = None;
        self.list_state.remeasure();
        cx.notify();
    }

    fn finish_saving_plan(&mut self, item_id: &str, markdown: &str, cx: &mut Context<Self>) {
        let Some((_, editor)) = self.composer_state.plan_save.editing.clone() else {
            return;
        };
        let Some(folder) = self.local_folder(cx) else {
            return;
        };
        let relative = editor.read(cx).text(cx).trim().to_string();
        match save_plan(&folder, &relative, markdown) {
            Ok(_) => {
                self.composer_state.plan_save.editing = None;
                self.composer_state.plan_save.last_path = Some(relative.clone());
                self.composer_state.plan_save.saved = Some((item_id.to_string(), relative));
                self.error = None;
            }
            Err(error) => self.error = Some(format!("Couldn't save the plan: {error:#}").into()),
        }
        self.list_state.remeasure();
        cx.notify();
    }

    /// Save to workspace on a plan's card, and the path to save it at while it's being saved.
    pub(crate) fn render_plan_save(
        &mut self,
        item_id: &str,
        markdown: &str,
        cx: &mut Context<Self>,
    ) -> (AnyElement, Option<AnyElement>) {
        let local = self.local_folder(cx).is_some();
        let editing = self
            .composer_state
            .plan_save
            .editing
            .as_ref()
            .filter(|(id, _)| id == item_id)
            .map(|(_, editor)| editor.clone());
        let button = div()
            .debug_selector({
                let item_id = item_id.to_string();
                move || format!("thread-plan-save-{item_id}")
            })
            .child(
                TextButton::new(SharedString::from(format!("plan-save-{item_id}")), "Save")
                    .disabled(!local || editing.is_some())
                    .tooltip(if local {
                        "Save the plan as Markdown in the thread's folder"
                    } else {
                        "The thread's folder is on T3's machine, so mustr can't save there"
                    })
                    .on_click(cx.listener({
                        let item_id = item_id.to_string();
                        let markdown = markdown.to_string();
                        move |this, _, window, cx| {
                            this.start_saving_plan(item_id.clone(), &markdown, window, cx)
                        }
                    })),
            )
            .into_any_element();
        let palette = palette(cx);
        let row = match editing {
            Some(editor) => Some(
                h_flex()
                    .debug_selector({
                        let item_id = item_id.to_string();
                        move || format!("thread-plan-save-path-{item_id}")
                    })
                    .gap_1p5()
                    .child(
                        Label::new("Save to")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .py_0p5()
                            .rounded(scaled(tokens::CONTROL_RADIUS))
                            .border(tokens::BORDER_WIDTH)
                            .border_color(palette.border.hsla())
                            .bg(palette.background.hsla())
                            .child(editor),
                    )
                    .child(
                        div()
                            .debug_selector({
                                let item_id = item_id.to_string();
                                move || format!("thread-plan-save-confirm-{item_id}")
                            })
                            .child(
                                TextButton::new(
                                    SharedString::from(format!("plan-save-confirm-{item_id}")),
                                    "Save",
                                )
                                .style(ButtonStyle::Primary)
                                .on_click(cx.listener({
                                    let item_id = item_id.to_string();
                                    let markdown = markdown.to_string();
                                    move |this, _, _, cx| {
                                        this.finish_saving_plan(&item_id, &markdown, cx)
                                    }
                                })),
                            ),
                    )
                    .child(
                        TextButton::new(
                            SharedString::from(format!("plan-save-cancel-{item_id}")),
                            "Cancel",
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.composer_state.plan_save.editing = None;
                            this.list_state.remeasure();
                            cx.notify();
                        })),
                    )
                    .into_any_element(),
            ),
            None => self
                .composer_state
                .plan_save
                .saved
                .as_ref()
                .filter(|(id, _)| id == item_id)
                .map(|(_, path)| {
                    Label::new(format!("Saved to {path}"))
                        .size(LabelSize::XSmall)
                        .color(Color::Success)
                        .into_any_element()
                }),
        };
        (button, row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn plans_are_named_and_saved_as_t3_saves_them() {
        assert_eq!(
            plan_file_name("Intro\n\n## Add retries (with backoff!) to `post()`\n- step"),
            "add-retries-with-backoff-to-post.md"
        );
        assert_eq!(
            plan_file_name("    # Indented code, not a heading\nNo title"),
            "plan.md"
        );
        assert_eq!(plan_file_name("#NoSpace"), "plan.md");
        assert_eq!(plan_file_name("# Ünïcode only"), "n-code-only.md");

        let folder = std::env::temp_dir().join(format!("mustr-plan-save-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&folder).unwrap();
        let path = save_plan(&folder, "docs/plans/retries.md", "# Retries\n\n- one\n\n\n").unwrap();
        assert_eq!(path, folder.join("docs/plans/retries.md"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# Retries\n\n- one\n"
        );
        for outside in [
            "../escape.md",
            "/tmp/escape.md",
            "docs/../../escape.md",
            "  ",
        ] {
            assert!(save_plan(&folder, outside, "# x").is_err(), "{outside}");
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/tmp/elsewhere.md", folder.join("link.md")).unwrap();
            assert!(save_plan(&folder, "link.md", "# x").is_err());
        }
        std::fs::remove_dir_all(folder).ok();
    }
}
