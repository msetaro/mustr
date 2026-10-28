//! Which T3 engine mustr's agents run on (`"t3": {"mode": …}` in mustr's settings), and mustr's
//! own copy of it.
//!
//! - `"installed"` (the default): the user's own T3 Code (`~/.t3`), so their projects and
//!   threads appear in mustr. mustr starts its background service when it isn't running,
//!   installs T3 Code the way T3's installer does when it's missing, and offers `t3 update` when
//!   its protocol is older than mustr's.
//! - `"embedded"`: mustr's own hidden copy of the pinned T3 release, in mustr's data directory,
//!   on a port of its own. Also the fallback when the installed T3 can't be used.
//! - `"external"`: a T3 server at a URL (`"t3": {"url": …}` or `--t3-server`), paired by pasting
//!   a credential. For development and T3 servers on other machines.
//!
//! The embedded engine never touches the user's T3 Code: its runtime is
//! `<data dir>/t3-runtime/<version>`, its home `<data dir>/t3-home`, and its port a free one
//! that isn't T3's 3773.

use anyhow::{Context as _, Result, anyhow};
use futures::{FutureExt as _, StreamExt as _, channel::mpsc};
use gpui::{AppContext as _, AsyncApp, Context, SharedString, Subscription, Task, WeakEntity};
use http_client::{HttpClient, Url};
use parking_lot::Mutex;
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use t3_runtime::{
    Sleep,
    engine::{self, Backoff, BootstrapSecret, EngineLaunch, EngineLock, EngineProcess},
    install::{InstallStep, Installer},
    release::Target,
};
use util::ResultExt as _;

/// Overrides the mode from settings (the `--t3-mode` flag overrides both).
pub const MODE_ENV: &str = "MUSTR_T3_MODE";
const READY_TIMEOUT: Duration = Duration::from_secs(90);
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(250);
const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(15);
const HEALTH_CHECK_FAILURES: u32 = 3;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum T3Mode {
    #[default]
    Installed,
    Embedded,
    External,
}

impl T3Mode {
    pub fn parse(mode: &str) -> Result<Self> {
        match mode.trim() {
            "installed" => Ok(Self::Installed),
            "embedded" => Ok(Self::Embedded),
            "external" => Ok(Self::External),
            other => Err(anyhow!(
                "\"t3\": {{\"mode\": {other:?}}} isn't \"installed\", \"embedded\", or \"external\""
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Installed => "installed",
            Self::Embedded => "embedded",
            Self::External => "external",
        }
    }
}

/// Picks the mode: a server URL from `--t3-server` or `MUSTR_T3_SERVER` means external; then the
/// `--t3-mode` flag, `MUSTR_T3_MODE`, `"t3": {"mode"}` in mustr's settings, and `"installed"`.
pub fn resolve_mode(
    cli_mode: Option<&str>,
    server_url_given: bool,
    settings_text: Option<&str>,
) -> Result<T3Mode> {
    if server_url_given {
        return Ok(T3Mode::External);
    }
    let from_env = std::env::var(MODE_ENV)
        .ok()
        .filter(|mode| !mode.trim().is_empty());
    let from_settings = settings_text.and_then(mode_from_settings);
    match cli_mode.map(str::to_string).or(from_env).or(from_settings) {
        Some(mode) => T3Mode::parse(&mode),
        None => Ok(T3Mode::default()),
    }
}

// Zed's settings types ignore keys they don't know, so mustr keeps its own in the same file.
fn mode_from_settings(text: &str) -> Option<String> {
    let settings: Value = settings::parse_json_with_comments(text).log_err()?;
    settings
        .get("t3")?
        .get("mode")?
        .as_str()
        .map(str::to_string)
}

/// The `Sleep` the runtime crate waits with: GPUI's executor timer.
pub fn executor_sleep(cx: &AsyncApp) -> Sleep {
    let executor = cx.background_executor().clone();
    Arc::new(move |duration| executor.timer(duration).boxed())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineState {
    Idle,
    /// Downloading or unpacking the runtime, with the step.
    Installing(SharedString),
    Starting,
    Restarting {
        attempt: u32,
        reason: SharedString,
    },
    Ready {
        url: Url,
        version: Option<SharedString>,
        /// Bumped each time the engine (re)starts, so the connection can tell runs apart.
        run: usize,
    },
    Failed(SharedString),
}

struct EnginePaths {
    runtime_dir: PathBuf,
    base_dir: PathBuf,
    lock_path: PathBuf,
    log_file: PathBuf,
}

impl EnginePaths {
    fn new() -> Self {
        let data_dir = paths::data_dir();
        Self {
            runtime_dir: data_dir.join("t3-runtime"),
            base_dir: data_dir.join("t3-home"),
            lock_path: data_dir.join("t3-engine.lock"),
            log_file: crate::data_dir::logs_dir().join("t3-engine.log"),
        }
    }
}

/// mustr's own copy of T3's engine: installed on first use, started at launch on a background
/// executor, restarted with backoff when it crashes, and stopped when mustr quits.
pub struct EmbeddedEngine {
    paths: Arc<EnginePaths>,
    secret: BootstrapSecret,
    state: EngineState,
    process: Arc<Mutex<Option<EngineProcess>>>,
    lock: Arc<Mutex<Option<EngineLock>>>,
    stopping: Arc<AtomicBool>,
    runs: usize,
    supervisor: Option<Task<()>>,
    _quit: Subscription,
}

impl EmbeddedEngine {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let quit = cx.on_app_quit(|this, _| {
            this.stop_blocking();
            async {}
        });
        Self {
            paths: Arc::new(EnginePaths::new()),
            secret: BootstrapSecret::generate(),
            state: EngineState::Idle,
            process: Arc::default(),
            lock: Arc::default(),
            stopping: Arc::default(),
            runs: 0,
            supervisor: None,
            _quit: quit,
        }
    }

    pub fn state(&self) -> &EngineState {
        &self.state
    }

    /// The token the running engine accepts for the `environment-bootstrap` exchange.
    pub fn bootstrap_token(&self) -> String {
        self.secret.current_token()
    }

    pub fn log_file(&self) -> PathBuf {
        self.paths.log_file.clone()
    }

    /// Starts the engine unless it is already starting or running.
    pub fn start(&mut self, cx: &mut Context<Self>) {
        if self.supervisor.is_some() {
            return;
        }
        self.stopping.store(false, Ordering::SeqCst);
        self.supervisor = Some(cx.spawn(async move |this, cx| {
            let result = supervise(this.clone(), cx).await;
            this.update(cx, |this, cx| {
                this.supervisor = None;
                if let Err(error) = result {
                    log::error!("the agents engine failed: {error:#}");
                    this.state = EngineState::Failed(format!("{error:#}").into());
                } else if !matches!(this.state, EngineState::Failed(_)) {
                    this.state = EngineState::Idle;
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Stops the engine if it's running, and starts it again.
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        self.stopping.store(true, Ordering::SeqCst);
        self.supervisor = None;
        let process = self.process.lock().take();
        let sleep = executor_sleep(&cx.to_async());
        let stopped = cx.background_spawn(async move {
            if let Some(mut process) = process {
                process.stop(engine::STOP_GRACE, &sleep).await;
            }
        });
        self.set_state(EngineState::Starting, cx);
        cx.spawn(async move |this, cx| {
            stopped.await;
            this.update(cx, |this, cx| this.start(cx)).ok();
        })
        .detach();
    }

    fn set_state(&mut self, state: EngineState, cx: &mut Context<Self>) {
        if self.state != state {
            self.state = state;
            cx.notify();
        }
    }

    /// Stops the engine and waits for it, so no engine outlives mustr. GPUI gives quit handlers
    /// only a moment, so this blocks; the engine gets [`engine::STOP_GRACE`] to stop its agents.
    fn stop_blocking(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        let process = self.process.lock().take();
        if let Some(mut process) = process {
            let pid = process.pid();
            let status = process.stop_blocking(engine::STOP_GRACE);
            log::info!("stopped the agents engine (pid {pid}): {status:?}");
        }
        self.lock.lock().take();
    }
}

/// Installs the runtime if needed, then runs the engine until mustr stops it, restarting it
/// with backoff when it exits on its own.
async fn supervise(this: WeakEntity<EmbeddedEngine>, cx: &mut AsyncApp) -> Result<()> {
    let (paths, secret, process_slot, lock_slot, stopping) = this.read_with(cx, |this, _| {
        (
            this.paths.clone(),
            this.secret.clone(),
            this.process.clone(),
            this.lock.clone(),
            this.stopping.clone(),
        )
    })?;
    let set_state = |state: EngineState, cx: &mut AsyncApp| {
        this.update(cx, |this, cx| this.set_state(state, cx)).ok();
    };
    let http = cx.update(|cx| cx.http_client());
    let sleep = executor_sleep(cx);

    if lock_slot.lock().is_none() {
        let lock = EngineLock::try_acquire(&paths.lock_path)?.with_context(|| {
            format!(
                "Another mustr is running its agents engine from {}. Quit it, or start this \
                 one with another --user-data-dir.",
                paths::data_dir().display()
            )
        })?;
        *lock_slot.lock() = Some(lock);
    }

    let target = Target::current()?;
    let version = t3_client::pinned_server_version();
    let runtime = {
        let (progress_tx, mut progress_rx) = mpsc::unbounded::<InstallStep>();
        let install = cx.background_spawn({
            let installer = Installer::new(http.clone(), target, sleep.clone());
            let runtime_dir = paths.runtime_dir.clone();
            async move {
                let progress = move |step: InstallStep| {
                    progress_tx.unbounded_send(step).ok();
                };
                installer.ensure(&runtime_dir, version, &progress).await
            }
        });
        let mut install = install.fuse();
        loop {
            futures::select! {
                step = progress_rx.next() => if let Some(step) = step {
                    set_state(EngineState::Installing(step.describe().into()), cx);
                },
                result = install => break result.context("setting up the agents engine")?,
            }
        }
    };
    cx.background_spawn({
        let runtime_dir = paths.runtime_dir.clone();
        async move { t3_runtime::install::prune_versions(&runtime_dir, version, target) }
    })
    .await
    .log_err();
    cx.background_spawn({
        let base_dir = paths.base_dir.clone();
        async move { engine::stop_stale_engine(&base_dir, engine::STOP_GRACE) }
    })
    .await;

    let mut backoff = Backoff::default();
    loop {
        if stopping.load(Ordering::SeqCst) {
            return Ok(());
        }
        if backoff.attempts() == 0 {
            set_state(EngineState::Starting, cx);
        }
        let launch = EngineLaunch {
            executable: runtime.executable.clone(),
            base_dir: paths.base_dir.clone(),
            port: engine::pick_port()?,
            log_file: paths.log_file.clone(),
        };
        let url = launch.url();
        let started = Instant::now();
        let reason = match run_once(
            &this,
            &launch,
            &secret,
            &process_slot,
            &stopping,
            http.as_ref(),
            &sleep,
            cx,
        )
        .await
        {
            Ok(RunEnd::Stopped) => return Ok(()),
            Ok(RunEnd::Fatal(reason)) => {
                return Err(anyhow!(
                    "The agents engine can't start: {reason}. See {}.",
                    paths.log_file.display()
                ));
            }
            Ok(RunEnd::Exited(reason)) => reason,
            Err(error) => format!("{error:#}"),
        };
        // Whatever happened, nothing of this run keeps running.
        let process = process_slot.lock().take();
        if let Some(mut process) = process {
            cx.background_spawn({
                let sleep = sleep.clone();
                async move { process.stop(engine::STOP_GRACE, &sleep).await }
            })
            .await;
        }
        if stopping.load(Ordering::SeqCst) {
            return Ok(());
        }
        log::warn!("the agents engine at {url} stopped: {reason}");
        if started.elapsed() >= Backoff::STABLE_AFTER {
            backoff.reset();
        }
        if backoff.exhausted() {
            return Err(anyhow!(
                "The agents engine keeps stopping ({reason}). See {}.",
                paths.log_file.display()
            ));
        }
        let delay = backoff.next_delay();
        set_state(
            EngineState::Restarting {
                attempt: backoff.attempts(),
                reason: reason.into(),
            },
            cx,
        );
        cx.background_executor().timer(delay).await;
    }
}

enum RunEnd {
    /// mustr asked it to stop.
    Stopped,
    /// It exited or stopped answering; worth restarting.
    Exited(String),
    /// It can't run (another server owns its base dir); restarting won't help.
    Fatal(String),
}

#[allow(clippy::too_many_arguments)]
async fn run_once(
    this: &WeakEntity<EmbeddedEngine>,
    launch: &EngineLaunch,
    secret: &BootstrapSecret,
    process_slot: &Arc<Mutex<Option<EngineProcess>>>,
    stopping: &Arc<AtomicBool>,
    http: &dyn HttpClient,
    sleep: &Sleep,
    cx: &mut AsyncApp,
) -> Result<RunEnd> {
    let envelope = launch.envelope(secret, engine::unix_millis())?;
    let process = cx
        .background_spawn({
            let launch = launch.clone();
            async move { EngineProcess::spawn(&launch, envelope).await }
        })
        .await?;
    let pid = process.pid();
    log::info!(
        "started the agents engine (pid {pid}) at {}, logging to {}",
        launch.url(),
        launch.log_file.display()
    );
    *process_slot.lock() = Some(process);
    let has_exited = || {
        process_slot
            .lock()
            .as_mut()
            .and_then(EngineProcess::try_exit)
    };

    let url = launch.url();
    let descriptor = engine::wait_until_ready(http, &url, has_exited, READY_TIMEOUT, sleep).await;
    let descriptor = match descriptor {
        Ok(descriptor) => descriptor,
        Err(error) => {
            if let Some(status) = has_exited()
                && engine::exit_is_fatal(&status)
            {
                return Ok(RunEnd::Fatal(engine::describe_exit(&status)));
            }
            return Ok(RunEnd::Exited(format!("{error:#}")));
        }
    };
    if let Err(error) = t3_client::check_protocol_compatibility(&descriptor) {
        return Ok(RunEnd::Fatal(format!("{error:#}")));
    }
    this.update(cx, |this, cx| {
        this.runs += 1;
        let run = this.runs;
        this.set_state(
            EngineState::Ready {
                url: url.clone(),
                version: descriptor.server_version.clone().map(Into::into),
                run,
            },
            cx,
        );
    })?;
    log::info!(
        "the agents engine is ready: T3 {} at {url}",
        descriptor.server_version.as_deref().unwrap_or("?")
    );

    let mut since_health_check = Duration::ZERO;
    let mut failed_checks = 0;
    loop {
        cx.background_executor().timer(EXIT_POLL_INTERVAL).await;
        if stopping.load(Ordering::SeqCst) {
            return Ok(RunEnd::Stopped);
        }
        if let Some(status) = has_exited() {
            return Ok(if engine::exit_is_fatal(&status) {
                RunEnd::Fatal(engine::describe_exit(&status))
            } else {
                RunEnd::Exited(format!("it exited ({})", engine::describe_exit(&status)))
            });
        }
        since_health_check += EXIT_POLL_INTERVAL;
        if since_health_check >= HEALTH_CHECK_INTERVAL {
            since_health_check = Duration::ZERO;
            if engine::answers(http, &url, sleep).await {
                failed_checks = 0;
            } else {
                failed_checks += 1;
                log::warn!("the agents engine didn't answer a health check ({failed_checks})");
                if failed_checks >= HEALTH_CHECK_FAILURES {
                    return Ok(RunEnd::Exited("it stopped answering".to_string()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_resolve_in_order() {
        let settings = r#"{
            // mustr's own key; Zed ignores it.
            "t3": { "mode": "embedded" },
        }"#;
        // A server URL always means external.
        assert_eq!(
            resolve_mode(Some("embedded"), true, Some(settings)).unwrap(),
            T3Mode::External
        );
        assert_eq!(
            resolve_mode(Some("external"), false, Some(settings)).unwrap(),
            T3Mode::External
        );
        // The env var is process-global; only check what doesn't depend on it.
        if std::env::var_os(MODE_ENV).is_none() {
            assert_eq!(
                resolve_mode(None, false, Some(settings)).unwrap(),
                T3Mode::Embedded
            );
            assert_eq!(resolve_mode(None, false, None).unwrap(), T3Mode::Installed);
            assert_eq!(
                resolve_mode(None, false, Some("{ not json")).unwrap(),
                T3Mode::Installed
            );
            assert!(resolve_mode(None, false, Some(r#"{"t3": {"mode": "cloud"}}"#)).is_err());
        }
        for mode in [T3Mode::Installed, T3Mode::Embedded, T3Mode::External] {
            assert_eq!(T3Mode::parse(mode.as_str()).unwrap(), mode);
        }
    }
}
