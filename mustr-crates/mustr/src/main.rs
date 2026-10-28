// Copied from crates/zed/src/main.rs: release builds on Windows don't open a console.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent_diff;
mod agent_review;
mod agent_review_store;
mod agent_threads;
mod app_settings;
mod data_dir;
mod home;
mod inbox;
mod keymap;
mod nav_panel;
mod navigation;
mod pull_request_review;
mod review_backend;
mod review_drafts;
mod status_bar;
mod t3_connection;
#[cfg(test)]
mod test_rpc;
mod themes;
mod thread_backend;
mod title_bar;
mod window;
mod worktree_git;

use anyhow::{Context as _, Result};
use assets::Assets;
use clap::Parser;
use client::{Client, ProxySettings, UserStore};
use db::kvp::KeyValueStore;
use fs::{Fs, RealFs};
use futures::channel::oneshot;
use git::GitHostingProviderRegistry;
use gpui::{App, AppContext as _, Application, Task};
use gpui_tokio::Tokio;
use language::LanguageRegistry;
use node_runtime::{NodeBinaryOptions, NodeRuntime};
use project::{project_settings::ProjectSettings, trusted_worktrees};
use release_channel::AppVersion;
use reqwest_client::ReqwestClient;
use session::{AppSession, Session};
use settings::{Settings as _, SettingsStore, watch_config_file};
use std::{ffi::OsString, io::IsTerminal as _, path::PathBuf, process, sync::Arc};
use util::ResultExt as _;
use workspace::{AppState, WorkspaceStore};

#[derive(Parser, Debug)]
#[command(name = "mustr", disable_version_flag = true, max_term_width = 100)]
struct Args {
    /// Sets a custom directory for all of mustr's data (settings, database,
    /// language servers, and logs).
    ///
    /// On Linux/FreeBSD, the default is `$XDG_DATA_HOME/mustr`.
    /// On macOS, the default is `~/Library/Application Support/mustr`.
    /// On Windows, the default is `%LOCALAPPDATA%\mustr`.
    #[arg(long, value_name = "DIR", verbatim_doc_comment)]
    user_data_dir: Option<String>,

    /// The T3 Code server to connect to. Overrides `MUSTR_T3_SERVER` and the
    /// `"t3": {"url": ...}` setting. The default is http://127.0.0.1:3773.
    #[arg(long, value_name = "URL", verbatim_doc_comment)]
    t3_server: Option<String>,

    /// Opens the first open pull request once the list loads (for smoke tests).
    #[arg(long, hide = true)]
    open_first_pr: bool,

    /// Opens a review of this agent thread's work once the threads load (for smoke tests).
    #[arg(long, hide = true, value_name = "THREAD_ID")]
    review_thread: Option<String>,

    /// The scope `--review-thread` opens: `changes` (the default) or `turn:N`.
    #[arg(long, hide = true, value_name = "SCOPE", value_parser = parse_review_scope)]
    thread_scope: Option<review::ReviewScope>,

    /// Opens this agent thread's chat at startup (for smoke tests and screenshots).
    #[arg(long, hide = true, value_name = "THREAD_ID")]
    open_thread: Option<String>,

    /// Opens a new agent thread's form at startup (for smoke tests and screenshots).
    #[arg(long, hide = true)]
    new_thread: bool,

    /// Relays a Git or SSH password prompt over a Unix socket, so git doesn't
    /// need netcat.
    #[cfg(not(target_os = "windows"))]
    #[arg(long, hide = true)]
    askpass: Option<String>,

    /// Prints the environment as JSON.
    #[arg(long, hide = true)]
    printenv: bool,
}

fn main() {
    #[cfg(unix)]
    util::prevent_root_execution();

    let args = Args::parse();

    // Zed's crates re-run the current executable in these modes: git_ui's
    // askpass script, and the login-shell environment capture.
    #[cfg(not(target_os = "windows"))]
    if let Some(socket) = &args.askpass {
        askpass::main(socket);
        return;
    }
    if args.printenv {
        util::shell_env::print_env();
        return;
    }

    let home_options = home::HomeOptions {
        open_first_pull_request: args.open_first_pr,
        review_thread: args.review_thread.map(|thread_id| {
            (
                thread_id,
                args.thread_scope.unwrap_or(review::ReviewScope::Changes),
            )
        }),
    };
    let startup_threads = window::StartupThreads {
        open_thread: args.open_thread.clone(),
        new_thread: args.new_thread,
    };
    let data_dir = match data_dir::init(args.user_data_dir) {
        Ok(data_dir) => data_dir,
        Err(error) => {
            eprintln!("mustr failed to launch: {error:#}");
            process::exit(1);
        }
    };

    zlog::init();
    match zlog::init_output_file(data_dir::log_file(), Some(data_dir::old_log_file())) {
        Ok(()) if std::io::stdout().is_terminal() => zlog::init_output_stdout(),
        Ok(()) => {}
        Err(error) => {
            eprintln!("Could not open log file: {error}. Logging to stdout instead");
            zlog::init_output_stdout();
        }
    }

    #[cfg(unix)]
    util::increase_open_file_limit().log_err();

    let app_version = AppVersion::load(env!("CARGO_PKG_VERSION"), None, None);
    log::info!("========== starting mustr {app_version} ==========");
    log::info!("Data directory: {}", data_dir.display());

    app_settings::ensure_settings_file(paths::settings_file())
        .context("creating the settings file")
        .log_err();
    let settings_text = std::fs::read_to_string(paths::settings_file()).ok();
    let t3_server_url = t3_connection::resolve_server_url(args.t3_server, settings_text.as_deref());
    match &t3_server_url {
        Ok(url) => log::info!("T3 server: {url}"),
        Err(error) => log::error!("{error:#}"),
    }

    // Copied from crates/zed/src/main.rs: `rope` and `sum_tree` build large
    // trees on rayon's global pool.
    rayon::ThreadPoolBuilder::new()
        .num_threads(std::thread::available_parallelism().map_or(1, |n| n.get().div_ceil(2)))
        .stack_size(10 * 1024 * 1024)
        .thread_name(|index| format!("RayonWorker{index}"))
        .build_global()
        .context("building rayon's global thread pool")
        .log_err();

    let app = Application::new_inaccessible(gpui_platform::current_platform(false))
        .with_assets(mustr_ui::MustrAssets::new(Assets))
        .with_restart_arguments(vec![
            OsString::from("--user-data-dir"),
            data_dir.as_os_str().to_owned(),
        ]);

    let app_db = db::AppDatabase::new();
    let session = app.background_executor().spawn(Session::new(
        uuid::Uuid::new_v4().to_string(),
        KeyValueStore::from_app_db(&app_db),
    ));
    let fs = RealFs::new(None, app.background_executor());
    let (user_keymap_file_rx, user_keymap_watcher) = watch_config_file(
        &app.background_executor(),
        fs.clone(),
        paths::keymap_file().clone(),
    );
    let shell_env_loaded_rx = load_login_shell_environment(&app);

    app.on_reopen(|cx| {
        if cx.windows().is_empty()
            && let Some(app_state) = AppState::try_global(cx)
        {
            window::open_home_window(app_state, cx);
        }
    });

    app.run(move |cx| {
        cx.set_global(app_db);
        init_settings(fs.clone(), app_version, cx);
        keymap::handle_keymap_file_changes(user_keymap_file_rx, user_keymap_watcher, cx);
        let app_state = match init_app_state(fs, session, shell_env_loaded_rx, cx) {
            Ok(app_state) => app_state,
            Err(error) => {
                log::error!("mustr failed to start: {error:#}");
                eprintln!("mustr failed to start: {error:#}");
                cx.quit();
                return;
            }
        };
        init_ui(&app_state, cx);
        cx.set_global(home_options);
        cx.set_global(startup_threads);
        t3_connection::init(t3_server_url, cx);
        let session = t3_connection::T3Connection::global(cx)
            .read(cx)
            .current_session();
        thread_backend::init(session, cx);
        cx.activate(true);
        window::open_home_window(app_state, cx);
    });
}

fn parse_review_scope(scope: &str) -> Result<review::ReviewScope, String> {
    match scope {
        "changes" => Ok(review::ReviewScope::Changes),
        _ => scope
            .strip_prefix("turn:")
            .and_then(|turn| turn.parse().ok())
            .map(review::ReviewScope::Turn)
            .ok_or_else(|| format!("{scope:?} isn't `changes` or `turn:N`")),
    }
}

// Copied from crates/zed/src/main.rs. Apps launched from a desktop don't get the
// login shell's environment (PATH and so on), which git and language servers need.
fn load_login_shell_environment(app: &Application) -> oneshot::Receiver<()> {
    let (shell_env_loaded_tx, shell_env_loaded_rx) = oneshot::channel();
    if !std::io::stdout().is_terminal() {
        app.background_executor()
            .spawn(async {
                #[cfg(unix)]
                util::load_login_shell_environment().await.log_err();
                shell_env_loaded_tx.send(()).ok();
            })
            .detach();
    }
    shell_env_loaded_rx
}

fn init_settings(fs: Arc<dyn Fs>, app_version: semver::Version, cx: &mut App) {
    let trusted_paths = workspace::WorkspaceDb::global(cx)
        .fetch_trusted_worktrees()
        .context("fetching trusted worktrees")
        .log_err()
        .unwrap_or_default();
    trusted_worktrees::init(trusted_paths, cx);
    menu::init();
    zed_actions::init();
    release_channel::init(app_version, cx);
    gpui_tokio::init(cx);
    settings::init(cx);
    // Before anything reads settings, so no Zed default (telemetry included)
    // is ever in effect.
    app_settings::apply_default_overrides(cx)
        .context("applying mustr's default settings")
        .log_err();
    zlog_settings::init(cx);
    app_settings::watch_settings_files(fs, cx);
}

/// Creates the services every workspace needs. Zed's sign-in, collab, hosted
/// AI, telemetry upload, and auto-update are never initialized.
fn init_app_state(
    fs: Arc<dyn Fs>,
    session: Task<Session>,
    shell_env_loaded_rx: oneshot::Receiver<()>,
    cx: &mut App,
) -> Result<Arc<AppState>> {
    let user_agent = format!(
        "mustr/{} ({}; {})",
        AppVersion::global(cx),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let proxy_url = ProxySettings::get_global(cx).proxy_url();
    let http = {
        let _guard = Tokio::handle(cx).enter();
        ReqwestClient::proxy_and_user_agent(proxy_url, &user_agent)
            .context("starting the HTTP client")?
    };
    cx.set_http_client(Arc::new(http));
    <dyn Fs>::set_global(fs.clone(), cx);

    GitHostingProviderRegistry::set_global(Arc::new(GitHostingProviderRegistry::new()), cx);
    git_hosting_providers::init(cx);

    // mustr makes no requests to any zed.dev host, because Zed's terms forbid
    // using non-public APIs of its service. So mustr doesn't initialize Zed's
    // extension host (`extension_host`, `language_extension`,
    // `theme_extension`): `ExtensionStore` checks api.zed.dev/extensions/updates
    // on every launch, and no setting turns that off. Built-in languages come
    // from the `languages` crate and bundled themes from `theme_settings`, so
    // neither needs it. Dev extensions do need it, and installing one also
    // needs `extensions_ui` and network access to build it, so mustr doesn't
    // support them.
    //
    // `Project` and `Workspace` still require a `Client`. mustr never signs it
    // in or connects it, and telemetry is off, so its zed.dev base URL is never
    // used.
    let client = Client::production(cx);
    cx.set_http_client(client.http_client());

    let mut languages = LanguageRegistry::new(cx.background_executor().clone());
    languages.set_language_server_download_dir(paths::languages_dir().clone());
    let languages = Arc::new(languages);
    let node_runtime = NodeRuntime::new(
        client.http_client(),
        Some(shell_env_loaded_rx),
        watch_node_options(cx),
    );
    ui::on_new_scrollbars::<SettingsStore>(cx);
    languages::init(languages.clone(), fs.clone(), node_runtime.clone(), cx);

    let user_store = cx.new(|cx| UserStore::new(client.clone(), cx));
    let workspace_store = cx.new(|cx| WorkspaceStore::new(client.clone(), cx));
    Client::set_global(client.clone(), cx);
    project::Project::init(&client, cx);
    feature_flags::FeatureFlagStore::init(cx);

    let session = cx.foreground_executor().block_on(session);
    let app_state = Arc::new(AppState {
        languages,
        client,
        user_store,
        workspace_store,
        fs,
        build_window_options: window::build_window_options,
        node_runtime,
        session: cx.new(|cx| AppSession::new(session, cx)),
    });
    AppState::set_global(app_state.clone(), cx);
    theme_settings::init(theme::LoadThemes::All(Box::new(Assets)), cx);

    Ok(app_state)
}

// Copied from crates/zed/src/main.rs (the `NodeBinaryOptions` observer in `main`).
fn watch_node_options(cx: &mut App) -> watch::Receiver<Option<NodeBinaryOptions>> {
    let (mut options_tx, options_rx) = watch::channel(None);
    cx.observe_global::<SettingsStore>(move |cx| {
        let settings = &ProjectSettings::get_global(cx).node;
        let options = NodeBinaryOptions {
            allow_path_lookup: !settings.ignore_system_version,
            allow_binary_download: true,
            use_paths: settings.path.as_ref().map(|node_path| {
                let node_path = PathBuf::from(shellexpand::tilde(node_path).as_ref());
                let npm_path = settings
                    .npm_path
                    .as_ref()
                    .map(|path| PathBuf::from(shellexpand::tilde(&path).as_ref()));
                let npm_path = npm_path.unwrap_or_else(|| {
                    node_path
                        .parent()
                        .map_or_else(|| PathBuf::from("npm"), |parent| parent.join("npm"))
                });
                (node_path, npm_path)
            }),
        };
        options_tx.send(Some(options)).log_err();
    })
    .detach();
    options_rx
}

/// Initializes the Zed features mustr keeps. See the init inventory in
/// mustr-docs/ZED-INTEGRATION.md for what's left out and why.
fn init_ui(app_state: &Arc<AppState>, cx: &mut App) {
    mustr_ui::init(cx);
    command_palette::init(cx);
    themes::init(app_state.fs.clone(), app_state.languages.clone(), cx);
    editor::init(cx);
    diagnostics::init(cx);
    workspace::init(app_state.clone(), cx);
    ui_prompt::init(cx);
    go_to_line::init(cx);
    file_finder::init(cx);
    search::init(cx);
    lsp_locations::init(cx);
    vim::init(cx);
    terminal_view::init(cx);
    theme_selector::init(cx);
    git_ui::init(cx);
    inspector_ui::init(app_state.clone(), cx);
    which_key::init(cx);
    app_settings::observe_app_settings(cx);
    title_bar::init(cx);
    status_bar::init(cx);
    thread_chat::init(cx);
    window::init(cx);
}
