//! Review: the Review page's sections of pull requests, each a name and a GitHub-style query
//! (`review_sections.rs`, mustr's `"review": {"sections"}` setting): add, edit, reorder, remove,
//! and where each lists from (GitHub's search, or T3's projects).

use super::rows::{Group, SettingRow};
use crate::review_sections::{
    DialogFocus, ProjectRepositories, ReviewSettings, SectionDialog, default_sections,
    move_section, remove_section, section_source, set_sections,
};
use gpui::{App, ParentElement as _, SharedString, Styled as _, Window};
use mustr_ui::{ButtonStyle, TextButton};
use ui::{IconButton, IconName, IconSize, Tooltip, prelude::*};
use workspace::Workspace;

pub(super) fn groups(cx: &mut App) -> Vec<Group> {
    let sections = ReviewSettings::get(cx).sections;
    let repositories = ProjectRepositories::get(cx);
    let last = sections.len().saturating_sub(1);
    let mut rows = sections
        .iter()
        .enumerate()
        .map(|(index, section)| {
            let buttons = h_flex()
                .gap_1()
                .child(move_button(index, -1, index == 0))
                .child(move_button(index, 1, index == last))
                .child(
                    TextButton::new(
                        SharedString::from(format!("settings-review-edit-{index}")),
                        "Edit",
                    )
                    .style(ButtonStyle::Default)
                    .on_click(move |_, window, cx| {
                        edit(Some(index), DialogFocus::Query, window, cx)
                    }),
                )
                .child(
                    TextButton::new(
                        SharedString::from(format!("settings-review-remove-{index}")),
                        "Remove",
                    )
                    .style(ButtonStyle::Default)
                    .on_click(move |_, _, cx| remove_section(index, cx)),
                );
            let source = section_source(section, &repositories);
            SettingRow::new(
                SharedString::from(format!("settings-row-review-section-{index}")),
                section.name.clone(),
                format!("{} · {}", section.query, source.label()),
                buttons,
            )
            .keywords("review pull requests section query github search reorder move")
        })
        .collect::<Vec<_>>();
    rows.push(
        SettingRow::new(
            "settings-row-review-add-section",
            "Add a section",
            "A name, and a query in GitHub's search syntax, such as \
             is:open is:pr review-requested:@me. Queries search all of GitHub with the GitHub \
             CLI's sign-in; one can list only T3's projects instead. The Review page's section \
             menus edit them too.",
            h_flex()
                .gap_1()
                .child(
                    TextButton::new("settings-review-add", "Add section")
                        .style(ButtonStyle::Default)
                        .on_click(|_, window, cx| edit(None, DialogFocus::Name, window, cx)),
                )
                .child(
                    TextButton::new("settings-review-reset", "Use the defaults")
                        .style(ButtonStyle::Default)
                        .on_click(|_, _, cx| set_sections(default_sections(), cx)),
                ),
        )
        .keywords("review pull requests section query add default reset github"),
    );
    vec![Group::new("Pull request sections", rows)]
}

/// Moves section `index` up (`-1`) or down (`1`).
fn move_button(index: usize, offset: isize, disabled: bool) -> impl IntoElement {
    let (direction, icon, tooltip) = if offset < 0 {
        ("up", IconName::ArrowUp, "Move up")
    } else {
        ("down", IconName::ArrowDown, "Move down")
    };
    let id = format!("settings-review-{direction}-{index}");
    div().debug_selector(move || id.clone()).child(
        IconButton::new(
            SharedString::from(format!("settings-review-{direction}-{index}")),
            icon,
        )
        .icon_size(IconSize::Small)
        .disabled(disabled)
        .tooltip(Tooltip::text(tooltip))
        .on_click(move |_, _, cx| move_section(index, offset, cx)),
    )
}

fn edit(index: Option<usize>, focus: DialogFocus, window: &mut Window, cx: &mut App) {
    if let Some(workspace) = Workspace::for_window(window, cx) {
        workspace.update(cx, |workspace, cx| {
            SectionDialog::open(index, focus, workspace, window, cx)
        });
    }
}
