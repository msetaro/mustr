//! mustr's own icons, drawn after the mockups, and an asset source that serves them next to
//! Zed's. Zed's icons are compiled into its `assets` crate, so mustr can't add files there.

use gpui::{AssetSource, SharedString};
use std::borrow::Cow;
use ui::Icon;

/// The directory mustr's icons are served from: a subdirectory of Zed's, which has none, because
/// `ui::Icon::from_path` only treats paths under `icons/` as SVGs.
const ICON_DIR: &str = "icons/mustr/";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MustrIcon {
    Agent,
    /// A pencil, for the rail's Edit.
    Edit,
    Inbox,
    PullRequest,
    Settings,
}

impl MustrIcon {
    const ALL: [Self; 5] = [
        Self::Agent,
        Self::Edit,
        Self::Inbox,
        Self::PullRequest,
        Self::Settings,
    ];

    pub fn path(self) -> &'static str {
        match self {
            Self::Agent => "icons/mustr/agent.svg",
            Self::Edit => "icons/mustr/edit.svg",
            Self::Inbox => "icons/mustr/inbox.svg",
            Self::PullRequest => "icons/mustr/pull_request.svg",
            Self::Settings => "icons/mustr/settings.svg",
        }
    }

    fn svg(self) -> &'static str {
        match self {
            Self::Agent => include_str!("../assets/icons/agent.svg"),
            Self::Edit => include_str!("../assets/icons/edit.svg"),
            Self::Inbox => include_str!("../assets/icons/inbox.svg"),
            Self::PullRequest => include_str!("../assets/icons/pull_request.svg"),
            Self::Settings => include_str!("../assets/icons/settings.svg"),
        }
    }

    pub fn icon(self) -> Icon {
        Icon::from_path(self.path())
    }
}

/// Serves mustr's icons, and everything else from `fallback` (Zed's assets).
pub struct MustrAssets<A> {
    fallback: A,
}

impl<A: AssetSource> MustrAssets<A> {
    pub fn new(fallback: A) -> Self {
        Self { fallback }
    }
}

impl<A: AssetSource> AssetSource for MustrAssets<A> {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        match MustrIcon::ALL.into_iter().find(|icon| icon.path() == path) {
            Some(icon) => Ok(Some(Cow::Borrowed(icon.svg().as_bytes()))),
            None => self.fallback.load(path),
        }
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        let mut paths = self.fallback.list(path)?;
        if ICON_DIR.starts_with(path) || path.starts_with(ICON_DIR) {
            paths.extend(
                MustrIcon::ALL
                    .into_iter()
                    .map(MustrIcon::path)
                    .filter(|icon_path| icon_path.starts_with(path))
                    .map(SharedString::from),
            );
        }
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoAssets;

    impl AssetSource for NoAssets {
        fn load(&self, _: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
            Ok(None)
        }

        fn list(&self, _: &str) -> anyhow::Result<Vec<SharedString>> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn serves_mustr_icons_and_falls_back_for_the_rest() {
        let assets = MustrAssets::new(NoAssets);
        for icon in MustrIcon::ALL {
            let svg = assets.load(icon.path()).unwrap().unwrap();
            assert!(svg.starts_with(b"<svg"), "{icon:?}");
        }
        assert!(assets.load("icons/file.svg").unwrap().is_none());
        assert_eq!(assets.list("icons/").unwrap().len(), MustrIcon::ALL.len());
        assert_eq!(assets.list("icons/mustr/a").unwrap().len(), 1);
        assert!(assets.list("fonts/").unwrap().is_empty());
    }
}
