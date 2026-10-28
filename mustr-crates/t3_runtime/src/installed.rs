//! The user's own T3 Code: its home (`$T3CODE_HOME` or `~/.t3`), the runtimes in it, the server
//! running from it, its background service, and the `t3` commands mustr runs on it.
//!
//! mustr reads this install and asks T3's own CLI to change it (`t3 service install`,
//! `t3 service restart`, `t3 update`), so the result is what T3's tools would produce. When the
//! user has no T3 Code yet, mustr installs it the way `https://t3.codes/install.sh` does
//! ([`crate::install`] plus [`link_launcher`]), then sets up the service with `t3 service install`.

use anyhow::{Context as _, Result, bail};
use futures::FutureExt as _;
use serde::Deserialize;
use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    time::Duration,
};
use url::Url;

use crate::{
    Sleep,
    install::{self, InstalledRuntime, is_executable},
    release::{Os, Target},
};

/// T3's service unit names (`apps/server/src/cloud/bootService.ts`), fixed by T3.
pub const SYSTEMD_UNIT_FILE: &str = "t3code.service";
pub const LAUNCHD_PLIST_FILE: &str = "com.t3tools.t3code.service.plist";
const CLI_TIMEOUT: Duration = Duration::from_secs(120);

/// Where T3 keeps its data: `$T3CODE_HOME`, or `~/.t3`.
pub fn t3_home() -> Option<PathBuf> {
    std::env::var_os("T3CODE_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".t3")))
}

/// `<home>/runtime/versions`, where T3's installer, service, and `t3 update` keep runtimes.
pub fn versions_dir(t3_home: &Path) -> PathBuf {
    t3_home.join("runtime").join("versions")
}

pub fn t3_executable_name() -> &'static str {
    if cfg!(windows) { "t3.exe" } else { "t3" }
}

/// Finds a local T3 CLI.
///
/// The CLI opens T3's database with its own migrations, so the runtime matching the server's
/// version comes first (`mustr-docs/T3-RUNTIME.md` §8). Then `t3` on `PATH`, which is T3's
/// launcher, then the newest installed runtime.
pub fn find_t3_cli(
    t3_home: Option<&Path>,
    path_env: Option<&OsStr>,
    server_version: Option<&str>,
) -> Option<PathBuf> {
    let versions_dir = t3_home.map(versions_dir);
    if let (Some(versions_dir), Some(version)) = (&versions_dir, server_version)
        // The version comes from the server, so it must not be able to name another directory.
        && semver::Version::parse(version).is_ok()
    {
        let candidate = versions_dir.join(version).join(t3_executable_name());
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    if let Some(path_env) = path_env {
        let on_path = std::env::split_paths(path_env)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(t3_executable_name()))
            .find(|candidate| is_executable(candidate));
        if on_path.is_some() {
            return on_path;
        }
    }
    newest_runtime_cli(versions_dir.as_deref()?)
}

fn newest_runtime_cli(versions_dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(versions_dir)
        .ok()?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let version = semver::Version::parse(entry.file_name().to_str()?).ok()?;
            let cli = entry.path().join(t3_executable_name());
            is_executable(&cli).then_some((version, cli))
        })
        .max_by(|(left, _), (right, _)| left.cmp(right))
        .map(|(_, cli)| cli)
}

/// What `<home>/userdata/server-runtime.json` says about the server running from a T3 home.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerRecord {
    pub pid: u32,
    pub origin: Url,
    pub server_version: Option<String>,
    pub service_managed: bool,
}

pub fn read_server_record(t3_home: &Path) -> Option<ServerRecord> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Record {
        pid: u32,
        origin: String,
        #[serde(default)]
        server_version: Option<String>,
        #[serde(default)]
        service_managed: Option<bool>,
    }
    let path = t3_home.join("userdata").join("server-runtime.json");
    let bytes = std::fs::read(&path).ok()?;
    let record: Record = serde_json::from_slice(&bytes)
        .map_err(|error| log::warn!("{} isn't T3's server record: {error}", path.display()))
        .ok()?;
    let origin = Url::parse(&record.origin).ok()?;
    matches!(origin.scheme(), "http" | "https").then_some(ServerRecord {
        pid: record.pid,
        origin,
        server_version: record.server_version,
        service_managed: record.service_managed.unwrap_or(false),
    })
}

/// The server recorded in a T3 home, if its process is still alive.
pub fn running_server(t3_home: &Path) -> Option<ServerRecord> {
    read_server_record(t3_home).filter(|record| process_is_alive(record.pid))
}

pub fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        // SAFETY: signal 0 only checks that the process exists and may be signaled.
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        use sysinfo::{Pid, ProcessesToUpdate, System};
        let mut system = System::new();
        let pid = Pid::from_u32(pid);
        system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
        system.process(pid).is_some()
    }
}

/// The service managers T3's `t3 service` supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceManager {
    /// A systemd user unit, `~/.config/systemd/user/t3code.service`.
    Systemd,
    /// A launch agent, `~/Library/LaunchAgents/com.t3tools.t3code.service.plist`.
    Launchd,
}

impl ServiceManager {
    /// T3 supports Linux with systemd and macOS with launchd; Windows has no service.
    pub fn for_os(os: Os) -> Option<Self> {
        match os {
            Os::Linux => Some(Self::Systemd),
            Os::MacOs => Some(Self::Launchd),
            Os::Windows => None,
        }
    }

    pub fn unit_path(self, user_home: &Path) -> PathBuf {
        match self {
            Self::Systemd => user_home
                .join(".config")
                .join("systemd")
                .join("user")
                .join(SYSTEMD_UNIT_FILE),
            Self::Launchd => user_home
                .join("Library")
                .join("LaunchAgents")
                .join(LAUNCHD_PLIST_FILE),
        }
    }

    /// Whether T3's service is installed for `t3_home`. The unit names T3's home in its
    /// environment (`T3CODE_HOME`), so a service for another home doesn't count.
    pub fn is_installed_for(self, user_home: &Path, t3_home: &Path) -> bool {
        let Ok(unit) = std::fs::read_to_string(self.unit_path(user_home)) else {
            return false;
        };
        match unit_t3_home(&unit) {
            Some(home) => Path::new(&home) == t3_home,
            None => true,
        }
    }
}

/// The `T3CODE_HOME` a T3 service unit or plist sets (T3's `readServiceBaseDir`).
fn unit_t3_home(unit: &str) -> Option<String> {
    let systemd = unit.lines().find_map(|line| {
        line.trim()
            .strip_prefix("Environment=T3CODE_HOME=")
            .map(|value| value.trim().trim_matches('"').to_string())
    });
    systemd.or_else(|| {
        let after_key = unit.split("<key>T3CODE_HOME</key>").nth(1)?;
        let start = after_key.find("<string>")? + "<string>".len();
        let end = after_key[start..].find("</string>")? + start;
        Some(after_key[start..end].trim().to_string())
    })
}

/// A `t3` command mustr runs: argv only, never through a shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

impl std::fmt::Display for CliCommand {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = self.program.file_name().map_or_else(
            || self.program.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        write!(formatter, "{name}")?;
        for arg in &self.args {
            write!(formatter, " {}", arg.to_string_lossy())?;
        }
        Ok(())
    }
}

impl CliCommand {
    fn new(cli: &Path, args: &[&str], t3_home: &Path) -> Self {
        let mut args = args.iter().map(OsString::from).collect::<Vec<_>>();
        args.push("--base-dir".into());
        args.push(t3_home.as_os_str().to_owned());
        Self {
            program: cli.to_path_buf(),
            args,
        }
    }

    /// `t3 service install`: installs T3's background service (systemd user unit or launch agent)
    /// for this home, and starts it.
    pub fn service_install(cli: &Path, t3_home: &Path) -> Self {
        Self::new(cli, &["service", "install"], t3_home)
    }

    /// `t3 service restart`: starts an installed service that isn't running.
    pub fn service_restart(cli: &Path, t3_home: &Path) -> Self {
        Self::new(cli, &["service", "restart"], t3_home)
    }

    /// `t3 update <version> --yes`: downloads that release into the home and switches the CLI and
    /// the service to it. `--yes` restarts the service without a prompt, which a GUI can't answer.
    pub fn update(cli: &Path, version: &str, t3_home: &Path) -> Self {
        Self::new(cli, &["update", version, "--yes"], t3_home)
    }

    /// Runs the command with T3's variables scrubbed and no stdin, returning its stdout.
    pub async fn run(&self, sleep: &Sleep) -> Result<String> {
        let mut command = util::command::new_command(&self.program);
        command
            .args(&self.args)
            .stdin(util::command::Stdio::null())
            .kill_on_drop(true);
        for key in crate::scrubbed_env_keys(std::env::vars_os().map(|(key, _)| key)) {
            command.env_remove(key);
        }
        log::info!("running `{self}`");
        let output = futures::select! {
            output = command.output().fuse() => output.with_context(|| format!("running `{self}`"))?,
            _ = sleep(CLI_TIMEOUT).fuse() => bail!("`{self}` didn't finish within {}s", CLI_TIMEOUT.as_secs()),
        };
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = if stderr.trim().is_empty() {
                stdout.trim().to_string()
            } else {
                stderr.trim().to_string()
            };
            bail!(
                "`{self}` failed ({}): {}",
                output.status,
                crate::truncate(&detail, 400)
            );
        }
        Ok(stdout)
    }
}

/// `~/.local/bin`, where T3's installer links `t3` (`T3CODE_INSTALL_BIN_DIR`'s default).
pub fn default_bin_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".local").join("bin"))
}

/// The installer's last step ("Setting up the t3 command" in `install.sh`): links
/// `<bin_dir>/t3` to the runtime, and lists that link in `<home>/runtime/launchers` so T3's
/// pruning keeps the runtime it points at. On Windows, `install.ps1` writes a `t3.cmd` shim
/// instead of a link.
pub fn link_launcher(
    runtime: &InstalledRuntime,
    bin_dir: &Path,
    t3_home: &Path,
) -> Result<PathBuf> {
    std::fs::create_dir_all(bin_dir).with_context(|| format!("creating {}", bin_dir.display()))?;
    #[cfg(unix)]
    let launcher = {
        let launcher = bin_dir.join("t3");
        // `ln -sfn`: replace whatever is there.
        match std::fs::symlink_metadata(&launcher) {
            Ok(metadata) if metadata.is_dir() => {
                bail!("{} is a directory", launcher.display())
            }
            Ok(_) => std::fs::remove_file(&launcher)
                .with_context(|| format!("replacing {}", launcher.display()))?,
            Err(_) => {}
        }
        std::os::unix::fs::symlink(&runtime.executable, &launcher)
            .with_context(|| format!("linking {}", launcher.display()))?;
        launcher
    };
    #[cfg(windows)]
    let launcher = {
        let launcher = bin_dir.join("t3.cmd");
        std::fs::write(
            &launcher,
            format!("@echo off\r\n\"{}\" %*\r\n", runtime.executable.display()),
        )
        .with_context(|| format!("writing {}", launcher.display()))?;
        launcher
    };

    // `cd -P`: the launcher's path with the bin directory's symlinks resolved.
    let listed = std::fs::canonicalize(bin_dir)
        .unwrap_or_else(|_| bin_dir.to_path_buf())
        .join(launcher.file_name().unwrap_or_default());
    let launchers = t3_home.join("runtime").join("launchers");
    let existing = std::fs::read_to_string(&launchers).unwrap_or_default();
    let listed_text = listed.to_string_lossy();
    if !existing.lines().any(|line| line == listed_text) {
        if let Some(parent) = launchers.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut contents = existing;
        if !contents.is_empty() && !contents.ends_with('\n') {
            contents.push('\n');
        }
        contents.push_str(&listed_text);
        contents.push('\n');
        std::fs::write(&launchers, contents)
            .with_context(|| format!("writing {}", launchers.display()))?;
    }
    Ok(launcher)
}

/// What mustr finds of the user's own T3 Code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallState {
    /// No runtime in the home, and no `t3` on `PATH`.
    NotInstalled,
    /// T3 is installed; `server` is set when a server is running from the home.
    Installed {
        cli: Option<PathBuf>,
        runtimes: Vec<InstalledRuntime>,
        server: Option<ServerRecord>,
        /// T3's background service is installed for this home.
        service_installed: bool,
        /// T3 supports a background service on this platform.
        service_supported: bool,
    },
}

pub fn detect(
    t3_home: &Path,
    user_home: Option<&Path>,
    path_env: Option<&OsStr>,
    target: Target,
) -> InstallState {
    let server = running_server(t3_home);
    let runtimes = install::installed_versions(&versions_dir(t3_home), target);
    let cli = find_t3_cli(
        Some(t3_home),
        path_env,
        server
            .as_ref()
            .and_then(|server| server.server_version.as_deref()),
    );
    if cli.is_none() && runtimes.is_empty() && server.is_none() {
        return InstallState::NotInstalled;
    }
    let manager = ServiceManager::for_os(target.os);
    let service_installed = manager
        .zip(user_home)
        .is_some_and(|(manager, user_home)| manager.is_installed_for(user_home, t3_home));
    InstallState::Installed {
        cli,
        runtimes,
        server,
        service_installed,
        service_supported: manager.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cli(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let cli = dir.join(t3_executable_name());
        std::fs::write(&cli, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        cli
    }

    fn make_runtime(versions: &Path, version: &str) -> PathBuf {
        let cli = make_cli(&versions.join(version));
        std::fs::write(
            versions.join(version).join(install::INSTALL_COMPLETE_FILE),
            format!("{version}\n"),
        )
        .unwrap();
        cli
    }

    fn this_target() -> Target {
        Target::from_rust(if cfg!(windows) { "windows" } else { "linux" }, "x86_64").unwrap()
    }

    #[test]
    fn finds_the_runtime_matching_the_server_first() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join(".t3");
        let versions = versions_dir(&home);
        let matching = make_cli(&versions.join("0.1.1"));
        make_cli(&versions.join("0.1.10"));
        let bin = temp.path().join("bin");
        make_cli(&bin);

        let path_env = std::env::join_paths([&bin]).unwrap();
        assert_eq!(
            find_t3_cli(Some(&home), Some(&path_env), Some("0.1.1")),
            Some(matching)
        );
    }

    #[test]
    fn falls_back_to_path_then_to_the_newest_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join(".t3");
        let versions = versions_dir(&home);
        make_cli(&versions.join("0.1.9"));
        let newest = make_cli(&versions.join("0.1.10"));
        make_cli(&versions.join("0.1.10-nightly.1"));
        std::fs::create_dir_all(versions.join("not-a-version")).unwrap();
        // A directory without the binary, e.g. an interrupted install.
        std::fs::create_dir_all(versions.join("0.3.0")).unwrap();
        let bin = temp.path().join("bin");
        let on_path = make_cli(&bin);
        let path_env = std::env::join_paths([temp.path().join("empty"), bin]).unwrap();

        // The server's version isn't installed, so `t3` on PATH wins.
        assert_eq!(
            find_t3_cli(Some(&home), Some(&path_env), Some("0.4.0")),
            Some(on_path)
        );
        // Without one on PATH: the newest by semver, not by string order ("0.1.9" > "0.1.10").
        let empty_path = std::env::join_paths([temp.path().join("empty")]).unwrap();
        assert_eq!(
            find_t3_cli(Some(&home), Some(&empty_path), None),
            Some(newest)
        );
    }

    #[test]
    fn ignores_server_versions_that_are_paths_and_non_executables() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join(".t3");
        let versions = versions_dir(&home);
        make_cli(&temp.path().join("evil"));
        let not_executable = versions.join("0.1.1");
        std::fs::create_dir_all(&not_executable).unwrap();
        std::fs::write(not_executable.join(t3_executable_name()), "").unwrap();

        assert_eq!(find_t3_cli(Some(&home), None, Some("../../evil")), None);
        #[cfg(unix)]
        assert_eq!(find_t3_cli(Some(&home), None, Some("0.1.1")), None);
        assert_eq!(find_t3_cli(None, None, Some("0.1.1")), None);
    }

    #[test]
    fn reads_t3s_server_record() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        assert_eq!(read_server_record(home), None);
        std::fs::create_dir_all(home.join("userdata")).unwrap();
        let path = home.join("userdata/server-runtime.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"version":1,"pid":{},"ownerId":"o","port":3773,"origin":"http://127.0.0.1:3773","startedAt":"2026-10-27T00:00:00Z","serviceManaged":true,"serverVersion":"0.1.1"}}"#,
                std::process::id()
            ),
        )
        .unwrap();
        let record = read_server_record(home).unwrap();
        assert_eq!(
            record,
            ServerRecord {
                pid: std::process::id(),
                origin: Url::parse("http://127.0.0.1:3773").unwrap(),
                server_version: Some("0.1.1".to_string()),
                service_managed: true,
            }
        );
        // This test process is alive.
        assert_eq!(running_server(home), Some(record));

        std::fs::write(&path, r#"{"pid":1,"origin":"file:///etc/passwd"}"#).unwrap();
        assert_eq!(read_server_record(home), None);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(read_server_record(home), None);
    }

    #[test]
    fn dead_pids_are_not_running() {
        assert!(process_is_alive(std::process::id()));
        assert!(!process_is_alive(0));
        // Far above any real pid limit.
        assert!(!process_is_alive(i32::MAX as u32 - 1));
    }

    #[test]
    fn service_units_are_t3s() {
        let user_home = Path::new("/home/someone");
        assert_eq!(
            ServiceManager::Systemd.unit_path(user_home),
            Path::new("/home/someone/.config/systemd/user/t3code.service")
        );
        assert_eq!(
            ServiceManager::Launchd.unit_path(user_home),
            Path::new("/home/someone/Library/LaunchAgents/com.t3tools.t3code.service.plist")
        );
        assert_eq!(
            ServiceManager::for_os(Os::Linux),
            Some(ServiceManager::Systemd)
        );
        assert_eq!(
            ServiceManager::for_os(Os::MacOs),
            Some(ServiceManager::Launchd)
        );
        assert_eq!(ServiceManager::for_os(Os::Windows), None);
    }

    #[test]
    fn a_service_counts_only_for_its_own_home() {
        let temp = tempfile::tempdir().unwrap();
        let user_home = temp.path();
        let home = user_home.join(".t3");
        assert!(!ServiceManager::Systemd.is_installed_for(user_home, &home));

        let unit = ServiceManager::Systemd.unit_path(user_home);
        std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
        std::fs::write(
            &unit,
            format!(
                "[Service]\nEnvironment=T3CODE_HOME={}\nExecStart=/x/t3 __service-launcher\n",
                home.display()
            ),
        )
        .unwrap();
        assert!(ServiceManager::Systemd.is_installed_for(user_home, &home));
        assert!(!ServiceManager::Systemd.is_installed_for(user_home, &user_home.join("other")));

        let plist = ServiceManager::Launchd.unit_path(user_home);
        std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
        std::fs::write(
            &plist,
            format!(
                "<dict>\n    <key>T3CODE_HOME</key>\n    <string>{}</string>\n</dict>",
                home.display()
            ),
        )
        .unwrap();
        assert!(ServiceManager::Launchd.is_installed_for(user_home, &home));
        assert!(!ServiceManager::Launchd.is_installed_for(user_home, &user_home.join("other")));
    }

    #[test]
    fn t3_commands_are_t3s() {
        let cli = Path::new("/home/someone/.t3/runtime/versions/0.1.1/t3");
        let home = Path::new("/home/someone/.t3");
        let argv = |command: CliCommand| {
            assert_eq!(command.program, cli);
            command
                .args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            argv(CliCommand::service_install(cli, home)),
            ["service", "install", "--base-dir", "/home/someone/.t3"]
        );
        assert_eq!(
            argv(CliCommand::service_restart(cli, home)),
            ["service", "restart", "--base-dir", "/home/someone/.t3"]
        );
        assert_eq!(
            argv(CliCommand::update(cli, "0.1.3", home)),
            [
                "update",
                "0.1.3",
                "--yes",
                "--base-dir",
                "/home/someone/.t3"
            ]
        );
        assert_eq!(
            CliCommand::service_install(cli, home).to_string(),
            "t3 service install --base-dir /home/someone/.t3"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cli_commands_report_failures() {
        let temp = tempfile::tempdir().unwrap();
        let cli = temp.path().join("t3");
        std::fs::write(
            &cli,
            "#!/bin/sh\necho \"args: $*\"\n[ \"$1\" = fail ] && { echo nope >&2; exit 3; }\nexit 0\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let sleep = crate::test_sleep();
        let command = CliCommand::service_restart(&cli, temp.path());
        let stdout = smol::block_on(command.run(&sleep)).unwrap();
        assert!(
            stdout.contains("args: service restart --base-dir"),
            "{stdout}"
        );

        let failing = CliCommand {
            program: cli,
            args: vec!["fail".into()],
        };
        let error = smol::block_on(failing.run(&sleep)).unwrap_err();
        assert!(format!("{error:#}").contains("nope"), "{error:#}");
    }

    #[cfg(unix)]
    #[test]
    fn links_the_launcher_like_t3s_installer() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join(".t3");
        let versions = versions_dir(&home);
        let executable = make_runtime(&versions, "0.1.1");
        let runtime = install::find_installed(&versions, "0.1.1", this_target()).unwrap();
        let bin = temp.path().join(".local/bin");
        // Something already there is replaced, like `ln -sfn`.
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("t3"), "old").unwrap();

        let launcher = link_launcher(&runtime, &bin, &home).unwrap();
        assert_eq!(launcher, bin.join("t3"));
        assert_eq!(std::fs::read_link(&launcher).unwrap(), executable);
        let listed = std::fs::canonicalize(&bin).unwrap().join("t3");
        assert_eq!(
            std::fs::read_to_string(home.join("runtime/launchers")).unwrap(),
            format!("{}\n", listed.display())
        );
        // Linking again doesn't list it twice.
        link_launcher(&runtime, &bin, &home).unwrap();
        assert_eq!(
            std::fs::read_to_string(home.join("runtime/launchers"))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }

    #[test]
    fn detects_what_is_installed() {
        let temp = tempfile::tempdir().unwrap();
        let user_home = temp.path();
        let home = user_home.join(".t3");
        let empty_path = std::env::join_paths([user_home.join("empty")]).unwrap();
        let target = this_target();
        assert_eq!(
            detect(&home, Some(user_home), Some(&empty_path), target),
            InstallState::NotInstalled
        );

        let cli = make_runtime(&versions_dir(&home), "0.1.1");
        let InstallState::Installed {
            cli: found,
            runtimes,
            server,
            service_installed,
            service_supported,
        } = detect(&home, Some(user_home), Some(&empty_path), target)
        else {
            panic!("expected an install");
        };
        assert_eq!(found, Some(cli));
        assert_eq!(runtimes.len(), 1);
        assert_eq!(server, None);
        assert!(!service_installed);
        assert_eq!(service_supported, !cfg!(windows));
    }
}
