//! Zoom: ⌘= and ⌘- (Ctrl on Linux and Windows) make all of mustr bigger or smaller, the way a
//! browser zooms a page, and ⌘0 goes back to mustr's default size.
//!
//! mustr sizes its UI in rems of `ui_font_size` (`mustr_ui::tokens::scaled`), so a zoom step
//! scales `ui_font_size` by about 10%, and `buffer_font_size` by as much, so code keeps its size
//! next to the rest of the UI. The new sizes are saved in the user's settings file, so a zoom
//! lasts across restarts. Reset Zoom removes both settings, so mustr's defaults
//! (`assets/default_overrides.json`) apply again.
//!
//! Terminals (Zed's `TerminalElement`, local and remote alike) draw their text at the terminal's
//! own `terminal.font_size` when it's set, or else at `buffer_font_size`. So a terminal follows
//! the zoom through `buffer_font_size`, but a `terminal.font_size` (which Zed users often set)
//! would pin it: Zed scales it only with Zed's unsaved font size adjustments
//! (`theme_settings::adjusted_font_size`). A zoom step scales it too, by as much as the UI, and
//! Reset Zoom removes it, so terminals go back to the code's size.
//!
//! Zed binds the same keys to its own font size actions, whose handlers are in `crates/zed`,
//! which mustr doesn't include. mustr's bindings ([`key_bindings`]) are bound after Zed's, so
//! they win.

use fs::Fs;
use gpui::{App, KeyBinding, Pixels, actions, px};
use settings::{Settings as _, update_settings_file};
use terminal::terminal_settings::TerminalSettings;
use theme_settings::{ThemeSettings, clamp_font_size};

actions!(
    mustr,
    [
        /// Makes everything bigger, text and layout alike, by about 10%.
        ZoomIn,
        /// Makes everything smaller, text and layout alike, by about 10%.
        ZoomOut,
        /// Goes back to mustr's default size.
        ResetZoom,
    ]
);

/// How much one step scales the font sizes. Browsers step by about as much near 100%.
const ZOOM_STEP: f32 = 1.1;

/// Font sizes are kept to half pixels.
const FONT_SIZE_PRECISION: f32 = 0.5;

pub fn init(cx: &mut App) {
    cx.on_action(|_: &ZoomIn, cx| zoom(Step::In, cx))
        .on_action(|_: &ZoomOut, cx| zoom(Step::Out, cx))
        .on_action(|_: &ResetZoom, cx| reset_zoom(cx));
}

/// ⌘= and ⌘+ zoom in, ⌘- zooms out, and ⌘0 resets (Ctrl on Linux and Windows), as in Zed's
/// keymaps (`assets/keymaps/default-*.json`), which bind them to Zed's font size actions with no
/// context and, again, in the `Onboarding` and `Welcome` contexts.
///
/// These have no context either. A binding without a context matches as deep as the focused
/// element (`Keymap::binding_enabled` in `crates/gpui/src/keymap.rs`), as deep as any binding
/// in a context can, and among equally deep bindings the later one wins. mustr binds these after
/// Zed's keymaps and base keymaps (`keymap::load_default_keymap`), so they win in every context:
/// the workspace, editors, terminals, Zed's welcome and onboarding pages, and mustr's own views.
/// The user's keymap is bound later still, so the user can rebind them.
pub fn key_bindings() -> Vec<KeyBinding> {
    // macOS's menu bar shows an action's first binding, so ⌘+ comes before ⌘=.
    let mut bindings = vec![
        KeyBinding::new("secondary-+", ZoomIn, None),
        KeyBinding::new("secondary-=", ZoomIn, None),
        KeyBinding::new("secondary--", ZoomOut, None),
        KeyBinding::new("secondary-0", ResetZoom, None),
    ];
    // Zed's Windows keymap writes ⇧= rather than +.
    if cfg!(target_os = "windows") {
        bindings.push(KeyBinding::new("ctrl-shift-=", ZoomIn, None));
    }
    bindings
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    In,
    Out,
}

/// Saves the sizes one step from the current ones. Copied from Zed's handlers for
/// `zed_actions::IncreaseUiFontSize` and the like (`initialize_workspace` in
/// `crates/zed/src/zed.rs`), which compute the new size inside the settings update, so steps
/// taken faster than the settings file is written each start from the last one.
fn zoom(step: Step, cx: &mut App) {
    clear_unsaved_adjustments(cx);
    update_settings_file(<dyn Fs>::global(cx), cx, move |settings, cx| {
        let theme = ThemeSettings::get_global(cx);
        let ui = theme.ui_font_size(cx);
        let (ui_font_size, buffer_font_size) = zoomed(ui, theme.buffer_font_size(cx), step);
        settings.theme.ui_font_size = Some(f32::from(ui_font_size).into());
        settings.theme.buffer_font_size = Some(f32::from(buffer_font_size).into());
        if let Some(terminal) = TerminalSettings::get_global(cx).font_size {
            let terminal = clamp_font_size(round(terminal * (ui_font_size / ui)));
            settings.terminal.get_or_insert_default().font_size = Some(f32::from(terminal).into());
        }
    });
}

/// Removes the sizes from the user's settings, so mustr's defaults apply: the UI's, the code's,
/// and the terminal's, so terminals go back to the code's size.
fn reset_zoom(cx: &mut App) {
    clear_unsaved_adjustments(cx);
    update_settings_file(<dyn Fs>::global(cx), cx, |settings, _| {
        settings.theme.ui_font_size = None;
        settings.theme.buffer_font_size = None;
        if let Some(terminal) = settings.terminal.as_mut() {
            terminal.font_size = None;
        }
    });
}

/// The size a terminal draws its text at: what Zed's `TerminalElement` uses for a terminal view
/// (`TerminalMode::Standalone`, as mustr's local and remote terminals are) in
/// `crates/terminal_view/src/terminal_element.rs`.
#[cfg(test)]
fn terminal_font_size(cx: &App) -> Pixels {
    TerminalSettings::get_global(cx).font_size.map_or_else(
        || ThemeSettings::get_global(cx).buffer_font_size(cx),
        |size| theme_settings::adjusted_font_size(size, cx),
    )
}

/// Zed can also change the font sizes without saving them (`theme_settings::adjust_ui_font_size`),
/// which would hide the saved ones. Nothing in mustr does, but a zoom shows the saved sizes
/// whatever happened.
fn clear_unsaved_adjustments(cx: &mut App) {
    theme_settings::reset_ui_font_size(cx);
    theme_settings::reset_buffer_font_size(cx);
}

/// The UI and code font sizes one step from `ui` and `buffer`: the UI size scaled by
/// [`ZOOM_STEP`] (or divided by it), rounded to half a pixel and clamped to Zed's limits, and
/// the code size scaled by as much as the UI size changed, so the two keep their ratio.
fn zoomed(ui: Pixels, buffer: Pixels, step: Step) -> (Pixels, Pixels) {
    let factor = match step {
        Step::In => ZOOM_STEP,
        Step::Out => ZOOM_STEP.recip(),
    };
    let mut new_ui = clamp_font_size(round(ui * factor));
    if new_ui == ui {
        // Rounding can undo a step at small sizes; move by the smallest step instead.
        let nudge = px(FONT_SIZE_PRECISION);
        new_ui = clamp_font_size(match step {
            Step::In => ui + nudge,
            Step::Out => ui - nudge,
        });
    }
    let new_buffer = clamp_font_size(round(buffer * (new_ui / ui)));
    (new_ui, new_buffer)
}

fn round(size: Pixels) -> Pixels {
    px((f32::from(size) / FONT_SIZE_PRECISION).round() * FONT_SIZE_PRECISION)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{KeyContext, Keystroke, TestAppContext, UpdateGlobal as _};
    use project::FakeFs;
    use serde_json::{Value, json};
    use settings::{BaseKeymapContent, SettingsStore};
    use std::sync::Arc;

    #[test]
    fn steps_scale_both_sizes_by_about_ten_percent() {
        let step = |ui: f32, buffer: f32, step| {
            let (ui, buffer) = zoomed(px(ui), px(buffer), step);
            (f32::from(ui), f32::from(buffer))
        };
        assert_eq!(step(26., 26., Step::In), (28.5, 28.5));
        assert_eq!(step(28.5, 28.5, Step::In), (31.5, 31.5));
        assert_eq!(step(31.5, 31.5, Step::Out), (28.5, 28.5));
        assert_eq!(step(28.5, 28.5, Step::Out), (26., 26.));
        assert_eq!(step(26., 26., Step::Out), (23.5, 23.5));
        // Code keeps its size relative to the UI.
        assert_eq!(step(26., 13., Step::In), (28.5, 14.5));
        assert_eq!(step(20., 30., Step::Out), (18., 27.));
        // Each step moves, even where rounding would undo it.
        assert_eq!(step(6.5, 6.5, Step::Out), (6., 6.));
        assert_eq!(step(6., 6., Step::In), (6.5, 6.5));
    }

    #[test]
    fn steps_stop_at_zeds_limits() {
        let step = |ui: f32, buffer: f32, step| {
            let (ui, buffer) = zoomed(px(ui), px(buffer), step);
            (f32::from(ui), f32::from(buffer))
        };
        assert_eq!(step(6., 6., Step::Out), (6., 6.));
        assert_eq!(step(100., 100., Step::In), (100., 100.));
        assert_eq!(step(95., 95., Step::In), (100., 100.));
        assert_eq!(step(60., 95., Step::In), (66., 100.));
        assert_eq!(step(6.5, 12., Step::Out), (6., 11.));
    }

    async fn init_test(user_settings: &str, cx: &mut TestAppContext) -> Arc<FakeFs> {
        let fs = FakeFs::new(cx.executor());
        // A fake file system: the path is the real settings file's, but nothing touches it.
        let settings_file = paths::settings_file();
        fs.insert_tree(
            settings_file.parent().unwrap(),
            json!({ settings_file.file_name().unwrap().to_str().unwrap(): user_settings }),
        )
        .await;
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            crate::app_settings::apply_default_overrides(cx).unwrap();
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            <dyn Fs>::set_global(fs.clone(), cx);
            SettingsStore::update_global(cx, |store, cx| {
                store.set_user_settings(user_settings, cx).result().unwrap();
            });
            init(cx);
        });
        fs
    }

    async fn saved_settings(fs: &FakeFs) -> Value {
        let text = fs.load(paths::settings_file()).await.unwrap();
        settings::parse_json_with_comments(&text).unwrap()
    }

    fn font_sizes(cx: &mut TestAppContext) -> (f32, f32) {
        cx.read(|cx| {
            let theme = ThemeSettings::get_global(cx);
            (
                theme.ui_font_size(cx).into(),
                theme.buffer_font_size(cx).into(),
            )
        })
    }

    /// Each step is saved to the settings file, both sizes together, next to the user's other
    /// settings. Steps faster than the file is written each start from the previous one.
    #[gpui::test]
    async fn zooming_saves_both_sizes(cx: &mut TestAppContext) {
        let fs = init_test(
            r#"{
  // The user's own settings.
  "vim_mode": true,
  "ui_font_size": 20,
  "buffer_font_size": 15
}"#,
            cx,
        )
        .await;
        cx.update(|cx| cx.dispatch_action(&ZoomIn));
        cx.update(|cx| cx.dispatch_action(&ZoomIn));
        cx.run_until_parked();
        assert_eq!(
            saved_settings(&fs).await,
            json!({"vim_mode": true, "ui_font_size": 24.0, "buffer_font_size": 18.0})
        );
        assert_eq!(font_sizes(cx), (24., 18.));
        let text = fs.load(paths::settings_file()).await.unwrap();
        assert!(text.contains("// The user's own settings."), "{text}");

        cx.update(|cx| cx.dispatch_action(&ZoomOut));
        cx.run_until_parked();
        assert_eq!(
            saved_settings(&fs).await,
            json!({"vim_mode": true, "ui_font_size": 22.0, "buffer_font_size": 16.5})
        );
        assert_eq!(font_sizes(cx), (22., 16.5));
    }

    /// Reset removes both sizes from the settings file, so mustr's defaults apply.
    #[gpui::test]
    async fn reset_removes_both_sizes(cx: &mut TestAppContext) {
        let fs = init_test(
            r#"{"vim_mode": true, "ui_font_size": 40, "buffer_font_size": 30}"#,
            cx,
        )
        .await;
        cx.update(|cx| cx.dispatch_action(&ResetZoom));
        cx.run_until_parked();
        assert_eq!(saved_settings(&fs).await, json!({"vim_mode": true}));

        // mustr's defaults (`assets/default_overrides.json`).
        let overrides: Value =
            settings::parse_json_with_comments(include_str!("../assets/default_overrides.json"))
                .unwrap();
        let defaults = (
            overrides["ui_font_size"].as_f64().unwrap() as f32,
            overrides["buffer_font_size"].as_f64().unwrap() as f32,
        );
        assert_eq!(font_sizes(cx), defaults);

        // Zooming from the defaults saves the sizes again.
        cx.update(|cx| cx.dispatch_action(&ZoomIn));
        cx.run_until_parked();
        let (ui, buffer) = zoomed(px(defaults.0), px(defaults.1), Step::In);
        assert_eq!(
            saved_settings(&fs).await,
            json!({
                "vim_mode": true,
                "ui_font_size": f32::from(ui),
                "buffer_font_size": f32::from(buffer),
            })
        );
    }

    /// Terminals draw their text at the code's size, so they follow the zoom with it.
    #[gpui::test]
    async fn terminals_follow_the_zoom(cx: &mut TestAppContext) {
        let fs = init_test(r#"{"ui_font_size": 26, "buffer_font_size": 26}"#, cx).await;
        cx.read(|cx| assert_eq!(terminal_font_size(cx), px(26.)));
        cx.update(|cx| cx.dispatch_action(&ZoomOut));
        cx.run_until_parked();
        cx.read(|cx| assert_eq!(terminal_font_size(cx), px(23.5)));
        assert_eq!(
            saved_settings(&fs).await,
            json!({"ui_font_size": 23.5, "buffer_font_size": 23.5})
        );
        cx.update(|cx| cx.dispatch_action(&ResetZoom));
        cx.run_until_parked();
        cx.read(|cx| {
            assert_eq!(
                terminal_font_size(cx),
                ThemeSettings::get_global(cx).buffer_font_size(cx)
            )
        });
    }

    /// A terminal size of the user's own (`terminal.font_size`) would pin terminals, so a zoom
    /// scales it by as much as the UI, and Reset Zoom removes it.
    #[gpui::test]
    async fn the_terminals_own_size_follows_the_zoom(cx: &mut TestAppContext) {
        let fs = init_test(
            r#"{"ui_font_size": 26, "buffer_font_size": 26, "terminal": {"font_size": 15, "line_height": "standard"}}"#,
            cx,
        )
        .await;
        cx.read(|cx| assert_eq!(terminal_font_size(cx), px(15.)));
        cx.update(|cx| cx.dispatch_action(&ZoomIn));
        cx.run_until_parked();
        // 15 × 28.5 / 26, to the half pixel.
        cx.read(|cx| assert_eq!(terminal_font_size(cx), px(16.5)));
        cx.update(|cx| cx.dispatch_action(&ZoomOut));
        cx.update(|cx| cx.dispatch_action(&ZoomOut));
        cx.run_until_parked();
        cx.read(|cx| assert_eq!(terminal_font_size(cx), px(13.5)));
        assert_eq!(
            saved_settings(&fs).await["terminal"],
            json!({"font_size": 13.5, "line_height": "standard"})
        );

        // Reset goes back to the code's size, at mustr's default.
        cx.update(|cx| cx.dispatch_action(&ResetZoom));
        cx.run_until_parked();
        assert_eq!(
            saved_settings(&fs).await,
            json!({"terminal": {"line_height": "standard"}})
        );
        cx.read(|cx| {
            assert_eq!(
                terminal_font_size(cx),
                ThemeSettings::get_global(cx).buffer_font_size(cx)
            )
        });
    }

    /// The View menu shows each action's first binding (`set_menus` in
    /// `crates/gpui_macos/src/platform.rs`): ⌘+, ⌘-, and ⌘0 on macOS.
    #[gpui::test]
    fn the_view_menu_shows_the_zoom_keys(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            crate::app_settings::apply_default_overrides(cx).unwrap();
            crate::keymap::load_default_keymap(cx);
            let keymap = cx.key_bindings();
            let keymap = keymap.borrow();
            for (action, key) in [
                (&ZoomIn as &dyn gpui::Action, "secondary-+"),
                (&ZoomOut, "secondary--"),
                (&ResetZoom, "secondary-0"),
            ] {
                let first = keymap.bindings_for_action(action).next().unwrap();
                let keystroke = Keystroke::parse(key).unwrap();
                assert_eq!(
                    first.match_keystrokes(&[keystroke]),
                    Some(false),
                    "{} shows {:?}",
                    action.name(),
                    first.keystrokes()
                );
            }
        });
    }

    /// The keys zoom from wherever focus is, with any base keymap, in vim mode or not: mustr's
    /// bindings beat Zed's, which bind the same keys to Zed's own font size actions (and, in
    /// JetBrains' keymap, to folding and the git panel).
    #[gpui::test]
    fn the_zoom_keys_beat_zeds_in_every_context(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            crate::app_settings::apply_default_overrides(cx).unwrap();
        });
        let contexts: &[&[&str]] = &[
            &["Workspace"],
            &["Workspace", "Pane", "Editor mode=full"],
            &["Workspace", "Pane", "Terminal"],
            &["Workspace", "Pane", "Onboarding"],
            &["Workspace", "Pane", "Welcome"],
            &["Workspace", "Pane", "ImageViewer"],
            &["Workspace", "Dock", "GitPanel"],
            &[
                "Workspace",
                "Pane",
                "ThreadView",
                "ThreadComposer",
                "Editor mode=auto_height",
            ],
            &["Workspace", "Pane", "ReviewView", "Editor mode=full"],
            &[
                "Workspace",
                "Dock",
                "MustrNavigation",
                "ThreadsSidebar",
                "Editor mode=single_line",
            ],
        ];
        let keys = [
            ("secondary-=", "mustr::ZoomIn"),
            ("secondary-+", "mustr::ZoomIn"),
            ("secondary--", "mustr::ZoomOut"),
            ("secondary-0", "mustr::ResetZoom"),
        ];
        for base_keymap in [
            BaseKeymapContent::Zed,
            BaseKeymapContent::VSCode,
            BaseKeymapContent::JetBrains,
            BaseKeymapContent::SublimeText,
            BaseKeymapContent::Atom,
            BaseKeymapContent::TextMate,
            BaseKeymapContent::Emacs,
            BaseKeymapContent::Cursor,
        ] {
            for vim_mode in [false, true] {
                cx.update(|cx| {
                    SettingsStore::update_global(cx, |store, cx| {
                        store.update_user_settings(cx, |settings| {
                            settings.base_keymap = Some(base_keymap);
                            settings.vim_mode = Some(vim_mode);
                        });
                    });
                    cx.clear_key_bindings();
                    crate::keymap::load_default_keymap(cx);
                    let keymap = cx.key_bindings();
                    let keymap = keymap.borrow();
                    for context in contexts {
                        let stack: Vec<_> = context
                            .iter()
                            .map(|context| KeyContext::parse(context).unwrap())
                            .collect();
                        for (key, action) in keys {
                            let keystroke = Keystroke::parse(key).unwrap();
                            let (bindings, _) = keymap.bindings_for_input(&[keystroke], &stack);
                            assert_eq!(
                                bindings.first().map(|binding| binding.action().name()),
                                Some(action),
                                "{key} in {context:?}, {base_keymap:?}, vim mode {vim_mode}"
                            );
                        }
                    }
                });
            }
        }
    }
}
