use super::*;
use crate::updates::bundle::{
    BUNDLE_ID, previous_path,
    tests::{bundle_version, fake_bundle},
};
use gpui::{Focusable as _, TestAppContext, VisualTestContext};
use http_client::{FakeHttpClient, Response};
use mustr_release::{SigningKey, asset_name};
use parking_lot::Mutex;
use pretty_assertions::assert_eq;
use settings::SettingsStore;
use std::collections::BTreeMap;
use workspace::MultiWorkspace;

const BASE_URL: &str = "https://releases.test/mustr/latest";

fn version(text: &str) -> Version {
    Version::parse(text).unwrap()
}

#[test]
fn versions_compare_by_semver_precedence() {
    let newer = |current: &str, candidate: &str| is_newer(&version(current), &version(candidate));
    assert!(newer("0.1.0", "0.2.0"));
    assert!(newer("0.9.0", "0.10.0"));
    assert!(!newer("0.2.0", "0.2.0"));
    assert!(!newer("0.2.0", "0.1.9"), "never a downgrade");
    // A prerelease comes before its release, and after the release before it.
    assert!(newer("0.1.0", "0.2.0-beta.1"));
    assert!(newer("0.2.0-beta.1", "0.2.0"));
    assert!(!newer("0.2.0", "0.2.0-rc.1"));
    assert!(newer("0.2.0-beta.1", "0.2.0-beta.2"));
    assert!(newer("0.2.0-beta.9", "0.2.0-beta.10"));
    assert!(newer("0.2.0-beta.2", "0.2.0-rc.1"));
    // Build metadata doesn't make a release newer.
    assert!(!newer("0.2.0", "0.2.0+build.7"));
}

#[test]
fn the_updater_runs_only_in_release_bundles_on_macos() {
    let (_, public_key) = SigningKey::generate().unwrap();
    let key_file = format!("# mustr's key\n{}\n", public_key.to_base64());
    let bundled = Path::new("/Applications/mustr.app/Contents/MacOS/mustr");
    let environment = |macos, release_build, executable| Environment {
        macos,
        release_build,
        executable,
        public_key_file: &key_file,
        debug_bundle: None,
        debug_public_key: None,
    };
    assert_eq!(
        setup(environment(true, true, Some(bundled))),
        Ok(Setup {
            bundle: PathBuf::from("/Applications/mustr.app"),
            public_key,
        })
    );
    assert_eq!(
        setup(environment(false, true, Some(bundled))),
        Err("mustr updates itself only in its macOS app".into())
    );
    assert_eq!(
        setup(environment(true, false, Some(bundled))),
        Err("debug builds don't update themselves".into())
    );
    assert_eq!(
        setup(environment(
            true,
            true,
            Some(Path::new("/Users/someone/mustr/target/release/mustr"))
        )),
        Err(
            "mustr isn't running from an app bundle (/Users/someone/mustr/target/release/mustr)"
                .into()
        )
    );
    let translocated = Path::new(
        "/private/var/folders/x/T/AppTranslocation/1234/d/mustr.app/Contents/MacOS/mustr",
    );
    assert!(
        setup(environment(true, true, Some(translocated)))
            .unwrap_err()
            .contains("App Translocation")
    );

    // The placeholder key file mustr ships with until the release key exists.
    let placeholder = Environment {
        public_key_file: PUBLIC_KEY_FILE,
        ..environment(true, true, Some(bundled))
    };
    assert_eq!(
        setup(placeholder),
        Err("this build has no update-signing key \
             (resources/update-signing-key.pub is still the placeholder)"
            .into())
    );
    let broken = Environment {
        public_key_file: "not a key",
        ..environment(true, true, Some(bundled))
    };
    assert!(
        setup(broken)
            .unwrap_err()
            .starts_with("this build's update-signing key is invalid")
    );

    // A debug build's stand-in bundle works on any platform.
    let (_, debug_key) = SigningKey::generate().unwrap();
    let debug = Environment {
        debug_bundle: Some(PathBuf::from("/tmp/stand-in/mustr.app")),
        debug_public_key: Some(debug_key.to_base64()),
        ..environment(false, false, None)
    };
    assert_eq!(
        setup(debug),
        Ok(Setup {
            bundle: PathBuf::from("/tmp/stand-in/mustr.app"),
            public_key: debug_key,
        })
    );
}

#[test]
fn shipped_key_file_is_the_placeholder_or_a_key() {
    // A malformed key file would turn updates off in every build; catch it here instead.
    PublicKey::from_file_text(PUBLIC_KEY_FILE).unwrap();
}

#[test]
fn releases_come_from_github_unless_overridden() {
    assert_eq!(base_url(None), DEFAULT_BASE_URL);
    assert_eq!(base_url(Some("  ".into())), DEFAULT_BASE_URL);
    assert_eq!(
        base_url(Some("http://127.0.0.1:8000/release/".into())),
        "http://127.0.0.1:8000/release"
    );
}

#[test]
fn the_setting_turns_automatic_checks_off() {
    let parse = |text: &str| UpdateSettings::from_settings_text(text).unwrap();
    assert_eq!(parse(""), UpdateSettings { enabled: true });
    assert_eq!(
        parse("{\n  // mustr settings\n}\n"),
        UpdateSettings { enabled: true }
    );
    assert_eq!(
        parse(r#"{ "updates": { "enabled": false } }"#),
        UpdateSettings { enabled: false }
    );
    assert_eq!(
        parse(r#"{ "updates": {} }"#),
        UpdateSettings { enabled: true }
    );
    assert!(UpdateSettings::from_settings_text(r#"{ "updates": { "enabled": "no" } }"#).is_err());
}

// A release server and a stand-in for mustr.app.

/// What the fake `ditto` extracts: an app with this identifier and version.
fn zip_holding(identifier: &str, version: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "identifier": identifier, "version": version }))
        .unwrap()
}

fn fake_extract() -> Extract {
    Arc::new(|zip, destination| {
        async move {
            let app: serde_json::Value = serde_json::from_slice(&std::fs::read(zip)?)?;
            fake_bundle(
                &destination.join("mustr.app"),
                app["identifier"].as_str().unwrap_or_default(),
                app["version"].as_str().unwrap_or_default(),
            );
            Ok(())
        }
        .boxed()
    })
}

/// mustr `version`'s release files, as the workflow publishes them, signed with `key`.
fn release(version_text: &str, zip: &[u8], key: &SigningKey) -> BTreeMap<String, Vec<u8>> {
    let version = version(version_text);
    let name = asset_name(&version, PLATFORM);
    let (size, sha256) = mustr_release::size_and_sha256(zip).unwrap();
    let manifest = Manifest {
        release_notes_url: mustr_release::release_notes_url(&version),
        published_at: "2026-10-28T12:00:00Z".to_string(),
        platforms: BTreeMap::from([(
            PLATFORM.to_string(),
            Asset {
                name: name.clone(),
                size,
                sha256,
            },
        )]),
        version,
    };
    let json = manifest.to_json().unwrap();
    BTreeMap::from([
        (
            mustr_release::SIGNATURE_FILE.to_string(),
            key.sign(&json).into_bytes(),
        ),
        (mustr_release::MANIFEST_FILE.to_string(), json),
        (name, zip.to_vec()),
    ])
}

#[derive(Clone, Default)]
struct Server {
    files: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn http(&self) -> Arc<dyn HttpClient> {
        let server = self.clone();
        FakeHttpClient::create(move |request| {
            let url = request.uri().to_string();
            let name = url
                .strip_prefix(BASE_URL)
                .and_then(|path| path.strip_prefix('/'))
                .unwrap_or_default()
                .to_string();
            server.requests.lock().push(name.clone());
            let body = server.files.lock().get(&name).cloned();
            async move {
                Ok(match body {
                    Some(body) => Response::builder().status(200).body(body.into())?,
                    None => Response::builder().status(404).body(AsyncBody::default())?,
                })
            }
        })
    }

    fn publish(&self, files: BTreeMap<String, Vec<u8>>) {
        *self.files.lock() = files;
    }

    fn edit(&self, name: &str, edit: impl FnOnce(&mut Vec<u8>)) {
        edit(self.files.lock().get_mut(name).unwrap());
    }

    fn requests(&self) -> Vec<String> {
        std::mem::take(&mut self.requests.lock())
    }
}

struct Install {
    _dir: tempfile::TempDir,
    applications: PathBuf,
    bundle: PathBuf,
    updates_dir: PathBuf,
    server: Server,
    key: SigningKey,
}

impl Install {
    /// mustr 0.1.0 in a stand-in Applications folder.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let applications = dir.path().join("Applications");
        let bundle = applications.join("mustr.app");
        fake_bundle(&bundle, BUNDLE_ID, "0.1.0");
        let (private_key, _) = SigningKey::generate().unwrap();
        Self {
            updates_dir: dir.path().join("data/updates"),
            applications,
            bundle,
            _dir: dir,
            server: Server::default(),
            key: SigningKey::from_text(&private_key).unwrap(),
        }
    }

    fn config(&self) -> Config {
        Config {
            http: self.server.http(),
            base_url: BASE_URL.to_string(),
            public_key: self.key.public_key(),
            current_version: version("0.1.0"),
            bundle: self.bundle.clone(),
            updates_dir: self.updates_dir.clone(),
            extract: fake_extract(),
        }
    }

    fn publish(&self, version_text: &str) {
        let zip = zip_holding(BUNDLE_ID, version_text);
        self.server.publish(release(version_text, &zip, &self.key));
    }

    fn applications(&self) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(&self.applications)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn leftover_downloads(&self) -> Vec<String> {
        std::fs::read_dir(&self.updates_dir)
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }
}

async fn window(
    config: Result<Config, SharedString>,
    automatic: bool,
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, Entity<Updater>, &mut VisualTestContext) {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        command_palette_hooks::init(cx);
        init_with(config, automatic, cx);
    });
    let fs = project::FakeFs::new(cx.executor());
    let project = project::Project::test(fs, [], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    cx.run_until_parked();
    let updater = cx.update(|_, cx| Updater::global(cx).unwrap());
    (workspace, updater, cx)
}

/// mustr › Check for Updates…, or "mustr: check for updates" in the palette.
fn check_for_updates(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    workspace.update_in(cx, |workspace, window, cx| {
        let pane = workspace.active_pane().read(cx).focus_handle(cx);
        window.focus(&pane, cx);
    });
    cx.dispatch_action(CheckForUpdates);
    cx.run_until_parked();
}

fn reports(updater: &Entity<Updater>, cx: &mut VisualTestContext) -> Vec<String> {
    updater.update(cx, |updater, _| std::mem::take(&mut updater.reports))
}

fn status(updater: &Entity<Updater>, cx: &mut VisualTestContext) -> Status {
    updater.read_with(cx, |updater, _| updater.status().clone())
}

fn restart_is_in_the_palette(cx: &mut VisualTestContext) -> bool {
    cx.update(|_, cx| {
        !CommandPaletteFilter::try_global(cx)
            .unwrap()
            .is_hidden(&RestartToUpdate)
    })
}

#[gpui::test]
async fn a_manual_check_installs_the_update_and_offers_a_restart(cx: &mut TestAppContext) {
    let install = Install::new();
    install.publish("0.2.0");
    let (workspace, updater, cx) = window(Ok(install.config()), false, cx).await;
    assert!(cx.debug_bounds("restart-to-update").is_none());
    assert!(!restart_is_in_the_palette(cx));

    check_for_updates(&workspace, cx);
    assert_eq!(
        install.server.requests(),
        [
            "manifest.json",
            "manifest.json.sig",
            "mustr-0.2.0-macos-aarch64.zip"
        ]
    );
    assert_eq!(
        reports(&updater, cx),
        [
            "Downloading mustr 0.2.0…",
            "mustr 0.2.0 is installed. Restart mustr to start using it."
        ]
    );
    assert_eq!(status(&updater, cx), Status::Ready(version("0.2.0")));
    assert!(
        workspace.read_with(cx, |workspace, _| !workspace
            .notification_views()
            .is_empty()),
        "the toast is in the window that asked"
    );
    // The update replaced the bundle; the running app's bundle waits next to it.
    assert_eq!(bundle_version(&install.bundle), "0.2.0");
    assert_eq!(bundle_version(&previous_path(&install.bundle)), "0.1.0");
    assert_eq!(install.applications(), [".mustr.app.previous", "mustr.app"]);
    assert_eq!(install.leftover_downloads(), Vec::<String>::new());
    assert!(restart_is_in_the_palette(cx));

    // Asking again doesn't download it again.
    check_for_updates(&workspace, cx);
    assert_eq!(install.server.requests(), Vec::<String>::new());
    assert_eq!(
        reports(&updater, cx),
        ["mustr 0.2.0 is installed. Restart mustr to start using it."]
    );

    // It restarts only when the user clicks.
    let restart = cx.expect_restart();
    let bounds = cx
        .debug_bounds("restart-to-update")
        .expect("the status bar shows Restart to update");
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    let (path, _) = restart.await.expect("mustr restarts");
    // On macOS, GPUI reopens the bundle at its path, which now holds the update.
    assert_eq!(
        path,
        cfg!(target_os = "macos").then(|| install.bundle.clone())
    );
}

#[gpui::test]
async fn a_manual_check_says_when_mustr_is_up_to_date(cx: &mut TestAppContext) {
    let install = Install::new();
    install.publish("0.1.0");
    let (workspace, updater, cx) = window(Ok(install.config()), false, cx).await;
    check_for_updates(&workspace, cx);
    assert_eq!(
        install.server.requests(),
        ["manifest.json", "manifest.json.sig"]
    );
    assert_eq!(reports(&updater, cx), ["mustr is up to date (0.1.0)."]);
    assert_eq!(status(&updater, cx), Status::Idle);
    assert_eq!(install.applications(), ["mustr.app"]);

    // An older release, say after a release was pulled, isn't a downgrade.
    install.publish("0.0.9");
    check_for_updates(&workspace, cx);
    assert_eq!(reports(&updater, cx), ["mustr is up to date (0.1.0)."]);
    assert_eq!(bundle_version(&install.bundle), "0.1.0");

    install.server.publish(BTreeMap::new());
    check_for_updates(&workspace, cx);
    assert_eq!(
        reports(&updater, cx),
        [
            "Couldn't update mustr: there's no published release to update from \
             (https://releases.test/mustr/latest/manifest.json isn't there)"
        ]
    );
}

#[gpui::test]
async fn releases_must_be_signed_with_mustrs_key(cx: &mut TestAppContext) {
    let install = Install::new();
    let (workspace, updater, cx) = window(Ok(install.config()), false, cx).await;
    let refused = "Couldn't update mustr: the update manifest's signature doesn't match mustr's \
                   signing key";

    // A manifest changed after it was signed.
    install.publish("0.2.0");
    install.server.edit("manifest.json", |manifest| {
        let text = String::from_utf8(manifest.clone()).unwrap();
        *manifest = text.replace("\"size\": ", "\"size\": 1").into_bytes();
    });
    check_for_updates(&workspace, cx);
    assert_eq!(reports(&updater, cx), [refused]);
    assert_eq!(
        install.server.requests(),
        ["manifest.json", "manifest.json.sig"],
        "nothing is downloaded"
    );

    // A release signed with some other key.
    let (other_key, _) = SigningKey::generate().unwrap();
    let other_key = SigningKey::from_text(&other_key).unwrap();
    install.server.publish(release(
        "0.2.0",
        &zip_holding(BUNDLE_ID, "0.2.0"),
        &other_key,
    ));
    check_for_updates(&workspace, cx);
    assert_eq!(reports(&updater, cx), [refused]);

    // No signature at all.
    install.publish("0.2.0");
    install.server.files.lock().remove("manifest.json.sig");
    check_for_updates(&workspace, cx);
    assert_eq!(
        reports(&updater, cx),
        [
            "Couldn't update mustr: there's no published release to update from \
             (https://releases.test/mustr/latest/manifest.json.sig isn't there)"
        ]
    );

    assert_eq!(status(&updater, cx), Status::Idle);
    assert_eq!(install.applications(), ["mustr.app"]);
    assert_eq!(bundle_version(&install.bundle), "0.1.0");
}

#[gpui::test]
async fn downloads_must_match_the_manifest(cx: &mut TestAppContext) {
    let install = Install::new();
    let (workspace, updater, cx) = window(Ok(install.config()), false, cx).await;
    let zip_name = "mustr-0.2.0-macos-aarch64.zip";
    let size = zip_holding(BUNDLE_ID, "0.2.0").len();

    // The same size, different bytes.
    install.publish("0.2.0");
    install.server.edit(zip_name, |zip| zip[0] ^= 1);
    check_for_updates(&workspace, cx);
    let report = reports(&updater, cx).pop().unwrap();
    assert!(
        report.starts_with(
            "Couldn't update mustr: the download of mustr-0.2.0-macos-aarch64.zip doesn't \
             match the SHA-256 its manifest lists"
        ),
        "{report}"
    );
    assert_eq!(install.leftover_downloads(), Vec::<String>::new());

    install.publish("0.2.0");
    install
        .server
        .edit(zip_name, |zip| zip.extend_from_slice(b"more"));
    check_for_updates(&workspace, cx);
    assert_eq!(
        reports(&updater, cx).last().unwrap(),
        &format!(
            "Couldn't update mustr: the download of {zip_name} is larger than the {size} bytes \
             its manifest lists, so mustr stopped it"
        )
    );

    install.publish("0.2.0");
    install.server.edit(zip_name, |zip| zip.truncate(10));
    check_for_updates(&workspace, cx);
    assert_eq!(
        reports(&updater, cx).last().unwrap(),
        &format!(
            "Couldn't update mustr: the download of {zip_name} has 10 bytes, and its manifest \
             lists {size}"
        )
    );

    // A signed release whose app isn't mustr at that version.
    let zip = zip_holding("com.example.other", "0.2.0");
    install.server.publish(release("0.2.0", &zip, &install.key));
    check_for_updates(&workspace, cx);
    assert_eq!(
        reports(&updater, cx).last().unwrap(),
        "Couldn't update mustr: the update isn't mustr: its bundle identifier is \
         \"com.example.other\", not dev.mustr.mustr"
    );
    let zip = zip_holding(BUNDLE_ID, "0.1.5");
    install.server.publish(release("0.2.0", &zip, &install.key));
    check_for_updates(&workspace, cx);
    assert_eq!(
        reports(&updater, cx).last().unwrap(),
        "Couldn't update mustr: the update's app is version \"0.1.5\", and its manifest says \
         0.2.0"
    );

    assert_eq!(status(&updater, cx), Status::Idle);
    assert_eq!(install.applications(), ["mustr.app"]);
    assert_eq!(bundle_version(&install.bundle), "0.1.0");
}

#[gpui::test]
async fn mustr_checks_on_its_own_after_launch(cx: &mut TestAppContext) {
    let install = Install::new();
    install.publish("0.2.0");
    // From an update before this launch.
    fake_bundle(&previous_path(&install.bundle), BUNDLE_ID, "0.0.9");
    let (_workspace, updater, cx) = window(Ok(install.config()), true, cx).await;
    assert_eq!(install.server.requests(), Vec::<String>::new());

    cx.executor().advance_clock(FIRST_CHECK_DELAY);
    cx.run_until_parked();
    assert_eq!(status(&updater, cx), Status::Ready(version("0.2.0")));
    assert_eq!(install.server.requests().len(), 3);
    assert_eq!(reports(&updater, cx), Vec::<String>::new(), "no toasts");
    // The stale previous bundle went first; this one is the running app's.
    assert_eq!(bundle_version(&previous_path(&install.bundle)), "0.1.0");
    assert!(cx.debug_bounds("restart-to-update").is_some());

    // Once an update waits for a restart, mustr stops checking.
    cx.executor().advance_clock(CHECK_INTERVAL * 2);
    cx.run_until_parked();
    assert_eq!(install.server.requests(), Vec::<String>::new());
}

#[gpui::test]
async fn automatic_checks_follow_the_setting(cx: &mut TestAppContext) {
    let install = Install::new();
    install.publish("0.2.0");
    fake_bundle(&previous_path(&install.bundle), BUNDLE_ID, "0.0.9");
    let (_workspace, updater, cx) = window(Ok(install.config()), false, cx).await;

    cx.executor()
        .advance_clock(FIRST_CHECK_DELAY + CHECK_INTERVAL);
    cx.run_until_parked();
    assert_eq!(install.server.requests(), Vec::<String>::new());
    assert_eq!(status(&updater, cx), Status::Idle);
    // The launch still cleans up after the last update.
    assert_eq!(install.applications(), ["mustr.app"]);

    // Turned back on, the next round checks.
    updater.update(cx, |updater, _| updater.automatic = true);
    cx.executor().advance_clock(CHECK_INTERVAL);
    cx.run_until_parked();
    assert_eq!(status(&updater, cx), Status::Ready(version("0.2.0")));
}

#[gpui::test]
async fn builds_that_dont_update_say_why(cx: &mut TestAppContext) {
    let (workspace, updater, cx) =
        window(Err("debug builds don't update themselves".into()), true, cx).await;
    check_for_updates(&workspace, cx);
    assert_eq!(
        reports(&updater, cx),
        ["mustr doesn't update itself here: debug builds don't update themselves."]
    );
    cx.executor().advance_clock(FIRST_CHECK_DELAY);
    cx.run_until_parked();
    assert_eq!(
        status(&updater, cx),
        Status::Off("debug builds don't update themselves".into())
    );
    assert!(cx.debug_bounds("restart-to-update").is_none());

    // There's nothing to restart into.
    let mut restart = cx.expect_restart();
    cx.dispatch_action(RestartToUpdate);
    cx.run_until_parked();
    assert_eq!(restart.try_recv(), Ok(None));
}
