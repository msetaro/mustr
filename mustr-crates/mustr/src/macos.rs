//! What mustr adds on macOS: the app-level actions that Zed's macOS keymap and mustr's app menu
//! use, the standard About panel, and a Dock icon when mustr runs outside an app bundle (for
//! example, from `cargo run`).

use gpui::App;
use objc2::{AllocAnyThread as _, MainThreadMarker, rc::Retained, runtime::AnyObject};
use objc2_app_kit::{
    NSAboutPanelOptionApplicationName, NSAboutPanelOptionApplicationVersion,
    NSAboutPanelOptionCredits, NSApplication, NSImage,
};
use objc2_foundation::{NSAttributedString, NSData, NSDictionary, NSString};
use std::path::Path;
use workspace::Workspace;

// Copied from crates/zed/src/zed.rs, which declares them in Zed's `zed` namespace. mustr's binary
// doesn't link that crate, so the names in Zed's namespace are their aliases: Zed's macOS keymap
// binds them (cmd-h, alt-cmd-h, cmd-m), and the app menu shows those shortcuts, while the command
// palette lists them under mustr's name ("mustr: hide", not "zed: hide").
gpui::actions!(
    mustr,
    [
        /// Shows mustr's version in the standard About panel.
        About,
        /// Hides mustr.
        #[action(deprecated_aliases = ["zed::Hide"])]
        Hide,
        /// Hides all other applications.
        #[action(deprecated_aliases = ["zed::HideOthers"])]
        HideOthers,
        /// Shows all hidden applications.
        #[action(deprecated_aliases = ["zed::ShowAll"])]
        ShowAll,
        /// Minimizes the current window.
        #[action(deprecated_aliases = ["zed::Minimize"])]
        Minimize,
        /// Zooms the current window.
        #[action(deprecated_aliases = ["zed::Zoom"])]
        Zoom,
    ]
);

/// A Retina-sized copy of the icon in mustr's app bundle (`resources/mustr.icns`), drawn with
/// the margins macOS icons have.
const APP_ICON_PNG: &[u8] = include_bytes!("../resources/app-icon@2x.png");

const CREDITS: &str = "A review-first desktop app, built on Zed and backed by T3 Code. \
                       Not affiliated with Zed Industries or with T3 Code's authors.";

pub fn init(cx: &mut App) {
    // Copied from crates/zed/src/zed.rs (`init` and `initialize_workspace`).
    cx.on_action(|_: &Hide, cx| cx.hide())
        .on_action(|_: &HideOthers, cx| cx.hide_other_apps())
        .on_action(|_: &ShowAll, cx| cx.unhide_other_apps())
        .on_action(|_: &About, _| show_about_panel());
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace
            .register_action(|_, _: &Minimize, window, _| window.minimize_window())
            .register_action(|_, _: &Zoom, window, _| window.zoom_window());
    })
    .detach();

    let runs_unbundled = std::env::current_exe()
        .map(|exe| !runs_from_app_bundle(&exe))
        .unwrap_or(true);
    if runs_unbundled {
        set_dock_icon();
    }
}

/// Whether `exe` is an app bundle's executable (`<name>.app/Contents/MacOS/<exe>`). macOS takes
/// a bundle's Dock icon from its `Info.plist`; anything else shows a generic one.
fn runs_from_app_bundle(exe: &Path) -> bool {
    exe.parent()
        .filter(|dir| dir.ends_with("Contents/MacOS"))
        .and_then(|dir| dir.parent()?.parent())
        .is_some_and(|bundle| bundle.extension().is_some_and(|ext| ext == "app"))
}

fn app_icon() -> Option<Retained<NSImage>> {
    let data = NSData::with_bytes(APP_ICON_PNG);
    let image = NSImage::initWithData(NSImage::alloc(), &data);
    if image.is_none() {
        log::error!("couldn't decode mustr's app icon");
    }
    image
}

fn set_dock_icon() {
    let Some(main_thread) = MainThreadMarker::new() else {
        log::error!("the Dock icon can only be set on the main thread");
        return;
    };
    let Some(icon) = app_icon() else {
        return;
    };
    let app = NSApplication::sharedApplication(main_thread);
    // SAFETY: on the main thread, with a valid image.
    unsafe { app.setApplicationIconImage(Some(&icon)) };
}

/// AppKit's standard About panel, which also shows the app's icon.
fn show_about_panel() {
    let Some(main_thread) = MainThreadMarker::new() else {
        return;
    };
    let name = NSString::from_str("mustr");
    let version = NSString::from_str(env!("CARGO_PKG_VERSION"));
    let credits = NSAttributedString::from_nsstring(&NSString::from_str(CREDITS));
    // SAFETY: reading constant keys that AppKit exports.
    let keys = unsafe {
        [
            NSAboutPanelOptionApplicationName,
            NSAboutPanelOptionApplicationVersion,
            NSAboutPanelOptionCredits,
        ]
    };
    let values: [&AnyObject; 3] = [&name, &version, &credits];
    let options = NSDictionary::from_slices(&keys, &values);
    let app = NSApplication::sharedApplication(main_thread);
    // SAFETY: each value has the type AppKit documents for its key.
    unsafe { app.orderFrontStandardAboutPanelWithOptions(&options) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_bundle_executables_count_as_bundled() {
        assert!(runs_from_app_bundle(Path::new(
            "/Applications/mustr.app/Contents/MacOS/mustr"
        )));
        assert!(!runs_from_app_bundle(Path::new(
            "/Users/someone/mustr/target/debug/mustr"
        )));
        assert!(!runs_from_app_bundle(Path::new(
            "/Users/someone/Contents/MacOS/mustr"
        )));
    }
}
