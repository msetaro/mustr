//! Programs' icons, for Open in: the brand icons T3 Code's app draws for its editors (copied from
//! T3's `apps/web/src/components/Icons.tsx` and `JetBrainsIcons.tsx`, MIT; see
//! `THIRD_PARTY_NOTICES.md`), and [`AppIcon`], which draws a program's icon crisply at any zoom
//! and on any screen.
//!
//! An [`AppIcon`] tries its sources in turn ([`AppIconSource`]):
//! - **A system icon**, such as the icon macOS shows for an installed app: a function gives a PNG
//!   at a pixel size.
//! - **A brand icon** ([`BrandIcon`]): T3's, in color, or in the text color for one-color logos.
//! - **A glyph**, drawn in the text color: mustr's generic program icon, for instance.
//!
//! Bitmaps are drawn at exactly the device pixels they cover (the icon's size, zoomed, times the
//! screen's scale factor), so they're sharp at mustr's design size, zoomed in, and on Retina
//! screens. Each is made once per program and pixel size and kept in [`AppIconCache`]; one that
//! can't be made is remembered too, so the next source is used without trying again.

use crate::{MustrIcon, palette, tokens::scaled};
use gpui::{
    App, Global, ImageSource, IntoElement, Pixels, RenderImage, RenderOnce, SharedString, Size,
    Styled as _, SvgSize, Window, div, img, px, svg,
};
use image::{ImageBuffer, Rgba, imageops::FilterType};
use std::{collections::HashMap, rc::Rc, sync::Arc};

/// The icons T3 Code draws for its editors, by T3's editor (`EDITORS` in its
/// `packages/contracts/src/editor.ts`), and Finder and File Explorer for its file manager.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BrandIcon {
    Cursor,
    Trae,
    Kiro,
    VsCode,
    VsCodeInsiders,
    VsCodium,
    Zed,
    Antigravity,
    IntelliJIdea,
    Aqua,
    CLion,
    DataGrip,
    DataSpell,
    GoLand,
    PhpStorm,
    PyCharm,
    Rider,
    RubyMine,
    RustRover,
    WebStorm,
    Finder,
    FileExplorer,
}

/// What a brand icon is drawn from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Art {
    /// A colored SVG, rasterized.
    Svg(&'static str),
    /// A bitmap, scaled to size.
    Png(&'static [u8]),
    /// A one-color SVG, served at this path and drawn in the text color.
    Mono(&'static str, &'static str),
}

macro_rules! svg_art {
    ($name:literal) => {
        Art::Svg(include_str!(concat!("../assets/app_icons/", $name, ".svg")))
    };
}

macro_rules! mono_art {
    ($name:literal) => {
        Art::Mono(
            concat!("icons/mustr/apps/", $name, ".svg"),
            include_str!(concat!("../assets/app_icons/", $name, ".svg")),
        )
    };
}

impl BrandIcon {
    pub const ALL: [Self; 22] = [
        Self::Cursor,
        Self::Trae,
        Self::Kiro,
        Self::VsCode,
        Self::VsCodeInsiders,
        Self::VsCodium,
        Self::Zed,
        Self::Antigravity,
        Self::IntelliJIdea,
        Self::Aqua,
        Self::CLion,
        Self::DataGrip,
        Self::DataSpell,
        Self::GoLand,
        Self::PhpStorm,
        Self::PyCharm,
        Self::Rider,
        Self::RubyMine,
        Self::RustRover,
        Self::WebStorm,
        Self::Finder,
        Self::FileExplorer,
    ];

    fn art(self) -> Art {
        match self {
            // Cursor's mark is near-black in T3's light theme and near-white in its dark one.
            Self::Cursor => mono_art!("cursor"),
            Self::Trae => mono_art!("trae"),
            Self::Kiro => svg_art!("kiro"),
            Self::VsCode => svg_art!("vscode"),
            Self::VsCodeInsiders => svg_art!("vscode_insiders"),
            Self::VsCodium => svg_art!("vscodium"),
            Self::Zed => mono_art!("zed"),
            Self::Antigravity => Art::Png(include_bytes!("../assets/app_icons/antigravity.png")),
            Self::IntelliJIdea => svg_art!("intellij_idea"),
            Self::Aqua => svg_art!("aqua"),
            Self::CLion => svg_art!("clion"),
            Self::DataGrip => svg_art!("datagrip"),
            Self::DataSpell => svg_art!("dataspell"),
            Self::GoLand => svg_art!("goland"),
            Self::PhpStorm => svg_art!("phpstorm"),
            Self::PyCharm => svg_art!("pycharm"),
            Self::Rider => svg_art!("rider"),
            Self::RubyMine => svg_art!("rubymine"),
            Self::RustRover => svg_art!("rustrover"),
            Self::WebStorm => svg_art!("webstorm"),
            Self::Finder => svg_art!("finder"),
            Self::FileExplorer => svg_art!("file_explorer"),
        }
    }

    /// The icon's name in [`AppIconCache`].
    fn cache_key(self) -> SharedString {
        format!("brand:{self:?}").into()
    }
}

/// The one-color brand icons, which [`crate::MustrAssets`] serves for `svg()` to draw.
pub(crate) fn served_svgs() -> impl Iterator<Item = (&'static str, &'static str)> {
    BrandIcon::ALL
        .into_iter()
        .filter_map(|icon| match icon.art() {
            Art::Mono(path, svg) => Some((path, svg)),
            Art::Svg(_) | Art::Png(_) => None,
        })
}

/// Gives a program's icon as a PNG of a pixel size (the width and height it's drawn at), or
/// `None` when there's none.
pub type RenderPng = Rc<dyn Fn(u32) -> Option<Vec<u8>>>;

/// One place an [`AppIcon`] can come from.
#[derive(Clone)]
pub enum AppIconSource {
    /// A bitmap the system draws, such as macOS's icon for an installed app. `key` names it in
    /// the cache (for example, by the app's bundle identifier).
    System {
        key: SharedString,
        render: RenderPng,
    },
    Brand(BrandIcon),
    /// A one-color SVG, drawn in the muted text color.
    Glyph(SharedString),
}

impl AppIconSource {
    /// mustr's generic program icon.
    pub fn generic() -> Self {
        Self::Glyph(MustrIcon::App.path().into())
    }
}

impl std::fmt::Debug for AppIconSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::System { key, .. } => f.debug_tuple("System").field(key).finish(),
            Self::Brand(icon) => f.debug_tuple("Brand").field(icon).finish(),
            Self::Glyph(path) => f.debug_tuple("Glyph").field(path).finish(),
        }
    }
}

/// The bitmaps drawn so far, by source and pixel size, with `None` for those that couldn't be.
#[derive(Default)]
pub struct AppIconCache {
    images: HashMap<(SharedString, u32), Option<Arc<RenderImage>>>,
}

impl Global for AppIconCache {}

impl AppIconCache {
    /// The bitmap of `key` at `pixels`, made by `make` the first time it's asked for.
    fn get_or_make(
        cx: &mut App,
        key: &SharedString,
        pixels: u32,
        make: impl FnOnce(&mut App) -> Option<Arc<RenderImage>>,
    ) -> Option<Arc<RenderImage>> {
        let entry = (key.clone(), pixels);
        if let Some(image) = cx.default_global::<Self>().images.get(&entry) {
            return image.clone();
        }
        let image = make(cx);
        if image.is_none() {
            log::debug!("no {key} icon at {pixels}px");
        }
        cx.default_global::<Self>()
            .images
            .insert(entry, image.clone());
        image
    }

    /// How many bitmaps have been made or tried, for tests.
    pub fn len(cx: &App) -> usize {
        cx.try_global::<Self>()
            .map_or(0, |cache| cache.images.len())
    }

    /// Whether `key`'s bitmap at `pixels` was made (`Some(true)`), couldn't be (`Some(false)`),
    /// or hasn't been asked for.
    pub fn made(cx: &App, key: &str, pixels: u32) -> Option<bool> {
        let cache = cx.try_global::<Self>()?;
        cache
            .images
            .get(&(SharedString::from(key.to_string()), pixels))
            .map(Option::is_some)
    }
}

/// The device pixels an icon `size` wide (at mustr's design size) covers in `window`.
pub fn device_pixels(size: Pixels, window: &Window) -> u32 {
    let logical = scaled(size).to_pixels(window.rem_size());
    ((logical * window.scale_factor()) / px(1.)).round().max(1.) as u32
}

/// A PNG as a bitmap `pixels` wide, scaled to fit if it isn't (keeping its proportions).
pub fn png_to_image(png: &[u8], pixels: u32) -> Option<Arc<RenderImage>> {
    let decoded = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .inspect_err(|error| log::warn!("couldn't read an app icon: {error}"))
        .ok()?
        .into_rgba8();
    let decoded = if decoded.width() == pixels && decoded.height() == pixels {
        decoded
    } else {
        resize(decoded, pixels)
    };
    Some(bgra_image(decoded))
}

/// Scales `image` to fit `pixels`, keeping its proportions. Scales the colors premultiplied by
/// their alpha, so transparent pixels don't darken the edges.
fn resize(
    mut image: ImageBuffer<Rgba<u8>, Vec<u8>>,
    pixels: u32,
) -> ImageBuffer<Rgba<u8>, Vec<u8>> {
    let (width, height) = image.dimensions();
    let scale = pixels as f32 / width.max(height).max(1) as f32;
    let new_width = ((width as f32 * scale).round() as u32).max(1);
    let new_height = ((height as f32 * scale).round() as u32).max(1);
    for pixel in image.pixels_mut() {
        let alpha = pixel[3] as u16;
        for channel in 0..3 {
            pixel[channel] = ((pixel[channel] as u16 * alpha + 127) / 255) as u8;
        }
    }
    let mut resized = image::imageops::resize(&image, new_width, new_height, FilterType::Lanczos3);
    for pixel in resized.pixels_mut() {
        let alpha = pixel[3] as u16;
        for channel in 0..3 {
            pixel[channel] = (pixel[channel] as u16 * 255 + alpha / 2)
                .checked_div(alpha)
                .map_or(0, |value| value.min(255) as u8);
        }
    }
    resized
}

/// GPUI's bitmaps are BGRA, with straight alpha.
fn bgra_image(mut image: ImageBuffer<Rgba<u8>, Vec<u8>>) -> Arc<RenderImage> {
    for pixel in image.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(
        image
    )]))
}

fn svg_to_image(svg: &str, pixels: u32, cx: &App) -> Option<Arc<RenderImage>> {
    let renderer = cx.svg_renderer();
    let tree = renderer
        .parse_svg(svg.as_bytes())
        .inspect_err(|error| log::warn!("couldn't read an app icon: {error}"))
        .ok()?;
    let size = Size::new(
        gpui::DevicePixels(pixels as i32),
        gpui::DevicePixels(pixels as i32),
    );
    renderer
        .render_parsed(&tree, SvgSize::Size(size))
        .inspect_err(|error| log::warn!("couldn't draw an app icon: {error}"))
        .ok()
}

/// A program's icon, `size` wide and tall at mustr's design size: the first of its sources that
/// draws.
#[derive(IntoElement)]
pub struct AppIcon {
    sources: Vec<AppIconSource>,
    size: Pixels,
}

impl AppIcon {
    pub fn new(sources: impl IntoIterator<Item = AppIconSource>, size: Pixels) -> Self {
        Self {
            sources: sources.into_iter().collect(),
            size,
        }
    }

    fn bitmap(source: &AppIconSource, pixels: u32, cx: &mut App) -> Option<Arc<RenderImage>> {
        match source {
            AppIconSource::System { key, render } => {
                let render = render.clone();
                AppIconCache::get_or_make(cx, key, pixels, |_| {
                    png_to_image(&render(pixels)?, pixels)
                })
            }
            AppIconSource::Brand(icon) => match icon.art() {
                Art::Svg(svg) => AppIconCache::get_or_make(cx, &icon.cache_key(), pixels, |cx| {
                    svg_to_image(svg, pixels, cx)
                }),
                Art::Png(png) => AppIconCache::get_or_make(cx, &icon.cache_key(), pixels, |_| {
                    png_to_image(png, pixels)
                }),
                Art::Mono(..) => None,
            },
            AppIconSource::Glyph(_) => None,
        }
    }

    /// The SVG a one-color source draws, if it's one.
    fn mono_path(source: &AppIconSource) -> Option<SharedString> {
        match source {
            AppIconSource::Brand(icon) => match icon.art() {
                Art::Mono(path, _) => Some(path.into()),
                Art::Svg(_) | Art::Png(_) => None,
            },
            AppIconSource::Glyph(path) => Some(path.clone()),
            AppIconSource::System { .. } => None,
        }
    }
}

impl RenderOnce for AppIcon {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let pixels = device_pixels(self.size, window);
        let size = scaled(self.size);
        for source in &self.sources {
            if let Some(image) = Self::bitmap(source, pixels, cx) {
                return img(ImageSource::Render(image))
                    .flex_none()
                    .size(size)
                    .into_any_element();
            }
            if let Some(path) = Self::mono_path(source) {
                let color = match source {
                    AppIconSource::Glyph(_) => palette(cx).text_muted.hsla(),
                    _ => palette(cx).text.hsla(),
                };
                return svg()
                    .path(path)
                    .flex_none()
                    .size(size)
                    .text_color(color)
                    .into_any_element();
            }
        }
        div().flex_none().size(size).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, ParentElement as _, Render, TestAppContext};
    use std::cell::RefCell;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = ImageBuffer::from_pixel(width, height, Rgba([200u8, 40, 40, 255]));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    #[test]
    fn every_brand_icon_reads() {
        let renderer = gpui::SvgRenderer::new(Arc::new(()));
        for icon in BrandIcon::ALL {
            match icon.art() {
                Art::Svg(svg) | Art::Mono(_, svg) => {
                    assert!(renderer.parse_svg(svg.as_bytes()).is_ok(), "{icon:?}");
                }
                Art::Png(png) => {
                    let image = png_to_image(png, 28).expect("the PNG reads");
                    assert_eq!(image.size(0), Size::new(28.into(), 28.into()), "{icon:?}");
                }
            }
        }
    }

    #[test]
    fn bitmaps_are_scaled_to_the_pixels_they_cover() {
        let image = png_to_image(&png(64, 64), 28).unwrap();
        assert_eq!(image.size(0), Size::new(28.into(), 28.into()));
        // Wider than tall keeps its proportions.
        let image = png_to_image(&png(64, 32), 28).unwrap();
        assert_eq!(image.size(0), Size::new(28.into(), 14.into()));
        // BGRA, with the color kept through the scaling.
        assert_eq!(&image.as_bytes(0).unwrap()[..4], &[40, 40, 200, 255]);
        assert!(png_to_image(b"not a png", 28).is_none());
    }

    struct Icons {
        sources: Vec<Vec<AppIconSource>>,
        size: Pixels,
    }

    impl Render for Icons {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().children(
                self.sources
                    .iter()
                    .map(|sources| AppIcon::new(sources.clone(), self.size)),
            )
        }
    }

    /// Icons are made once per source and pixel size: the icon's size, zoomed, times the
    /// screen's scale factor. A system icon that can't be made falls back to the next source,
    /// and isn't asked for again.
    #[gpui::test]
    fn icons_are_drawn_at_the_screens_pixels_and_kept(cx: &mut TestAppContext) {
        let asked = Rc::new(RefCell::new(Vec::<u32>::new()));
        let render = |found: bool| -> RenderPng {
            let asked = asked.clone();
            Rc::new(move |pixels| {
                asked.borrow_mut().push(pixels);
                found.then(|| png(pixels, pixels))
            })
        };
        let sources = vec![
            vec![
                AppIconSource::System {
                    key: "macos:com.example.found".into(),
                    render: render(true),
                },
                AppIconSource::Brand(BrandIcon::VsCode),
            ],
            vec![
                AppIconSource::System {
                    key: "macos:com.example.missing".into(),
                    render: render(false),
                },
                AppIconSource::Brand(BrandIcon::VsCode),
            ],
            vec![AppIconSource::Brand(BrandIcon::Zed)],
            vec![AppIconSource::generic()],
        ];
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            crate::init(cx);
        });
        let (_, cx) = cx.add_window_view(|_, _| Icons {
            sources,
            size: px(14.),
        });
        let scale = cx.update(|window, _| window.scale_factor());
        let at_13 = (14. * scale).round() as u32;
        let draw_at = |rem_size: f32, cx: &mut gpui::VisualTestContext| {
            cx.update(|window, _| {
                window.set_rem_size(px(rem_size));
                window.refresh();
            });
            cx.run_until_parked();
        };
        draw_at(13., cx);
        cx.update(|_, cx| {
            assert_eq!(
                AppIconCache::made(cx, "macos:com.example.found", at_13),
                Some(true)
            );
            assert_eq!(
                AppIconCache::made(cx, "macos:com.example.missing", at_13),
                Some(false)
            );
            // The missing app's icon fell back to T3's.
            assert_eq!(AppIconCache::made(cx, "brand:VsCode", at_13), Some(true));
            // One-color icons are drawn as SVGs, not kept as bitmaps.
            assert_eq!(AppIconCache::made(cx, "brand:Zed", at_13), None);
        });

        // Each system icon was asked for once at that size, and drawn again, nothing is made
        // again.
        let asked_at = |pixels| {
            asked
                .borrow()
                .iter()
                .filter(|asked| **asked == pixels)
                .count()
        };
        assert_eq!(asked_at(at_13), 2);
        let cached = cx.update(|_, cx| AppIconCache::len(cx));
        draw_at(13., cx);
        assert_eq!(asked_at(at_13), 2);
        assert_eq!(cx.update(|_, cx| AppIconCache::len(cx)), cached);

        // Zoomed to 26, each is made at twice the pixels.
        draw_at(26., cx);
        cx.update(|_, cx| {
            assert_eq!(
                AppIconCache::made(cx, "macos:com.example.found", at_13 * 2),
                Some(true)
            );
            assert_eq!(
                AppIconCache::made(cx, "brand:VsCode", at_13 * 2),
                Some(true)
            );
        });
    }
}
