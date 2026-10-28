//! mustr's own copy of T3 Code's engine, started the way T3's desktop app starts its bundled
//! server (`apps/desktop/src/backend/DesktopBackendConfiguration.ts` in T3):
//!
//! - `t3 --mode desktop --bootstrap-fd 0 …`: desktop mode binds loopback, opens no browser, prints
//!   no pairing token, and exits when its launcher (mustr) goes away.
//! - On the descriptor, one JSON line, the `DesktopBackendBootstrap` envelope
//!   (`packages/contracts/src/desktopBootstrap.ts`) carrying a random secret. T3's desktop sends it
//!   on fd 3; its WSL launcher sends it on stdin (fd 0), which mustr uses on every platform
//!   because it needs no extra descriptors.
//! - The secret derives a rotating bootstrap token (HMAC-SHA256, 12-hour windows,
//!   `packages/shared/src/desktopBootstrapToken.ts`) that mustr exchanges at `/oauth/token` with
//!   the `environment-bootstrap` grant, so there is no pairing step and no `t3` CLI involved.
//! - Its own `--base-dir`, a free loopback port mustr picks (never T3's default 3773), and a
//!   scrubbed environment, so it can't touch the user's own T3 Code (`~/.t3`), its service, or
//!   its port.

use anyhow::{Context as _, Result, bail};
use futures::{AsyncWriteExt as _, FutureExt as _};
use http_client::{HttpClient, Url};
use serde_json::json;
use sha2::{Digest as _, Sha256};
use std::{
    ffi::OsString,
    fs::File,
    path::{Path, PathBuf},
    process::ExitStatus,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use t3_client::EnvironmentDescriptor;

use crate::Sleep;

pub const ENGINE_HOST: &str = "127.0.0.1";
/// T3's default port, which the user's own T3 Code listens on. mustr's engine never takes it.
pub const T3_DEFAULT_PORT: u16 = 3773;
/// `DESKTOP_BOOTSTRAP_TOKEN_WINDOW_MS` in T3's `desktopBootstrapToken.ts`.
pub const BOOTSTRAP_TOKEN_WINDOW_MS: u64 = 12 * 60 * 60 * 1000;
/// `SERVER_EXIT_CODE_STATE_DIR_OWNED`: another server owns the base dir. Never restart on it.
pub const EXIT_CODE_STATE_DIR_OWNED: i32 = 78;
/// How long the engine gets to stop its agents before it is killed. T3's desktop and service
/// launcher allow 5 seconds.
pub const STOP_GRACE: Duration = Duration::from_secs(5);
const READY_POLL_INTERVAL: Duration = Duration::from_millis(100);
const READY_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_LOG_BYTES: u64 = 20 * 1024 * 1024;

/// The secret the engine derives its accepted bootstrap tokens from. Never logged.
#[derive(Clone, PartialEq, Eq)]
pub struct BootstrapSecret(String);

impl std::fmt::Debug for BootstrapSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("BootstrapSecret(…)")
    }
}

impl BootstrapSecret {
    /// 32 random bytes in hex, like T3's desktop app.
    pub fn generate() -> Self {
        let bytes: [u8; 32] = rand::random();
        Self(hex::encode(bytes))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The token for the window containing `now_ms`. The engine accepts the previous, current,
    /// and next window's token.
    pub fn token_at(&self, now_ms: u64) -> String {
        let window = now_ms / BOOTSTRAP_TOKEN_WINDOW_MS;
        hex::encode(hmac_sha256(
            self.0.as_bytes(),
            format!("t3-desktop-bootstrap:{window}").as_bytes(),
        ))
    }

    pub fn current_token(&self) -> String {
        self.token_at(unix_millis())
    }
}

pub fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// HMAC-SHA256 (RFC 2104), as Node's `createHmac("sha256", key)`.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block_key = [0u8; BLOCK];
    if key.len() > BLOCK {
        block_key[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block_key[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block_key.map(|byte| byte ^ 0x36));
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(block_key.map(|byte| byte ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// How one run of the engine is started.
#[derive(Clone, Debug)]
pub struct EngineLaunch {
    pub executable: PathBuf,
    /// The engine's own T3 home (`--base-dir`): threads, projects, settings, and logs.
    pub base_dir: PathBuf,
    pub port: u16,
    /// Where the engine's stdout and stderr go.
    pub log_file: PathBuf,
}

impl EngineLaunch {
    pub fn url(&self) -> Url {
        Url::parse(&format!("http://{ENGINE_HOST}:{}", self.port))
            .unwrap_or_else(|_| unreachable!("a loopback URL with a port parses"))
    }

    pub fn args(&self) -> Vec<OsString> {
        let mut args = ["--mode", "desktop", "--bootstrap-fd", "0", "--base-dir"]
            .map(OsString::from)
            .to_vec();
        args.push(self.base_dir.clone().into_os_string());
        args.extend(
            [
                "--host",
                ENGINE_HOST,
                "--port",
                &self.port.to_string(),
                "--no-browser",
            ]
            .map(OsString::from),
        );
        args
    }

    /// The `DesktopBackendBootstrap` line the engine reads from its stdin at startup.
    pub fn envelope(&self, secret: &BootstrapSecret, now_ms: u64) -> Result<String> {
        let t3_home = self
            .base_dir
            .to_str()
            .context("the engine's data directory path isn't valid UTF-8")?;
        let envelope = json!({
            "mode": "desktop",
            "noBrowser": true,
            "port": self.port,
            "t3Home": t3_home,
            "host": ENGINE_HOST,
            "desktopBootstrapToken": secret.token_at(now_ms),
            "desktopBootstrapSecret": secret.expose(),
            "tailscaleServeEnabled": false,
            // T3's schema requires a valid port here even with Tailscale off.
            "tailscaleServePort": 443,
        });
        Ok(format!("{envelope}\n"))
    }

    /// The engine's command, with the user's environment minus T3's own variables, and with
    /// T3's analytics off.
    pub fn command(&self) -> std::process::Command {
        let mut command = util::command::new_std_command(&self.executable);
        command.args(self.args()).current_dir(&self.base_dir);
        for key in crate::scrubbed_env_keys(std::env::vars_os().map(|(key, _)| key)) {
            command.env_remove(key);
        }
        command.env("T3CODE_TELEMETRY_ENABLED", "false");
        command
    }
}

/// A free loopback port for the engine, never T3's default.
pub fn pick_port() -> Result<u16> {
    for _ in 0..8 {
        let listener = std::net::TcpListener::bind((ENGINE_HOST, 0))
            .context("finding a free port for the agents engine")?;
        let port = listener.local_addr()?.port();
        if port != T3_DEFAULT_PORT {
            return Ok(port);
        }
    }
    bail!("couldn't find a free port for the agents engine")
}

/// Holds `<data dir>/t3-engine.lock`, so one mustr data directory runs one engine.
#[derive(Debug)]
pub struct EngineLock {
    _file: File,
    path: PathBuf,
}

impl EngineLock {
    /// `None` when another process holds the lock.
    pub fn try_acquire(path: &Path) -> Result<Option<Self>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self {
                _file: file,
                path: path.to_path_buf(),
            })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => {
                Err(error).with_context(|| format!("locking {}", path.display()))
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Restart delays after crashes: 0.5 s doubling to 10 s, as T3's desktop app does.
#[derive(Clone, Debug, Default)]
pub struct Backoff {
    attempt: u32,
}

impl Backoff {
    pub const INITIAL: Duration = Duration::from_millis(500);
    pub const MAX: Duration = Duration::from_secs(10);
    /// Crashes in a row before mustr stops restarting and asks the user to retry.
    pub const MAX_ATTEMPTS: u32 = 6;
    /// A run this long resets the count.
    pub const STABLE_AFTER: Duration = Duration::from_secs(60);

    pub fn next_delay(&mut self) -> Duration {
        let delay = Self::INITIAL
            .saturating_mul(1 << self.attempt.min(16))
            .min(Self::MAX);
        self.attempt += 1;
        delay
    }

    pub fn attempts(&self) -> u32 {
        self.attempt
    }

    pub fn exhausted(&self) -> bool {
        self.attempt >= Self::MAX_ATTEMPTS
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

/// Whether an exit means the engine must not be restarted.
pub fn exit_is_fatal(status: &ExitStatus) -> bool {
    status.code() == Some(EXIT_CODE_STATE_DIR_OWNED)
}

pub fn describe_exit(status: &ExitStatus) -> String {
    if let Some(code) = status.code() {
        if code == EXIT_CODE_STATE_DIR_OWNED {
            return "another T3 Code server owns the engine's data directory".to_string();
        }
        return format!("exit code {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return format!("signal {signal}");
        }
    }
    status.to_string()
}

/// A running engine. Its own process group (Unix) or job object (Windows), so stopping it stops
/// the agents it started too.
pub struct EngineProcess {
    child: util::process::Child,
    pid: u32,
}

impl EngineProcess {
    pub async fn spawn(launch: &EngineLaunch, envelope: String) -> Result<Self> {
        std::fs::create_dir_all(&launch.base_dir)
            .with_context(|| format!("creating {}", launch.base_dir.display()))?;
        let log = open_log(&launch.log_file)?;
        let mut child = util::process::Child::spawn(
            launch.command(),
            std::process::Stdio::piped(),
            std::process::Stdio::from(log.try_clone()?),
            std::process::Stdio::from(log),
        )
        .with_context(|| format!("starting {}", launch.executable.display()))?;
        let pid = child.id();
        let mut stdin = child
            .stdin
            .take()
            .context("the agents engine has no stdin")?;
        // T3 reads the envelope within a second of starting; the pipe buffers it until then.
        // Closing stdin afterwards gives the engine end-of-file there.
        stdin
            .write_all(envelope.as_bytes())
            .await
            .context("handing the agents engine its bootstrap envelope")?;
        stdin.flush().await?;
        drop(stdin);
        Ok(Self { child, pid })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn try_exit(&mut self) -> Option<ExitStatus> {
        self.child.try_status().ok().flatten()
    }

    /// Completes when the engine exits.
    pub fn exited(&mut self) -> impl Future<Output = std::io::Result<ExitStatus>> + use<> {
        self.child.status()
    }

    /// Asks the engine to stop (SIGTERM). On Windows there is no such request; [`Self::kill`]
    /// ends it.
    pub fn terminate(&self) {
        #[cfg(unix)]
        // SAFETY: `kill` has no memory-safety preconditions.
        unsafe {
            libc::kill(self.pid as libc::pid_t, libc::SIGTERM);
        }
    }

    /// Kills the engine and everything it started.
    pub fn kill(&mut self) {
        self.child.kill().ok();
    }

    /// Asks the engine to stop, waits up to `grace`, then kills it. Blocks: for app quit, where
    /// GPUI doesn't wait for async work.
    pub fn stop_blocking(&mut self, grace: Duration) -> Option<ExitStatus> {
        if let Some(status) = self.try_exit() {
            return Some(status);
        }
        self.terminate();
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if let Some(status) = self.try_exit() {
                // The engine is gone; end any agents it left behind too.
                self.kill();
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        log::warn!(
            "the agents engine (pid {}) didn't stop within {}s; killing it",
            self.pid,
            grace.as_secs()
        );
        self.kill();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Some(status) = self.try_exit() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    /// Like [`Self::stop_blocking`], without blocking a thread.
    pub async fn stop(&mut self, grace: Duration, sleep: &Sleep) -> Option<ExitStatus> {
        if let Some(status) = self.try_exit() {
            return Some(status);
        }
        self.terminate();
        let exited = self.exited();
        let status = futures::select! {
            status = exited.fuse() => status.ok(),
            _ = sleep(grace).fuse() => None,
        };
        // Also ends any agents the engine left behind.
        self.kill();
        match status {
            Some(status) => Some(status),
            None => {
                log::warn!(
                    "the agents engine (pid {}) didn't stop within {}s; killed it",
                    self.pid,
                    grace.as_secs()
                );
                self.exited().await.ok()
            }
        }
    }
}

/// Polls the engine's descriptor (`/.well-known/t3/environment`) until it answers, the engine
/// exits, or `timeout` passes.
pub async fn wait_until_ready(
    http: &dyn HttpClient,
    url: &Url,
    process: &mut EngineProcess,
    timeout: Duration,
    sleep: &Sleep,
) -> Result<EnvironmentDescriptor> {
    let started = Instant::now();
    loop {
        if let Some(status) = process.try_exit() {
            bail!(
                "the agents engine stopped while starting ({})",
                describe_exit(&status)
            );
        }
        let probe = futures::select! {
            descriptor = t3_client::fetch_descriptor(http, url).fuse() => descriptor.ok(),
            _ = sleep(READY_REQUEST_TIMEOUT).fuse() => None,
        };
        if let Some(descriptor) = probe {
            return Ok(descriptor);
        }
        if started.elapsed() >= timeout {
            bail!(
                "the agents engine didn't answer at {url} within {}s",
                timeout.as_secs()
            );
        }
        sleep(READY_POLL_INTERVAL).await;
    }
}

/// Stops an engine left running from this base dir by a mustr that crashed, which would
/// otherwise hold the base dir (T3 exits with [`EXIT_CODE_STATE_DIR_OWNED`]). The caller must
/// hold the [`EngineLock`], so no live mustr owns it. Only a process whose command line names
/// this base dir is touched.
pub fn stop_stale_engine(base_dir: &Path, grace: Duration) -> Option<u32> {
    let record = crate::installed::read_server_record(base_dir)?;
    let pid = record.pid;
    if pid == std::process::id() || !crate::installed::process_is_alive(pid) {
        return None;
    }
    if !process_mentions(pid, base_dir) {
        log::info!(
            "{} names pid {pid}, which isn't an engine for that directory; leaving it alone",
            base_dir.display()
        );
        return None;
    }
    log::warn!("stopping a stale agents engine (pid {pid}) left by an earlier mustr");
    signal_and_wait(pid, grace);
    Some(pid)
}

fn process_mentions(pid: u32, base_dir: &Path) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut system = System::new();
    let sysinfo_pid = Pid::from_u32(pid);
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[sysinfo_pid]),
        true,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
    );
    let needle = base_dir.as_os_str();
    system
        .process(sysinfo_pid)
        .is_some_and(|process| process.cmd().iter().any(|arg| arg == needle))
}

fn signal_and_wait(pid: u32, grace: Duration) {
    #[cfg(unix)]
    {
        // SAFETY: `kill` has no memory-safety preconditions.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline && crate::installed::process_is_alive(pid) {
            std::thread::sleep(Duration::from_millis(50));
        }
        // The engine leads its own process group; end it with everything it started.
        // SAFETY: as above.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = grace;
        use sysinfo::{Pid, ProcessesToUpdate, System};
        let mut system = System::new();
        let sysinfo_pid = Pid::from_u32(pid);
        system.refresh_processes(ProcessesToUpdate::Some(&[sysinfo_pid]), true);
        if let Some(process) = system.process(sysinfo_pid) {
            process.kill();
        }
    }
}

/// Opens the engine's log for appending, moving a large one to `<name>.old` first.
fn open_log(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > MAX_LOG_BYTES) {
        let mut old = path.as_os_str().to_owned();
        old.push(".old");
        std::fs::rename(path, old).ok();
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc_4231() {
        assert_eq!(
            hex::encode(hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // A key longer than SHA-256's block is hashed first.
        assert_eq!(
            hex::encode(hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn bootstrap_tokens_rotate_like_t3s() {
        // Node: createHmac("sha256", "a".repeat(64)).update("t3-desktop-bootstrap:41666").
        let secret = BootstrapSecret("a".repeat(64));
        let now = 1_800_000_000_000;
        assert_eq!(now / BOOTSTRAP_TOKEN_WINDOW_MS, 41666);
        assert_eq!(
            secret.token_at(now),
            "993db5641bef4c6c71b8055458c4e6f0afdafc719085cd691f41fd2cf02cace8"
        );
        // The same window, the same token; the next window, another.
        let window_start = 41666 * BOOTSTRAP_TOKEN_WINDOW_MS;
        assert_eq!(secret.token_at(window_start), secret.token_at(now));
        assert_ne!(
            secret.token_at(window_start + BOOTSTRAP_TOKEN_WINDOW_MS),
            secret.token_at(now)
        );

        let generated = BootstrapSecret::generate();
        assert_eq!(generated.expose().len(), 64);
        assert_ne!(generated, BootstrapSecret::generate());
        assert!(!format!("{generated:?}").contains(generated.expose()));
    }

    fn launch() -> EngineLaunch {
        EngineLaunch {
            executable: PathBuf::from("/data/mustr/t3-runtime/0.1.1/t3"),
            base_dir: PathBuf::from("/data/mustr/t3-home"),
            port: 41234,
            log_file: PathBuf::from("/data/mustr/logs/t3-engine.log"),
        }
    }

    #[test]
    fn the_command_is_t3s_desktop_launch() {
        let launch = launch();
        assert_eq!(
            launch.args(),
            [
                "--mode",
                "desktop",
                "--bootstrap-fd",
                "0",
                "--base-dir",
                "/data/mustr/t3-home",
                "--host",
                "127.0.0.1",
                "--port",
                "41234",
                "--no-browser"
            ]
            .map(OsString::from)
        );
        assert_eq!(launch.url().as_str(), "http://127.0.0.1:41234/");

        let command = launch.command();
        assert_eq!(command.get_program(), "/data/mustr/t3-runtime/0.1.1/t3");
        assert_eq!(
            command.get_current_dir(),
            Some(Path::new("/data/mustr/t3-home"))
        );
        let envs = command
            .get_envs()
            .map(|(key, value)| (key.to_owned(), value.map(ToOwned::to_owned)))
            .collect::<Vec<_>>();
        assert!(envs.contains(&(
            OsString::from("T3CODE_TELEMETRY_ENABLED"),
            Some(OsString::from("false"))
        )));
        // Whatever T3 variables this test process has are removed, never passed on.
        for (key, _) in std::env::vars_os() {
            if crate::scrubbed_env_keys([key.clone()]).len() == 1 {
                assert!(envs.contains(&(key, None)));
            }
        }
    }

    #[test]
    fn the_envelope_is_t3s_desktop_bootstrap() {
        let secret = BootstrapSecret("a".repeat(64));
        let envelope = launch().envelope(&secret, 1_800_000_000_000).unwrap();
        assert!(envelope.ends_with('\n') && envelope.matches('\n').count() == 1);
        let value: serde_json::Value = serde_json::from_str(&envelope).unwrap();
        assert_eq!(
            value,
            json!({
                "mode": "desktop",
                "noBrowser": true,
                "port": 41234,
                "t3Home": "/data/mustr/t3-home",
                "host": "127.0.0.1",
                "desktopBootstrapToken": "993db5641bef4c6c71b8055458c4e6f0afdafc719085cd691f41fd2cf02cace8",
                "desktopBootstrapSecret": "a".repeat(64),
                "tailscaleServeEnabled": false,
                "tailscaleServePort": 443,
            })
        );
    }

    #[test]
    fn picks_a_free_port_that_isnt_t3s() {
        let port = pick_port().unwrap();
        assert_ne!(port, T3_DEFAULT_PORT);
        assert!(port > 0);
    }

    #[test]
    fn one_engine_per_data_dir() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("data").join("t3-engine.lock");
        let lock = EngineLock::try_acquire(&path).unwrap().expect("first lock");
        assert_eq!(lock.path(), path);
        assert!(EngineLock::try_acquire(&path).unwrap().is_none());
        drop(lock);
        // A process another test forks holds a copy of the descriptor until it execs, so the
        // lock can take a moment to free up.
        let deadline = Instant::now() + Duration::from_secs(5);
        while EngineLock::try_acquire(&path).unwrap().is_none() {
            assert!(Instant::now() < deadline, "the lock was never released");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn backoff_doubles_to_a_cap_and_resets() {
        let mut backoff = Backoff::default();
        let delays = (0..7).map(|_| backoff.next_delay()).collect::<Vec<_>>();
        assert_eq!(
            delays,
            [500, 1000, 2000, 4000, 8000, 10_000, 10_000].map(Duration::from_millis)
        );
        assert!(backoff.exhausted());
        backoff.reset();
        assert_eq!(backoff.attempts(), 0);
        assert!(!backoff.exhausted());
        assert_eq!(backoff.next_delay(), Backoff::INITIAL);
        // Far past the cap, it doesn't overflow.
        let mut long = Backoff { attempt: 1000 };
        assert_eq!(long.next_delay(), Backoff::MAX);
    }

    #[cfg(unix)]
    #[test]
    fn exits_are_described_and_state_dir_conflicts_are_fatal() {
        use std::os::unix::process::ExitStatusExt as _;
        let owned = ExitStatus::from_raw(EXIT_CODE_STATE_DIR_OWNED << 8);
        assert!(exit_is_fatal(&owned));
        assert!(describe_exit(&owned).contains("owns the engine's data directory"));
        let crashed = ExitStatus::from_raw(1 << 8);
        assert!(!exit_is_fatal(&crashed));
        assert_eq!(describe_exit(&crashed), "exit code 1");
        assert_eq!(describe_exit(&ExitStatus::from_raw(9)), "signal 9");
    }

    /// A stand-in engine: a shell script that reads the envelope from stdin into a file, then
    /// waits for SIGTERM (or ignores it, to test the kill).
    #[cfg(unix)]
    fn fake_engine(dir: &Path, ignore_term: bool) -> EngineLaunch {
        use std::os::unix::fs::PermissionsExt as _;
        let script = dir.join("t3");
        let trap = if ignore_term {
            "trap '' TERM"
        } else {
            "trap 'exit 0' TERM"
        };
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n{trap}\nhead -n 1 > \"$PWD/envelope.json\"\necho started\nwhile :; do sleep 0.05; done\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        EngineLaunch {
            executable: script,
            base_dir: dir.join("home"),
            port: 41234,
            log_file: dir.join("logs").join("t3-engine.log"),
        }
    }

    #[cfg(unix)]
    fn wait_for(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_process_gets_its_envelope_and_stops_gracefully() {
        let temp = tempfile::tempdir().unwrap();
        let launch = fake_engine(temp.path(), false);
        let secret = BootstrapSecret::generate();
        let envelope = launch.envelope(&secret, unix_millis()).unwrap();
        let mut process = smol::block_on(EngineProcess::spawn(&launch, envelope.clone())).unwrap();
        let envelope_file = launch.base_dir.join("envelope.json");
        wait_for(|| std::fs::read_to_string(&envelope_file).is_ok_and(|text| !text.is_empty()));
        assert_eq!(std::fs::read_to_string(&envelope_file).unwrap(), envelope);
        wait_for(|| {
            std::fs::read_to_string(&launch.log_file).is_ok_and(|log| log.contains("started"))
        });
        assert!(process.try_exit().is_none());
        let pid = process.pid();

        let status = smol::block_on(process.stop(STOP_GRACE, &crate::test_sleep()));
        assert_eq!(status.and_then(|status| status.code()), Some(0));
        assert!(!crate::installed::process_is_alive(pid));
    }

    #[cfg(unix)]
    #[test]
    fn an_engine_that_ignores_sigterm_is_killed() {
        let temp = tempfile::tempdir().unwrap();
        let launch = fake_engine(temp.path(), true);
        let mut process = smol::block_on(EngineProcess::spawn(&launch, "{}\n".into())).unwrap();
        wait_for(|| {
            std::fs::read_to_string(&launch.log_file).is_ok_and(|log| log.contains("started"))
        });
        let pid = process.pid();
        let started = Instant::now();
        let status = process.stop_blocking(Duration::from_millis(300));
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert!(status.is_some_and(|status| !status.success()));
        assert!(!crate::installed::process_is_alive(pid));
    }

    #[cfg(unix)]
    #[test]
    fn a_stale_engine_for_this_base_dir_is_stopped() {
        // The test doesn't reap the process until afterwards, so it looks alive for the whole
        // grace period.
        const SHORT_GRACE: Duration = Duration::from_millis(300);
        let temp = tempfile::tempdir().unwrap();
        let launch = fake_engine(temp.path(), false);
        // As a crashed mustr would leave it: running, with its record in the base dir. The
        // handle is kept only to reap it, so the test doesn't see a zombie as alive.
        let mut process = smol::block_on(EngineProcess::spawn(&launch, "{}\n".into())).unwrap();
        let pid = process.pid();
        let userdata = launch.base_dir.join("userdata");
        std::fs::create_dir_all(&userdata).unwrap();
        let record =
            json!({ "version": 1, "pid": pid, "port": 41234, "origin": "http://127.0.0.1:41234" });
        std::fs::write(userdata.join("server-runtime.json"), record.to_string()).unwrap();

        // Another directory's record naming the same process is left alone.
        let other = temp.path().join("other");
        std::fs::create_dir_all(other.join("userdata")).unwrap();
        std::fs::write(
            other.join("userdata/server-runtime.json"),
            record.to_string(),
        )
        .unwrap();
        assert_eq!(stop_stale_engine(&other, SHORT_GRACE), None);
        assert!(crate::installed::process_is_alive(pid));

        assert!(process.try_exit().is_none());

        assert_eq!(stop_stale_engine(&launch.base_dir, SHORT_GRACE), Some(pid));
        wait_for(|| process.try_exit().is_some());
        assert_eq!(stop_stale_engine(&launch.base_dir, SHORT_GRACE), None);
    }

    #[test]
    fn large_logs_are_rotated() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("t3-engine.log");
        std::fs::write(&path, vec![b'x'; MAX_LOG_BYTES as usize + 1]).unwrap();
        drop(open_log(&path).unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        assert!(temp.path().join("t3-engine.log.old").is_file());
    }
}
