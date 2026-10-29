//! Review: the Review page's sections of pull requests, each a name and a GitHub-style query
//! (`review_sections.rs`, mustr's `"review": {"sections"}` setting).

use super::rows::{Group, SettingRow};
use crate::review_sections::{
    DialogFocus, ReviewSettings, SectionDialog, default_sections, remove_section, set_sections,
};
use gpui::{App, ParentElement as _, SharedString, Styled as _, Window};
use mustr_ui::{ButtonStyle, TextButton};
use ui::h_flex;
use workspace::Workspace;

pub(super) fn groups(cx: &mut App) -> Vec<Group> {
    let sections = ReviewSettings::get(cx).sections;
    let mut rows = sections
        .iter()
        .enumerate()
        .map(|(index, section)| {
            let buttons = h_flex()
                .gap_1()
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
            SettingRow::new(
                SharedString::from(format!("settings-row-review-section-{index}")),
                section.name.clone(),
                section.query.clone(),
                buttons,
            )
            .keywords("review pull requests section query github search")
        })
        .collect::<Vec<_>>();
    rows.push(
        SettingRow::new(
            "settings-row-review-add-section",
            "Add a section",
            "A name, and a query in GitHub's search syntax, such as \
             is:open is:pr review-requested:@me. The Review page's section menus edit them too.",
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
        .keywords("review pull requests section query add default reset"),
    );
    vec![Group::new("Pull request sections", rows)]
}

fn edit(index: Option<usize>, focus: DialogFocus, window: &mut Window, cx: &mut App) {
    if let Some(workspace) = Workspace::for_window(window, cx) {
        workspace.update(cx, |workspace, cx| {
            SectionDialog::open(index, focus, workspace, window, cx)
        });
    }
}
