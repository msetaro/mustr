//! Agents and threads: mustr's own thread settings (follow-ups, notifications, the Dock's count),
//! a link to the Agents page, and, under a heading naming the server, T3 Code's settings and its
//! worktree cleanup.

use super::{
    Found, Section, SettingsView, platform_keys,
    rows::{self, Group, Query, SettingRow},
};
use crate::notifications::{self, NotificationSettings};
use gpui::{Action as _, AnyElement, App, Context, Window};
use thread_chat::thread_settings::FollowUpBehavior;

pub(super) fn render(
    view: &mut SettingsView,
    query: &Query,
    window: &mut Window,
    cx: &mut Context<SettingsView>,
) -> Found {
    let section = Section::Agents.title();
    let mut found = Found::default();
    for group in [threads(cx), notification_settings(cx)] {
        if let Some(group) = group.filter(section, query) {
            found.count += group.rows().len();
            found.elements.push(group.render(cx));
        }
    }

    let t3_groups = view.t3.update(cx, |t3, cx| t3.groups(window, cx));
    let cleanup = Group::new(
        "",
        vec![
            SettingRow::whole(
                "settings-worktree-cleanup",
                "Automatic worktree cleanup",
                view.cleanup.clone(),
            )
            .keywords("worktrees remove delete storage disk"),
        ],
    )
    .bare();
    // "T3" in the search finds all of T3's settings.
    let t3_query = if !query.is_empty() && query.matches(&["t3 code"]) && query.matches(&["t3"]) {
        Query::default()
    } else {
        query.clone()
    };
    let t3 = t3_groups
        .into_iter()
        .chain([cleanup])
        .filter_map(|group| group.filter(section, &t3_query))
        .collect::<Vec<_>>();
    if !t3.is_empty() {
        found.elements.push(view.t3.read(cx).render_header(cx));
        for group in t3 {
            found.count += group.rows().len();
            found.elements.push(group.render(cx));
        }
    }
    found
}

fn threads(cx: &mut App) -> Group {
    let behavior = FollowUpBehavior::get(cx);
    let follow_ups = rows::segmented(
        "settings-follow-ups",
        &["Queue", "Steer"],
        match behavior {
            FollowUpBehavior::Queue => 0,
            FollowUpBehavior::Steer => 1,
        },
        None,
        |choice, _, cx| {
            let behavior = if choice == 1 {
                FollowUpBehavior::Steer
            } else {
                FollowUpBehavior::Queue
            };
            behavior.set(cx);
            cx.refresh_windows();
        },
    );
    Group::new(
        "Threads",
        vec![
            SettingRow::new(
                "settings-row-follow-ups",
                "While the agent works",
                format!(
                    "Enter queues a message for after the run, or steers the run with it. {} \
                     does the other.",
                    platform_keys("⌘⇧Enter", "Ctrl-Shift-Enter")
                ),
                follow_ups,
            )
            .keywords("follow-ups follow up queue steer enter send"),
            SettingRow::new(
                "settings-row-agents",
                "Agents",
                "Install the agents T3 Code runs, sign in to them, and add accounts.",
                rows::button("settings-open-agents", "Open Agents", None, |window, cx| {
                    window.dispatch_action(crate::agent_setup::OpenAgents.boxed_clone(), cx)
                }),
            )
            .keywords("claude codex install sign in login accounts providers models"),
        ],
    )
    .note("Kept by mustr")
}

fn notification_settings(cx: &mut App) -> Group {
    let settings = NotificationSettings::get(cx);
    let toggle =
        |id: &'static str, on: bool, set: fn(&mut NotificationSettings, bool)| -> AnyElement {
            rows::switch(id, on, None, move |on, _, cx| {
                let mut settings = NotificationSettings::get(cx);
                set(&mut settings, on);
                notifications::set_settings(settings, cx);
                cx.refresh_windows();
            })
        };
    Group::new(
        "Notifications",
        vec![
            SettingRow::new(
                "settings-row-notify-completed",
                "Thread completed",
                "An agent finished its run.",
                toggle(
                    "settings-notify-completed",
                    settings.completed,
                    |settings, on| settings.completed = on,
                ),
            )
            .keywords("notify done finished"),
            SettingRow::new(
                "settings-row-notify-approval",
                "Approval needed",
                "An agent asks to run a command, change files, or use a tool.",
                toggle(
                    "settings-notify-approval",
                    settings.needs_approval,
                    |settings, on| settings.needs_approval = on,
                ),
            )
            .keywords("notify permission request"),
            SettingRow::new(
                "settings-row-notify-input",
                "Input needed",
                "An agent asks you a question, or for a secret.",
                toggle(
                    "settings-notify-input",
                    settings.needs_input,
                    |settings, on| settings.needs_input = on,
                ),
            )
            .keywords("notify question"),
            SettingRow::new(
                "settings-row-notify-failed",
                "Thread failed",
                "An agent's run ended with an error.",
                toggle("settings-notify-failed", settings.failed, |settings, on| {
                    settings.failed = on
                }),
            )
            .keywords("notify error"),
            SettingRow::new(
                "settings-row-notify-usage-limit",
                "Usage limit reached",
                "An agent stopped at its plan's usage limit.",
                toggle(
                    "settings-notify-usage-limit",
                    settings.usage_limit,
                    |settings, on| settings.usage_limit = on,
                ),
            )
            .keywords("notify quota rate limit"),
            SettingRow::new(
                "settings-row-notify-sound",
                "Sound",
                "Notifications play a sound, on macOS.",
                toggle("settings-notify-sound", settings.sound, |settings, on| {
                    settings.sound = on
                }),
            )
            .keywords("notify audio chime"),
            SettingRow::new(
                "settings-row-dock-badge",
                "Count on the Dock",
                "mustr's Dock icon shows how many threads wait on you, on macOS.",
                toggle(
                    "settings-dock-badge",
                    settings.dock_badge,
                    |settings, on| settings.dock_badge = on,
                ),
            )
            .keywords("badge notify waiting"),
        ],
    )
    .note("When you're looking elsewhere")
}
