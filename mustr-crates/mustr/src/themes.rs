use anyhow::Context as _;
use fs::Fs;
use futures::StreamExt as _;
use gpui::{App, TaskExt as _};
use language::LanguageRegistry;
use std::{sync::Arc, time::Duration};
use theme::{ActiveTheme as _, GlobalTheme, ThemeRegistry};
use theme_settings::load_user_theme;
use util::ResultExt as _;

pub fn init(fs: Arc<dyn Fs>, languages: Arc<LanguageRegistry>, cx: &mut App) {
    load_embedded_fonts(cx).log_err();
    #[cfg(target_os = "linux")]
    prewarm_fonts(cx);

    // Copied from crates/zed/src/main.rs. Syntax highlighting reads its colors
    // from the language registry's theme, so it has to follow the active theme.
    languages.set_theme(cx.theme().clone());
    cx.observe_global::<GlobalTheme>(move |cx| languages.set_theme(cx.theme().clone()))
        .detach();

    load_user_themes_in_background(fs.clone(), cx);
    watch_themes(fs, cx);
}

// Copied from crates/zed/src/main.rs (`load_embedded_fonts`).
fn load_embedded_fonts(cx: &App) -> anyhow::Result<()> {
    let asset_source = cx.asset_source();
    let font_paths = asset_source.list("fonts")?;
    let embedded_fonts = parking_lot::Mutex::new(Vec::new());
    let executor = cx.background_executor();

    cx.foreground_executor().block_on(executor.scoped(|scope| {
        let asset_source = &asset_source;
        let embedded_fonts = &embedded_fonts;
        for font_path in &font_paths {
            if !font_path.ends_with(".ttf") {
                continue;
            }
            scope.spawn(async move {
                match asset_source.load(font_path) {
                    Ok(Some(font_bytes)) => embedded_fonts.lock().push(font_bytes),
                    Ok(None) => log::error!("embedded font {font_path} is missing"),
                    Err(error) => log::error!("failed to load embedded font {font_path}: {error}"),
                }
            });
        }
    }));

    cx.text_system().add_fonts(embedded_fonts.into_inner())
}

// Copied from crates/zed/src/main.rs (`prewarm_fonts`).
#[cfg(target_os = "linux")]
fn prewarm_fonts(cx: &mut App) {
    use gpui::AppContext as _;

    let theme_settings = theme::theme_settings(cx);
    let ui_font = theme_settings.ui_font(cx).clone();
    let buffer_font = theme_settings.buffer_font(cx).clone();
    let mut fonts = vec![ui_font.clone(), buffer_font.clone()];
    let mut add_variant = |base_font: &gpui::Font, weight, style| {
        let mut font = base_font.clone();
        font.weight = weight;
        font.style = style;
        fonts.push(font);
    };

    for weight in [
        gpui::FontWeight::MEDIUM,
        gpui::FontWeight::SEMIBOLD,
        gpui::FontWeight::BOLD,
    ] {
        add_variant(&ui_font, weight, gpui::FontStyle::Normal);
    }
    add_variant(&ui_font, gpui::FontWeight::NORMAL, gpui::FontStyle::Italic);
    add_variant(
        &buffer_font,
        gpui::FontWeight::BOLD,
        gpui::FontStyle::Normal,
    );
    add_variant(
        &buffer_font,
        gpui::FontWeight::NORMAL,
        gpui::FontStyle::Italic,
    );
    add_variant(
        &buffer_font,
        gpui::FontWeight::BOLD,
        gpui::FontStyle::Italic,
    );

    let text_system = cx.text_system().clone();
    cx.background_spawn(async move {
        text_system.prewarm_fonts(&fonts);
    })
    .detach();
}

// Copied from crates/zed/src/main.rs (`load_user_themes_in_background`).
fn load_user_themes_in_background(fs: Arc<dyn Fs>, cx: &mut App) {
    cx.spawn(async move |cx| {
        let theme_registry = cx.update(|cx| ThemeRegistry::global(cx));
        let themes_dir = paths::themes_dir().as_ref();
        match fs
            .metadata(themes_dir)
            .await
            .ok()
            .flatten()
            .map(|metadata| metadata.is_dir)
        {
            Some(is_dir) => {
                anyhow::ensure!(is_dir, "Themes dir path {themes_dir:?} is not a directory")
            }
            None => {
                fs.create_dir(themes_dir).await.with_context(|| {
                    format!("Failed to create themes dir at path {themes_dir:?}")
                })?;
            }
        }

        let mut theme_paths = fs
            .read_dir(themes_dir)
            .await
            .with_context(|| format!("reading themes from {themes_dir:?}"))?;

        while let Some(theme_path) = theme_paths.next().await {
            let Some(theme_path) = theme_path.log_err() else {
                continue;
            };
            let Some(bytes) = fs
                .load_bytes(&theme_path)
                .await
                .with_context(|| format!("loading theme bytes from {theme_path:?}"))
                .log_err()
            else {
                continue;
            };

            load_user_theme(&theme_registry, &bytes).log_err();
        }

        cx.update(theme_settings::reload_theme);
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

// Copied from crates/zed/src/main.rs (`watch_themes`).
fn watch_themes(fs: Arc<dyn Fs>, cx: &mut App) {
    cx.spawn(async move |cx| {
        let (mut events, _) = fs
            .watch(paths::themes_dir(), Duration::from_millis(100))
            .await;

        while let Some(paths) = events.next().await {
            for event in paths {
                if fs
                    .metadata(&event.path)
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|metadata| !metadata.is_dir)
                {
                    let theme_registry = cx.update(|cx| ThemeRegistry::global(cx));
                    if let Some(bytes) = fs
                        .load_bytes(&event.path)
                        .await
                        .with_context(|| format!("loading theme bytes from {:?}", event.path))
                        .log_err()
                        && load_user_theme(&theme_registry, &bytes).log_err().is_some()
                    {
                        cx.update(theme_settings::reload_theme);
                    }
                }
            }
        }
    })
    .detach()
}
