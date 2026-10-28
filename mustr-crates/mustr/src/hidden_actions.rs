//! Zed's AI and hosted-service actions, which mustr hides from the command palette and never
//! binds to keys (mustr-docs/ZED-SERVICES.md).
//!
//! mustr's agent features are its own (`thread_chat`, `review`), backed by T3 Code. Zed's AI
//! (the agent panel and its threads, external agents, the inline assistant, edit predictions,
//! Copilot, the threads sidebar, skills, MCP servers) and Zed's services (sign-in, collab,
//! auto-update, feedback, extensions, remote servers) aren't initialized. Their actions are still
//! compiled in, because Zed's core crates define them, and some still get handlers from those
//! crates (`editor::ShowEditPrediction`, `workspace::ToggleEditPrediction`,
//! `git::GenerateCommitMessage`). Hiding them by name keeps them out of the palette, vim's `:`
//! commands, and the keymap, whichever crate defines them.

use command_palette_hooks::{
    CommandInterceptResult, CommandPaletteFilter, GlobalCommandPaletteInterceptor,
};
use gpui::App;
use settings::{Settings as _, SettingsStore};
use std::any::TypeId;
use vim_mode_setting::{HelixModeSetting, VimModeSetting};

/// Namespaces that hold only Zed AI actions.
const AI_NAMESPACES: &[&str] = &[
    "acp",
    "agent",
    "agents",
    "agents_sidebar",
    "assistant",
    "assistant2",
    "context_server",
    "copilot",
    "edit_prediction",
    "inline_assistant",
    "multi_workspace",
    "skill_creator",
    "supermaven",
    "zed_predict_onboarding",
    "zeta",
];

/// Zed AI actions in namespaces that also hold actions mustr keeps.
const AI_ACTIONS: &[&str] = &[
    "editor::AcceptEditPrediction",
    "editor::AcceptNextLineEditPrediction",
    "editor::AcceptNextWordEditPrediction",
    "editor::NextEditPrediction",
    "editor::PreviousEditPrediction",
    "editor::ShowEditPrediction",
    "editor::ToggleEditPrediction",
    // Zed's diff review, which sends review comments to Zed's agent. mustr's reviews are its own.
    "editor::CancelEditReviewComment",
    "editor::ConfirmEditReviewComment",
    "editor::DeleteReviewComment",
    "editor::EditReviewComment",
    "editor::SendReviewToAgent",
    "editor::SubmitDiffReviewComment",
    "editor::ToggleReviewCommentsExpanded",
    "git::GenerateCommitMessage",
    "git::ReviewDiff",
    "workspace::ToggleEditPrediction",
    "zed::AcpRegistry",
];

/// Namespaces that hold only actions for Zed's hosted services.
const SERVICE_NAMESPACES: &[&str] = &[
    "auto_update",
    "call",
    "channel_modal",
    "cli",
    "client",
    "collab",
    "collab_panel",
    "feedback",
    "notification_panel",
    "remote_debug",
];

/// Actions for Zed's hosted services, Zed's site, and Zed's onboarding, in namespaces that also
/// hold actions mustr keeps.
const SERVICE_ACTIONS: &[&str] = &[
    "projects::OpenDevContainer",
    "projects::OpenFolderInWsl",
    "projects::OpenRemote",
    "projects::OpenWsl",
    "workspace::Feedback",
    "workspace::FollowNextCollaborator",
    "zed::Extensions",
    "zed::GetMerch",
    "zed::OpenAccountSettings",
    "zed::OpenDocs",
    "zed::OpenOnboarding",
    "zed::OpenServerSettings",
    "zed::OpenStatusPage",
    "zed::OpenTelemetryLog",
    "zed::OpenZedUrl",
    "zed::ShowUpdateNotification",
    "zed::ShowWelcome",
];

/// Whether mustr hides the action with this (canonical) name.
pub fn is_hidden(action_name: &str) -> bool {
    let namespace = action_name.split("::").next().unwrap_or_default();
    AI_NAMESPACES.contains(&namespace)
        || SERVICE_NAMESPACES.contains(&namespace)
        || AI_ACTIONS.contains(&action_name)
        || SERVICE_ACTIONS.contains(&action_name)
}

/// Hides the actions from the command palette, including vim's `:` commands. Call it after
/// `command_palette::init`, which installs the filter, and after `vim::init`.
pub fn init(cx: &mut App) {
    // Every compiled-in action, including those that take arguments and so can't be built by
    // name.
    let action_types: Vec<TypeId> = gpui::generate_list_of_all_registered_actions()
        .filter(|action| is_hidden(action.name))
        .map(|action| action.type_id)
        .collect();
    CommandPaletteFilter::update_global(cx, |filter, _| {
        for namespace in AI_NAMESPACES.iter().chain(SERVICE_NAMESPACES) {
            filter.hide_namespace(namespace);
        }
        filter.hide_action_types(&action_types);
    });
    filter_vim_commands(cx);
}

/// The command palette shows vim's `:` commands without consulting [`CommandPaletteFilter`], and
/// vim maps some of them to hidden actions (`:AI` opens Zed's agent panel). While vim mode is on,
/// mustr wraps vim's interceptor and drops those results.
fn filter_vim_commands(cx: &mut App) {
    let mut wrapped = false;
    // Vim installs its interceptor from its own settings observer, which `vim::init` registers
    // first, so this one sees it.
    cx.observe_global::<SettingsStore>(move |cx| {
        if !(VimModeSetting::get_global(cx).0 || HelixModeSetting::get_global(cx).0) {
            wrapped = false;
            return;
        }
        if wrapped {
            return;
        }
        let Some(vim_interceptor) = cx.try_global::<GlobalCommandPaletteInterceptor>().cloned()
        else {
            return;
        };
        wrapped = true;
        GlobalCommandPaletteInterceptor::set(cx, move |query, workspace, cx| {
            // `intercept` runs whichever interceptor is installed, so run vim's in place of this
            // one for the call.
            let filtered = cx.remove_global::<GlobalCommandPaletteInterceptor>();
            cx.set_global(vim_interceptor.clone());
            let results = GlobalCommandPaletteInterceptor::intercept(query, workspace, cx);
            cx.set_global(filtered);
            cx.spawn(async move |_| {
                let mut results = match results {
                    Some(results) => results.await,
                    None => CommandInterceptResult::default(),
                };
                results
                    .results
                    .retain(|result| !is_hidden(result.action.name()));
                results
            })
        });
    })
    .detach();
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use gpui::{
        AppContext as _, Focusable as _, TestAppContext, UpdateGlobal as _, VisualTestContext,
    };

    /// mustr's own action namespaces. Their agent-thread actions are T3's agents, not Zed's.
    const MUSTR_NAMESPACES: &[&str] = &["mustr", "review", "thread_chat"];

    /// Actions whose names look like AI or a Zed service but aren't.
    const NOT_AI_OR_SERVICES: &[&str] = &[
        // Opens the language server's documentation for the symbol under the cursor.
        "editor::OpenDocs",
        // Git remotes.
        "branch_picker::ShowRemoteBranches",
        "git::CreateRemote",
        "git::FilterRemotes",
        // Zed's git panel onboarding, which mustr doesn't show (it has no handler).
        "git_onboarding::OpenGitIntegrationOnboarding",
    ];

    /// Whether an action looks like Zed AI or a Zed service, by its name. This catches actions
    /// that Zed adds upstream, so a sync can't bring one into mustr's palette unnoticed.
    pub(crate) fn looks_like_ai_or_a_zed_service(action_name: &str) -> bool {
        const KEYWORDS: &[&str] = &[
            // Zed AI.
            "Acp",
            "Agent",
            "AGENTS",
            "Assist",
            "Copilot",
            "EditPrediction",
            "LanguageModel",
            "ModelSelector",
            "Predict",
            "Prompt",
            "Skill",
            "Supermaven",
            "Thread",
            "Zeta",
            "CommitMessage",
            "ReviewComment",
            "ReviewDiff",
            // Zed's services and site.
            "Account",
            "BugReport",
            "Channel",
            "Collab",
            "Docs",
            "EmailZed",
            "Extensions",
            "Feedback",
            "Merch",
            "Onboarding",
            "ReleaseNotes",
            "Reconnect",
            "RequestFeature",
            "ServerSettings",
            "SignIn",
            "SignOut",
            "StatusPage",
            "Telemetry",
            "Update",
            "Welcome",
            "ZedUrl",
            // Remote projects, whose server mustr would have to download from Zed.
            "DevContainer",
            "Remote",
            "Wsl",
        ];
        let (namespace, name) = action_name.split_once("::").unwrap_or(("", action_name));
        if MUSTR_NAMESPACES.contains(&namespace) || NOT_AI_OR_SERVICES.contains(&action_name) {
            return false;
        }
        AI_NAMESPACES.contains(&namespace)
            || SERVICE_NAMESPACES.contains(&namespace)
            || KEYWORDS.iter().any(|keyword| name.contains(keyword))
    }

    #[gpui::test]
    fn every_ai_and_service_action_is_hidden_from_the_palette(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            command_palette_hooks::init(cx);
            init(cx);
            let filter = CommandPaletteFilter::try_global(cx).unwrap();

            // Every action compiled into mustr, so this also covers Zed's AI actions in crates
            // that mustr links but doesn't initialize.
            let mut checked = 0;
            for name in cx.all_action_names().to_vec() {
                // A deprecated alias builds the action it names.
                let action = cx.build_action(name, None).ok();
                let canonical = action.as_ref().map_or(name, |action| action.name());
                if looks_like_ai_or_a_zed_service(name) || looks_like_ai_or_a_zed_service(canonical)
                {
                    checked += 1;
                    assert!(
                        is_hidden(canonical),
                        "{name} looks like Zed AI or a Zed service; hide it in hidden_actions.rs \
                         or list it in NOT_AI_OR_SERVICES"
                    );
                    if let Some(action) = &action {
                        assert!(filter.is_hidden(&**action), "the palette shows {name}");
                    }
                } else {
                    assert!(!is_hidden(canonical), "{name} is hidden by mistake");
                }
            }
            assert!(checked > 100, "only {checked} AI and service actions found");

            // Each action hidden by name still exists, so a rename upstream doesn't unhide it.
            let registered: Vec<_> = gpui::generate_list_of_all_registered_actions()
                .map(|action| action.name)
                .collect();
            for name in AI_ACTIONS.iter().chain(SERVICE_ACTIONS) {
                assert!(registered.contains(name), "{name} no longer exists");
            }

            // mustr's own agent features stay.
            for name in [
                "thread_chat::NewThread",
                "thread_chat::SendMessage",
                "review::ToggleFileTree",
            ] {
                let action = cx.build_action(name, None).unwrap();
                assert!(!filter.is_hidden(&*action), "the palette hides {name}");
            }
        });
    }

    /// What the command palette lists (`CommandPalette::new`, crates/command_palette): the
    /// focused editor's available actions that the filter doesn't hide.
    #[gpui::test]
    async fn the_palette_lists_no_ai_or_service_actions(cx: &mut TestAppContext) {
        let app_state = cx.update(|cx| {
            let app_state = workspace::AppState::test(cx);
            crate::app_settings::apply_default_overrides(cx).unwrap();
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            command_palette::init(cx);
            editor::init(cx);
            workspace::init(app_state.clone(), cx);
            search::init(cx);
            go_to_line::init(cx);
            git_ui::init(cx);
            vim::init(cx);
            init(cx);
            app_state
        });
        let project = project::Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        let editor = workspace.update_in(cx, |workspace, window, cx| {
            let editor = cx.new(|cx| editor::Editor::multi_line(window, cx));
            workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
            editor
        });
        editor.update_in(cx, |editor, window, cx| {
            window.focus(&editor.focus_handle(cx), cx)
        });
        cx.run_until_parked();

        let (available, listed) = palette_actions(cx);
        // Zed's crates do register AI actions on the editor and the workspace.
        for name in [
            "editor::ShowEditPrediction",
            "workspace::ToggleEditPrediction",
            // `AppState::test` runs `client::init`, which mustr doesn't.
            "client::SignIn",
        ] {
            assert!(
                available.iter().any(|a| a == name),
                "{name} isn't available"
            );
        }
        let leaked: Vec<_> = listed
            .iter()
            .filter(|name| looks_like_ai_or_a_zed_service(name))
            .collect();
        assert!(leaked.is_empty(), "the palette lists {leaked:?}");
        assert!(listed.iter().any(|name| name == "editor::Copy"));
    }

    fn palette_actions(cx: &mut VisualTestContext) -> (Vec<String>, Vec<String>) {
        cx.update(|window, cx| {
            let filter = CommandPaletteFilter::try_global(cx).unwrap();
            let available = window.available_actions(cx);
            let listed = available
                .iter()
                .filter(|action| !filter.is_hidden(&***action))
                .map(|action| action.name().to_string())
                .collect();
            let available = available
                .iter()
                .map(|action| action.name().to_string())
                .collect();
            (available, listed)
        })
    }

    #[gpui::test]
    async fn vim_commands_dont_open_zeds_agent(cx: &mut TestAppContext) {
        let app_state = cx.update(|cx| {
            let app_state = workspace::AppState::test(cx);
            crate::app_settings::apply_default_overrides(cx).unwrap();
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            command_palette::init(cx);
            editor::init(cx);
            workspace::init(app_state.clone(), cx);
            vim::init(cx);
            app_state
        });
        cx.update(|cx| set_vim_mode(true, cx));
        let project = project::Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());

        // Vim maps `:AI` to Zed's agent panel.
        assert_eq!(
            intercept(":AI", &workspace, cx).await,
            ["agent::ToggleFocus"]
        );

        // As in mustr, where vim mode turns on after `init` runs.
        cx.update(|_, cx| {
            init(cx);
            set_vim_mode(false, cx);
            set_vim_mode(true, cx);
        });
        assert_eq!(intercept(":AI", &workspace, cx).await, Vec::<String>::new());
        // Vim's other commands still work.
        assert_eq!(intercept(":w", &workspace, cx).await, ["vim::VimSave"]);
    }

    fn set_vim_mode(vim_mode: bool, cx: &mut App) {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| settings.vim_mode = Some(vim_mode));
        });
    }

    async fn intercept(
        query: &str,
        workspace: &gpui::Entity<workspace::Workspace>,
        cx: &mut VisualTestContext,
    ) -> Vec<String> {
        let task = cx.update(|_, cx| {
            GlobalCommandPaletteInterceptor::intercept(query, workspace.downgrade(), cx)
        });
        let results = match task {
            Some(task) => task.await,
            None => CommandInterceptResult::default(),
        };
        results
            .results
            .iter()
            .map(|result| result.action.name().to_string())
            .collect()
    }
}
