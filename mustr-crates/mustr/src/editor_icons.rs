//! The icon of each program Open in offers (T3's editors, `availableEditors`), for the thread
//! toolbar's Open in, its menus, and the review header's.
//!
//! - **On macOS, when T3 runs on this Mac**, the app's own icon, as Finder and the Dock show it:
//!   the app is found by its bundle identifier (`NSWorkspace`'s
//!   `URLForApplicationWithBundleIdentifier:`), and its icon (`iconForFile:`) is drawn into a PNG
//!   of exactly the pixels it covers, kept by [`mustr_ui::app_icons`]. Where each app was found
//!   (or that it wasn't) is kept too, so each is looked up once.
//! - **Otherwise** (another OS, T3 on another machine, or the app not found): T3's own icon for
//!   the editor ([`BrandIcon`], copied from T3's app), or mustr's generic program icon for an
//!   editor mustr doesn't know (a newer T3's). The file manager is Finder on macOS, File Explorer
//!   on Windows, and a folder elsewhere, as in T3.

use gpui::{App, Global, IntoElement as _, ParentElement as _, Pixels, SharedString, Styled as _};
use mustr_ui::{AppIcon, AppIconSource, BrandIcon, app_icons::RenderPng, tokens::scaled};
use std::{collections::HashMap, rc::Rc};
use t3_client::t3_api::types::EditorId;
use ui::{Color, ContextMenu, FluentBuilder as _, Icon, IconName, IconSize, Label, h_flex};

/// Each of T3's editors (`EDITORS` in T3's `packages/contracts/src/editor.ts`) with the bundle
/// identifiers its app has had on macOS, most likely first, and T3's icon for it. The last ones
/// aren't T3 0.1.3's, but match the ids a newer T3 would give them.
const EDITOR_APPS: &[(&str, &[&str], Option<BrandIcon>)] = &[
    (
        "cursor",
        &["com.todesktop.230313mzl4w4u92"],
        Some(BrandIcon::Cursor),
    ),
    ("trae", &["com.trae.app"], Some(BrandIcon::Trae)),
    ("kiro", &["dev.kiro.desktop"], Some(BrandIcon::Kiro)),
    ("vscode", &["com.microsoft.VSCode"], Some(BrandIcon::VsCode)),
    (
        "vscode-insiders",
        &["com.microsoft.VSCodeInsiders"],
        Some(BrandIcon::VsCodeInsiders),
    ),
    ("vscodium", &["com.vscodium"], Some(BrandIcon::VsCodium)),
    (
        "zed",
        &["dev.zed.Zed", "dev.zed.Zed-Preview"],
        Some(BrandIcon::Zed),
    ),
    (
        "antigravity",
        &["com.google.antigravity"],
        Some(BrandIcon::Antigravity),
    ),
    (
        "idea",
        &[
            "com.jetbrains.intellij",
            "com.jetbrains.intellij.ce",
            "com.jetbrains.intellij-EAP",
        ],
        Some(BrandIcon::IntelliJIdea),
    ),
    ("aqua", &["com.jetbrains.aqua"], Some(BrandIcon::Aqua)),
    (
        "clion",
        &["com.jetbrains.CLion", "com.jetbrains.CLion-EAP"],
        Some(BrandIcon::CLion),
    ),
    (
        "datagrip",
        &["com.jetbrains.datagrip"],
        Some(BrandIcon::DataGrip),
    ),
    (
        "dataspell",
        &["com.jetbrains.dataspell"],
        Some(BrandIcon::DataSpell),
    ),
    (
        "goland",
        &["com.jetbrains.goland", "com.jetbrains.goland-EAP"],
        Some(BrandIcon::GoLand),
    ),
    (
        "phpstorm",
        &["com.jetbrains.PhpStorm", "com.jetbrains.PhpStorm-EAP"],
        Some(BrandIcon::PhpStorm),
    ),
    (
        "pycharm",
        &[
            "com.jetbrains.pycharm",
            "com.jetbrains.pycharm.ce",
            "com.jetbrains.pycharm-EAP",
        ],
        Some(BrandIcon::PyCharm),
    ),
    (
        "rider",
        &["com.jetbrains.rider", "com.jetbrains.rider-EAP"],
        Some(BrandIcon::Rider),
    ),
    (
        "rubymine",
        &["com.jetbrains.rubymine", "com.jetbrains.rubymine-EAP"],
        Some(BrandIcon::RubyMine),
    ),
    (
        "rustrover",
        &["com.jetbrains.rustrover", "com.jetbrains.rustrover-EAP"],
        Some(BrandIcon::RustRover),
    ),
    (
        "webstorm",
        &["com.jetbrains.WebStorm", "com.jetbrains.WebStorm-EAP"],
        Some(BrandIcon::WebStorm),
    ),
    ("file-manager", &["com.apple.finder"], None),
    // Not in T3 0.1.3.
    ("windsurf", &["com.exafunction.windsurf"], None),
    (
        "sublime-text",
        &["com.sublimetext.4", "com.sublimetext.3"],
        None,
    ),
    ("xcode", &["com.apple.dt.Xcode"], None),
    ("terminal", &["com.apple.Terminal"], None),
    ("iterm2", &["com.googlecode.iterm2"], None),
    ("ghostty", &["com.mitchellh.ghostty"], None),
];

/// The OS mustr runs on, which picks the file manager's icon and whether apps are looked up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Mac,
    Windows,
    Other,
}

impl Os {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Mac
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Other
        }
    }
}

/// The bundle identifiers of `editor`'s app on macOS.
pub fn bundle_ids(editor: &EditorId) -> &'static [&'static str] {
    EDITOR_APPS
        .iter()
        .find(|(id, _, _)| *id == editor.as_str())
        .map_or(&[], |(_, bundle_ids, _)| bundle_ids)
}

/// T3's icon for `editor` on `os`, if it has one.
pub fn brand_icon(editor: &EditorId, os: Os) -> Option<BrandIcon> {
    if editor.as_str() == "file-manager" {
        return match os {
            Os::Mac => Some(BrandIcon::Finder),
            Os::Windows => Some(BrandIcon::FileExplorer),
            Os::Other => None,
        };
    }
    EDITOR_APPS
        .iter()
        .find(|(id, _, _)| *id == editor.as_str())
        .and_then(|(_, _, brand)| *brand)
}

/// Finds apps by bundle identifier and draws their icons, remembering where each app is. On
/// macOS it asks `NSWorkspace`; elsewhere it finds nothing. Tests put their own in.
pub struct SystemApps {
    find: Rc<dyn Fn(&str) -> Option<String>>,
    render: Rc<dyn Fn(&str, u32) -> Option<Vec<u8>>>,
    /// Each bundle identifier looked up, with its app's path, or `None` when it isn't installed.
    found: HashMap<String, Option<String>>,
}

impl Global for SystemApps {}

impl Default for SystemApps {
    fn default() -> Self {
        #[cfg(target_os = "macos")]
        return Self::new(mac::app_path, mac::icon_png);
        #[cfg(not(target_os = "macos"))]
        Self::new(|_| None, |_, _| None)
    }
}

impl SystemApps {
    pub fn new(
        find: impl Fn(&str) -> Option<String> + 'static,
        render: impl Fn(&str, u32) -> Option<Vec<u8>> + 'static,
    ) -> Self {
        Self {
            find: Rc::new(find),
            render: Rc::new(render),
            found: HashMap::new(),
        }
    }

    /// The path of the first of `bundle_ids` that's installed, looking each up once.
    fn locate(&mut self, bundle_ids: &[&str]) -> Option<String> {
        bundle_ids.iter().find_map(|bundle_id| {
            let find = self.find.clone();
            self.found
                .entry(bundle_id.to_string())
                .or_insert_with(|| {
                    let path = find(bundle_id);
                    match &path {
                        Some(path) => log::debug!("found {bundle_id} at {path}"),
                        None => log::debug!("{bundle_id} isn't installed"),
                    }
                    path
                })
                .clone()
        })
    }

    /// How many bundle identifiers have been looked up, for tests.
    #[cfg(test)]
    pub fn looked_up(&self) -> usize {
        self.found.len()
    }
}

/// Where `editor`'s icon comes from, best first: the installed app's icon (on macOS, when T3
/// runs on this machine and the app is found), T3's icon, then a generic one.
pub fn icon_sources(editor: &EditorId, os: Os, local: bool, cx: &mut App) -> Vec<AppIconSource> {
    let mut sources = Vec::new();
    if os == Os::Mac && local {
        let apps = cx.default_global::<SystemApps>();
        if let Some(path) = apps.locate(bundle_ids(editor)) {
            let render = apps.render.clone();
            let app = path.clone();
            let render: RenderPng = Rc::new(move |pixels| render(&app, pixels));
            sources.push(AppIconSource::System {
                key: SharedString::from(format!("app:{path}")),
                render,
            });
        }
    }
    match brand_icon(editor, os) {
        Some(brand) => sources.push(AppIconSource::Brand(brand)),
        None if editor.as_str() == "file-manager" => {
            sources.push(AppIconSource::Glyph(IconName::Folder.path().into()))
        }
        None => {}
    }
    sources.push(AppIconSource::generic());
    sources
}

/// `editor`'s icon here: in this OS, and with T3 on this machine or not.
pub fn editor_icon_sources(editor: &EditorId, cx: &mut App) -> Vec<AppIconSource> {
    let local = crate::worktrees::worktrees_are_local(cx);
    icon_sources(editor, Os::current(), local, cx)
}

/// The size of an editor's icon in the toolbar's Open in, at mustr's design size.
pub const BUTTON_ICON_SIZE: Pixels = gpui::px(15.);
/// The size of an editor's icon in a menu, at mustr's design size.
pub const MENU_ICON_SIZE: Pixels = gpui::px(16.);

/// A menu entry for `editor`: its icon and `label`, with a check when it's the one Open in uses.
pub fn editor_entry(
    menu: ContextMenu,
    sources: Vec<AppIconSource>,
    label: String,
    checked: bool,
    handler: impl Fn(&mut gpui::Window, &mut App) + 'static,
) -> ContextMenu {
    menu.custom_entry(
        move |_, _| {
            h_flex()
                .w_full()
                .gap(scaled(gpui::px(8.)))
                .child(AppIcon::new(sources.clone(), MENU_ICON_SIZE))
                .child(Label::new(label.clone()))
                .child(gpui::div().flex_1())
                .when(checked, |this| {
                    this.child(
                        Icon::new(IconName::Check)
                            .size(IconSize::Custom(scaled(gpui::px(14.))))
                            .color(Color::Muted),
                    )
                })
                .into_any_element()
        },
        handler,
    )
}

#[cfg(target_os = "macos")]
mod mac {
    use objc2::{AllocAnyThread as _, rc::autoreleasepool};
    use objc2_app_kit::{
        NSBitmapImageFileType, NSBitmapImageRep, NSCompositingOperation, NSDeviceRGBColorSpace,
        NSGraphicsContext, NSImageInterpolation, NSWorkspace,
    };
    use objc2_foundation::{NSDictionary, NSPoint, NSRect, NSSize, NSString};

    /// Where the app with `bundle_id` is installed, as Launch Services finds it.
    pub fn app_path(bundle_id: &str) -> Option<String> {
        autoreleasepool(|_| {
            let workspace = NSWorkspace::sharedWorkspace();
            let url =
                workspace.URLForApplicationWithBundleIdentifier(&NSString::from_str(bundle_id))?;
            Some(url.path()?.to_string())
        })
    }

    /// The app's icon as Finder shows it, drawn into a PNG `pixels` wide and tall (macOS picks
    /// the icon's best size for it).
    pub fn icon_png(app_path: &str, pixels: u32) -> Option<Vec<u8>> {
        let side = isize::try_from(pixels).ok().filter(|side| *side > 0)?;
        autoreleasepool(|_| {
            let image = NSWorkspace::sharedWorkspace().iconForFile(&NSString::from_str(app_path));
            // SAFETY: a null `planes` makes the bitmap allocate its own memory, and the other
            // arguments describe an 8-bit RGBA bitmap, which AppKit supports.
            let bitmap = unsafe {
                NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
                    NSBitmapImageRep::alloc(),
                    std::ptr::null_mut(),
                    side,
                    side,
                    8,
                    4,
                    true,
                    false,
                    NSDeviceRGBColorSpace,
                    0,
                    0,
                )
            }?;
            // One point per pixel, so the icon is drawn at the bitmap's own size.
            let size = NSSize::new(pixels as f64, pixels as f64);
            bitmap.setSize(size);
            let context = NSGraphicsContext::graphicsContextWithBitmapImageRep(&bitmap)?;
            NSGraphicsContext::saveGraphicsState_class();
            NSGraphicsContext::setCurrentContext(Some(&context));
            context.setImageInterpolation(NSImageInterpolation::High);
            image.drawInRect_fromRect_operation_fraction(
                NSRect::new(NSPoint::new(0., 0.), size),
                NSRect::ZERO,
                NSCompositingOperation::SourceOver,
                1.,
            );
            context.flushGraphics();
            NSGraphicsContext::restoreGraphicsState_class();
            // SAFETY: an empty dictionary is a valid set of properties.
            let png = unsafe {
                bitmap.representationUsingType_properties(
                    NSBitmapImageFileType::PNG,
                    &NSDictionary::new(),
                )
            }?;
            Some(png.to_vec())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::cell::RefCell;

    fn editor(id: &str) -> EditorId {
        serde_json::from_value(serde_json::json!(id)).unwrap()
    }

    /// Every editor T3 0.1.3 lists has an app to look for on macOS and an icon of T3's (the file
    /// manager has Finder's on macOS).
    #[test]
    fn every_editor_of_t3s_has_an_app_and_an_icon() {
        for (id, _) in crate::thread_git::EDITOR_ORDER {
            let editor = editor(id);
            assert!(!bundle_ids(&editor).is_empty(), "{id}");
            assert!(brand_icon(&editor, Os::Mac).is_some(), "{id}");
        }
        assert_eq!(
            brand_icon(&editor("file-manager"), Os::Windows),
            Some(BrandIcon::FileExplorer)
        );
        assert_eq!(brand_icon(&editor("file-manager"), Os::Other), None);
        assert_eq!(bundle_ids(&editor("vscode")), ["com.microsoft.VSCode"]);
        assert_eq!(bundle_ids(&editor("emacs")), [] as [&str; 0]);
    }

    fn kinds(sources: &[AppIconSource]) -> Vec<String> {
        sources
            .iter()
            .map(|source| match source {
                AppIconSource::System { key, .. } => key.to_string(),
                AppIconSource::Brand(brand) => format!("{brand:?}"),
                AppIconSource::Glyph(path) => path.to_string(),
            })
            .collect()
    }

    /// On a Mac with T3 on it, an installed app's own icon comes first, then T3's; elsewhere, or
    /// with the app missing, T3's icon. Each bundle identifier is looked up once.
    #[gpui::test]
    fn the_app_icon_comes_first_on_a_mac_with_t3_on_it(cx: &mut TestAppContext) {
        let lookups = Rc::new(RefCell::new(Vec::<String>::new()));
        cx.update(|cx| {
            let lookups = lookups.clone();
            cx.set_global(SystemApps::new(
                move |bundle_id| {
                    lookups.borrow_mut().push(bundle_id.to_string());
                    (bundle_id == "dev.zed.Zed-Preview")
                        .then(|| "/Applications/Zed Preview.app".to_string())
                },
                |_, _| None,
            ));
        });
        cx.update(|cx| {
            // Zed Preview is installed, as the second of Zed's bundle identifiers.
            assert_eq!(
                kinds(&icon_sources(&editor("zed"), Os::Mac, true, cx)),
                [
                    "app:/Applications/Zed Preview.app",
                    "Zed",
                    "icons/mustr/app.svg"
                ]
            );
            // VS Code isn't.
            assert_eq!(
                kinds(&icon_sources(&editor("vscode"), Os::Mac, true, cx)),
                ["VsCode", "icons/mustr/app.svg"]
            );
            // T3 on another machine: its apps aren't this Mac's.
            assert_eq!(
                kinds(&icon_sources(&editor("zed"), Os::Mac, false, cx)),
                ["Zed", "icons/mustr/app.svg"]
            );
            // Linux: T3's icons, a folder for the file manager, and the generic icon for an
            // editor mustr doesn't know.
            assert_eq!(
                kinds(&icon_sources(&editor("cursor"), Os::Other, true, cx)),
                ["Cursor", "icons/mustr/app.svg"]
            );
            assert_eq!(
                kinds(&icon_sources(&editor("file-manager"), Os::Other, true, cx)),
                [
                    IconName::Folder.path().to_string(),
                    "icons/mustr/app.svg".to_string()
                ]
            );
            assert_eq!(
                kinds(&icon_sources(&editor("emacs"), Os::Mac, true, cx)),
                ["icons/mustr/app.svg"]
            );
            // Asked again, nothing is looked up again.
            icon_sources(&editor("zed"), Os::Mac, true, cx);
            icon_sources(&editor("vscode"), Os::Mac, true, cx);
            assert_eq!(cx.global::<SystemApps>().looked_up(), 3);
        });
        assert_eq!(
            *lookups.borrow(),
            ["dev.zed.Zed", "dev.zed.Zed-Preview", "com.microsoft.VSCode"]
        );
    }

    /// The app's icon is drawn at the size asked for, from the app found.
    #[gpui::test]
    fn the_app_icon_is_drawn_from_the_app_found(cx: &mut TestAppContext) {
        let drawn = Rc::new(RefCell::new(Vec::<(String, u32)>::new()));
        cx.update(|cx| {
            let drawn = drawn.clone();
            cx.set_global(SystemApps::new(
                |_| Some("/Applications/Cursor.app".to_string()),
                move |path, pixels| {
                    drawn.borrow_mut().push((path.to_string(), pixels));
                    None
                },
            ));
            let sources = icon_sources(&editor("cursor"), Os::Mac, true, cx);
            let AppIconSource::System { render, .. } = &sources[0] else {
                panic!("{sources:?}");
            };
            assert_eq!(render(56), None);
        });
        assert_eq!(
            *drawn.borrow(),
            [("/Applications/Cursor.app".to_string(), 56)]
        );
    }
}
