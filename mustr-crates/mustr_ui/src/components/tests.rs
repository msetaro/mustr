use super::*;
use crate::{COLORBLIND_SAFE_THEME_NAME, DARK_THEME_NAME, LIGHT_THEME_NAME};
use gpui::{
    Context, IntoElement, Modifiers, ParentElement as _, Render, Styled as _, TestAppContext,
    UpdateGlobal as _, VisualTestContext, Window, div,
};
use settings::{SettingsStore, ThemeName, ThemeSelection};
use std::{cell::RefCell, rc::Rc};
use theme::LoadThemes;
use ui::{Icon, IconName};

type Log = Rc<RefCell<Vec<String>>>;

/// Renders one of each component, recording clicks in `log`.
struct Gallery {
    log: Log,
    selected_segment: usize,
}

impl Render for Gallery {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let log = |name: &'static str, log: &Log| {
            let log = log.clone();
            move |_: &gpui::ClickEvent, _: &mut Window, _: &mut gpui::App| {
                log.borrow_mut().push(name.to_string())
            }
        };
        let entity = cx.entity().downgrade();
        let segment_log = self.log.clone();
        let close_log = self.log.clone();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                SegmentedControl::new("style")
                    .segment("Unified")
                    .segment_with_tooltip("Split", "Split the diff")
                    .selected(self.selected_segment)
                    .on_select(move |index, _, cx| {
                        segment_log.borrow_mut().push(format!("segment {index}"));
                        entity
                            .update(cx, |gallery, cx| {
                                gallery.selected_segment = index;
                                cx.notify();
                            })
                            .ok();
                    }),
            )
            .child(
                SplitButton::new("submit", "Submit review")
                    .style(ButtonStyle::Primary)
                    .count("3")
                    .on_click(log("submit", &self.log))
                    .on_menu_click(log("submit menu", &self.log)),
            )
            .child(
                WindowTab::new("tab", "acme/api #482")
                    .active(true)
                    .on_click(log("tab", &self.log))
                    .on_close(move |_, _| close_log.borrow_mut().push("close".into())),
            )
            .child(
                Chip::new("since", "Since your last review")
                    .accent(true)
                    .menu_indicator(true)
                    .dot(gpui::red()),
            )
            .child(Pill::tinted("Pending", gpui::blue()))
            .child(Pill::filled("Open", gpui::green()))
            .child(TextButton::new("github", "Open on GitHub"))
            .child(StatusHint::new(["j", "k"], "hunk"))
            .child(Kbd::new("⌘K"))
            .child(RailButton::new("inbox", Icon::new(IconName::Envelope)).badge("4"))
            .child(Card::new().p_1().child("A card"))
    }
}

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        theme_settings::init(LoadThemes::JustBase, cx);
        crate::init(cx);
    });
}

fn open_gallery(cx: &mut TestAppContext) -> (Log, &mut VisualTestContext) {
    let log = Log::default();
    let (_, cx) = cx.add_window_view({
        let log = log.clone();
        |_, _| Gallery {
            log,
            selected_segment: 0,
        }
    });
    cx.run_until_parked();
    (log, cx)
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} should be rendered"));
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
}

#[gpui::test]
fn components_render_in_every_mustr_theme(cx: &mut TestAppContext) {
    init_test(cx);
    let (_, cx) = open_gallery(cx);
    for name in [
        DARK_THEME_NAME,
        LIGHT_THEME_NAME,
        COLORBLIND_SAFE_THEME_NAME,
    ] {
        cx.update(|_, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.theme.theme = Some(ThemeSelection::Static(ThemeName(name.into())));
                });
            });
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("segment-Unified").is_some(), "{name}");
        assert!(cx.debug_bounds("split-button-menu").is_some(), "{name}");
    }
}

#[gpui::test]
fn segmented_control_reports_the_clicked_segment(cx: &mut TestAppContext) {
    init_test(cx);
    let (log, cx) = open_gallery(cx);
    click(cx, "segment-Split");
    click(cx, "segment-Unified");
    assert_eq!(*log.borrow(), ["segment 1", "segment 0"]);
}

#[gpui::test]
fn split_button_halves_have_separate_actions(cx: &mut TestAppContext) {
    init_test(cx);
    let (log, cx) = open_gallery(cx);
    click(cx, "split-button-main");
    click(cx, "split-button-menu");
    assert_eq!(*log.borrow(), ["submit", "submit menu"]);
}

#[gpui::test]
fn closing_a_window_tab_does_not_activate_it(cx: &mut TestAppContext) {
    init_test(cx);
    let (log, cx) = open_gallery(cx);
    click(cx, "window-tab-close");
    assert_eq!(*log.borrow(), ["close"]);
}

#[test]
fn key_labels() {
    let ctrl_shift = Modifiers {
        control: true,
        shift: true,
        ..Modifiers::none()
    };
    assert_eq!(key_label(&ctrl_shift, "p", KeyStyle::Words), "Ctrl+Shift+P");
    assert_eq!(
        key_label(&Modifiers::command(), "k", KeyStyle::Symbols),
        "⌘K"
    );
    assert_eq!(key_label(&ctrl_shift, "p", KeyStyle::Symbols), "⌃⇧P");
    assert_eq!(key_label(&Modifiers::none(), "]", KeyStyle::Words), "]");
    assert_eq!(
        key_label(&Modifiers::none(), "escape", KeyStyle::Words),
        "Esc"
    );
    assert_eq!(
        key_label(&Modifiers::alt(), "f12", KeyStyle::Words),
        "Alt+F12"
    );
}
