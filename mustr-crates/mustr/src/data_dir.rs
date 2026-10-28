use anyhow::{Context as _, Result};
use std::{path::PathBuf, sync::OnceLock};

/// Points every `paths::*` directory at mustr's own data directory, so mustr
/// never reads or writes Zed's settings or database.
///
/// Must run before anything else touches `paths`.
pub fn init(custom_dir: Option<String>) -> Result<&'static PathBuf> {
    let dir = match custom_dir {
        Some(dir) => dir,
        None => default_data_dir()?
            .into_os_string()
            .into_string()
            .ok()
            .context("the default data directory path is not valid UTF-8")?,
    };
    let data_dir = paths::set_custom_data_dir(&dir);
    for dir in [
        paths::config_dir(),
        paths::languages_dir(),
        paths::database_dir(),
        logs_dir(),
    ] {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    Ok(data_dir)
}

fn default_data_dir() -> Result<PathBuf> {
    let base = dirs::data_local_dir().context("could not determine the local data directory")?;
    let name = if cfg!(any(target_os = "linux", target_os = "freebsd")) {
        "mustr"
    } else {
        "mustr"
    };
    Ok(base.join(name))
}

// `paths::logs_dir()` is `~/Library/Logs/Zed` on macOS, so mustr keeps its
// logs under its own data directory on every platform.
pub fn logs_dir() -> &'static PathBuf {
    static LOGS_DIR: OnceLock<PathBuf> = OnceLock::new();
    LOGS_DIR.get_or_init(|| paths::data_dir().join("logs"))
}

pub fn log_file() -> &'static PathBuf {
    static LOG_FILE: OnceLock<PathBuf> = OnceLock::new();
    LOG_FILE.get_or_init(|| logs_dir().join("mustr.log"))
}

pub fn old_log_file() -> &'static PathBuf {
    static OLD_LOG_FILE: OnceLock<PathBuf> = OnceLock::new();
    OLD_LOG_FILE.get_or_init(|| logs_dir().join("mustr.log.old"))
}
