//! mustr's Settings page: every setting mustr reads, and T3 Code's settings, as controls. ⌘, (Ctrl+,
//! elsewhere), mustr › Settings…, the palette's "mustr: open settings", and the rail's gear open
//! it; Key Bindings… opens its Keyboard shortcuts.
//!
//! - **Sections** (a list on the left): General, Editor, Terminal, Agents and threads (mustr's own
//!   thread settings and T3 Code's, under a heading naming the server), Connection, Updates, and
//!   Keyboard shortcuts. The search box above them finds settings by name in every section.
//! - **Changes apply at once,** and are saved where mustr keeps them: Zed's settings through
//!   `settings::update_settings_file`, mustr's own (`"notifications"`, `"updates"`, `"t3"`)
//!   through [`crate::app_settings::update_mustr_setting`], the follow-up behavior in mustr's
//!   database, saved servers in `servers.json`, T3's settings with `server.updateSettings`, and
//!   key bindings in the key bindings file. There's no Save button.
//! - **No files.** mustr never opens its settings or key bindings file for the user: they're
//!   storage behind this page. When one can't be read, the page says why, and can start it over
//!   from mustr's defaults (keeping the old one next to it).

mod agents;
mod connection;
mod editing;
mod general;
pub(crate) mod key_bindings;
mod review;
pub(crate) mod rows;
mod terminal;
#[cfg(test)]
mod tests;
mod updates;

use crate::{
    app_settings::{self, ConfigFileErrors},
    t3_connection::T3Connection,
    t3_settings::T3SettingsView,
    worktree_cleanup::WorktreeCleanup,
};
use editor::{Editor, EditorEvent};
use fs::Fs;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollHandle,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px,
};
use key_bindings::KeyBindingsView;
use mustr_ui::{ButtonStyle, Card, MustrIcon, TextButton, palette, tokens, tokens::scaled};
use rows::{Group, Query};
use settings::{SettingsContent, SettingsStore};
use ui::{
    Color, Headline, HeadlineSize, Icon, IconName, IconSize, Label, LabelCommon as _, LabelSize,
    prelude::*,
};
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

/// How wide the settings get, at mustr's design size.
const CONTENT_MAX_WIDTH: f32 = 760.;
/// The section list's width, at mustr's design size.
const NAV_WIDTH: f32 = 196.;
/// A window narrower than this, at mustr's design size, shows the sections above the settings
/// rather than beside them.
const MIN_WIDTH_FOR_NAV: f32 = 720.;

/// The settings a search found in a section, drawn.
#[derive(Default)]
pub(crate) struct Found {
    pub count: usize,
    pub elements: Vec<AnyElement>,
}

/// A part of the Settings page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Section {
    General,
    Editor,
    Terminal,
    Agents,
    Review,
    Connection,
    Updates,
    KeyboardShortcuts,
}

impl Section {
    pub const ALL: [Self; 8] = [
        Self::General,
        Self::Editor,
        Self::Terminal,
        Self::Agents,
        Self::Review,
        Self::Connection,
        Self::Updates,
        Self::KeyboardShortcuts,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Editor => "Editor",
            Self::Terminal => "Terminal",
            Self::Agents => "Agents and threads",
            Self::Review => "Review",
            Self::Connection => "Connection",
            Self::Updates => "Updates",
            Self::KeyboardShortcuts => "Keyboard shortcuts",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::General => "How mustr looks: its theme, size, and fonts.",
            Self::Editor => "Files you open and the code in reviews.",
            Self::Terminal => "The terminals of threads and projects.",
            Self::Agents => {
                "How mustr tells you about agent threads, and T3 Code's settings for them."
            }
            Self::Review => "The Review page's sections of pull requests, and their queries.",
            Self::Connection => "The T3 Code server mustr runs agents on.",
            Self::Updates => "Which mustr this is, and how it updates itself.",
            Self::KeyboardShortcuts => "Every action's keys. Change one, or give an action keys.",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Self::General => MustrIcon::Settings.icon(),
            Self::Editor => Icon::new(IconName::FileCode),
            Self::Terminal => Icon::new(IconName::Terminal),
            Self::Agents => MustrIcon::Agent.icon(),
            Self::Review => MustrIcon::PullRequest.icon(),
            Self::Connection => Icon::new(IconName::Server),
            Self::Updates => Icon::new(IconName::ArrowCircle),
            Self::KeyboardShortcuts => Icon::new(IconName::Keyboard),
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::General => "settings-section-general",
            Self::Editor => "settings-section-editor",
            Self::Terminal => "settings-section-terminal",
            Self::Agents => "settings-section-agents",
            Self::Review => "settings-section-review",
            Self::Connection => "settings-section-connection",
            Self::Updates => "settings-section-updates",
            Self::KeyboardShortcuts => "settings-section-keyboard-shortcuts",
        }
    }
}

/// Opens the Settings page at `section`, or shows it there if it's open. `query` searches it:
/// the Keyboard shortcuts' own search for that section, or else the page's.
pub fn show(
    workspace: &mut Workspace,
    section: Section,
    query: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace
        .panes()
        .iter()
        .find_map(|pane| pane.read(cx).items_of_type::<SettingsView>().next());
    let view = match existing {
        Some(view) => {
            workspace.activate_item(&view, true, true, window, cx);
            view
        }
        None => {
            let view = cx.new(|cx| SettingsView::new(window, cx));
            workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
            view
        }
    };
    view.update(cx, |view, cx| view.open_section(section, query, window, cx));
}

/// Changes Zed's settings in the settings file. The page reads them back from the settings store
/// once they're saved.
pub(crate) fn update_settings(
    cx: &mut App,
    update: impl 'static + Send + FnOnce(&mut SettingsContent, &App),
) {
    settings::update_settings_file(<dyn Fs>::global(cx), cx, update);
}

/// Zed's settings as they apply: the defaults (mustr's included), then the user's.
pub(crate) fn merged(cx: &App) -> &SettingsContent {
    cx.global::<SettingsStore>().merged_settings()
}

/// The defaults alone, without the user's settings.
pub(crate) fn defaults(cx: &App) -> &SettingsContent {
    cx.global::<SettingsStore>()
        .get_content_for_file(settings::SettingsFile::Default)
        .unwrap_or_else(|| merged(cx))
}

/// A key's label as mustr shows it on this platform: `⌘=` on macOS, `Ctrl+=` elsewhere.
pub(crate) fn platform_keys(mac: &'static str, other: &'static str) -> &'static str {
    if cfg!(target_os = "macos") {
        mac
    } else {
        other
    }
}

pub struct SettingsView {
    focus_handle: FocusHandle,
    section: Section,
    search: Entity<Editor>,
    query: Query,
    /// T3 Code's settings, in Agents and threads.
    t3: Entity<T3SettingsView>,
    /// T3's automatic worktree cleanup, in Agents and threads.
    cleanup: Entity<WorktreeCleanup>,
    keys: Entity<KeyBindingsView>,
    /// The terminal's shell, when it's a program of the user's.
    shell: Entity<Editor>,
    /// The user picked a program of their own for the shell, but hasn't saved one yet.
    custom_shell: bool,
    /// The connection the T3 parts were made for, to make them again for a new one.
    connection_generation: Option<usize>,
    /// What happened to the last "Start from the defaults".
    reset_message: Option<SharedString>,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl SettingsView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search settings", window, cx);
            editor
        });
        let shell = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("/bin/zsh", window, cx);
            editor
        });
        terminal::fill_shell(&shell, window, cx);
        let backend = thread_chat::backend(cx);
        let t3 = cx.new(|cx| T3SettingsView::new(backend, window, cx));
        let cleanup = cx.new(|cx| {
            let mut cleanup = WorktreeCleanup::new(cx);
            cleanup.set_expanded(true, cx);
            cleanup
        });
        let keys = cx.new(|cx| KeyBindingsView::new(window, cx));
        let mut subscriptions = vec![
            cx.subscribe_in(
                &search,
                window,
                |this, search, event: &EditorEvent, _, cx| {
                    if matches!(event, EditorEvent::BufferEdited) {
                        this.query = Query::new(&search.read(cx).text(cx));
                        this.scroll.set_offset(gpui::point(px(0.), px(0.)));
                        cx.notify();
                    }
                },
            ),
            cx.subscribe(&shell, |_, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    cx.notify();
                }
            }),
            cx.observe(&t3, |_, _, cx| cx.notify()),
            cx.observe(&cleanup, |_, _, cx| cx.notify()),
            cx.observe(&keys, |_, _, cx| cx.notify()),
            cx.observe_global_in::<SettingsStore>(window, |this, window, cx| {
                if !this.custom_shell {
                    terminal::fill_shell(&this.shell, window, cx);
                }
                cx.notify()
            }),
            cx.observe_global::<ConfigFileErrors>(|_, cx| cx.notify()),
        ];
        if let Some(connection) = T3Connection::try_global(cx) {
            subscriptions.push(cx.observe_in(
                &connection,
                window,
                |this, connection, window, cx| this.connection_changed(&connection, window, cx),
            ));
        }
        if let Some(updater) = crate::updates::Updater::global(cx) {
            subscriptions.push(cx.observe(&updater, |_, _, cx| cx.notify()));
        }
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            section: Section::General,
            search,
            query: Query::default(),
            t3,
            cleanup,
            keys,
            shell,
            custom_shell: false,
            connection_generation: None,
            reset_message: None,
            scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        };
        if let Some(connection) = T3Connection::try_global(cx) {
            let connection = connection.read(cx);
            this.connection_generation = connection
                .status()
                .is_connected()
                .then(|| connection.generation());
        }
        this
    }

    #[cfg(test)]
    pub fn section(&self) -> Section {
        self.section
    }

    /// Shows `section`, clearing the search, or searching for `query`.
    pub fn open_section(
        &mut self,
        section: Section,
        query: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match (section, query) {
            (Section::KeyboardShortcuts, Some(query)) => {
                self.set_search("", window, cx);
                self.keys
                    .update(cx, |keys, cx| keys.set_query(&query, window, cx));
            }
            (_, Some(query)) => self.set_search(&query, window, cx),
            (_, None) => self.set_search("", window, cx),
        }
        self.set_section(section, window, cx);
    }

    fn set_section(&mut self, section: Section, window: &mut Window, cx: &mut Context<Self>) {
        if section != Section::KeyboardShortcuts {
            self.keys
                .update(cx, |keys, cx| keys.cancel_recording(window, cx));
        }
        if self.section != section {
            self.section = section;
            self.scroll.set_offset(gpui::point(px(0.), px(0.)));
        }
        cx.notify();
    }

    fn set_search(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.read(cx).text(cx) != text {
            self.search
                .update(cx, |search, cx| search.set_text(text, window, cx));
        }
        self.query = Query::new(text);
    }

    /// Makes T3's parts again for a new connection: they read T3's settings once, from the
    /// server they were made for.
    fn connection_changed(
        &mut self,
        connection: &Entity<T3Connection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let connection = connection.read(cx);
        let generation = connection
            .status()
            .is_connected()
            .then(|| connection.generation());
        if generation.is_none() || generation == self.connection_generation {
            cx.notify();
            return;
        }
        self.connection_generation = generation;
        let backend = thread_chat::backend(cx);
        self.t3 = cx.new(|cx| T3SettingsView::new(backend, window, cx));
        self.cleanup = cx.new(|cx| {
            let mut cleanup = WorktreeCleanup::new(cx);
            cleanup.set_expanded(true, cx);
            cleanup
        });
        self._subscriptions
            .push(cx.observe(&self.t3, |_, _, cx| cx.notify()));
        self._subscriptions
            .push(cx.observe(&self.cleanup, |_, _, cx| cx.notify()));
        cx.notify();
    }

    /// A section's settings that `query` finds (all of them, with an empty query).
    fn groups(
        &mut self,
        section: Section,
        query: &Query,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Found {
        let filter = |groups: Vec<Group>, cx: &App| {
            let mut found = Found::default();
            for group in groups {
                if let Some(group) = group.filter(section.title(), query) {
                    found.count += group.rows().len();
                    found.elements.push(group.render(cx));
                }
            }
            found
        };
        match section {
            Section::General => filter(general::groups(window, cx), cx),
            Section::Editor => filter(editing::groups(window, cx), cx),
            Section::Terminal => {
                let groups = terminal::groups(self, window, cx);
                filter(groups, cx)
            }
            Section::Agents => agents::render(self, query, window, cx),
            Section::Review => filter(review::groups(cx), cx),
            Section::Connection => filter(connection::groups(window, cx), cx),
            Section::Updates => filter(updates::groups(cx), cx),
            Section::KeyboardShortcuts => Found::default(),
        }
    }

    fn render_nav(&self, counts: Option<&[usize]>, cx: &mut Context<Self>) -> AnyElement {
        let palette = palette(cx);
        v_flex()
            .gap_0p5()
            .children(Section::ALL.iter().enumerate().map(|(index, section)| {
                let section = *section;
                let active = counts.is_none() && self.section == section;
                let count = counts.map(|counts| counts[index]);
                let id = section.id();
                h_flex()
                    .id(id)
                    .debug_selector(move || id.to_string())
                    .w_full()
                    .gap_2()
                    .px_2()
                    .py(scaled(px(5.)))
                    .rounded(scaled(tokens::CONTROL_RADIUS))
                    .cursor_pointer()
                    .when(active, |this| this.bg(palette.surface_2.hsla()))
                    .hover(|style| style.bg(palette.surface_2.hsla()))
                    .when(count == Some(0), |this| this.opacity(0.5))
                    .child(section.icon().size(IconSize::Small).color(if active {
                        Color::Default
                    } else {
                        Color::Muted
                    }))
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(section.title())
                                .size(LabelSize::Small)
                                .color(if active { Color::Default } else { Color::Muted })
                                .truncate(),
                        ),
                    )
                    .children(count.filter(|count| *count > 0).map(|count| {
                        Label::new(count.to_string())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_search("", window, cx);
                        this.set_section(section, window, cx);
                    }))
            }))
            .into_any_element()
    }

    fn render_search(&self, cx: &App) -> impl IntoElement {
        let palette = palette(cx);
        h_flex()
            .debug_selector(|| "settings-search".into())
            .w_full()
            .gap_1p5()
            .px_2()
            .py(scaled(px(5.)))
            .rounded(scaled(tokens::CONTROL_RADIUS))
            .border(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .bg(palette.background.hsla())
            .child(
                Icon::new(IconName::MagnifyingGlass)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(div().flex_1().min_w_0().child(self.search.clone()))
    }

    /// Why a settings file can't be read, with a way to start over.
    fn render_file_error(
        &self,
        what: &'static str,
        error: SharedString,
        path: &'static std::path::Path,
        initial: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = SharedString::from(format!("settings-reset-{what}"));
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let card = Card::new()
            .border_color(cx.theme().status().error_border)
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Label::new(format!("mustr couldn't read its {what}"))
                            .weight(FontWeight::MEDIUM)
                            .color(Color::Error),
                    )
                    .child(rows::muted(format!(
                        "Until it can, it uses its defaults for what it couldn't read, and changes \
                         here may not be saved. Starting from the defaults keeps the old file as \
                         {file_name}.bak."
                    )))
                    .child(
                        div()
                            .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
                            .text_color(palette(cx).text_muted.hsla())
                            .font_buffer(cx)
                            .child(error),
                    )
                    .child(
                        h_flex().child(
                            TextButton::new(id, "Start from the defaults")
                                .style(ButtonStyle::Default)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.reset_message =
                                        Some(match app_settings::set_aside(path, initial) {
                                            Ok(backup) => format!(
                                                "Started over. The old file is {}.",
                                                backup.display()
                                            )
                                            .into(),
                                            Err(error) => {
                                                format!("Couldn't start over: {error:#}").into()
                                            }
                                        });
                                    cx.notify();
                                })),
                        ),
                    ),
            );
        div()
            .debug_selector(move || format!("settings-file-error-{what}"))
            .child(card)
            .into_any_element()
    }

    fn render_file_errors(&self, keymap: bool, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let errors = ConfigFileErrors::get(cx);
        let mut elements = Vec::new();
        if let Some(error) = errors.settings {
            elements.push(self.render_file_error(
                "settings",
                error,
                paths::settings_file(),
                app_settings::INITIAL_SETTINGS,
                cx,
            ));
        }
        if keymap && let Some(error) = errors.keymap {
            elements.push(self.render_file_error(
                "key bindings",
                error,
                paths::keymap_file(),
                crate::keymap::INITIAL_KEYMAP,
                cx,
            ));
        }
        if let Some(message) = self.reset_message.clone() {
            elements.push(rows::muted(message).into_any_element());
        }
        elements
    }

    fn render_section(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let section = self.section;
        let header = v_flex()
            .gap_0p5()
            .child(Headline::new(section.title()).size(HeadlineSize::Large))
            .child(rows::muted(section.description()));
        if section == Section::KeyboardShortcuts {
            return v_flex()
                .size_full()
                .px_6()
                .pt_6()
                .pb_2()
                .gap_4()
                .child(header)
                .children(self.render_file_errors(true, cx))
                .child(self.keys.clone())
                .into_any_element();
        }
        let groups = self.groups(section, &Query::default(), window, cx);
        self.scrolling(
            v_flex()
                .gap_5()
                .child(header)
                .children(self.render_file_errors(false, cx))
                .children(groups.elements),
        )
    }

    fn render_results(
        &mut self,
        results: Vec<(Section, Found)>,
        shortcuts: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = palette(cx);
        let any = results.iter().any(|(_, found)| found.count > 0) || shortcuts > 0;
        let query = self.search.read(cx).text(cx);
        let mut content = v_flex()
            .gap_5()
            .child(Headline::new("Search").size(HeadlineSize::Large))
            .children(self.render_file_errors(true, cx));
        for (section, found) in results {
            if found.count == 0 {
                continue;
            }
            content = content.child(
                v_flex()
                    .gap_3()
                    .child(
                        div()
                            .pb_1()
                            .border_b(tokens::BORDER_WIDTH)
                            .border_color(palette.border.hsla())
                            .child(
                                Label::new(section.title())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .children(found.elements),
            );
        }
        if shortcuts > 0 {
            let label = if shortcuts == 1 {
                "1 keyboard shortcut matches".to_string()
            } else {
                format!("{shortcuts} keyboard shortcuts match")
            };
            content = content.child(
                div()
                    .debug_selector(|| "settings-search-shortcuts".into())
                    .child(
                        Card::new().child(
                            h_flex()
                                .w_full()
                                .flex_wrap()
                                .gap_2()
                                .justify_between()
                                .child(Label::new(label).weight(FontWeight::MEDIUM))
                                .child(rows::button(
                                    "settings-search-show-shortcuts",
                                    "Show them",
                                    None,
                                    {
                                        let this = cx.entity().downgrade();
                                        let query = query.clone();
                                        move |window, cx| {
                                            this.update(cx, |this, cx| {
                                                this.open_section(
                                                    Section::KeyboardShortcuts,
                                                    Some(query.clone()),
                                                    window,
                                                    cx,
                                                )
                                            })
                                            .ok();
                                        }
                                    },
                                )),
                        ),
                    ),
            );
        }
        if !any {
            content = content.child(
                div()
                    .debug_selector(|| "settings-search-empty".into())
                    .child(rows::muted(format!(
                        "No settings match “{}”.",
                        query.trim()
                    ))),
            );
        }
        self.scrolling(content)
    }

    fn scrolling(&self, content: impl IntoElement) -> AnyElement {
        v_flex()
            .id("settings-scroll")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(
                div()
                    .w_full()
                    .max_w(scaled(px(CONTENT_MAX_WIDTH)))
                    .mx_auto()
                    .px_6()
                    .py_6()
                    .child(content),
            )
            .into_any_element()
    }
}

impl EventEmitter<ItemEvent> for SettingsView {}

impl Focusable for SettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for SettingsView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Settings".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(MustrIcon::Settings.icon())
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        let wide = window.viewport_size().width
            >= scaled(px(MIN_WIDTH_FOR_NAV)).to_pixels(window.rem_size());
        let searching = !self.query.is_empty();
        let query = self.query.clone();
        let (content, counts) = if searching {
            let mut results = Vec::new();
            for section in Section::ALL {
                if section != Section::KeyboardShortcuts {
                    results.push((section, self.groups(section, &query, window, cx)));
                }
            }
            let shortcuts = self.keys.read(cx).count_matches(&query, cx);
            let counts = Section::ALL
                .iter()
                .map(|section| match section {
                    Section::KeyboardShortcuts => shortcuts,
                    _ => results
                        .iter()
                        .find(|(found, _)| found == section)
                        .map_or(0, |(_, found)| found.count),
                })
                .collect::<Vec<_>>();
            (self.render_results(results, shortcuts, cx), Some(counts))
        } else {
            (self.render_section(window, cx), None)
        };
        let nav = self.render_nav(counts.as_deref(), cx);
        let search = self.render_search(cx);
        let body = if wide {
            h_flex()
                .size_full()
                .items_start()
                .child(
                    v_flex()
                        .debug_selector(|| "settings-nav".into())
                        .flex_none()
                        .w(scaled(px(NAV_WIDTH)))
                        .h_full()
                        .gap_3()
                        .px_2()
                        .py_4()
                        .border_r(tokens::BORDER_WIDTH)
                        .border_color(palette.border.hsla())
                        .bg(palette.surface.hsla())
                        .child(search)
                        .child(nav),
                )
                .child(div().flex_1().min_w_0().h_full().child(content))
        } else {
            v_flex()
                .size_full()
                .child(
                    v_flex()
                        .debug_selector(|| "settings-nav".into())
                        .flex_none()
                        .w_full()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .border_b(tokens::BORDER_WIDTH)
                        .border_color(palette.border.hsla())
                        .bg(palette.surface.hsla())
                        .child(search)
                        .child(self.render_section_chips(counts.as_deref(), cx)),
                )
                .child(div().flex_1().min_h_0().w_full().child(content))
        };
        div()
            .key_context("MustrSettings")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(palette.background.hsla())
            .child(body)
    }
}

impl SettingsView {
    /// The sections as a row of chips that wraps, for a narrow window.
    fn render_section_chips(&self, counts: Option<&[usize]>, cx: &mut Context<Self>) -> AnyElement {
        let palette = palette(cx);
        h_flex()
            .w_full()
            .flex_wrap()
            .gap_1()
            .children(Section::ALL.iter().enumerate().map(|(index, section)| {
                let section = *section;
                let active = counts.is_none() && self.section == section;
                let count = counts.map(|counts| counts[index]);
                let id = section.id();
                div()
                    .id(id)
                    .debug_selector(move || id.to_string())
                    .px_2()
                    .py_0p5()
                    .rounded(scaled(tokens::CONTROL_RADIUS))
                    .border(tokens::BORDER_WIDTH)
                    .border_color(palette.border.hsla())
                    .cursor_pointer()
                    .when(active, |this| this.bg(palette.surface_2.hsla()))
                    .when(count == Some(0), |this| this.opacity(0.5))
                    .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
                    .text_color(if active {
                        palette.text.hsla()
                    } else {
                        palette.text_muted.hsla()
                    })
                    .child(match count {
                        Some(count) if count > 0 => format!("{} {count}", section.title()),
                        _ => section.title().to_string(),
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_search("", window, cx);
                        this.set_section(section, window, cx);
                    }))
            }))
            .into_any_element()
    }
}
