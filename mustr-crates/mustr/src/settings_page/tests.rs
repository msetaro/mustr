//! The Settings page in a test workspace, on a fake file system and T3's recording backend: what
//! opens it, what each control saves and where, the search, and the keyboard shortcuts. Nothing
//! here touches a real file or a real T3.

use super::{general::Themes, *};
use crate::{
    app_settings::{INITIAL_SETTINGS, set_value_in_settings_text},
    keymap::INITIAL_KEYMAP,
    notifications::NotificationSettings,
    servers::SavedServers,
    settings_page::key_bindings::{BindingSpec, KeymapChange, remove_action, user_actions},
    t3_settings::tests::{FakeBackend, update},
    window::{OpenKeyBindings, OpenSettings},
};
use collections::HashMap;
use gpui::{Action as _, KeybindingKeystroke, Keystroke, TestAppContext, VisualTestContext};
use pretty_assertions::assert_eq;
use project::FakeFs;
use serde_json::{Value, json};
use settings::{
    KeybindSource, KeymapFile, KeymapFileLoadResult, ThemeAppearanceMode, ThemeName, ThemeSelection,
};
use std::sync::Arc;
use thread_chat::{ServerConfigs, thread_settings::FollowUpBehavior};
use workspace::MultiWorkspace;

struct Page<'a> {
    workspace: Entity<Workspace>,
    fs: Arc<FakeFs>,
    backend: Arc<FakeBackend>,
    cx: &'a mut VisualTestContext,
    _servers: tempfile::TempDir,
}

impl Page<'_> {
    fn view(&mut self) -> Entity<SettingsView> {
        self.workspace.read_with(self.cx, |workspace, cx| {
            workspace
                .active_item(cx)
                .and_then(|item| item.downcast::<SettingsView>())
                .expect("the Settings page is the active item")
        })
    }

    fn section(&mut self) -> Section {
        let view = self.view();
        view.read_with(self.cx, |view, _| view.section())
    }

    fn click(&mut self, selector: &'static str) {
        let bounds = self
            .cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} isn't drawn"));
        self.cx
            .simulate_click(bounds.center(), gpui::Modifiers::none());
        self.cx.run_until_parked();
    }

    fn shows(&mut self, selector: &'static str) -> bool {
        self.cx.debug_bounds(selector).is_some()
    }

    fn open(&mut self, section: Section) {
        let workspace = self.workspace.clone();
        workspace.update_in(self.cx, |workspace, window, cx| {
            show(workspace, section, None, window, cx)
        });
        self.cx.run_until_parked();
    }

    fn search(&mut self, text: &str) {
        let view = self.view();
        view.update_in(self.cx, |view, window, cx| {
            view.search
                .update(cx, |search, cx| search.set_text(text, window, cx))
        });
        self.cx.run_until_parked();
    }

    async fn settings(&mut self) -> Value {
        let text = self.fs.load(paths::settings_file()).await.unwrap();
        settings::parse_json_with_comments(&text).unwrap()
    }

    async fn settings_text(&mut self) -> String {
        self.fs.load(paths::settings_file()).await.unwrap()
    }

    async fn keymap_text(&mut self) -> String {
        self.fs.load(paths::keymap_file()).await.unwrap_or_default()
    }
}

async fn open_page(cx: &mut TestAppContext) -> Page<'_> {
    let backend = FakeBackend::new();
    let servers = tempfile::tempdir().unwrap();
    let app_state = cx.update(|cx| {
        let app_state = workspace::AppState::test(cx);
        crate::app_settings::apply_default_overrides(cx).unwrap();
        editor::init(cx);
        workspace::init(app_state.clone(), cx);
        thread_chat::set_backend(backend.clone(), cx);
        ServerConfigs::global(cx).update(cx, |configs, cx| configs.set_providers(Vec::new(), cx));
        SavedServers::set_global_for_test(servers.path().join("servers.json"), cx);
        crate::window::register_app_actions(cx);
        crate::t3_settings::init(cx);
        crate::zoom::init(cx);
        app_state
    });
    let fs = app_state.fs.as_fake();
    let settings_file = paths::settings_file();
    fs.insert_tree(
        settings_file.parent().unwrap(),
        json!({ settings_file.file_name().unwrap().to_str().unwrap(): INITIAL_SETTINGS }),
    )
    .await;
    let project = project::Project::test(fs.clone(), [], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    cx.simulate_resize(gpui::size(px(1400.), px(2400.)));
    cx.dispatch_action(OpenSettings);
    cx.run_until_parked();
    Page {
        workspace,
        fs,
        backend,
        cx,
        _servers: servers,
    }
}

#[gpui::test]
async fn every_way_in_opens_the_page_not_a_file(cx: &mut TestAppContext) {
    let mut page = open_page(cx).await;
    assert_eq!(page.section(), Section::General);
    for (action, section) in [
        (OpenKeyBindings.boxed_clone(), Section::KeyboardShortcuts),
        (
            zed_actions::OpenSettingsFile.boxed_clone(),
            Section::General,
        ),
        (
            zed_actions::OpenKeymapFile.boxed_clone(),
            Section::KeyboardShortcuts,
        ),
        (zed_actions::OpenSettings.boxed_clone(), Section::General),
        (
            zed_actions::OpenKeymap.boxed_clone(),
            Section::KeyboardShortcuts,
        ),
        (
            crate::t3_settings::OpenT3Settings.boxed_clone(),
            Section::Agents,
        ),
    ] {
        let name = action.name();
        page.cx
            .update(|window, cx| window.dispatch_action(action, cx));
        page.cx.run_until_parked();
        assert_eq!(page.section(), section, "{name}");
    }
    // One page, and no editor: nothing opened a file.
    page.workspace.read_with(page.cx, |workspace, cx| {
        let items = workspace.active_pane().read(cx).items().count();
        assert_eq!(items, 1);
        assert!(workspace.active_item_as::<editor::Editor>(cx).is_none());
    });

    // The palette's "change the key binding" searches the shortcuts for the action.
    page.cx.dispatch_action(zed_actions::ChangeKeybinding {
        action: "editor::Copy".into(),
    });
    page.cx.run_until_parked();
    assert_eq!(page.section(), Section::KeyboardShortcuts);
    let view = page.view();
    let keys = view.read_with(page.cx, |view, _| view.keys.clone());
    keys.read_with(page.cx, |keys, _| {
        let shown = keys.shown();
        assert_eq!(
            shown.first().map(|row| row.action_name),
            Some("editor::Copy")
        );
        assert!(
            shown
                .iter()
                .all(|row| row.action_name.starts_with("editor::Copy"))
        );
    });
}

#[gpui::test]
async fn zeds_settings_are_saved_in_the_settings_file(cx: &mut TestAppContext) {
    let mut page = open_page(cx).await;
    page.click("segment-Light");
    page.click("settings-zoom-increase");
    page.click("settings-code-size-decrease");
    page.open(Section::Editor);
    page.click("settings-tab-size-increase");
    page.click("settings-vim");
    page.click("settings-line-numbers");
    page.click("settings-inline-blame");
    page.click("settings-format-on-save");
    page.open(Section::Terminal);
    page.click("segment-Bar");
    page.click("settings-terminal-font-size-increase");
    assert_eq!(
        page.settings().await,
        json!({
            "theme": { "mode": "light", "light": "mustr Light", "dark": "mustr Dark" },
            "ui_font_size": 14.5,
            "buffer_font_size": 13.5,
            "tab_size": 5,
            "vim_mode": true,
            "gutter": { "line_numbers": false },
            "git": { "inline_blame": { "enabled": false } },
            "format_on_save": "on",
            "terminal": { "cursor_shape": "bar", "font_size": 14.5 },
        })
    );
    // The file keeps its comment, and says to use the page.
    assert!(
        page.settings_text().await.contains("mustr's Settings page"),
        "{}",
        page.settings_text().await
    );

    // The terminal follows the code again.
    page.click("settings-terminal-font-size-reset");
    assert_eq!(
        page.settings().await["terminal"],
        json!({ "cursor_shape": "bar" })
    );
    // A shell of the user's, saved with Enter.
    page.click("segment-Another program");
    let view = page.view();
    view.update_in(page.cx, |view, window, cx| {
        view.shell
            .update(cx, |shell, cx| shell.set_text("/usr/bin/fish", window, cx))
    });
    page.cx.dispatch_action(menu::Confirm);
    page.cx.run_until_parked();
    assert_eq!(
        page.settings().await["terminal"],
        json!({ "cursor_shape": "bar", "shell": { "program": "/usr/bin/fish" } })
    );
    page.click("segment-Your login shell");
    assert_eq!(page.settings().await["terminal"]["shell"], json!("system"));
}

#[gpui::test]
async fn themes_keep_the_other_appearances_theme(cx: &mut TestAppContext) {
    let mut page = open_page(cx).await;
    page.click("segment-Colorblind-safe");
    assert_eq!(
        page.settings().await["theme"],
        json!({ "mode": "dark", "light": "mustr Light", "dark": "mustr Colorblind-safe" })
    );
    // Following the system keeps the colorblind-safe dark theme.
    page.click("segment-Match system");
    assert_eq!(
        page.settings().await["theme"],
        json!({ "mode": "system", "light": "mustr Light", "dark": "mustr Colorblind-safe" })
    );
    page.click("segment-Dark");
    assert_eq!(
        page.settings().await["theme"],
        json!({ "mode": "dark", "light": "mustr Light", "dark": "mustr Dark" })
    );
}

#[test]
fn a_zed_theme_gives_way_to_mustrs() {
    let one_dark = Themes::of(&ThemeSelection::Static(ThemeName("One Dark".into())), false);
    assert_eq!(one_dark.choice(), 0);
    assert_eq!(one_dark.others(), vec![Arc::<str>::from("One Dark")]);
    assert_eq!(
        one_dark.chose(1),
        ThemeSelection::Dynamic {
            mode: ThemeAppearanceMode::Light,
            light: ThemeName(mustr_ui::LIGHT_THEME_NAME.into()),
            dark: ThemeName(mustr_ui::DARK_THEME_NAME.into()),
        }
    );
}

#[gpui::test]
async fn mustrs_own_settings_are_saved_next_to_zeds(cx: &mut TestAppContext) {
    let mut page = open_page(cx).await;
    page.open(Section::Agents);
    page.click("settings-notify-failed");
    page.click("settings-notify-completed");
    page.click("settings-dock-badge");
    page.cx.update(|_, cx| {
        let settings = NotificationSettings::get(cx);
        assert!(settings.failed && !settings.completed && !settings.dock_badge);
    });
    page.open(Section::Updates);
    page.click("settings-update-automatically");
    assert_eq!(
        page.settings().await,
        json!({
            "notifications": { "failed": true, "completed": false, "dock_badge": false },
            "updates": { "enabled": false },
        })
    );
    // Turning one back on keeps the others.
    page.open(Section::Agents);
    page.click("settings-notify-completed");
    assert_eq!(
        page.settings().await["notifications"],
        json!({ "failed": true, "completed": true, "dock_badge": false })
    );
}

#[gpui::test]
async fn follow_ups_are_mustrs_own(cx: &mut TestAppContext) {
    let mut page = open_page(cx).await;
    page.open(Section::Agents);
    page.cx
        .update(|_, cx| assert_eq!(FollowUpBehavior::get(cx), FollowUpBehavior::Queue));
    page.click("segment-Steer");
    page.cx
        .update(|_, cx| assert_eq!(FollowUpBehavior::get(cx), FollowUpBehavior::Steer));
    // T3 keeps its own in its app; nothing goes to the server.
    assert_eq!(page.backend.updates(), []);
}

#[gpui::test]
async fn t3s_settings_are_in_agents_and_threads(cx: &mut TestAppContext) {
    let mut page = open_page(cx).await;
    page.open(Section::Agents);
    assert!(page.shows("t3-settings-header"));
    assert!(page.shows("t3-settings-row-model"));
    assert!(page.shows("source-control-rescan"));
    assert!(page.shows("settings-worktree-cleanup"));
    page.click("t3-settings-settle-on-merge");
    page.click("segment-New worktree");
    assert_eq!(
        page.backend.updates(),
        [
            update(json!({ "sidebarAutoSettleOnMerge": false })),
            update(json!({ "defaultThreadEnvMode": "worktree" })),
        ]
    );
    // Nothing went to the settings file.
    assert_eq!(page.settings().await, json!({}));
}

#[gpui::test]
async fn the_search_finds_settings_in_every_section(cx: &mut TestAppContext) {
    let mut page = open_page(cx).await;
    page.cx.update(|_, cx| {
        crate::keymap::load_default_keymap(cx);
        cx.set_global(crate::keymap::KeymapVersion(1));
    });
    page.search("vim");
    assert!(page.shows("settings-row-vim"));
    assert!(!page.shows("settings-row-tab-size"));
    assert!(!page.shows("settings-row-theme"));
    // By a description's words, and a group's name, across sections.
    page.search("size");
    for row in [
        "settings-row-zoom",
        "settings-row-code-size",
        "settings-row-tab-size",
        "settings-row-terminal-font-size",
    ] {
        assert!(page.shows(row), "{row}");
    }
    assert!(!page.shows("settings-row-vim"));
    // T3's too, and "T3" finds all of them.
    page.search("branch");
    assert!(page.shows("t3-settings-row-branch-naming"));
    assert!(page.shows("t3-settings-header"));
    assert!(!page.shows("t3-settings-row-model"));
    page.search("t3");
    assert!(page.shows("t3-settings-row-model"));
    assert!(page.shows("settings-row-server"));
    // Shortcuts are counted, and shown in their own section.
    page.search("copy");
    assert!(page.shows("settings-search-shortcuts"));
    page.click("settings-search-show-shortcuts");
    assert_eq!(page.section(), Section::KeyboardShortcuts);
    page.search("nothing matches this");
    assert!(page.shows("settings-search-empty"));
    // A section from the list ends the search.
    page.click("settings-section-terminal");
    assert_eq!(page.section(), Section::Terminal);
    assert!(page.shows("settings-row-terminal-cursor"));
}

#[gpui::test]
async fn an_unreadable_settings_file_is_shown_on_the_page(cx: &mut TestAppContext) {
    let mut page = open_page(cx).await;
    assert!(!page.shows("settings-file-error-settings"));
    page.cx
        .update(|_, cx| ConfigFileErrors::set_settings(Some("expected `,` at line 3".into()), cx));
    page.cx.run_until_parked();
    assert!(page.shows("settings-file-error-settings"));
    // Not the key bindings', outside Keyboard shortcuts.
    page.cx
        .update(|_, cx| ConfigFileErrors::set_keymap(Some("bad keys".into()), cx));
    page.cx.run_until_parked();
    assert!(!page.shows("settings-file-error-key bindings"));
    page.open(Section::KeyboardShortcuts);
    assert!(page.shows("settings-file-error-key bindings"));
}

#[test]
fn a_file_set_aside_is_kept_next_to_the_new_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, "{ \"vim_mode\": tru").unwrap();
    let backup = app_settings::set_aside(&path, INITIAL_SETTINGS).unwrap();
    assert_eq!(backup, dir.path().join("settings.json.bak"));
    assert_eq!(
        std::fs::read_to_string(&backup).unwrap(),
        "{ \"vim_mode\": tru"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), INITIAL_SETTINGS);
}

#[test]
fn mustrs_settings_are_written_into_the_text() {
    // Into mustr's new file, whose comments are inside the braces.
    let text = set_value_in_settings_text(
        INITIAL_SETTINGS,
        &["notifications", "sound"],
        Some(&json!(true)),
    )
    .unwrap();
    let parsed: Value = settings::parse_json_with_comments(&text).unwrap();
    assert_eq!(parsed, json!({ "notifications": { "sound": true } }));
    assert!(text.contains("// mustr's settings"), "{text}");

    // Next to the user's own, keeping their comments.
    let text =
        "{\n  // Mine.\n  \"vim_mode\": true,\n  \"notifications\": { \"failed\": true }\n}\n";
    let text = set_value_in_settings_text(text, &["notifications", "failed"], Some(&json!(false)))
        .unwrap();
    assert!(text.contains("// Mine."), "{text}");
    let parsed: Value = settings::parse_json_with_comments(&text).unwrap();
    assert_eq!(
        parsed,
        json!({ "vim_mode": true, "notifications": { "failed": false } })
    );

    // Removing what isn't there leaves the text alone.
    assert_eq!(
        set_value_in_settings_text(&text, &["t3"], None).unwrap(),
        text
    );
    let pinned =
        set_value_in_settings_text(&text, &["t3"], Some(&json!({ "mode": "embedded" }))).unwrap();
    let unpinned = set_value_in_settings_text(&pinned, &["t3"], None).unwrap();
    let parsed: Value = settings::parse_json_with_comments(&unpinned).unwrap();
    assert_eq!(
        parsed,
        json!({ "vim_mode": true, "notifications": { "failed": false } })
    );

    // A file mustr can't read is never written.
    assert!(
        set_value_in_settings_text("{ \"vim_mode\": tru", &["updates", "enabled"], None).is_err()
    );
}

// Keyboard shortcuts.

fn keystrokes(keys: &str) -> Vec<KeybindingKeystroke> {
    keys.split(' ')
        .map(|key| KeybindingKeystroke::from_keystroke(Keystroke::parse(key).unwrap()))
        .collect()
}

fn apply(change: KeymapChange, text: &str, cx: &mut TestAppContext) -> String {
    cx.update(|cx| {
        change
            .apply(
                text.to_string(),
                cx.keyboard_mapper().as_ref(),
                &HashMap::default(),
            )
            .unwrap()
    })
}

fn sections(text: &str) -> Value {
    settings::parse_json_with_comments(text).unwrap()
}

#[gpui::test]
fn changing_mustrs_shortcut_adds_the_users_and_unbinds_the_old_keys(cx: &mut TestAppContext) {
    let binding = BindingSpec {
        action: "mustr::OpenSettings".into(),
        context: None,
        keystrokes: keystrokes("ctrl-,"),
        arguments: None,
        source: KeybindSource::Default,
    };
    let text = apply(
        KeymapChange::Replace {
            binding: binding.clone(),
            keystrokes: keystrokes("ctrl-alt-s"),
        },
        INITIAL_KEYMAP,
        cx,
    );
    assert_eq!(
        sections(&text),
        json!([
            { "bindings": { "ctrl-alt-s": "mustr::OpenSettings" } },
            { "unbind": { "ctrl-,": "mustr::OpenSettings" } },
        ])
    );
    assert!(text.contains("mustr's Settings page"), "{text}");

    // Changing the user's own replaces it.
    let text = apply(
        KeymapChange::Replace {
            binding: BindingSpec {
                keystrokes: keystrokes("ctrl-alt-s"),
                source: KeybindSource::User,
                ..binding
            },
            keystrokes: keystrokes("ctrl-k ctrl-s"),
        },
        &text,
        cx,
    );
    assert_eq!(
        sections(&text),
        json!([
            { "bindings": { "ctrl-k ctrl-s": "mustr::OpenSettings" } },
            { "unbind": { "ctrl-,": "mustr::OpenSettings" } },
        ])
    );

    // An action without keys gets some, everywhere.
    let text = apply(
        KeymapChange::Add {
            action: "editor::SortLinesCaseSensitive".into(),
            keystrokes: keystrokes("ctrl-alt-o"),
        },
        &text,
        cx,
    );
    assert_eq!(
        sections(&text)[2],
        json!({ "bindings": { "ctrl-alt-o": "editor::SortLinesCaseSensitive" } })
    );
    assert_eq!(
        user_actions(&text, &HashMap::default()),
        ["mustr::OpenSettings", "editor::SortLinesCaseSensitive"]
            .into_iter()
            .map(String::from)
            .collect()
    );

    // Back to mustr's: everything the file says about the action goes, and nothing else.
    let text = apply(
        KeymapChange::Reset {
            action: "mustr::OpenSettings".into(),
        },
        &text,
        cx,
    );
    assert_eq!(
        sections(&text),
        json!([{ "bindings": { "ctrl-alt-o": "editor::SortLinesCaseSensitive" } }])
    );
}

#[gpui::test]
fn removing_a_default_shortcut_unbinds_it(cx: &mut TestAppContext) {
    let text = apply(
        KeymapChange::Remove {
            binding: BindingSpec {
                action: "editor::Copy".into(),
                context: Some("Editor".into()),
                keystrokes: keystrokes("ctrl-c"),
                arguments: None,
                source: KeybindSource::Default,
            },
        },
        INITIAL_KEYMAP,
        cx,
    );
    assert_eq!(
        sections(&text),
        json!([{ "context": "Editor", "unbind": { "ctrl-c": "editor::Copy" } }])
    );
}

#[test]
fn resetting_keeps_other_entries_and_comments() {
    let text = r#"[
  // My keys.
  {
    "context": "Editor",
    "bindings": {
      "ctrl-alt-c": "editor::Copy",
      "ctrl-alt-v": "editor::Paste"
    },
    "unbind": { "ctrl-c": "editor::Copy" }
  },
  { "bindings": { "ctrl-alt-x": ["editor::Copy", {}] } }
]"#;
    let text = remove_action(text, "editor::Copy", &HashMap::default(), 2).unwrap();
    assert!(text.contains("// My keys."), "{text}");
    assert_eq!(
        sections(&text),
        json!([{ "context": "Editor", "bindings": { "ctrl-alt-v": "editor::Paste" } }])
    );
}

#[gpui::test]
async fn recording_a_shortcut_saves_it_without_running_the_old_one(cx: &mut TestAppContext) {
    let mut page = open_page(cx).await;
    page.cx.update(|_, cx| {
        crate::keymap::load_default_keymap(cx);
        cx.set_global(crate::keymap::KeymapVersion(1));
    });
    page.open(Section::KeyboardShortcuts);
    let view = page.view();
    let keys = view.read_with(page.cx, |view, _| view.keys.clone());
    keys.update_in(page.cx, |keys, window, cx| {
        keys.set_query("mustr::OpenSettings", window, cx)
    });
    page.cx.run_until_parked();
    keys.read_with(page.cx, |keys, _| {
        let shown = keys.shown();
        assert_eq!(shown.len(), 1, "{shown:?}");
        assert_eq!(shown[0].keys, "ctrl-,");
        assert_eq!(shown[0].source, Some(KeybindSource::Default));
    });
    page.click("key-binding-change-0");
    keys.read_with(page.cx, |keys, _| assert!(keys.is_recording()));
    // Keys while recording are the recording's, not the bindings': ctrl-, doesn't open the
    // settings again. Shift alone is waited out.
    page.cx.simulate_keystrokes("shift ctrl-, ctrl-alt-s");
    assert_eq!(page.section(), Section::KeyboardShortcuts);
    keys.read_with(page.cx, |keys, cx| {
        assert_eq!(
            keys.recorded_for_test(cx),
            ["ctrl-,", "ctrl-alt-s"].map(String::from)
        )
    });
    page.click("key-binding-save");
    page.cx.run_until_parked();
    let text = page.keymap_text().await;
    assert_eq!(
        sections(&text),
        json!([
            { "bindings": { "ctrl-, ctrl-alt-s": "mustr::OpenSettings" } },
            { "unbind": { "ctrl-,": "mustr::OpenSettings" } },
        ])
    );

    // Once mustr loads the file, the list has the new keys, as the user's, and not the old.
    page.cx.update(|_, cx| {
        let KeymapFileLoadResult::Success { mut key_bindings } = KeymapFile::load(&text, cx) else {
            panic!("the saved keymap doesn't load");
        };
        for binding in &mut key_bindings {
            binding.set_meta(KeybindSource::User.meta());
        }
        cx.bind_keys(key_bindings);
        cx.set_global(crate::keymap::KeymapVersion(2));
    });
    page.cx.run_until_parked();
    keys.read_with(page.cx, |keys, _| {
        let shown = keys
            .shown()
            .iter()
            .map(|row| (row.keys.clone(), row.source))
            .collect::<Vec<_>>();
        assert_eq!(
            shown,
            [("ctrl-, ctrl-alt-s".to_string(), Some(KeybindSource::User))]
        );
    });

    // Esc cancels.
    keys.update_in(page.cx, |keys, window, cx| {
        keys.start_recording_for_test(0, window, cx)
    });
    page.cx.simulate_keystrokes("ctrl-k escape");
    keys.read_with(page.cx, |keys, _| assert!(!keys.is_recording()));
    assert_eq!(page.keymap_text().await, text);
}
