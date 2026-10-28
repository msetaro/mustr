use crate::{app_settings::INITIAL_SETTINGS, home::HomeView, nav_panel::NavPanel};
use gpui::{
    App, AppContext as _, Context, Entity, Global, Menu, MenuItem, OsAction, PromptLevel,
    TaskExt as _, TitlebarOptions, Window, WindowHandle, WindowKind, WindowOptions, point, px,
};
use review::{OpenThreadChat, ToggleFileTree};
use settings::{Settings as _, SettingsStore};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use util::ResultExt as _;
use uuid::Uuid;
use workspace::{
    AppState, CloseWindow, MultiWorkspace, OpenOptions, Pane, Workspace, WorkspaceSettings,
    create_and_open_local_file, with_active_or_new_workspace,
};
use zed_actions::{OpenKeymapFile, OpenSettingsFile, Quit};

/// The Wayland app ID and X11 `WM_CLASS`. It must match the `.desktop` file name.
const APP_ID: &str = "dev.mustr.mustr";

const INITIAL_KEYMAP: &str =
    "// mustr key bindings\n//\n// mustr uses Zed's keymap format.\n[\n]\n";

#[cfg(target_os = "macos")]
gpui::actions!(
    mustr,
    [
        /// Hides mustr.
        Hide,
        /// Hides all other applications.
        HideOthers,
        /// Shows all hidden applications.
        ShowAll,
    ]
);

pub fn init(cx: &mut App) {
    #[cfg(target_os = "macos")]
    cx.on_action(|_: &Hide, cx| cx.hide())
        .on_action(|_: &HideOthers, cx| cx.hide_other_apps())
        .on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    cx.on_action(quit)
        .on_action(|_: &OpenSettingsFile, cx| {
            open_config_file(paths::settings_file(), INITIAL_SETTINGS, cx)
        })
        .on_action(|_: &OpenKeymapFile, cx| {
            open_config_file(paths::keymap_file(), INITIAL_KEYMAP, cx)
        });

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
        add_search_bars(workspace, &center_pane, window, cx);
        let workspace_handle = cx.entity();

        // Zed adds its panels in `initialize_panels` (crates/zed/src/zed.rs); mustr's are
        // created synchronously, so dock restoration can finish right away.
        let nav_panel = cx.new(|cx| NavPanel::new(&workspace_handle, window, cx));
        workspace.add_panel(nav_panel.clone(), window, cx);
        workspace.finish_dock_restoration(cx);
        // The window's first item may already be open.
        let active_item = workspace.active_item(cx);
        let left_dock = workspace.left_dock().clone();
        nav_panel.update(cx, |nav_panel, cx| {
            nav_panel.active_item_changed(active_item.as_deref(), &left_dock, window, cx)
        });
        workspace.register_action(|workspace, _: &ToggleFileTree, window, cx| {
            workspace.toggle_panel_focus::<NavPanel>(window, cx);
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
                    add_search_bars(workspace, pane, window, cx);
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
        },
    );
    cx.spawn(async move |cx| {
        if let Err(error) = open_window.await {
            log::error!("Failed to open a window: {error:#}");
            eprintln!("mustr failed to open a window: {error:#}");
            cx.update(|cx| cx.quit());
        }
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
// ID and icon.
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
            traffic_light_position: Some(point(px(9.0), px(9.0))),
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

pub fn app_menus() -> Vec<Menu> {
    vec![
        Menu::new("mustr").items(vec![
            MenuItem::submenu(Menu::new("Settings").items([
                MenuItem::action("Open Settings File", OpenSettingsFile),
                MenuItem::action("Open Keymap File", OpenKeymapFile),
                MenuItem::separator(),
                MenuItem::action(
                    "Select Theme...",
                    zed_actions::theme_selector::Toggle::default(),
                ),
                MenuItem::action(
                    "Select Icon Theme...",
                    zed_actions::icon_theme_selector::Toggle::default(),
                ),
            ])),
            MenuItem::separator(),
            #[cfg(target_os = "macos")]
            MenuItem::os_submenu("Services", gpui::SystemMenuType::Services),
            #[cfg(target_os = "macos")]
            MenuItem::separator(),
            #[cfg(target_os = "macos")]
            MenuItem::action("Hide mustr", Hide),
            #[cfg(target_os = "macos")]
            MenuItem::action("Hide Others", HideOthers),
            #[cfg(target_os = "macos")]
            MenuItem::action("Show All", ShowAll),
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
// mustr only needs search.
fn add_search_bars(
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
        })
    });
}

static WAITING_QUIT_CONFIRMATION: AtomicBool = AtomicBool::new(false);

// Copied from crates/zed/src/zed.rs (`quit`).
fn quit(_: &Quit, cx: &mut App) {
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

// Copied from crates/zed/src/zed.rs (`open_settings_file`), for local
// workspaces only.
fn open_config_file(abs_path: &'static Path, initial_content: &'static str, cx: &mut App) {
    with_active_or_new_workspace(cx, move |_, window, cx| {
        cx.spawn_in(window, async move |workspace, cx| {
            let project = workspace.read_with(cx, |workspace, _| workspace.project().clone())?;
            // A dedicated worktree for the config directory keeps language
            // servers from restarting each time a settings file is reopened.
            project
                .update(cx, |project, cx| {
                    project.find_or_create_worktree(paths::config_dir().as_path(), false, cx)
                })
                .await?;
            workspace
                .update_in(cx, |_, window, cx| {
                    create_and_open_local_file(abs_path, window, cx, move || initial_content.into())
                })?
                .await?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    });
}
