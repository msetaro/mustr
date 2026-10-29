//! Agents' icons and T3's subagent badge.
//!
//! T3 draws no avatar for a thread's own agent or for the user. It shows one only for a
//! subagent: a small round badge with the subagent's provider icon, or a bot, and a dot at its
//! bottom right in the subagent's status color (`SubagentAvatar` and `subagentStatusVisual` in
//! T3's `chat/V2LifecycleRow.tsx`). The timeline's subagent rows, the subagents drawer, and
//! workflow rows use it, and the sidebar's rows show the provider icon alone.

use gpui::{AnyElement, App, Hsla, IntoElement, ParentElement as _, Styled as _, div, px, rgb};
use mustr_ui::{MustrIcon, palette, tokens::scaled};
use ui::{Color, Icon, IconName, IconSize, prelude::*};

/// An agent's icon by its driver, as the agents page and the model pickers show it, or `None`
/// for a driver with no icon of its own.
pub fn provider_icon(driver: &str) -> Option<IconName> {
    match driver {
        "claudeAgent" => Some(IconName::AiClaude),
        "codex" => Some(IconName::AiOpenAi),
        "antigravity" => Some(IconName::AiGoogle),
        "cursor" => Some(IconName::EditorCursor),
        "opencode" | "openCode" => Some(IconName::AiOpenCode),
        _ => None,
    }
}

/// The driver of a provider instance, from T3's config, or the instance id itself, which for
/// the built-in agents is their driver ("claudeAgent", "codex").
pub fn instance_driver(instance_id: &str, cx: &App) -> String {
    crate::usage::ServerConfigs::try_global(cx)
        .and_then(|configs| {
            configs
                .read(cx)
                .provider(instance_id)
                .map(|provider| provider.driver.clone())
        })
        .unwrap_or_else(|| instance_id.to_string())
}

/// An agent's brand color, where T3 gives it one (`PROVIDER_TEXT_COLOR_BY_PROVIDER`): Claude's
/// orange and Antigravity's blue. Others take the text color.
pub fn provider_color(driver: &str, cx: &App) -> Hsla {
    match driver {
        "claudeAgent" => rgb(0xd97757).into(),
        "antigravity" => rgb(0x5b87bf).into(),
        _ => palette(cx).text.hsla(),
    }
}

/// An agent's icon in its color, or a bot for one with no icon.
pub fn provider_icon_element(driver: &str, size: IconSize, cx: &App) -> Icon {
    match provider_icon(driver) {
        Some(icon) => Icon::new(icon)
            .size(size)
            .color(Color::Custom(provider_color(driver, cx))),
        None => MustrIcon::Bot.icon().size(size).color(Color::Muted),
    }
}

/// How T3 words a subagent's status (`STATUS_VISUALS` in `chat/V2LifecycleRow.tsx`).
pub fn subagent_status_label(status: &str) -> &'static str {
    match status {
        "pending" | "queued" | "preparing" | "starting" => "Queued",
        "running" => "Running",
        "waiting" => "Waiting",
        "idle" => "Idle · resumable",
        "completed" => "Completed",
        "failed" | "error" => "Failed",
        _ => "Stopped",
    }
}

/// A subagent's status dot, in T3's colors: blue while it works, green when it completed, red
/// when it failed, and muted when it's idle or stopped.
pub fn subagent_status_color(status: &str, cx: &App) -> Hsla {
    let palette = palette(cx);
    match status {
        "pending" | "queued" | "preparing" | "starting" | "running" | "waiting" => {
            palette.accent.hsla()
        }
        "completed" => palette.ok.hsla(),
        "failed" | "error" => cx.theme().status().error,
        "idle" => palette.text_muted.hsla().opacity(0.5),
        _ => palette.text_muted.hsla().opacity(0.6),
    }
}

/// Whether a subagent has settled: completed, failed, or stopped.
pub fn subagent_settled(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "error" | "cancelled" | "interrupted" | "stopped" | "rolled_back"
    )
}

/// T3's subagent badge: a 24px circle with the agent's icon and a status dot. `selector` names
/// it for tests, and its dot `<selector>-<status>`.
pub fn subagent_badge(
    selector: String,
    driver: Option<&str>,
    status: Option<&str>,
    cx: &App,
) -> AnyElement {
    let palette = palette(cx);
    let icon = match driver {
        Some(driver) => provider_icon_element(driver, IconSize::Small, cx),
        None => MustrIcon::Bot
            .icon()
            .size(IconSize::Small)
            .color(Color::Muted),
    };
    let dot = status.map(|status| {
        let dot_selector = format!("{selector}-{status}");
        div()
            .debug_selector(move || dot_selector)
            .absolute()
            .right(px(-1.))
            .bottom(px(-1.))
            .size(scaled(px(9.)))
            .rounded_full()
            .border_2()
            .border_color(palette.background.hsla())
            .bg(subagent_status_color(status, cx))
    });
    div()
        .debug_selector(move || selector)
        .relative()
        .flex_none()
        .size(scaled(px(22.)))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .border_1()
        .border_color(palette.border.hsla())
        .bg(palette.surface_2.hsla())
        .child(icon)
        .children(dot)
        .into_any_element()
}

/// How long a subagent has run, or ran, as T3's `AgentElapsed` shows it: "12s", "3m", "1h 4m".
pub fn elapsed_label(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        let minutes = seconds % 3600 / 60;
        if minutes == 0 {
            format!("{}h", seconds / 3600)
        } else {
            format!("{}h {minutes}m", seconds / 3600)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subagent_statuses_read_as_t3s() {
        assert_eq!(subagent_status_label("pending"), "Queued");
        assert_eq!(subagent_status_label("running"), "Running");
        assert_eq!(subagent_status_label("idle"), "Idle · resumable");
        assert_eq!(subagent_status_label("cancelled"), "Stopped");
        assert_eq!(subagent_status_label("interrupted"), "Stopped");
        assert!(subagent_settled("failed"));
        assert!(!subagent_settled("waiting"));
        assert_eq!(provider_icon("claudeAgent"), Some(IconName::AiClaude));
        assert_eq!(provider_icon("codex"), Some(IconName::AiOpenAi));
        assert_eq!(provider_icon("someAcpAgent"), None);
        assert_eq!(elapsed_label(4), "4s");
        assert_eq!(elapsed_label(61), "1m");
        assert_eq!(elapsed_label(3600), "1h");
        assert_eq!(elapsed_label(3720), "1h 2m");
    }
}
