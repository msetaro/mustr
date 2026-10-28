//! T3 Code's engine on this machine, for mustr.
//!
//! mustr runs T3 Code's server unmodified. This crate gets and runs it:
//!
//! - [`release`]: which release archive runs here, and its published checksum.
//! - [`install`]: downloads an archive, verifies it against the release's `SHA256SUMS` (failing
//!   closed), and unpacks it the way T3's own installer does (`install.sh`).
//! - [`engine`]: mustr's own, hidden copy of the engine: its command line, the desktop bootstrap
//!   envelope and token, the single-engine lock, restart backoff, and the process.
//! - [`installed`]: the user's own T3 Code install (`~/.t3`): its runtimes, its running server,
//!   its background service, and the `t3` commands that start, install, and update it.
//!
//! See `mustr-docs/T3-RUNTIME.md` for the research behind it.

pub mod engine;
pub mod install;
pub mod installed;
pub mod release;

use futures::future::BoxFuture;
use std::{sync::Arc, time::Duration};

/// Waits for a while. mustr passes GPUI's executor timer, so tests control time; tests in this
/// crate sleep on a blocking thread.
pub type Sleep = Arc<dyn Fn(Duration) -> BoxFuture<'static, ()> + Send + Sync>;

/// Removes T3's own variables from a child's environment. A mustr started from a terminal inside
/// T3's service inherits `T3CODE_HOME`, `T3_SERVICE_LAUNCHER_CONTEXT`, and so on, which would make
/// the child act as that service or use its home (`mustr-docs/T3-RUNTIME.md` §10). mustr passes
/// everything it means explicitly instead.
pub fn scrubbed_env_keys(
    keys: impl IntoIterator<Item = std::ffi::OsString>,
) -> Vec<std::ffi::OsString> {
    keys.into_iter()
        .filter(|key| {
            key.to_str().is_some_and(|key| {
                key.starts_with("T3CODE_") || key.starts_with("T3_") || key == "VITE_DEV_SERVER_URL"
            })
        })
        .collect()
}

pub(crate) fn truncate(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((index, _)) => format!("{}…", &text[..index]),
        None => text.to_string(),
    }
}

#[cfg(test)]
pub(crate) fn test_sleep() -> Sleep {
    Arc::new(|duration| Box::pin(smol::unblock(move || std::thread::sleep(duration))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubs_t3s_variables_only() {
        let keys = [
            "T3CODE_HOME",
            "T3_SERVICE_LAUNCHER_CONTEXT",
            "T3CODE_PORT",
            "VITE_DEV_SERVER_URL",
            "PATH",
            "HOME",
            "MY_T3CODE_THING",
        ]
        .map(std::ffi::OsString::from);
        assert_eq!(
            scrubbed_env_keys(keys),
            [
                "T3CODE_HOME",
                "T3_SERVICE_LAUNCHER_CONTEXT",
                "T3CODE_PORT",
                "VITE_DEV_SERVER_URL"
            ]
            .map(std::ffi::OsString::from)
        );
    }
}
