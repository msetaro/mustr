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
    /// A program, for one mustr has no icon of (Open in's fallback).
    App,
    /// A pencil, for the rail's Edit.
    Edit,
    Inbox,
    PullRequest,
    Settings,
    Terminal,
}

impl MustrIcon {
    const ALL: [Self; 7] = [
        Self::Agent,
        Self::App,
        Self::Edit,
        Self::Inbox,
        Self::PullRequest,
        Self::Settings,
        Self::Terminal,
    ];

    pub fn path(self) -> &'static str {
        match self {
            Self::Agent => "icons/mustr/agent.svg",
            Self::App => "icons/mustr/app.svg",
            Self::Edit => "icons/mustr/edit.svg",
            Self::Inbox => "icons/mustr/inbox.svg",
            Self::PullRequest => "icons/mustr/pull_request.svg",
            Self::Settings => "icons/mustr/settings.svg",
            Self::Terminal => "icons/mustr/terminal.svg",
        }
    }

    fn svg(self) -> &'static str {
        match self {
            Self::Agent => include_str!("../assets/icons/agent.svg"),
            Self::App => include_str!("../assets/icons/app.svg"),
            Self::Edit => include_str!("../assets/icons/edit.svg"),
            Self::Inbox => include_str!("../assets/icons/inbox.svg"),
            Self::PullRequest => include_str!("../assets/icons/pull_request.svg"),
            Self::Settings => include_str!("../assets/icons/settings.svg"),
            Self::Terminal => include_str!("../assets/icons/terminal.svg"),
        }
    }

    pub fn icon(self) -> Icon {
        Icon::from_path(self.path())
    }
}

/// Every SVG mustr serves: its icons, and the one-color program icons Open in draws in the text
/// color ([`crate::app_icons`]).
fn served() -> impl Iterator<Item = (&'static str, &'static str)> {
    MustrIcon::ALL
        .into_iter()
        .map(|icon| (icon.path(), icon.svg()))
        .chain(crate::app_icons::served_svgs())
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
        match served().find(|(served, _)| *served == path) {
            Some((_, svg)) => Ok(Some(Cow::Borrowed(svg.as_bytes()))),
            None => self.fallback.load(path),
        }
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        let mut paths = self.fallback.list(path)?;
        if ICON_DIR.starts_with(path) || path.starts_with(ICON_DIR) {
            paths.extend(
                served()
                    .map(|(icon_path, _)| icon_path)
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
        for (path, _) in served() {
            let svg = assets.load(path).unwrap().unwrap();
            assert!(svg.starts_with(b"<svg"), "{path}");
        }
        assert!(assets.load("icons/file.svg").unwrap().is_none());
        assert_eq!(assets.list("icons/").unwrap().len(), served().count());
        assert_eq!(assets.list("icons/mustr/ag").unwrap().len(), 1);
        // Cursor's, Trae's and Zed's, which are one color.
        assert_eq!(assets.list("icons/mustr/apps/").unwrap().len(), 3);
        assert!(assets.list("fonts/").unwrap().is_empty());
    }
}
