//! mustr's updater, for its macOS app (`mustr-docs/RELEASING.md`).
//!
//! Shortly after launch and every few hours, it fetches the latest release's `manifest.json` and
//! `manifest.json.sig` from GitHub, checks the signature with the public key mustr is built with
//! (`resources/update-signing-key.pub`) before reading the manifest, and compares versions. For a
//! newer release, it downloads the zip into `<data dir>/updates/`, checks its size and SHA-256,
//! extracts it with `ditto`, checks the app's bundle identifier and version, and swaps it in for
//! the running bundle ([`bundle`]). Then the status bar offers "Restart to update". It never
//! restarts on its own, and it touches only its own bundle and `<data dir>/updates/`.
//!
//! It runs only in release builds made by `script/mustr-package-mac` and started from an app bundle
//! on macOS. A local `script/mustr-bundle-mac --release` build doesn't replace itself with the
//! published release. `"updates": {"enabled": false}` in the settings stops the automatic checks;
//! mustr › Check for Updates… still checks.
//!
//! The steps follow Zed's updater (`crates/auto_update/src/auto_update.rs`: `AutoUpdater::poll`
//! and `update`, `download_release`, `install_release_macos`), which mustr doesn't use: it reads
//! zed.dev's release API, and copies a disk image over the bundle with rsync.

pub mod bundle;
#[cfg(test)]
mod tests;

use crate::app_settings::UpdateSettings;
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use command_palette_hooks::CommandPaletteFilter;
use futures::{AsyncReadExt as _, FutureExt as _, StreamExt as _, future::BoxFuture};
use gpui::{
    Action as _, App, AppContext as _, AsyncApp, Context, Entity, Global, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Subscription, Task,
    WeakEntity, Window, div, px,
};
use http_client::{AsyncBody, HttpClient};
use mustr_release::{Asset, Manifest, PublicKey};
use mustr_ui::{Chip, tokens};
use semver::Version;
use std::{
    any::TypeId,
    io::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use util::ResultExt as _;
use workspace::{
    HideStatusItem, ItemHandle, StatusItemView, Toast, Workspace, notifications::NotificationId,
};

gpui::actions!(
    mustr,
    [
        /// Checks for a newer mustr now, and installs it.
        CheckForUpdates,
        /// Restarts mustr into the update it installed.
        RestartToUpdate,
    ]
);

/// Where the latest release's files are. GitHub redirects `latest/download/<file>` to the newest
/// release that isn't a draft or a prerelease.
pub const DEFAULT_BASE_URL: &str = "https://github.com/msetaro/mustr/releases/latest/download";
/// Points the updater somewhere else, such as a local server, for tests.
pub const BASE_URL_ENV: &str = "MUSTR_UPDATE_BASE_URL";
/// Debug builds only: treat this `.app` as the running bundle, which turns the updater on in a
/// debug build on any platform. For screenshots and manual checks against a local server.
const DEBUG_BUNDLE_ENV: &str = "MUSTR_DEBUG_UPDATE_BUNDLE";
/// Debug builds only: a public key (base64) to use instead of the one mustr is built with.
const DEBUG_PUBLIC_KEY_ENV: &str = "MUSTR_DEBUG_UPDATE_PUBLIC_KEY";

const PUBLIC_KEY_FILE: &str = include_str!("../resources/update-signing-key.pub");
/// `script/mustr-package-mac` sets `MUSTR_PACKAGED` when it builds what it ships.
const PACKAGED: bool = option_env!("MUSTR_PACKAGED").is_some();
/// The platform in the manifest that this build installs.
const PLATFORM: &str = mustr_release::PLATFORM_MACOS_AARCH64;
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(20);
const CHECK_INTERVAL: Duration = Duration::from_secs(4 * 60 * 60);
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_SIGNATURE_BYTES: u64 = 1024;

pub fn init(cx: &mut App) {
    let executable = std::env::current_exe().ok();
    let executable = executable
        .as_deref()
        .map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
    let setup = setup(Environment {
        macos: cfg!(target_os = "macos"),
        release_build: !cfg!(debug_assertions),
        packaged: PACKAGED,
        executable: executable.as_deref(),
        public_key_file: PUBLIC_KEY_FILE,
        debug_bundle: debug_variable(DEBUG_BUNDLE_ENV).map(PathBuf::from),
        debug_public_key: debug_variable(DEBUG_PUBLIC_KEY_ENV),
    });
    let config = setup.map(|setup| Config {
        http: cx.http_client(),
        base_url: base_url(std::env::var(BASE_URL_ENV).ok()),
        public_key: setup.public_key,
        current_version: Version::parse(env!("CARGO_PKG_VERSION"))
            .unwrap_or_else(|_| Version::new(0, 0, 0)),
        bundle: setup.bundle,
        updates_dir: paths::data_dir().join("updates"),
        extract: Arc::new(|zip, destination| extract_zip(zip, destination).boxed()),
    });
    let automatic = std::fs::read_to_string(paths::settings_file())
        .ok()
        .and_then(|text| UpdateSettings::from_settings_text(&text).log_err())
        .unwrap_or_default()
        .enabled;
    init_with(config, automatic, cx);
    watch_settings(cx);
}

/// An environment variable that only debug builds read.
fn debug_variable(name: &str) -> Option<String> {
    if !cfg!(debug_assertions) {
        return None;
    }
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Starts the updater with `config`, or off for the reason given.
pub(crate) fn init_with(config: Result<Config, SharedString>, automatic: bool, cx: &mut App) {
    match &config {
        Ok(config) => log::info!(
            "Updates: on for {} (mustr {}), from {}{}",
            config.bundle.display(),
            config.current_version,
            config.base_url,
            if automatic {
                ""
            } else {
                "; automatic checks are off in the settings"
            }
        ),
        Err(reason) => log::info!("Updates: off, because {reason}"),
    }
    let updater = cx.new(|cx| Updater::new(config, automatic, cx));
    cx.set_global(GlobalUpdater(updater));
    show_restart_action(false, cx);
    cx.on_action(|_: &RestartToUpdate, cx| restart_to_update(cx));

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        workspace.register_action(|workspace, _: &CheckForUpdates, _, cx| {
            let reporter = workspace.weak_handle();
            if let Some(updater) = Updater::global(cx) {
                updater.update(cx, |updater, cx| updater.check(Some(reporter), cx));
            }
        });
        let Some(window) = window else {
            return;
        };
        let Some(updater) = Updater::global(cx) else {
            return;
        };
        let item = cx.new(|cx| UpdateStatusItem::new(updater, cx));
        workspace.status_bar().update(cx, |status_bar, cx| {
            status_bar.add_right_item(item, window, cx);
        });
    })
    .detach();
}

/// Follows `"updates"` in the settings file. An invalid file keeps the last setting.
fn watch_settings(cx: &mut App) {
    let (mut contents, watcher) = settings::watch_config_file(
        cx.background_executor(),
        <dyn fs::Fs>::global(cx),
        paths::settings_file().clone(),
    );
    cx.spawn(async move |cx| {
        let _watcher = watcher;
        while let Some(text) = contents.next().await {
            let Some(settings) = UpdateSettings::from_settings_text(&text).log_err() else {
                continue;
            };
            cx.update(|cx| {
                if let Some(updater) = Updater::global(cx) {
                    updater.update(cx, |updater, _| updater.automatic = settings.enabled);
                }
            });
        }
    })
    .detach();
}

/// What decides whether mustr updates itself.
pub(crate) struct Environment<'a> {
    pub macos: bool,
    pub release_build: bool,
    /// Built by `script/mustr-package-mac` ([`PACKAGED`]).
    pub packaged: bool,
    pub executable: Option<&'a Path>,
    pub public_key_file: &'a str,
    /// Debug builds only ([`DEBUG_BUNDLE_ENV`]).
    pub debug_bundle: Option<PathBuf>,
    /// Debug builds only ([`DEBUG_PUBLIC_KEY_ENV`]).
    pub debug_public_key: Option<String>,
}

#[derive(Debug, PartialEq)]
pub(crate) struct Setup {
    pub bundle: PathBuf,
    pub public_key: PublicKey,
}

/// The bundle to update and the key to check releases with, or why mustr doesn't update itself.
pub(crate) fn setup(environment: Environment) -> Result<Setup, SharedString> {
    let bundle = match environment.debug_bundle {
        Some(bundle) => bundle,
        None => {
            if !environment.macos {
                return Err("mustr updates itself only in its macOS app".into());
            }
            if !environment.release_build {
                return Err("debug builds don't update themselves".into());
            }
            if !environment.packaged {
                return Err(
                    "only the builds script/mustr-package-mac makes update themselves; \
                            this one was built locally"
                        .into(),
                );
            }
            let executable = environment
                .executable
                .ok_or_else(|| SharedString::from("mustr couldn't find its own executable"))?;
            let Some(bundle) = bundle::bundle_of_executable(executable) else {
                return Err(format!(
                    "mustr isn't running from an app bundle ({})",
                    executable.display()
                )
                .into());
            };
            bundle
        }
    };
    if bundle::is_translocated(&bundle) {
        return Err(
            "macOS runs this copy of mustr from a temporary, read-only place (App \
                    Translocation); move mustr.app to Applications to get updates"
                .into(),
        );
    }
    let public_key = match environment.debug_public_key {
        Some(key) => PublicKey::from_base64(&key).map(Some),
        None => PublicKey::from_file_text(environment.public_key_file),
    };
    match public_key {
        Ok(Some(public_key)) => Ok(Setup { bundle, public_key }),
        Ok(None) => Err("this build has no update-signing key \
                         (resources/update-signing-key.pub is still the placeholder)"
            .into()),
        Err(error) => Err(format!("this build's update-signing key is invalid: {error:#}").into()),
    }
}

/// The release folder to read from: [`DEFAULT_BASE_URL`], or the override, without a trailing
/// slash.
pub(crate) fn base_url(override_url: Option<String>) -> String {
    override_url
        .map(|url| url.trim().trim_end_matches('/').to_string())
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
}

/// Extracts a release zip into `destination`.
type Extract = Arc<dyn Fn(PathBuf, PathBuf) -> BoxFuture<'static, Result<()>> + Send + Sync>;

#[derive(Clone)]
pub(crate) struct Config {
    pub http: Arc<dyn HttpClient>,
    pub base_url: String,
    pub public_key: PublicKey,
    pub current_version: Version,
    /// The running app bundle, which the update replaces.
    pub bundle: PathBuf,
    /// `<data dir>/updates`, where downloads and extracted apps go. The updater owns it.
    pub updates_dir: PathBuf,
    pub extract: Extract,
}

/// `ditto` keeps what an app bundle needs (permissions, symlinks, extended attributes, and so its
/// code signature), and `codesign` checks that the signature still matches. Off macOS, only debug
/// builds get here ([`DEBUG_BUNDLE_ENV`]), and `unzip` stands in.
async fn extract_zip(zip: PathBuf, destination: PathBuf) -> Result<()> {
    let output = if cfg!(target_os = "macos") {
        util::command::new_command("/usr/bin/ditto")
            .args(["-x", "-k"])
            .arg(&zip)
            .arg(&destination)
            .output()
            .await
    } else {
        util::command::new_command("unzip")
            .arg("-q")
            .arg(&zip)
            .arg("-d")
            .arg(&destination)
            .output()
            .await
    }
    .with_context(|| format!("extracting {}", zip.display()))?;
    ensure!(
        output.status.success(),
        "couldn't extract {}: {}",
        zip.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    if cfg!(target_os = "macos") {
        let bundle = bundle::find_extracted(&destination)?;
        let output = util::command::new_command("/usr/bin/codesign")
            .args(["--verify", "--strict"])
            .arg(&bundle)
            .output()
            .await
            .context("running codesign")?;
        ensure!(
            output.status.success(),
            "the update's code signature doesn't verify: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Whether `candidate` is a newer release than `current`, by semantic-version precedence: a
/// prerelease comes before its release (0.2.0-beta.1 < 0.2.0), and build metadata doesn't count.
pub(crate) fn is_newer(current: &Version, candidate: &Version) -> bool {
    candidate.cmp_precedence(current).is_gt()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// The updater doesn't run in this build or install; the reason says why.
    Off(SharedString),
    Idle,
    Checking,
    Downloading(Version),
    Installing(Version),
    /// The update replaced the app bundle; restarting starts it.
    Ready(Version),
}

pub struct Updater {
    status: Status,
    config: Option<Config>,
    /// Whether it checks on its own (`"updates": {"enabled"}`).
    automatic: bool,
    check: Option<Task<()>>,
    /// The workspace whose user asked for the check in progress. It shows how the check goes.
    reporter: Option<WeakEntity<Workspace>>,
    _poll: Option<Task<()>>,
    /// The toasts it showed, for tests.
    #[cfg(test)]
    reports: Vec<String>,
}

/// How a toast about a check goes away.
enum ToastKind {
    /// After a few seconds.
    Brief,
    /// When the user closes it, or the next toast about updates replaces it.
    Sticky,
    /// Sticky, with a Restart button.
    Restart,
}

struct GlobalUpdater(Entity<Updater>);

impl Global for GlobalUpdater {}

enum Outcome {
    UpToDate(Version),
    Installed(Version),
}

impl Updater {
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalUpdater>()
            .map(|global| global.0.clone())
    }

    /// Whether the status bar shows an update's progress or "Restart to update".
    pub fn shows_status(cx: &App) -> bool {
        Self::global(cx).is_some_and(|updater| {
            matches!(
                updater.read(cx).status(),
                Status::Downloading(_) | Status::Installing(_) | Status::Ready(_)
            )
        })
    }

    fn new(config: Result<Config, SharedString>, automatic: bool, cx: &mut Context<Self>) -> Self {
        let (status, config) = match config {
            Ok(config) => (Status::Idle, Some(config)),
            Err(reason) => (Status::Off(reason), None),
        };
        let poll = config.is_some().then(|| Self::poll(cx));
        Self {
            status,
            config,
            automatic,
            check: None,
            reporter: None,
            _poll: poll,
            #[cfg(test)]
            reports: Vec::new(),
        }
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    fn poll(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FIRST_CHECK_DELAY).await;
            // mustr got this far after an update, so the bundle it replaced can go. Unless an
            // update was installed just now: then the previous bundle is the running app's.
            let replaced_bundle = this
                .read_with(cx, |this, _| {
                    let installed = matches!(this.status, Status::Installing(_) | Status::Ready(_));
                    let config = this.config.as_ref().filter(|_| !installed);
                    config.map(|config| config.bundle.clone())
                })
                .ok()
                .flatten();
            if let Some(bundle) = replaced_bundle {
                cx.background_spawn(async move {
                    match bundle::remove_previous(&bundle) {
                        Ok(true) => {
                            log::info!("Updates: removed the app the last update replaced")
                        }
                        Ok(false) => {}
                        Err(error) => log::error!("Updates: {error:#}"),
                    }
                })
                .await;
            }
            loop {
                let Ok(()) = this.update(cx, |this, cx| {
                    if this.automatic {
                        this.check(None, cx);
                    }
                }) else {
                    return;
                };
                cx.background_executor().timer(CHECK_INTERVAL).await;
            }
        })
    }

    /// Checks for an update and installs it. With a `reporter`, the user asked, so its window
    /// shows how it goes; automatic checks only show "Restart to update" when they install one.
    pub fn check(&mut self, reporter: Option<WeakEntity<Workspace>>, cx: &mut Context<Self>) {
        if reporter.is_some() {
            self.reporter = reporter;
        }
        let config = match (&self.status, &self.config) {
            (Status::Off(reason), _) => {
                let message = format!("mustr doesn't update itself here: {reason}.");
                self.report(message, ToastKind::Brief, cx);
                self.reporter = None;
                return;
            }
            (Status::Ready(version), _) => {
                let message = ready_message(version);
                self.report(message, ToastKind::Restart, cx);
                self.reporter = None;
                return;
            }
            (_, None) => return,
            // The check in progress reports to the window that asked last.
            (_, Some(_)) if self.check.is_some() => {
                let message = "mustr is already checking for updates.".to_string();
                self.report(message, ToastKind::Brief, cx);
                return;
            }
            (_, Some(config)) => config.clone(),
        };
        self.set_status(Status::Checking, cx);
        self.check = Some(cx.spawn(async move |this, cx| {
            let result = run_check(config, this.clone(), cx).await;
            this.update(cx, |this, cx| this.finish(result, cx)).ok();
        }));
    }

    fn finish(&mut self, result: Result<Outcome>, cx: &mut Context<Self>) {
        self.check = None;
        let manual = self.reporter.is_some();
        match result {
            Ok(Outcome::UpToDate(latest)) => {
                log::info!("Updates: mustr is up to date (the latest release is {latest})");
                self.set_status(Status::Idle, cx);
                let current = self
                    .config
                    .as_ref()
                    .map_or(latest, |config| config.current_version.clone());
                let message = format!("mustr is up to date ({current}).");
                self.report(message, ToastKind::Brief, cx);
            }
            Ok(Outcome::Installed(version)) => {
                log::info!("Updates: installed mustr {version}; it starts when mustr restarts");
                // Polling stops: the next check could only replace the update the user hasn't
                // restarted into yet.
                self._poll = None;
                self.set_status(Status::Ready(version.clone()), cx);
                self.report(ready_message(&version), ToastKind::Restart, cx);
            }
            Err(error) => {
                if manual {
                    log::error!("Updates: {error:#}");
                } else {
                    log::warn!("Updates: the automatic check failed: {error:#}");
                }
                self.set_status(Status::Idle, cx);
                let message = format!("Couldn't update mustr: {error:#}");
                self.report(message, ToastKind::Sticky, cx);
            }
        }
        self.reporter = None;
    }

    fn set_status(&mut self, status: Status, cx: &mut Context<Self>) {
        if let Status::Downloading(version) = &status {
            let message = format!("Downloading mustr {version}…");
            self.report(message, ToastKind::Sticky, cx);
        }
        show_restart_action(matches!(status, Status::Ready(_)), cx);
        self.status = status;
        cx.notify();
    }

    /// Shows how a check the user asked for goes, in their window. Each toast replaces the last.
    fn report(&mut self, message: String, kind: ToastKind, cx: &mut Context<Self>) {
        let Some(workspace) = self.reporter.as_ref().and_then(WeakEntity::upgrade) else {
            return;
        };
        #[cfg(test)]
        self.reports.push(message.clone());
        let toast = Toast::new(toast_id(), message);
        let toast = match kind {
            ToastKind::Brief => toast.autohide(),
            ToastKind::Sticky => toast,
            ToastKind::Restart => toast.on_click("Restart", |window, cx| {
                window.dispatch_action(RestartToUpdate.boxed_clone(), cx)
            }),
        };
        // Deferred: the check may have started in one of the workspace's own action handlers.
        cx.defer(move |cx| workspace.update(cx, |workspace, cx| workspace.show_toast(toast, cx)));
    }

    fn ready_bundle(&self) -> Option<&Path> {
        match (&self.status, &self.config) {
            (Status::Ready(_), Some(config)) => Some(&config.bundle),
            _ => None,
        }
    }
}

fn toast_id() -> NotificationId {
    struct UpdateToast;
    NotificationId::unique::<UpdateToast>()
}

fn ready_message(version: &Version) -> String {
    format!("mustr {version} is installed. Restart mustr to start using it.")
}

/// "mustr: restart to update" is in the command palette only while there's an update to
/// restart into.
fn show_restart_action(show: bool, cx: &mut App) {
    if CommandPaletteFilter::try_global(cx).is_none() {
        return;
    }
    CommandPaletteFilter::update_global(cx, |filter, _| {
        let action = [TypeId::of::<RestartToUpdate>()];
        if show {
            filter.show_action_types(&action);
        } else {
            filter.hide_action_types(&action);
        }
    });
}

/// Restarts into the installed update, as Zed's `workspace::Reload` restarts: unsaved files
/// prompt first, and `confirm_quit` asks.
fn restart_to_update(cx: &mut App) {
    let Some(updater) = Updater::global(cx) else {
        return;
    };
    let Some(bundle) = updater.read(cx).ready_bundle().map(Path::to_path_buf) else {
        log::info!("Updates: there's no installed update to restart into");
        return;
    };
    // GPUI's macOS restart reopens the bundle at this path, which now holds the update. Off
    // macOS (debug builds only) it re-runs the executable.
    if cfg!(target_os = "macos") {
        cx.set_restart_path(bundle);
    }
    workspace::reload(cx);
}

async fn run_check(
    config: Config,
    this: WeakEntity<Updater>,
    cx: &mut AsyncApp,
) -> Result<Outcome> {
    let manifest = cx
        .background_spawn({
            let config = config.clone();
            async move { fetch_manifest(&config).await }
        })
        .await?;
    if !is_newer(&config.current_version, &manifest.version) {
        return Ok(Outcome::UpToDate(manifest.version));
    }
    let version = manifest.version.clone();
    let asset = manifest
        .asset(PLATFORM)
        .cloned()
        .with_context(|| format!("mustr {version} has no download for this Mac ({PLATFORM})"))?;
    log::info!(
        "Updates: downloading mustr {version} ({} bytes)",
        asset.size
    );
    this.update(cx, |this, cx| {
        this.set_status(Status::Downloading(version.clone()), cx)
    })?;
    let zip = cx
        .background_spawn({
            let config = config.clone();
            async move { download(&config, &asset).await }
        })
        .await?;
    this.update(cx, |this, cx| {
        this.set_status(Status::Installing(version.clone()), cx)
    })?;
    cx.background_spawn({
        let version = version.clone();
        async move { install(&config, &version, &zip).await }
    })
    .await?;
    Ok(Outcome::Installed(version))
}

/// The latest release's manifest, once its signature checks out.
pub(crate) async fn fetch_manifest(config: &Config) -> Result<Manifest> {
    let manifest_url = format!("{}/{}", config.base_url, mustr_release::MANIFEST_FILE);
    let signature_url = format!("{}/{}", config.base_url, mustr_release::SIGNATURE_FILE);
    let manifest = fetch(config.http.as_ref(), &manifest_url, MAX_MANIFEST_BYTES).await?;
    let signature = fetch(config.http.as_ref(), &signature_url, MAX_SIGNATURE_BYTES).await?;
    mustr_release::verified_manifest(&manifest, &signature, &config.public_key)
}

async fn fetch(http: &dyn HttpClient, url: &str, limit: u64) -> Result<Vec<u8>> {
    let mut response = http
        .get(url, AsyncBody::empty(), true)
        .await
        .with_context(|| format!("couldn't reach {url}"))?;
    let status = response.status();
    if status.as_u16() == 404 {
        bail!("there's no published release to update from ({url} isn't there)");
    }
    ensure!(status.is_success(), "{url} answered {status}");
    let mut bytes = Vec::new();
    response
        .body_mut()
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .await
        .with_context(|| format!("downloading {url}"))?;
    ensure!(
        bytes.len() as u64 <= limit,
        "{url} is larger than {limit} bytes"
    );
    Ok(bytes)
}

/// Downloads the release zip into the updates directory, checking its size and SHA-256 against
/// the manifest's.
pub(crate) async fn download(config: &Config, asset: &Asset) -> Result<PathBuf> {
    clear_dir(&config.updates_dir)?;
    let partial = config.updates_dir.join(format!("{}.part", asset.name));
    let result = download_to(config, asset, &partial).await;
    if result.is_err() {
        std::fs::remove_file(&partial).ok();
    }
    result?;
    let zip = config.updates_dir.join(&asset.name);
    std::fs::rename(&partial, &zip).with_context(|| format!("saving {}", zip.display()))?;
    Ok(zip)
}

async fn download_to(config: &Config, asset: &Asset, destination: &Path) -> Result<()> {
    let url = format!("{}/{}", config.base_url, asset.name);
    let mut response = config
        .http
        .get(&url, AsyncBody::empty(), true)
        .await
        .with_context(|| format!("couldn't reach {url}"))?;
    ensure!(
        response.status().is_success(),
        "{url} answered {}",
        response.status()
    );
    let mut file = std::fs::File::create(destination)
        .with_context(|| format!("creating {}", destination.display()))?;
    let mut hasher = mustr_release::Sha256::default();
    let mut buffer = vec![0; 256 * 1024];
    let mut received = 0u64;
    loop {
        let read = response
            .body_mut()
            .read(&mut buffer)
            .await
            .with_context(|| format!("downloading {url}"))?;
        if read == 0 {
            break;
        }
        received += read as u64;
        ensure!(
            received <= asset.size,
            "the download of {} is larger than the {} bytes its manifest lists, so mustr \
             stopped it",
            asset.name,
            asset.size
        );
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])?;
    }
    file.sync_all()?;
    ensure!(
        received == asset.size,
        "the download of {} has {received} bytes, and its manifest lists {}",
        asset.name,
        asset.size
    );
    let sha256 = hasher.finish();
    ensure!(
        sha256 == asset.sha256,
        "the download of {} doesn't match the SHA-256 its manifest lists (expected {}, got \
         {sha256}), so mustr deleted it",
        asset.name,
        asset.sha256
    );
    Ok(())
}

/// Extracts the zip, checks the app in it, and swaps it in for the running bundle.
pub(crate) async fn install(config: &Config, version: &Version, zip: &Path) -> Result<()> {
    let extracted = config.updates_dir.join("extracted");
    remove_dir_if_present(&extracted)?;
    std::fs::create_dir_all(&extracted)
        .with_context(|| format!("creating {}", extracted.display()))?;
    (config.extract)(zip.to_path_buf(), extracted.clone()).await?;
    let new_bundle = bundle::find_extracted(&extracted)?;
    bundle::check(&new_bundle, version)?;
    bundle::replace(&config.bundle, &new_bundle)?;
    clear_dir(&config.updates_dir).log_err();
    Ok(())
}

/// Empties the updates directory, which holds one check's files at a time.
fn clear_dir(dir: &Path) -> Result<()> {
    remove_dir_if_present(dir)?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))
}

fn remove_dir_if_present(dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(anyhow!(error).context(format!("removing {}", dir.display())))
        }
        _ => Ok(()),
    }
}

/// The status bar's update state: what's downloading, then "Restart to update".
struct UpdateStatusItem {
    updater: Entity<Updater>,
    _observe: Subscription,
}

impl UpdateStatusItem {
    fn new(updater: Entity<Updater>, cx: &mut Context<Self>) -> Self {
        Self {
            _observe: cx.observe(&updater, |_, _, cx| cx.notify()),
            updater,
        }
    }
}

impl Render for UpdateStatusItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = mustr_ui::palette(cx);
        let container = div().flex_none().pr(tokens::scaled(px(4.)));
        match self.updater.read(cx).status() {
            Status::Downloading(version) | Status::Installing(version) => container
                .px(tokens::scaled(px(5.)))
                .text_size(tokens::text_size(tokens::TEXT_SIZE_XSMALL))
                .text_color(palette.text_muted.hsla())
                .whitespace_nowrap()
                .child(format!("Updating to mustr {version}…")),
            Status::Ready(version) => container
                .debug_selector(|| "restart-to-update".into())
                .child(
                    Chip::new("restart-to-update", "Restart to update")
                        .accent(true)
                        .tooltip(ready_message(version))
                        .on_click(|_, window, cx| {
                            window.dispatch_action(RestartToUpdate.boxed_clone(), cx)
                        }),
                ),
            Status::Off(_) | Status::Idle | Status::Checking => container,
        }
    }
}

impl StatusItemView for UpdateStatusItem {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}
