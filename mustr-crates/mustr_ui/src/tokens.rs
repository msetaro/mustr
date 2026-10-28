//! mustr's design tokens, from `mustr-docs/PLAN.md` §6 "Look and feel".
//!
//! `assets/themes/mustr/mustr.json` is written from the same values, and the
//! tests in this crate check that the two agree.

use gpui::{App, Hsla, Pixels, Rems, Rgba, px, rems};
use settings::Settings as _;
use theme_settings::ThemeSettings;

/// A color written as `0xRRGGBBAA`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ColorToken(pub u32);

impl ColorToken {
    pub fn rgba(self) -> Rgba {
        gpui::rgba(self.0)
    }

    pub fn hsla(self) -> Hsla {
        self.rgba().into()
    }
}

impl From<ColorToken> for Hsla {
    fn from(token: ColorToken) -> Self {
        token.hsla()
    }
}

impl From<ColorToken> for Rgba {
    fn from(token: ColorToken) -> Self {
        token.rgba()
    }
}

/// The colors of one mustr theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub background: ColorToken,
    pub surface: ColorToken,
    pub surface_2: ColorToken,
    pub border: ColorToken,
    pub text: ColorToken,
    pub text_muted: ColorToken,
    pub accent: ColorToken,
    /// Text on an accent background, such as a primary button.
    pub accent_foreground: ColorToken,
    /// A faint accent fill, for selected rows and accent chips.
    pub accent_soft: ColorToken,
    pub added_line: ColorToken,
    pub added_line_strong: ColorToken,
    pub deleted_line: ColorToken,
    pub deleted_line_strong: ColorToken,
    pub warning: ColorToken,
    pub ok: ColorToken,
    pub pending: ColorToken,
    pub agent: ColorToken,
    pub syntax: SyntaxPalette,
}

/// Code highlight colors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyntaxPalette {
    pub keyword: ColorToken,
    pub function: ColorToken,
    pub string: ColorToken,
    pub number: ColorToken,
    pub type_name: ColorToken,
    pub property: ColorToken,
    pub comment: ColorToken,
}

// Alphas from the mockups' `rgba(r, g, b, a)` values are rounded to the
// nearest byte, matching the theme JSON.
pub const DARK: Palette = Palette {
    background: ColorToken(0x0f1318ff),
    surface: ColorToken(0x151a21ff),
    surface_2: ColorToken(0x1c232cff),
    border: ColorToken(0x29313bff),
    text: ColorToken(0xd6dce3ff),
    text_muted: ColorToken(0x8792a0ff),
    accent: ColorToken(0x5aa2ffff),
    accent_foreground: ColorToken(0x04111fff),
    accent_soft: ColorToken(0x5aa2ff24),
    added_line: ColorToken(0x2ea04324),
    added_line_strong: ColorToken(0x2ea0436b),
    deleted_line: ColorToken(0xf851491f),
    deleted_line_strong: ColorToken(0xf8514966),
    warning: ColorToken(0xd29922ff),
    ok: ColorToken(0x3fb950ff),
    pending: ColorToken(0xb083f0ff),
    agent: ColorToken(0xd97757ff),
    syntax: SyntaxPalette {
        keyword: ColorToken(0xff7b72ff),
        function: ColorToken(0xd2a8ffff),
        string: ColorToken(0xa5d6ffff),
        number: ColorToken(0x79c0ffff),
        type_name: ColorToken(0xffa657ff),
        property: ColorToken(0x7ee787ff),
        comment: ColorToken(0x8b949eff),
    },
};

pub const LIGHT: Palette = Palette {
    background: ColorToken(0xffffffff),
    surface: ColorToken(0xf6f8faff),
    surface_2: ColorToken(0xeceff3ff),
    border: ColorToken(0xd8dee4ff),
    text: ColorToken(0x1f2328ff),
    text_muted: ColorToken(0x636c76ff),
    accent: ColorToken(0x0969daff),
    accent_foreground: ColorToken(0xffffffff),
    accent_soft: ColorToken(0x0969da1a),
    added_line: ColorToken(0xe6ffecff),
    added_line_strong: ColorToken(0xabf2bcff),
    deleted_line: ColorToken(0xffebe9ff),
    deleted_line_strong: ColorToken(0xffc1bdff),
    warning: ColorToken(0x9a6700ff),
    ok: ColorToken(0x1a7f37ff),
    pending: ColorToken(0x8250dfff),
    agent: ColorToken(0xd97757ff),
    syntax: SyntaxPalette {
        keyword: ColorToken(0xcf222eff),
        function: ColorToken(0x8250dfff),
        string: ColorToken(0x0a3069ff),
        number: ColorToken(0x0550aeff),
        type_name: ColorToken(0x953800ff),
        property: ColorToken(0x116329ff),
        comment: ColorToken(0x6e7781ff),
    },
};

/// Dark, with blue for added lines and orange for deleted ones.
pub const COLORBLIND_SAFE: Palette = Palette {
    added_line: ColorToken(0x388bfd26),
    added_line_strong: ColorToken(0x388bfd75),
    deleted_line: ColorToken(0xdb6d2824),
    deleted_line_strong: ColorToken(0xdb6d2875),
    ok: ColorToken(0x58a6ffff),
    ..DARK
};

/// Zed's name for the platform's UI font.
pub const UI_FONT_FAMILY: &str = ".SystemUIFont";
/// Zed's bundled monospace font.
pub const CODE_FONT_FAMILY: &str = ".ZedMono";

/// The `ui_font_size` mustr's designs are drawn at. A rem is the user's `ui_font_size`, so
/// [`text_size`] and [`scaled`] turn the sizes below into rems that follow it.
pub const UI_FONT_SIZE: Pixels = px(13.);

/// Key caps.
pub const TEXT_SIZE_TINY: Pixels = px(10.);
/// The status bar, pills, and counts.
pub const TEXT_SIZE_XSMALL: Pixels = px(10.5);
pub const TEXT_SIZE_SMALL: Pixels = px(11.5);
pub const TEXT_SIZE_MEDIUM: Pixels = px(12.);
pub const TEXT_SIZE_LARGE: Pixels = px(13.);

/// Cards.
pub const CORNER_RADIUS: Pixels = px(8.);
/// Buttons, tabs, segmented controls, and inputs.
pub const CONTROL_RADIUS: Pixels = px(6.);
/// Key caps.
pub const KBD_RADIUS: Pixels = px(4.);
pub const BORDER_WIDTH: Pixels = px(1.);

/// The width of the icon rail.
pub const RAIL_WIDTH: Pixels = px(38.);

/// A length from mustr's designs, in rems. The designs are drawn at [`UI_FONT_SIZE`], so lengths
/// written with this scale with the user's `ui_font_size`, the way a web page zooms. Hairlines
/// ([`BORDER_WIDTH`]) stay in pixels.
pub fn scaled(length: Pixels) -> Rems {
    rems(length / UI_FONT_SIZE)
}

/// One of the text sizes above, in rems, so it follows the user's `ui_font_size`.
pub fn text_size(size: Pixels) -> Rems {
    scaled(size)
}

/// [`scaled`], in pixels at the user's `ui_font_size`, for APIs that take pixels outside of
/// rendering, such as a dock panel's size. It's `scaled(length).to_pixels(window.rem_size())`,
/// read from the settings rather than from a window, so it's right before a window's first
/// frame too.
pub fn scaled_pixels(length: Pixels, cx: &App) -> Pixels {
    scaled(length).to_pixels(ThemeSettings::get_global(cx).ui_font_size(cx))
}
