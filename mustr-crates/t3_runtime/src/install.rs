//! Installs a T3 Code release archive into a `versions` directory the way T3's own installer
//! does (`https://t3.codes/install.sh`, `mustr-docs/T3-RUNTIME.md` §3):
//!
//! 1. download the release's `SHA256SUMS` and the archive into `<versions>/.staging-<random>`,
//! 2. check the archive's SHA-256 against `SHA256SUMS`, and stop on any mismatch,
//! 3. unpack it, keeping the archive's top-level `t3-<V>-<platform>-<arch>` directory's contents,
//! 4. check that `t3 --version` runs,
//! 5. write `.install-complete` (holding the version), and
//! 6. move the result to `<versions>/<V>` with a rename.
//!
//! A directory with `.install-complete` naming its version has passed all of these, so it is
//! reused as is. The same layout serves T3's home (`~/.t3/runtime/versions`), where T3's service
//! and `t3 update` look for runtimes, and mustr's own copy (`<mustr data>/t3-runtime`).

use anyhow::{Context as _, Result, anyhow, bail};
use futures::{AsyncReadExt as _, AsyncWriteExt as _, FutureExt as _};
use http_client::{AsyncBody, HttpClient};
use sha2::{Digest as _, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use crate::{
    Sleep,
    release::{self, ArchiveKind, Target},
};

pub const INSTALL_COMPLETE_FILE: &str = ".install-complete";
const STAGING_PREFIX: &str = ".staging-";
const SMOKE_TEST_TIMEOUT: Duration = Duration::from_secs(60);
const PROGRESS_INTERVAL_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledRuntime {
    pub version: String,
    pub dir: PathBuf,
    pub executable: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallStep {
    Downloading { received: u64, total: Option<u64> },
    Verifying,
    Extracting,
    Checking,
}

impl InstallStep {
    pub fn describe(self) -> String {
        const MB: f64 = 1024. * 1024.;
        match self {
            InstallStep::Downloading {
                received,
                total: Some(total),
            } if total > 0 => format!(
                "Downloading ({:.0} of {:.0} MB)",
                received as f64 / MB,
                total as f64 / MB
            ),
            InstallStep::Downloading { received, .. } => {
                format!("Downloading ({:.0} MB)", received as f64 / MB)
            }
            InstallStep::Verifying => "Verifying the download".to_string(),
            InstallStep::Extracting => "Unpacking".to_string(),
            InstallStep::Checking => "Checking that it runs".to_string(),
        }
    }
}

/// The runtime for `version` in `versions_dir`, if a finished install is there.
pub fn find_installed(
    versions_dir: &Path,
    version: &str,
    target: Target,
) -> Option<InstalledRuntime> {
    release::validate_version(version).ok()?;
    let dir = versions_dir.join(version);
    let marker = std::fs::read_to_string(dir.join(INSTALL_COMPLETE_FILE)).ok()?;
    if marker.trim() != version {
        return None;
    }
    let executable = dir.join(target.executable_name());
    is_executable(&executable).then(|| InstalledRuntime {
        version: version.to_string(),
        dir,
        executable,
    })
}

/// Every finished install in `versions_dir`, newest first.
pub fn installed_versions(versions_dir: &Path, target: Target) -> Vec<InstalledRuntime> {
    let Ok(entries) = std::fs::read_dir(versions_dir) else {
        return Vec::new();
    };
    let mut runtimes = entries
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().into_string().ok()?;
            let version = semver::Version::parse(&name).ok()?;
            Some((version, find_installed(versions_dir, &name, target)?))
        })
        .collect::<Vec<_>>();
    runtimes.sort_by(|(left, _), (right, _)| right.cmp(left));
    runtimes.into_iter().map(|(_, runtime)| runtime).collect()
}

pub fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub struct Installer {
    pub http: Arc<dyn HttpClient>,
    pub base_url: String,
    pub target: Target,
    pub sleep: Sleep,
    /// Run `t3 --version` before accepting an install, as T3's installer does. Tests that serve
    /// a fake archive turn it off.
    pub smoke_test: bool,
}

impl Installer {
    pub fn new(http: Arc<dyn HttpClient>, target: Target, sleep: Sleep) -> Self {
        Self {
            http,
            base_url: release::release_base_url(),
            target,
            sleep,
            smoke_test: true,
        }
    }

    /// Returns the finished install of `version` in `versions_dir`, installing it first if it
    /// isn't there.
    pub async fn ensure(
        &self,
        versions_dir: &Path,
        version: &str,
        progress: &(dyn Fn(InstallStep) + Send + Sync),
    ) -> Result<InstalledRuntime> {
        release::validate_version(version)?;
        if let Some(runtime) = find_installed(versions_dir, version, self.target) {
            return Ok(runtime);
        }
        std::fs::create_dir_all(versions_dir)
            .with_context(|| format!("creating {}", versions_dir.display()))?;
        let staging = StagingDir::create(versions_dir)?;
        let runtime = self
            .install_into(&staging.path, versions_dir, version, progress)
            .await?;
        drop(staging);
        log::info!("Installed T3 Code {version} in {}", runtime.dir.display());
        Ok(runtime)
    }

    async fn install_into(
        &self,
        staging: &Path,
        versions_dir: &Path,
        version: &str,
        progress: &(dyn Fn(InstallStep) + Send + Sync),
    ) -> Result<InstalledRuntime> {
        let asset = self.target.asset_name(version);
        progress(InstallStep::Downloading {
            received: 0,
            total: None,
        });
        let sums_url = release::release_file_url(&self.base_url, version, release::CHECKSUMS_FILE);
        let sums = match fetch_text(self.http.as_ref(), &sums_url).await? {
            Some(sums) => sums,
            None => bail!(
                "T3 Code {version} has no engine archives to download ({sums_url} isn't there)"
            ),
        };
        let expected = release::checksum_for(&sums, &asset)?;

        let archive = staging.join(&asset);
        let archive_url = release::release_file_url(&self.base_url, version, &asset);
        let actual = download(self.http.as_ref(), &archive_url, &archive, progress).await?;
        progress(InstallStep::Verifying);
        if actual != expected {
            bail!(
                "The download of {asset} doesn't match the checksum T3 Code published for it \
                 (expected {expected}, got {actual}), so mustr deleted it"
            );
        }

        progress(InstallStep::Extracting);
        extract(&archive, staging, self.target.archive_kind()).await?;
        std::fs::remove_file(&archive).ok();
        let root = staging.join(self.target.archive_stem(version));
        if !root.is_dir() {
            bail!(
                "{asset} has no {} directory",
                self.target.archive_stem(version)
            );
        }
        let executable = root.join(self.target.executable_name());
        if !is_executable(&executable) {
            bail!(
                "{asset} has no {} executable",
                self.target.executable_name()
            );
        }

        if self.smoke_test {
            progress(InstallStep::Checking);
            smoke_test(&executable, version, &self.sleep).await?;
        }
        std::fs::write(root.join(INSTALL_COMPLETE_FILE), format!("{version}\n"))?;

        let target_dir = versions_dir.join(version);
        if target_dir.exists() {
            // An unfinished install from before; T3's installer replaces it the same way.
            std::fs::rename(&target_dir, staging.join("replaced"))
                .with_context(|| format!("moving aside {}", target_dir.display()))?;
        }
        std::fs::rename(&root, &target_dir)
            .with_context(|| format!("moving the new runtime to {}", target_dir.display()))?;
        Ok(InstalledRuntime {
            version: version.to_string(),
            executable: target_dir.join(self.target.executable_name()),
            dir: target_dir,
        })
    }
}

/// Removes every version in `versions_dir` except `keep` and the newest other one (to roll back
/// to), and any unfinished install. Only for directories mustr owns: T3 prunes its own home.
pub fn prune_versions(versions_dir: &Path, keep: &str, target: Target) -> Result<Vec<String>> {
    let Ok(entries) = std::fs::read_dir(versions_dir) else {
        return Ok(Vec::new());
    };
    let mut others = Vec::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with(STAGING_PREFIX) {
            std::fs::remove_dir_all(entry.path()).ok();
        } else if name != keep
            && let Ok(version) = semver::Version::parse(&name)
        {
            others.push((version, name));
        }
    }
    others.sort_by(|(left, _), (right, _)| right.cmp(left));
    let previous = others
        .iter()
        .position(|(_, name)| find_installed(versions_dir, name, target).is_some());
    let mut removed = Vec::new();
    for (index, (_, name)) in others.into_iter().enumerate() {
        if Some(index) == previous {
            continue;
        }
        std::fs::remove_dir_all(versions_dir.join(&name))
            .with_context(|| format!("removing T3 Code {name}"))?;
        removed.push(name);
    }
    Ok(removed)
}

/// A staging directory that is deleted unless the install inside it finished and moved out.
struct StagingDir {
    path: PathBuf,
}

impl StagingDir {
    fn create(versions_dir: &Path) -> Result<Self> {
        let suffix: [u8; 6] = rand::random();
        let path = versions_dir.join(format!("{STAGING_PREFIX}{}", hex::encode(suffix)));
        std::fs::create_dir(&path).with_context(|| format!("creating {}", path.display()))?;
        Ok(Self { path })
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.path).ok();
    }
}

/// `GET url` as text, or `None` on a 404.
async fn fetch_text(http: &dyn HttpClient, url: &str) -> Result<Option<String>> {
    let mut response = http
        .get(url, AsyncBody::empty(), true)
        .await
        .with_context(|| format!("downloading {url}"))?;
    if response.status().as_u16() == 404 {
        return Ok(None);
    }
    if !response.status().is_success() {
        bail!("{url} returned {}", response.status());
    }
    let mut body = String::new();
    response
        .body_mut()
        .read_to_string(&mut body)
        .await
        .with_context(|| format!("downloading {url}"))?;
    Ok(Some(body))
}

/// Streams `url` into `destination` and returns its SHA-256 in lowercase hex.
async fn download(
    http: &dyn HttpClient,
    url: &str,
    destination: &Path,
    progress: &(dyn Fn(InstallStep) + Send + Sync),
) -> Result<String> {
    let mut response = http
        .get(url, AsyncBody::empty(), true)
        .await
        .with_context(|| format!("downloading {url}"))?;
    if !response.status().is_success() {
        bail!("{url} returned {}", response.status());
    }
    let total = response
        .headers()
        .get(http_client::http::header::CONTENT_LENGTH)
        .and_then(|length| length.to_str().ok()?.parse::<u64>().ok());
    let mut file = smol::fs::File::create(destination)
        .await
        .with_context(|| format!("creating {}", destination.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 256 * 1024];
    let mut received = 0u64;
    let mut reported = 0u64;
    loop {
        let read = response
            .body_mut()
            .read(&mut buffer)
            .await
            .with_context(|| format!("downloading {url}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read]).await?;
        received += read as u64;
        if received - reported >= PROGRESS_INTERVAL_BYTES {
            reported = received;
            progress(InstallStep::Downloading { received, total });
        }
    }
    file.flush().await?;
    file.sync_all().await?;
    if let Some(total) = total
        && received != total
    {
        bail!("the download of {url} stopped after {received} of {total} bytes");
    }
    progress(InstallStep::Downloading { received, total });
    Ok(hex::encode(hasher.finalize()))
}

/// The SHA-256 of a file, in lowercase hex.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

/// Fails unless `path`'s SHA-256 is `expected` (hex, any case).
pub fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let actual = sha256_file(path)?;
    if actual.eq_ignore_ascii_case(expected.trim()) {
        Ok(())
    } else {
        Err(anyhow!(
            "{} doesn't match its checksum (expected {expected}, got {actual})",
            path.display()
        ))
    }
}

async fn extract(archive: &Path, destination: &Path, kind: ArchiveKind) -> Result<()> {
    let file = smol::fs::File::open(archive)
        .await
        .with_context(|| format!("opening {}", archive.display()))?;
    match kind {
        ArchiveKind::TarGz => {
            let decoder = async_compression::futures::bufread::GzipDecoder::new(
                futures::io::BufReader::new(file),
            );
            // `unpack` refuses entries that would land outside `destination`.
            async_tar::Archive::new(decoder)
                .unpack(destination)
                .await
                .with_context(|| format!("unpacking {}", archive.display()))
        }
        ArchiveKind::Zip => util::archive::extract_zip(destination, file)
            .await
            .with_context(|| format!("unpacking {}", archive.display())),
    }
}

async fn smoke_test(executable: &Path, version: &str, sleep: &Sleep) -> Result<()> {
    let mut command = util::command::new_command(executable);
    command
        .arg("--version")
        .stdin(util::command::Stdio::null())
        .kill_on_drop(true);
    for key in crate::scrubbed_env_keys(std::env::vars_os().map(|(key, _)| key)) {
        command.env_remove(key);
    }
    let output = futures::select! {
        output = command.output().fuse() => output
            .with_context(|| format!("running {} --version", executable.display()))?,
        _ = sleep(SMOKE_TEST_TIMEOUT).fuse() => {
            bail!("{} --version didn't finish within {}s", executable.display(), SMOKE_TEST_TIMEOUT.as_secs())
        }
    };
    if !output.status.success() {
        bail!(
            "the downloaded T3 Code doesn't run ({}): {}",
            output.status,
            crate::truncate(String::from_utf8_lossy(&output.stderr).trim(), 300)
        );
    }
    let printed = String::from_utf8_lossy(&output.stdout);
    if !printed.contains(version) {
        log::warn!(
            "{} --version printed {:?}, not {version}",
            executable.display(),
            crate::truncate(printed.trim(), 100)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_client::{FakeHttpClient, Response};
    use parking_lot::Mutex;

    const VERSION: &str = "0.1.1";

    fn linux() -> Target {
        Target::from_rust("linux", "x86_64").unwrap()
    }

    /// A tar.gz shaped like T3's: `t3-<V>-<platform>-<arch>/` holding `t3` and `client/`.
    fn fake_archive(target: Target) -> Vec<u8> {
        smol::block_on(async {
            let stem = target.archive_stem(VERSION);
            let encoder = async_compression::futures::write::GzipEncoder::new(Vec::new());
            let mut builder = async_tar::Builder::new(encoder);
            let mut add = async |path: String, contents: &[u8], mode: u32| {
                let mut header = async_tar::Header::new_gnu();
                header.set_size(contents.len() as u64);
                header.set_mode(mode);
                header.set_cksum();
                builder
                    .append_data(&mut header, path, contents)
                    .await
                    .unwrap();
            };
            add(format!("{stem}/t3"), b"#!/bin/sh\necho 0.1.1\n", 0o755).await;
            add(format!("{stem}/client/index.html"), b"<html></html>", 0o644).await;
            let mut encoder = builder.into_inner().await.unwrap();
            encoder.close().await.unwrap();
            encoder.into_inner()
        })
    }

    struct FakeRelease {
        http: Arc<http_client::HttpClientWithUrl>,
        requests: Arc<Mutex<Vec<String>>>,
    }

    fn fake_release(archive: Vec<u8>, sums: String) -> FakeRelease {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let http = FakeHttpClient::create({
            let requests = requests.clone();
            move |request| {
                let uri = request.uri().to_string();
                requests.lock().push(uri.clone());
                // The release has only version 0.1.1.
                let body = if !uri.contains("/v0.1.1/") {
                    None
                } else if uri.ends_with("/SHA256SUMS") {
                    Some(sums.clone().into_bytes())
                } else if uri.ends_with(".tar.gz") {
                    Some(archive.clone())
                } else {
                    None
                };
                async move {
                    Ok(match body {
                        Some(body) => Response::builder()
                            .status(200)
                            .header("content-length", body.len())
                            .body(AsyncBody::from(body))?,
                        None => Response::builder().status(404).body(AsyncBody::empty())?,
                    })
                }
            }
        });
        FakeRelease { http, requests }
    }

    fn installer(release: &FakeRelease) -> Installer {
        Installer {
            http: release.http.clone(),
            base_url: "https://releases.example".to_string(),
            target: linux(),
            sleep: crate::test_sleep(),
            smoke_test: cfg!(unix),
        }
    }

    fn sums_for(archive: &[u8]) -> String {
        format!(
            "{}  t3-0.1.1-linux-x64.tar.gz\n{}  t3-0.1.1-linux-arm64.tar.gz\n",
            hex::encode(Sha256::digest(archive)),
            "0".repeat(64)
        )
    }

    fn no_progress() -> impl Fn(InstallStep) + Send + Sync {
        |_| {}
    }

    #[test]
    fn installs_verifies_and_then_reuses() {
        let temp = tempfile::tempdir().unwrap();
        let versions = temp.path().join("versions");
        let archive = fake_archive(linux());
        let release = fake_release(archive.clone(), sums_for(&archive));
        let installer = installer(&release);

        let steps = Arc::new(Mutex::new(Vec::new()));
        let record = {
            let steps = steps.clone();
            move |step: InstallStep| steps.lock().push(step)
        };
        let runtime = smol::block_on(installer.ensure(&versions, VERSION, &record)).unwrap();
        assert_eq!(runtime.dir, versions.join(VERSION));
        assert_eq!(runtime.executable, versions.join(VERSION).join("t3"));
        assert!(is_executable(&runtime.executable));
        assert!(runtime.dir.join("client/index.html").is_file());
        assert_eq!(
            std::fs::read_to_string(runtime.dir.join(INSTALL_COMPLETE_FILE)).unwrap(),
            "0.1.1\n"
        );
        // Nothing is left in staging.
        let leftovers = std::fs::read_dir(&versions)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(leftovers, [VERSION]);
        assert_eq!(
            *release.requests.lock(),
            [
                "https://releases.example/v0.1.1/SHA256SUMS",
                "https://releases.example/v0.1.1/t3-0.1.1-linux-x64.tar.gz"
            ]
        );
        let steps = steps.lock().clone();
        assert!(steps.contains(&InstallStep::Verifying));
        assert!(steps.contains(&InstallStep::Downloading {
            received: archive.len() as u64,
            total: Some(archive.len() as u64)
        }));

        // A finished install is reused without downloading anything.
        release.requests.lock().clear();
        assert_eq!(
            smol::block_on(installer.ensure(&versions, VERSION, &no_progress())).unwrap(),
            runtime
        );
        assert!(release.requests.lock().is_empty());
        assert_eq!(find_installed(&versions, VERSION, linux()), Some(runtime));
    }

    #[test]
    fn a_tampered_download_is_refused_and_deleted() {
        let temp = tempfile::tempdir().unwrap();
        let versions = temp.path().join("versions");
        let archive = fake_archive(linux());
        let sums = sums_for(&archive);
        let mut tampered = archive;
        let last = tampered.len() - 1;
        tampered[last] ^= 0xff;
        let release = fake_release(tampered, sums);

        let error = smol::block_on(installer(&release).ensure(&versions, VERSION, &no_progress()))
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("doesn't match the checksum"),
            "{error:#}"
        );
        assert_eq!(std::fs::read_dir(&versions).unwrap().count(), 0);
        assert_eq!(find_installed(&versions, VERSION, linux()), None);
    }

    #[test]
    fn an_unlisted_asset_or_a_missing_release_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let versions = temp.path().join("versions");
        let archive = fake_archive(linux());
        let release = fake_release(
            archive,
            format!("{}  t3-0.1.1-darwin-arm64.tar.gz\n", "0".repeat(64)),
        );
        let error = smol::block_on(installer(&release).ensure(&versions, VERSION, &no_progress()))
            .unwrap_err();
        assert!(format!("{error:#}").contains("isn't listed"), "{error:#}");
        // The archive itself was never downloaded.
        assert_eq!(release.requests.lock().len(), 1);

        let missing = installer(&release);
        let error = smol::block_on(missing.ensure(&versions, "0.0.1", &no_progress())).unwrap_err();
        assert!(
            format!("{error:#}").contains("has no engine archives"),
            "{error:#}"
        );
        assert_eq!(std::fs::read_dir(&versions).unwrap().count(), 0);
    }

    #[test]
    fn verifies_files_against_checksums() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("t3.tar.gz");
        std::fs::write(&path, b"hello").unwrap();
        let sum = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert_eq!(sha256_file(&path).unwrap(), sum);
        verify_sha256(&path, &sum.to_uppercase()).unwrap();
        std::fs::write(&path, b"hellO").unwrap();
        assert!(verify_sha256(&path, sum).is_err());
    }

    #[test]
    fn unfinished_installs_are_not_reused() {
        let temp = tempfile::tempdir().unwrap();
        let versions = temp.path();
        let dir = versions.join(VERSION);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("t3"), "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(dir.join("t3"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        assert_eq!(find_installed(versions, VERSION, linux()), None);
        std::fs::write(dir.join(INSTALL_COMPLETE_FILE), "0.1.0\n").unwrap();
        assert_eq!(find_installed(versions, VERSION, linux()), None);
        std::fs::write(dir.join(INSTALL_COMPLETE_FILE), "0.1.1\n").unwrap();
        assert!(find_installed(versions, VERSION, linux()).is_some());
        assert_eq!(find_installed(versions, "../0.1.1", linux()), None);
    }

    #[test]
    fn pruning_keeps_the_current_and_previous_versions() {
        let temp = tempfile::tempdir().unwrap();
        let versions = temp.path();
        for version in ["0.0.9", "0.1.0", "0.1.1", "0.1.2"] {
            let dir = versions.join(version);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(INSTALL_COMPLETE_FILE), version).unwrap();
            std::fs::write(dir.join("t3"), "").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(dir.join("t3"), std::fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
        }
        // An unfinished 0.2.0 isn't something to roll back to.
        std::fs::create_dir_all(versions.join("0.2.0")).unwrap();
        std::fs::create_dir_all(versions.join(".staging-abc")).unwrap();
        std::fs::create_dir_all(versions.join("notes")).unwrap();

        let mut removed = prune_versions(versions, "0.1.1", linux()).unwrap();
        removed.sort();
        assert_eq!(removed, ["0.0.9", "0.1.0", "0.2.0"]);
        let mut left = std::fs::read_dir(versions)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        left.sort();
        assert_eq!(left, ["0.1.1", "0.1.2", "notes"]);
        assert_eq!(installed_versions(versions, linux())[0].version, "0.1.2");
    }

    #[test]
    fn progress_reads_well() {
        assert_eq!(
            InstallStep::Downloading {
                received: 50 * 1024 * 1024,
                total: Some(116 * 1024 * 1024)
            }
            .describe(),
            "Downloading (50 of 116 MB)"
        );
        assert_eq!(InstallStep::Verifying.describe(), "Verifying the download");
    }
}
