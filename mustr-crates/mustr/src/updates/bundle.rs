//! The app bundle the updater replaces. These are plain file operations, so the swap and its
//! rollback are tested on any platform, with stand-in bundles in temporary directories.
//!
//! The new bundle goes in next to the running one in two renames within one directory: the
//! running bundle moves aside to `.mustr.app.previous`, and the new one takes its name. A
//! running app keeps working after its bundle is renamed (it has its files open), as Sparkle's
//! updates rely on. The previous bundle stays until mustr's next launch gets going, so a broken
//! update can still be undone by hand. The hidden names have no `.app` extension, so neither
//! Finder nor Launch Services treats them as apps.

use anyhow::{Context as _, Result, anyhow, ensure};
use semver::Version;
use std::{
    io,
    path::{Path, PathBuf},
};
use util::ResultExt as _;

/// The bundle identifier `script/mustr-bundle-mac` writes.
pub const BUNDLE_ID: &str = "dev.mustr.mustr";
const EXECUTABLE: &str = "mustr";

/// The `.app` bundle that holds `executable` as `<name>.app/Contents/MacOS/<executable>`.
pub fn bundle_of_executable(executable: &Path) -> Option<PathBuf> {
    let macos_dir = executable.parent()?;
    if !macos_dir.ends_with("Contents/MacOS") {
        return None;
    }
    let bundle = macos_dir.parent()?.parent()?;
    bundle
        .extension()
        .is_some_and(|extension| extension == "app")
        .then(|| bundle.to_path_buf())
}

/// Whether macOS runs the bundle from App Translocation's read-only copy, as it does for a
/// quarantined app opened where it was downloaded. Replacing that copy would do nothing.
pub fn is_translocated(bundle: &Path) -> bool {
    bundle
        .components()
        .any(|component| component.as_os_str() == "AppTranslocation")
}

#[derive(Debug, PartialEq, Eq)]
pub struct BundleInfo {
    pub identifier: String,
    pub version: String,
}

/// The identifier and version in a bundle's `Info.plist`.
pub fn read_info(bundle: &Path) -> Result<BundleInfo> {
    let path = bundle.join("Contents/Info.plist");
    let plist =
        plist::Value::from_file(&path).with_context(|| format!("reading {}", path.display()))?;
    let dictionary = plist
        .as_dictionary()
        .with_context(|| format!("{} isn't a dictionary", path.display()))?;
    let string = |key: &str| {
        dictionary
            .get(key)
            .and_then(plist::Value::as_string)
            .map(str::to_string)
            .with_context(|| format!("{} has no {key}", path.display()))
    };
    Ok(BundleInfo {
        identifier: string("CFBundleIdentifier")?,
        version: string("CFBundleShortVersionString")?,
    })
}

/// Checks that an extracted bundle is mustr at the version its manifest promised.
pub fn check(bundle: &Path, version: &Version) -> Result<()> {
    let info = read_info(bundle)?;
    ensure!(
        info.identifier == BUNDLE_ID,
        "the update isn't mustr: its bundle identifier is {:?}, not {BUNDLE_ID}",
        info.identifier
    );
    ensure!(
        Version::parse(&info.version).is_ok_and(|bundled| bundled == *version),
        "the update's app is version {:?}, and its manifest says {version}",
        info.version
    );
    let executable = bundle.join("Contents/MacOS").join(EXECUTABLE);
    ensure!(
        is_executable(&executable),
        "the update has no executable at {}",
        executable.display()
    );
    Ok(())
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

/// The one `.app` that `ditto` extracted into `dir` (the release zip holds `mustr.app`).
pub fn find_extracted(dir: &Path) -> Result<PathBuf> {
    let mut bundles = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().is_some_and(|extension| extension == "app") && path.is_dir() {
            bundles.push(path);
        }
    }
    match <[PathBuf; 1]>::try_from(bundles) {
        Ok([bundle]) => Ok(bundle),
        Err(bundles) => Err(anyhow!(
            "the update's zip should hold one app, and it holds {}",
            bundles.len()
        )),
    }
}

/// Where the replaced bundle waits until the next launch: `.mustr.app.previous` next to it.
pub fn previous_path(bundle: &Path) -> PathBuf {
    sibling(bundle, "previous")
}

fn staged_path(bundle: &Path) -> PathBuf {
    sibling(bundle, "new")
}

fn sibling(bundle: &Path, suffix: &str) -> PathBuf {
    let name = bundle
        .file_name()
        .map_or_else(|| "mustr.app".into(), |name| name.to_string_lossy());
    bundle.with_file_name(format!(".{name}.{suffix}"))
}

/// Puts `new_bundle` (from anywhere) where `running` is, and keeps `running` at
/// [`previous_path`]. On failure, `running` is where it was.
pub fn replace(running: &Path, new_bundle: &Path) -> Result<()> {
    replace_with(running, new_bundle, &|from, to| std::fs::rename(from, to))
}

pub(crate) fn replace_with(
    running: &Path,
    new_bundle: &Path,
    rename: &dyn Fn(&Path, &Path) -> io::Result<()>,
) -> Result<()> {
    let staged = staged_path(running);
    remove_if_present(&staged)?;
    // Next to the running bundle, so the swap below stays within one directory. The update was
    // extracted in mustr's data directory, which can be on another volume.
    if let Err(error) = rename(new_bundle, &staged) {
        log::info!(
            "Updates: couldn't move the update next to {} ({error}), so copying it",
            running.display()
        );
        if let Err(error) = copy_dir(new_bundle, &staged) {
            remove_if_present(&staged).log_err();
            return Err(error)
                .with_context(|| format!("copying the update next to {}", running.display()));
        }
    }

    let previous = previous_path(running);
    if let Err(error) = remove_if_present(&previous) {
        remove_if_present(&staged).log_err();
        return Err(error);
    }
    if let Err(error) = rename(running, &previous) {
        remove_if_present(&staged).log_err();
        return Err(error).with_context(|| format!("moving {} aside", running.display()));
    }
    if let Err(error) = rename(&staged, running) {
        let restored = rename(&previous, running);
        remove_if_present(&staged).log_err();
        return Err(match restored {
            Ok(()) => anyhow!(error).context(format!(
                "moving the update to {}; the app there is unchanged",
                running.display()
            )),
            Err(restore_error) => anyhow!(
                "moving the update to {} failed ({error}), and so did moving the app back \
                 ({restore_error}); it's at {}",
                running.display(),
                previous.display()
            ),
        });
    }
    Ok(())
}

/// Deletes the bundle an earlier update replaced. Returns whether there was one.
pub fn remove_previous(running: &Path) -> Result<bool> {
    let previous = previous_path(running);
    let existed = previous.symlink_metadata().is_ok();
    remove_if_present(&previous)?;
    Ok(existed)
}

fn remove_if_present(path: &Path) -> Result<()> {
    let result = match path.symlink_metadata() {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => Err(error),
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
    };
    result.with_context(|| format!("removing {}", path.display()))
}

/// Copies a directory tree, keeping symlinks as symlinks and each file's permissions.
fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::create_dir(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let source = entry.path();
        let destination = to.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(&source)?, &destination)?;
            #[cfg(not(unix))]
            std::fs::copy(&source, &destination).map(drop)?;
        } else if file_type.is_dir() {
            copy_dir(&source, &destination)?;
        } else {
            std::fs::copy(&source, &destination)?;
        }
    }
    std::fs::set_permissions(to, std::fs::metadata(from)?.permissions())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::cell::Cell;

    /// A stand-in for mustr.app: an `Info.plist` and an executable that says which build it is.
    pub(crate) fn fake_bundle(path: &Path, identifier: &str, version: &str) {
        let contents = path.join("Contents");
        std::fs::create_dir_all(contents.join("MacOS")).unwrap();
        std::fs::create_dir_all(contents.join("Resources")).unwrap();
        let mut info = plist::Dictionary::new();
        info.insert("CFBundleIdentifier".into(), identifier.into());
        info.insert("CFBundleShortVersionString".into(), version.into());
        info.insert("CFBundleExecutable".into(), EXECUTABLE.into());
        plist::Value::Dictionary(info)
            .to_file_xml(contents.join("Info.plist"))
            .unwrap();
        let executable = contents.join("MacOS").join(EXECUTABLE);
        std::fs::write(&executable, format!("mustr {version}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::os::unix::fs::symlink("MacOS/mustr", contents.join("current")).unwrap();
        }
    }

    pub(crate) fn bundle_version(bundle: &Path) -> String {
        read_info(bundle).unwrap().version
    }

    #[test]
    fn bundles_are_found_from_their_executable() {
        assert_eq!(
            bundle_of_executable(Path::new("/Applications/mustr.app/Contents/MacOS/mustr")),
            Some(PathBuf::from("/Applications/mustr.app"))
        );
        for unbundled in [
            "/Users/someone/mustr/target/release/mustr",
            "/Users/someone/Contents/MacOS/mustr",
            "mustr",
        ] {
            assert_eq!(bundle_of_executable(Path::new(unbundled)), None);
        }
        assert!(is_translocated(Path::new(
            "/private/var/folders/x/T/AppTranslocation/1234/d/mustr.app"
        )));
        assert!(!is_translocated(Path::new("/Applications/mustr.app")));
        assert_eq!(
            previous_path(Path::new("/Applications/mustr.app")),
            Path::new("/Applications/.mustr.app.previous")
        );
    }

    #[test]
    fn extracted_bundles_must_be_mustr_at_the_promised_version() {
        let dir = tempfile::tempdir().unwrap();
        let version = Version::new(0, 2, 0);
        let bundle = dir.path().join("mustr.app");
        fake_bundle(&bundle, BUNDLE_ID, "0.2.0");
        assert_eq!(find_extracted(dir.path()).unwrap(), bundle);
        check(&bundle, &version).unwrap();
        assert_eq!(
            check(&bundle, &Version::new(0, 2, 1))
                .unwrap_err()
                .to_string(),
            "the update's app is version \"0.2.0\", and its manifest says 0.2.1"
        );

        let impostor = tempfile::tempdir().unwrap();
        let impostor = impostor.path().join("mustr.app");
        fake_bundle(&impostor, "com.example.other", "0.2.0");
        assert_eq!(
            check(&impostor, &version).unwrap_err().to_string(),
            "the update isn't mustr: its bundle identifier is \"com.example.other\", not \
             dev.mustr.mustr"
        );

        std::fs::remove_file(bundle.join("Contents/MacOS/mustr")).unwrap();
        assert!(
            check(&bundle, &version)
                .unwrap_err()
                .to_string()
                .starts_with("the update has no executable"),
        );

        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            find_extracted(empty.path()).unwrap_err().to_string(),
            "the update's zip should hold one app, and it holds 0"
        );
    }

    #[test]
    fn the_update_takes_the_running_bundles_place() {
        let applications = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let running = applications.path().join("mustr.app");
        fake_bundle(&running, BUNDLE_ID, "0.1.0");
        let new_bundle = data.path().join("extracted/mustr.app");
        fake_bundle(&new_bundle, BUNDLE_ID, "0.2.0");
        // Left over from an update whose next launch never got going.
        fake_bundle(&previous_path(&running), BUNDLE_ID, "0.0.9");

        replace(&running, &new_bundle).unwrap();
        assert_eq!(bundle_version(&running), "0.2.0");
        assert_eq!(bundle_version(&previous_path(&running)), "0.1.0");
        assert!(!new_bundle.exists());
        assert_eq!(
            entries(applications.path()),
            [".mustr.app.previous", "mustr.app"]
        );

        // The next launch cleans up.
        assert!(remove_previous(&running).unwrap());
        assert!(!remove_previous(&running).unwrap());
        assert_eq!(entries(applications.path()), ["mustr.app"]);
    }

    #[test]
    fn updates_from_another_volume_are_copied_in() {
        let applications = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let running = applications.path().join("mustr.app");
        fake_bundle(&running, BUNDLE_ID, "0.1.0");
        let new_bundle = data.path().join("mustr.app");
        fake_bundle(&new_bundle, BUNDLE_ID, "0.2.0");

        // As rename(2) fails between volumes (EXDEV).
        let rename = |from: &Path, to: &Path| {
            if from.starts_with(data.path()) {
                Err(io::Error::other("cross-device link"))
            } else {
                std::fs::rename(from, to)
            }
        };
        replace_with(&running, &new_bundle, &rename).unwrap();
        assert_eq!(bundle_version(&running), "0.2.0");
        check(&running, &Version::new(0, 2, 0)).unwrap();
        #[cfg(unix)]
        assert_eq!(
            std::fs::read_link(running.join("Contents/current")).unwrap(),
            Path::new("MacOS/mustr")
        );
        assert_eq!(bundle_version(&previous_path(&running)), "0.1.0");
    }

    #[test]
    fn a_failed_swap_puts_the_running_bundle_back() {
        let applications = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let running = applications.path().join("mustr.app");
        fake_bundle(&running, BUNDLE_ID, "0.1.0");
        let new_bundle = data.path().join("mustr.app");
        fake_bundle(&new_bundle, BUNDLE_ID, "0.2.0");

        // The third rename moves the update into place; make it fail.
        let calls = Cell::new(0);
        let rename = |from: &Path, to: &Path| {
            calls.set(calls.get() + 1);
            if calls.get() == 3 {
                Err(io::Error::other("disk full"))
            } else {
                std::fs::rename(from, to)
            }
        };
        let error = replace_with(&running, &new_bundle, &rename).unwrap_err();
        assert!(
            format!("{error:#}").contains("the app there is unchanged: disk full"),
            "{error:#}"
        );
        assert_eq!(bundle_version(&running), "0.1.0");
        assert_eq!(entries(applications.path()), ["mustr.app"]);

        // Moving the running bundle aside fails (say, /Applications isn't writable).
        let new_bundle = data.path().join("again.app");
        fake_bundle(&new_bundle, BUNDLE_ID, "0.2.0");
        let rename = |from: &Path, to: &Path| {
            if from == running {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            } else {
                std::fs::rename(from, to)
            }
        };
        let error = replace_with(&running, &new_bundle, &rename).unwrap_err();
        assert!(format!("{error:#}").contains("moving"), "{error:#}");
        assert_eq!(bundle_version(&running), "0.1.0");
        assert_eq!(entries(applications.path()), ["mustr.app"]);
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}
