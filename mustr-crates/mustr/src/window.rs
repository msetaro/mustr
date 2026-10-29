use crate::{
    home::HomeView,
    nav_panel::NavPanel,
    settings_page::{self, Section},
};
use gpui::{
    App, AppContext as _, Context, Entity, Global, KeyBinding, Menu, MenuItem, OsAction,
    PromptLevel, TaskExt as _, TitlebarOptions, Window, WindowHandle, WindowKind, WindowOptions,
    px,
};
use review::{OpenThreadChat, ToggleFileTree};
use settings::{Settings as _, SettingsStore};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use util::ResultExt as _;
use uuid::Uuid;
use workspace::{
    AppState, CloseWindow, MultiWorkspace, OpenOptions, Pane, Workspace, WorkspaceSettings,
    with_active_or_new_workspace,
};

gpui::actions!(
    mustr,
    [
        /// Opens mustr's settings.
        OpenSettings,
        /// Opens mustr's keyboard shortcuts, to change them.
        OpenKeyBindings,
        /// Quits mustr.
        Quit,
    ]
);

/// The Wayland app ID and X11 `WM_CLASS`. It must match the `.desktop` file name.
const APP_ID: &str = "dev.mustr.mustr";

/// Handles the app's own actions: settings, key bindings and quit.
pub(crate) fn register_app_actions(cx: &mut App) {
    let open = |section: Section, query: Option<String>| {
        move |cx: &mut App| {
            let query = query.clone();
            with_active_or_new_workspace(cx, move |workspace, window, cx| {
                settings_page::show(workspace, section, query, window, cx)
            })
        }
    };
    let open_settings = move |cx: &mut App| open(Section::General, None)(cx);
    let open_key_bindings = move |cx: &mut App| open(Section::KeyboardShortcuts, None)(cx);
    // mustr's own actions, so the command palette doesn't list them under Zed's name ("zed: open
    // settings"). Zed's stay handled, for its key bindings and for Zed's code that dispatches
    // them, and are hidden from the palette (`hidden_actions.rs`). Every one opens mustr's
    // Settings page: mustr never opens its settings or key bindings files.
    cx.on_action(move |_: &OpenSettings, cx| open_settings(cx))
        .on_action(move |_: &OpenKeyBindings, cx| open_key_bindings(cx))
        .on_action(|_: &Quit, cx| quit(cx))
        .on_action(|_: &zed_actions::Quit, cx| quit(cx))
        .on_action(move |_: &zed_actions::OpenSettingsFile, cx| open_settings(cx))
        // Zed's keymaps bind its settings editor to cmd-, (ctrl-, on Linux).
        .on_action(move |_: &zed_actions::OpenSettings, cx| open_settings(cx))
        .on_action(move |_: &zed_actions::OpenKeymapFile, cx| open_key_bindings(cx))
        // Zed's keymap editor, which Zed's keymaps bind to cmd-k cmd-s.
        .on_action(move |_: &zed_actions::OpenKeymap, cx| open_key_bindings(cx))
        // The command palette's "change the key binding" (secondary confirm) for an action.
        .on_action(move |action: &zed_actions::ChangeKeybinding, cx| {
            open(Section::KeyboardShortcuts, Some(action.action.clone()))(cx)
        })
        // Links in Zed's views to one of Zed's settings: the Settings page, searching for it.
        .on_action(move |action: &zed_actions::OpenSettingsAt, cx| {
            let setting = action.path.rsplit('.').next().unwrap_or_default();
            open(Section::General, Some(setting.replace('_', " ")))(cx)
        });
}

pub fn init(cx: &mut App) {
    register_app_actions(cx);

    // Copied from crates/zed/src/zed.rs (`initialize_workspace`). Reading the old
    // value keeps rustc's unused-assignment lint from firing on the reassignment.
    let mut _on_close_subscription = bind_on_window_closed(cx);
    cx.observe_global::<SettingsStore>(move |cx| {
        _ = _on_close_subscription.is_some();
        _on_close_subscription = bind_on_window_closed(cx);
    })
    .detach();

    cx.observe_new(|_: &mut MultiWorkspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        // Copied from crates/zed/src/zed.rs (`initialize_workspace`). Closing is
        // asynchronous, so unsaved changes can prompt before the window goes away.
        let multi_workspace = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            multi_workspace
                .update(cx, |multi_workspace, cx| {
                    multi_workspace.close_window(&CloseWindow, window, cx);
                    false
                })
                .unwrap_or(true)
        });
    })
    .detach();

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        if let Some(specs) = window.gpu_specs() {
            log::info!("Using GPU: {specs:?}");
        }
        let center_pane = workspace.active_pane().clone();
        // An empty center pane shows Zed's "Welcome to Zed" page, with Zed's onboarding.
        // mustr's home is the Home tab, which the rail reopens.
        center_pane.update(cx, |pane, _| pane.set_should_display_welcome_page(false));
        add_toolbar_items(workspace, &center_pane, window, cx);
        let workspace_handle = cx.entity();
        // The rail's areas, and Edit's folder, before the panel that shows them.
        let areas = crate::areas::ensure(workspace, window, cx);
        crate::edit_area::register(workspace, cx);
        let edit_folder = crate::edit_area::EditFolder::ensure(workspace, cx);

        // Zed adds its panels in `initialize_panels` (crates/zed/src/zed.rs); mustr's are
        // created synchronously, so dock restoration can finish right away.
        let nav_panel =
            cx.new(|cx| NavPanel::new(&workspace_handle, areas, edit_folder, window, cx));
        NavPanel::register(workspace, &nav_panel);
        workspace.add_panel(nav_panel.clone(), window, cx);
        // Work packages add their dock panels here, created synchronously like the nav panel,
        // each package under its own label (`mustr-docs/PARITY.md` §3).
        // WP6, terminals and worktrees:
        crate::terminal_panel::add_panel(workspace, &workspace_handle, window, cx);
        workspace.finish_dock_restoration(cx);
        let left_dock = workspace.left_dock().clone();
        nav_panel.update(cx, |nav_panel, cx| {
            nav_panel.added_to_dock(&left_dock, window, cx)
        });
        // The review's toggle shows or hides its files.
        let weak_nav_panel = nav_panel.downgrade();
        workspace.register_action(move |_, _: &ToggleFileTree, window, cx| {
            let nav_panel = weak_nav_panel.clone();
            window.defer(cx, move |window, cx| {
                nav_panel
                    .update(cx, |nav_panel, cx| {
                        nav_panel.toggle_review_files(window, cx)
                    })
                    .ok();
            });
        });
        workspace.register_action(|workspace, action: &OpenThreadChat, window, cx| {
            let title = if action.title.is_empty() {
                "Agent thread".into()
            } else {
                action.title.clone().into()
            };
            thread_chat::ThreadView::open(action.thread_id.clone(), title, workspace, window, cx);
        });

        cx.subscribe_in(
            &workspace_handle,
            window,
            |workspace, _, event, window, cx| {
                if let workspace::Event::PaneAdded(pane) = event {
                    add_toolbar_items(workspace, pane, window, cx);
                }
            },
        )
        .detach();
    })
    .detach();

    cx.set_menus(app_menus());
}

/// Agent threads to open in the first window. For smoke tests and screenshots.
#[derive(Clone, Debug, Default)]
pub struct StartupThreads {
    pub open_thread: Option<String>,
    pub new_thread: bool,
    /// A folder to open in Edit.
    pub edit_folder: Option<std::path::PathBuf>,
}

impl Global for StartupThreads {}

/// Opens a window with an empty workspace showing mustr's home view.
pub fn open_home_window(app_state: Arc<AppState>, cx: &mut App) {
    let startup_threads = cx.try_global::<StartupThreads>().cloned();
    cx.remove_global::<StartupThreads>();
    let open_window = workspace::open_new(
        OpenOptions::default(),
        app_state,
        cx,
        |workspace, window, cx| {
            let workspace_handle = workspace.weak_handle();
            let home = cx.new(|cx| HomeView::new(workspace_handle, window, cx));
            workspace.add_item_to_active_pane(Box::new(home), None, true, window, cx);
            let Some(startup_threads) = startup_threads else {
                return;
            };
            if let Some(thread_id) = startup_threads.open_thread {
                thread_chat::open_thread(
                    thread_id,
                    "Agent thread".into(),
                    workspace.weak_handle(),
                    window,
                    cx,
                );
            }
            if startup_threads.new_thread {
                thread_chat::ThreadView::open_new(workspace, window, cx);
            }
            if let Some(folder) = startup_threads.edit_folder {
                crate::edit_area::open_folder(folder, workspace, window, cx);
            }
        },
    );
    cx.spawn(async move |cx| {
        if let Err(error) = open_window.await {
            log::error!("Failed to open a window: {error:#}");
            eprintln!("mustr failed to open a window: {error:#}");
            cx.update(|cx| cx.quit());
            return;
        }
        // Zed has restored the window's docks.
        cx.update(crate::terminal_panel::finish_launch);
        // The areas learn what the window opened while it was being made.
        cx.update(crate::areas::Areas::sync_all);
    })
    .detach();
}

// The window icon X11 shows. Wayland finds the icon through the desktop file
// for the app ID, and macOS through the app bundle. Copied from
// crates/zed/src/zed.rs, without the build step that resizes Zed's icon.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
static APP_ICON: std::sync::LazyLock<Option<Arc<image::RgbaImage>>> =
    std::sync::LazyLock::new(|| {
        const BYTES: &[u8] =
            include_bytes!("../resources/linux/hicolor/256x256/apps/dev.mustr.mustr.png");
        util::maybe!({
            let image = image::ImageReader::new(std::io::Cursor::new(BYTES))
                .with_guessed_format()?
                .decode()?
                .into();
            anyhow::Ok(Arc::new(image))
        })
        .log_err()
    });

// Copied from crates/zed/src/zed.rs (`build_window_options`), with mustr's app
// ID and icon, and the traffic lights where mustr's title bar wants them.
pub fn build_window_options(display_uuid: Option<Uuid>, cx: &mut App) -> WindowOptions {
    let display = display_uuid.and_then(|uuid| {
        cx.displays()
            .into_iter()
            .find(|display| display.uuid().ok() == Some(uuid))
    });
    let window_decorations = match std::env::var("ZED_WINDOW_DECORATIONS") {
        Ok(value) if value == "server" => gpui::WindowDecorations::Server,
        Ok(value) if value == "client" => gpui::WindowDecorations::Client,
        _ => match WorkspaceSettings::get_global(cx).window_decorations {
            settings::WindowDecorations::Server => gpui::WindowDecorations::Server,
            settings::WindowDecorations::Client => gpui::WindowDecorations::Client,
        },
    };
    let use_system_window_tabs = WorkspaceSettings::get_global(cx).use_system_window_tabs;

    WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: None,
            appears_transparent: true,
            // Centered in the title bar, whose height follows `ui_font_size`.
            traffic_light_position: Some(crate::title_bar::traffic_light_position(cx)),
        }),
        window_bounds: None,
        focus: false,
        show: false,
        kind: WindowKind::Normal,
        is_movable: true,
        app_owns_titlebar_drag: true,
        display_id: display.map(|display| display.id()),
        window_background: theme::ActiveTheme::theme(cx).window_background_appearance(),
        app_id: Some(APP_ID.to_owned()),
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        icon: APP_ICON.as_ref().cloned(),
        window_decorations: Some(window_decorations),
        window_min_size: Some(gpui::Size {
            width: px(360.0),
            height: px(240.0),
        }),
        tabbing_identifier: use_system_window_tabs.then(|| "mustr".to_owned()),
        ..Default::default()
    }
}

/// The app menu. macOS shows it in the menu bar; the shortcuts it shows are the key bindings of
/// its actions. Compare Zed's `crates/zed/src/zed/app_menus.rs`.
pub fn app_menus() -> Vec<Menu> {
    vec![
        Menu::new("mustr").items(vec![
            #[cfg(target_os = "macos")]
            MenuItem::action("About mustr", crate::macos::About),
            #[cfg(target_os = "macos")]
            MenuItem::action("Check for Updates…", crate::updates::CheckForUpdates),
            #[cfg(target_os = "macos")]
            MenuItem::separator(),
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::action("Key Bindings…", OpenKeyBindings),
            MenuItem::action(
                "Select Theme…",
                zed_actions::theme_selector::Toggle::default(),
            ),
            MenuItem::separator(),
            #[cfg(target_os = "macos")]
            MenuItem::os_submenu("Services", gpui::SystemMenuType::Services),
            #[cfg(target_os = "macos")]
            MenuItem::separator(),
            #[cfg(target_os = "macos")]
            MenuItem::action("Hide mustr", crate::macos::Hide),
            #[cfg(target_os = "macos")]
            MenuItem::action("Hide Others", crate::macos::HideOthers),
            #[cfg(target_os = "macos")]
            MenuItem::action("Show All", crate::macos::ShowAll),
            #[cfg(target_os = "macos")]
            MenuItem::separator(),
            MenuItem::action("Quit mustr", Quit),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", editor::actions::Undo, OsAction::Undo),
            MenuItem::os_action("Redo", editor::actions::Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", editor::actions::Cut, OsAction::Cut),
            MenuItem::os_action("Copy", editor::actions::Copy, OsAction::Copy),
            MenuItem::os_action("Paste", editor::actions::Paste, OsAction::Paste),
            MenuItem::separator(),
            MenuItem::os_action(
                "Select All",
                editor::actions::SelectAll,
                OsAction::SelectAll,
            ),
            MenuItem::separator(),
            MenuItem::action("Find", search::buffer_search::Deploy::find()),
        ]),
        // The shortcuts shown are the zoom actions' key bindings: ⌘+, ⌘-, and ⌘0.
        Menu::new("View").items([
            MenuItem::action("Zoom In", crate::zoom::ZoomIn),
            MenuItem::action("Zoom Out", crate::zoom::ZoomOut),
            MenuItem::action("Actual Size", crate::zoom::ResetZoom),
        ]),
        // AppKit lists the open windows at the end of the menu named "Window".
        #[cfg(target_os = "macos")]
        Menu::new("Window").items([
            MenuItem::action("Minimize", crate::macos::Minimize),
            MenuItem::action("Zoom", crate::macos::Zoom),
            MenuItem::separator(),
        ]),
    ]
}

// Copied from crates/zed/src/zed.rs (`bind_on_window_closed`).
fn bind_on_window_closed(cx: &mut App) -> Option<gpui::Subscription> {
    #[cfg(target_os = "macos")]
    {
        WorkspaceSettings::get_global(cx)
            .on_last_window_closed
            .is_quit_app()
            .then(|| {
                cx.on_window_closed(|cx, _window_id| {
                    if cx.windows().is_empty() {
                        cx.quit();
                    }
                })
            })
    }
    #[cfg(not(target_os = "macos"))]
    {
        Some(cx.on_window_closed(|cx, _window_id| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        }))
    }
}

// Zed's `initialize_pane` (crates/zed/src/zed.rs) adds many toolbar items;
// mustr adds search, and its own.
fn add_toolbar_items(
    workspace: &Workspace,
    pane: &Entity<Pane>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let languages = workspace.project().read(cx).languages().clone();
    pane.update(cx, |pane, cx| {
        pane.toolbar().update(cx, |toolbar, cx| {
            let buffer_search_bar =
                cx.new(|cx| search::BufferSearchBar::new(Some(languages), window, cx));
            toolbar.add_item(buffer_search_bar, window, cx);
            let project_search_bar = cx.new(|_| search::project_search::ProjectSearchBar::new());
            toolbar.add_item(project_search_bar, window, cx);
            // Work packages add their toolbar items here, each package under its own label
            // (`mustr-docs/PARITY.md` §3).
            // WP4, ship the work:
            let thread_toolbar = cx.new(|cx| {
                crate::thread_toolbar::ThreadToolbar::new(workspace.weak_handle(), window, cx)
            });
            toolbar.add_item(thread_toolbar, window, cx);
        })
    });
}

static WAITING_QUIT_CONFIRMATION: AtomicBool = AtomicBool::new(false);

/// The key bindings of mustr's app actions: Zed's keys for Zed's actions they stand in for.
pub fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("secondary-,", OpenSettings, None),
        KeyBinding::new("secondary-q", Quit, None),
    ]
}

// Copied from crates/zed/src/zed.rs (`quit`).
fn quit(cx: &mut App) {
    if WAITING_QUIT_CONFIRMATION.load(Ordering::Acquire) {
        return;
    }

    let should_confirm = WorkspaceSettings::get_global(cx).confirm_quit;
    cx.spawn(async move |cx| {
        let mut workspace_windows: Vec<WindowHandle<MultiWorkspace>> = cx.update(|cx| {
            cx.windows()
                .into_iter()
                .filter_map(|window| window.downcast::<MultiWorkspace>())
                .collect()
        });

        // If several windows need a save prompt, start with the active one.
        cx.update(|cx| {
            workspace_windows.sort_by_key(|window| window.is_active(cx) == Some(false));
        });

        if should_confirm && let Some(multi_workspace) = workspace_windows.first() {
            let answer = multi_workspace
                .update(cx, |_, window, cx| {
                    window.prompt(
                        PromptLevel::Info,
                        "Are you sure you want to quit?",
                        None,
                        &["Quit", "Cancel"],
                        cx,
                    )
                })
                .log_err();

            if let Some(answer) = answer {
                WAITING_QUIT_CONFIRMATION.store(true, Ordering::Release);
                let answer = answer.await.ok();
                WAITING_QUIT_CONFIRMATION.store(false, Ordering::Release);
                if answer != Some(0) {
                    return Ok(());
                }
            }
        }

        if !workspace::prepare_windows_to_quit(&workspace_windows, cx).await {
            return Ok(());
        }

        cx.update(|cx| cx.quit());
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Action, Keystroke, TestAppContext};

    /// The app menu shows the shortcuts of its actions: mustr binds its settings and quit to
    /// Zed's keys for them, cmd-, and cmd-q (ctrl- on Linux), and on macOS Zed's keymap binds
    /// cmd-h, alt-cmd-h, and cmd-m to hide and minimize, by their old names in Zed's namespace.
    #[gpui::test]
    fn zeds_keymap_binds_the_app_menus_shortcuts(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            crate::app_settings::apply_default_overrides(cx).unwrap();
            crate::keymap::load_default_keymap(cx);

            let expected: Vec<(Box<dyn Action>, &str)> = vec![
                (OpenSettings.boxed_clone(), "secondary-,"),
                (Quit.boxed_clone(), "secondary-q"),
                #[cfg(target_os = "macos")]
                (crate::macos::Hide.boxed_clone(), "cmd-h"),
                #[cfg(target_os = "macos")]
                (crate::macos::HideOthers.boxed_clone(), "alt-cmd-h"),
                #[cfg(target_os = "macos")]
                (crate::macos::Minimize.boxed_clone(), "cmd-m"),
            ];
            let key_bindings = cx.key_bindings();
            let keymap = key_bindings.borrow();
            for (action, keys) in expected {
                let keystroke = Keystroke::parse(keys).unwrap();
                assert!(
                    keymap
                        .bindings_for_action(action.as_ref())
                        .any(
                            |binding| binding.match_keystrokes(std::slice::from_ref(&keystroke))
                                == Some(false)
                        ),
                    "{} isn't bound to {keys}",
                    action.name()
                );
            }
        });
    }
}
