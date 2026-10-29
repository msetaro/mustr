//! Saved T3 servers, and switching between them (`mustr-docs/PARITY.md` §2.1, "Several
//! environments at once"). mustr talks to one server at a time; T3's app keeps several connected
//! at once (`settings/ConnectionsSettings.tsx`), which mustr's single connection, shell, and
//! thread backend aren't built for.
//!
//! - **The servers:** this computer's T3 Code (the installed engine), mustr's own engine, and the
//!   servers the user saved by name and URL, kept in `servers.json` in mustr's config directory.
//! - **Switching:** the title bar's connection chip opens a menu of them; picking one connects to
//!   it (`T3Connection::switch_to`) and remembers it for the next launch, unless a flag, an
//!   environment variable, or `"t3": {"mode"}` in the settings picks the engine.
//! - **Credentials:** each server keeps its own token, stored under its URL (the keychain, or
//!   `MUSTR_CREDENTIALS_FILE`). A saved server is paired by pasting a credential on Home, as
//!   `--t3-server` is. Removing a server forgets its token.

#[cfg(test)]
mod tests;

use crate::{
    navigation,
    t3_connection::{self, T3Connection},
    t3_engine::{self, T3Mode},
};
use anyhow::{Context as _, Result, anyhow};
use editor::Editor;
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, Global, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, WeakEntity, Window, div, px, rems,
};
use http_client::Url;
use mustr_ui::{ButtonStyle, TextButton, palette, tokens, tokens::scaled};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use ui::{Color, ContextMenu, IconPosition, Label, LabelCommon as _, LabelSize, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

const SERVERS_FILE: &str = "servers.json";

gpui::actions!(
    mustr,
    [
        /// Opens the list of T3 servers mustr can connect to, to add, remove, or switch to one.
        ManageServers,
    ]
);

/// A T3 server the user saved, by name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedServer {
    pub name: String,
    pub url: String,
}

/// Which server mustr connects to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ServerTarget {
    /// This computer's T3 Code.
    Installed,
    /// mustr's own engine.
    Embedded,
    /// A T3 server at a URL.
    Url { url: String },
}

/// The saved servers and the one picked last, as `servers.json` holds them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerList {
    #[serde(default)]
    pub servers: Vec<SavedServer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<ServerTarget>,
}

impl ServerList {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("{} isn't a list of T3 servers", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
    }

    /// Saves a server, or renames the one already saved at that URL. Returns its URL as saved.
    pub fn add(&mut self, name: &str, url: &str) -> Result<String> {
        let url = parse_url(url)?;
        let url = url.as_str().to_string();
        let name = name.trim();
        let name = if name.is_empty() {
            default_name(&url)
        } else {
            name.to_string()
        };
        match self.servers.iter_mut().find(|server| server.url == url) {
            Some(server) => server.name = name,
            None => self.servers.push(SavedServer {
                name,
                url: url.clone(),
            }),
        }
        Ok(url)
    }

    /// Forgets the server at `url`. If it was the one picked last, the next launch goes back to
    /// the default engine.
    pub fn remove(&mut self, url: &str) -> bool {
        let before = self.servers.len();
        self.servers.retain(|server| server.url != url);
        if self.current
            == Some(ServerTarget::Url {
                url: url.to_string(),
            })
        {
            self.current = None;
        }
        self.servers.len() != before
    }

    fn name_of(&self, url: &str) -> Option<&str> {
        self.servers
            .iter()
            .find(|server| server.url == url)
            .map(|server| server.name.as_str())
    }
}

/// A server URL as the user typed it: `http://` is assumed without a scheme, as T3's "Add
/// environment" does for a bare host.
pub fn parse_url(input: &str) -> Result<Url> {
    let input = input.trim();
    if input.is_empty() {
        return Err(anyhow!(
            "Enter the server's address, such as http://devbox:3773."
        ));
    }
    let with_scheme = if input.contains("://") {
        input.to_string()
    } else {
        format!("http://{input}")
    };
    let url = t3_connection::parse_server_url(&with_scheme)?;
    if url.host_str().is_none_or(str::is_empty) {
        return Err(anyhow!("{input:?} has no host."));
    }
    Ok(url)
}

/// `devbox:3773`, for a server saved without a name.
pub(crate) fn default_name(url: &str) -> String {
    Url::parse(url)
        .ok()
        .and_then(|url| {
            let host = url.host_str()?.to_string();
            Some(match url.port() {
                Some(port) => format!("{host}:{port}"),
                None => host,
            })
        })
        .unwrap_or_else(|| url.to_string())
}

fn servers_path() -> PathBuf {
    paths::config_dir().join(SERVERS_FILE)
}

/// The server picked last, for this launch, when nothing else picks the engine: no
/// `--t3-server` or `MUSTR_T3_SERVER`, no `--t3-mode` or `MUSTR_T3_MODE`, and no `"t3": {"mode"}`
/// in the settings.
pub fn remembered_target(
    cli_mode: Option<&str>,
    server_url_given: bool,
    settings_text: Option<&str>,
) -> Option<(T3Mode, Result<Url>)> {
    if server_url_given || t3_engine::mode_configured(cli_mode, settings_text) {
        return None;
    }
    let list = ServerList::load(&servers_path()).log_err()?;
    target_for(list.current?)
}

fn target_for(target: ServerTarget) -> Option<(T3Mode, Result<Url>)> {
    Some(match target {
        ServerTarget::Installed => return None,
        ServerTarget::Embedded => (
            T3Mode::Embedded,
            Err(anyhow!("mustr's own engine has no server URL")),
        ),
        ServerTarget::Url { url } => (T3Mode::External, t3_connection::parse_server_url(&url)),
    })
}

// The app's list.

/// The saved servers, shared by every window's menu and dialog.
pub struct SavedServers {
    path: PathBuf,
    list: ServerList,
}

struct GlobalSavedServers(Entity<SavedServers>);

impl Global for GlobalSavedServers {}

impl SavedServers {
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(global) = cx.try_global::<GlobalSavedServers>() {
            return global.0.clone();
        }
        let path = servers_path();
        let list = ServerList::load(&path).log_err().unwrap_or_default();
        let servers = cx.new(|_| Self { path, list });
        cx.set_global(GlobalSavedServers(servers.clone()));
        servers
    }

    #[cfg(test)]
    pub(crate) fn set_global_for_test(path: PathBuf, cx: &mut App) -> Entity<Self> {
        let list = ServerList::load(&path).unwrap_or_default();
        let servers = cx.new(|_| Self { path, list });
        cx.set_global(GlobalSavedServers(servers.clone()));
        servers
    }

    /// The servers the user saved, by name.
    pub fn servers(&self) -> &[SavedServer] {
        &self.list.servers
    }

    fn update_list(&mut self, change: impl FnOnce(&mut ServerList), cx: &mut Context<Self>) {
        change(&mut self.list);
        if let Err(error) = self.list.save(&self.path) {
            log::error!("couldn't save mustr's T3 servers: {error:#}");
        }
        cx.notify();
    }
}

/// Connects to `target` and remembers it for the next launch.
pub fn switch_server(target: ServerTarget, cx: &mut App) {
    let servers = SavedServers::global(cx);
    servers.update(cx, |servers, cx| {
        servers.update_list(|list| list.current = Some(target.clone()), cx)
    });
    T3Connection::global(cx).update(cx, |connection, cx| connection.switch_to(target, cx));
}

/// Saves a server and connects to it. Errors say what's wrong with the address.
pub fn add_server(name: &str, url: &str, cx: &mut App) -> Result<String> {
    let servers = SavedServers::global(cx);
    let mut list = servers.read(cx).list.clone();
    let url = list.add(name, url)?;
    servers.update(cx, |servers, cx| {
        servers.update_list(|saved| *saved = list, cx)
    });
    switch_server(ServerTarget::Url { url: url.clone() }, cx);
    Ok(url)
}

/// Forgets a saved server and its token. The server in use can't be removed.
pub fn remove_server(url: &str, cx: &mut App) {
    let connection = T3Connection::global(cx);
    if connection.read(cx).target()
        == (ServerTarget::Url {
            url: url.to_string(),
        })
    {
        return;
    }
    let servers = SavedServers::global(cx);
    servers.update(cx, |servers, cx| {
        servers.update_list(
            |list| {
                list.remove(url);
            },
            cx,
        )
    });
    if let Ok(url) = Url::parse(url) {
        connection.update(cx, |connection, cx| connection.forget_token(url, cx));
    }
}

/// The name the chip and Home give the server in use: its saved name, or `None` for the
/// built-in engines and unsaved URLs.
pub fn saved_name(cx: &App) -> Option<String> {
    let ServerTarget::Url { url } = T3Connection::try_global(cx)?.read(cx).target() else {
        return None;
    };
    cx.try_global::<GlobalSavedServers>()?
        .0
        .read(cx)
        .list
        .name_of(&url)
        .map(str::to_string)
}

/// Handles [`ManageServers`] in every workspace.
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ManageServers, window, cx| {
            open_dialog(workspace, window, cx)
        });
    })
    .detach();
}

pub fn open_dialog(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let handle = workspace.weak_handle();
    workspace.toggle_modal(window, cx, |window, cx| {
        ServersDialog::new(handle, window, cx)
    });
}

/// The connection chip's menu: the servers, with the one in use checked, then managing them and
/// the connection's details on Home.
pub fn server_menu(
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    let current = T3Connection::global(cx).read(cx).target();
    let saved = SavedServers::global(cx).read(cx).list.servers.clone();
    let current_saved = saved.iter().any(|server| {
        current
            == ServerTarget::Url {
                url: server.url.clone(),
            }
    });
    ContextMenu::build(window, cx, move |mut menu, _, _| {
        menu = menu.header("Servers");
        let entry = |menu: ContextMenu, label: String, target: ServerTarget| {
            let checked = current == target;
            menu.toggleable_entry(label, checked, IconPosition::End, None, move |_, cx| {
                if !checked {
                    switch_server(target.clone(), cx);
                }
            })
        };
        menu = entry(
            menu,
            "This computer's T3 Code".into(),
            ServerTarget::Installed,
        );
        menu = entry(menu, "mustr's own engine".into(), ServerTarget::Embedded);
        for server in saved {
            menu = entry(
                menu,
                format!("{} · {}", server.name, default_name(&server.url)),
                ServerTarget::Url { url: server.url },
            );
        }
        // An unsaved server from `--t3-server` or the settings.
        if let ServerTarget::Url { url } = &current
            && !current_saved
        {
            menu =
                menu.toggleable_entry(default_name(url), true, IconPosition::End, None, |_, _| {});
        }
        let manage = workspace.clone();
        let home = workspace.clone();
        menu.separator()
            .entry("Add or remove servers…", None, move |window, cx| {
                manage
                    .update(cx, |workspace, cx| open_dialog(workspace, window, cx))
                    .ok();
            })
            .entry("Connection details", None, move |window, cx| {
                home.update(cx, |workspace, cx| {
                    navigation::show_home(workspace, window, cx)
                })
                .ok();
            })
    })
}

// The dialog.

/// Lists the servers to switch to or remove, and adds one by name and URL.
pub struct ServersDialog {
    workspace: WeakEntity<Workspace>,
    name: Entity<Editor>,
    url: Entity<Editor>,
    error: Option<SharedString>,
    _servers: gpui::Subscription,
}

impl ServersDialog {
    fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Devbox (optional; the host by default)", window, cx);
            editor
        });
        let url = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("http://devbox:3773 or devbox:3773", window, cx);
            editor
        });
        let servers = SavedServers::global(cx);
        let subscription = cx.observe(&servers, |_, _, cx| cx.notify());
        Self {
            workspace,
            name,
            url,
            error: None,
            _servers: subscription,
        }
    }

    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name.read(cx).text(cx);
        let url = self.url.read(cx).text(cx);
        match add_server(&name, &url, cx) {
            Ok(_) => {
                cx.emit(DismissEvent);
                // Home shows the connection, and the pairing field a new server needs.
                self.workspace
                    .update(cx, |workspace, cx| {
                        navigation::show_home(workspace, window, cx)
                    })
                    .ok();
            }
            Err(error) => {
                self.error = Some(format!("{error:#}").into());
                cx.notify();
            }
        }
    }

    fn switch(&mut self, target: ServerTarget, window: &mut Window, cx: &mut Context<Self>) {
        switch_server(target, cx);
        cx.emit(DismissEvent);
        self.workspace
            .update(cx, |workspace, cx| {
                navigation::show_home(workspace, window, cx)
            })
            .ok();
    }

    fn render_row(
        &self,
        index: usize,
        name: String,
        detail: String,
        target: ServerTarget,
        current: &ServerTarget,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let palette = palette(cx);
        let in_use = *current == target;
        let removable = match (&target, in_use) {
            (ServerTarget::Url { url }, false) => Some(url.clone()),
            _ => None,
        };
        let selector = format!("server-row-{index}");
        h_flex()
            .debug_selector(move || selector)
            .w_full()
            .gap_2()
            .py(scaled(px(6.)))
            .when(index > 0, |this| {
                this.border_t(tokens::BORDER_WIDTH)
                    .border_color(palette.border.hsla())
            })
            .child(
                div()
                    .flex_none()
                    .size(scaled(px(7.)))
                    .rounded_full()
                    .bg(if in_use {
                        palette.ok.hsla()
                    } else {
                        palette.border.hsla()
                    }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(name).truncate())
                    .child(
                        Label::new(detail)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
            .when(in_use, |this| {
                this.child(
                    Label::new("In use")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .when(!in_use, |this| {
                let target = target.clone();
                this.child(
                    div()
                        .debug_selector(move || format!("server-connect-{index}"))
                        .child(
                            TextButton::new(("server-connect", index), "Connect").on_click(
                                cx.listener(move |this, _, window, cx| {
                                    this.switch(target.clone(), window, cx)
                                }),
                            ),
                        ),
                )
            })
            .when_some(removable, |this, url| {
                this.child(
                    div()
                        .debug_selector(move || format!("server-remove-{index}"))
                        .child(
                            TextButton::new(("server-remove", index), "Remove")
                                .on_click(cx.listener(move |_, _, _, cx| remove_server(&url, cx))),
                        ),
                )
            })
    }
}

impl EventEmitter<DismissEvent> for ServersDialog {}

impl ModalView for ServersDialog {}

impl Focusable for ServersDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.url.focus_handle(cx)
    }
}

impl Render for ServersDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        let current = T3Connection::global(cx).read(cx).target();
        let saved = SavedServers::global(cx).read(cx).list.servers.clone();
        let mut rows = vec![
            (
                "This computer's T3 Code".to_string(),
                "The T3 Code installed here, with its projects and threads".to_string(),
                ServerTarget::Installed,
            ),
            (
                "mustr's own engine".to_string(),
                "T3 Code's engine, run by mustr in its own data folder".to_string(),
                ServerTarget::Embedded,
            ),
        ];
        rows.extend(saved.into_iter().map(|server| {
            let url = server.url.trim_end_matches('/').to_string();
            (server.name, url, ServerTarget::Url { url: server.url })
        }));
        if let ServerTarget::Url { url } = &current
            && !rows.iter().any(|(_, _, target)| *target == current)
        {
            rows.push((
                default_name(url),
                format!(
                    "{} · from the command line or settings",
                    url.trim_end_matches('/')
                ),
                current.clone(),
            ));
        }
        let field = |editor: &Entity<Editor>| {
            div()
                .flex_1()
                .min_w_0()
                .px_2()
                .py_1()
                .rounded(scaled(tokens::CONTROL_RADIUS))
                .border(tokens::BORDER_WIDTH)
                .border_color(palette.border.hsla())
                .bg(palette.background.hsla())
                .child(editor.clone())
        };
        let rows = rows
            .into_iter()
            .enumerate()
            .map(|(index, (name, detail, target))| {
                self.render_row(index, name, detail, target, &current, cx)
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        v_flex()
            .key_context("MustrServers")
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.add(window, cx)))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .w(tokens::modal_width(rems(34.), window))
            .p_4()
            .gap_3()
            .rounded(scaled(tokens::CORNER_RADIUS))
            .border(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .bg(palette.surface.hsla())
            .shadow_lg()
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        Label::new("T3 servers")
                            .size(LabelSize::Large)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .child(
                        Label::new(
                            "mustr connects to one at a time, and keeps each one's sign-in.",
                        )
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
            )
            .child(v_flex().w_full().children(rows))
            .child(
                v_flex()
                    .gap_1p5()
                    .pt_2()
                    .border_t(tokens::BORDER_WIDTH)
                    .border_color(palette.border.hsla())
                    .child(Label::new("Add a server").weight(FontWeight::MEDIUM))
                    .child(
                        Label::new(
                            "A T3 Code server on another machine, such as one reached over \
                             Tailscale or SSH forwarding. Pair it on Home with a credential from \
                             t3 auth pairing create on that machine.",
                        )
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .child(labeled("Address", field(&self.url)))
                    .child(labeled("Name", field(&self.name))),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .debug_selector(|| "servers-error".into())
                        .child(Label::new(error).size(LabelSize::Small).color(Color::Error)),
                )
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        TextButton::new("servers-close", "Close")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        div().debug_selector(|| "servers-add".into()).child(
                            TextButton::new("servers-add", "Add and connect")
                                .style(ButtonStyle::Primary)
                                .on_click(cx.listener(|this, _, window, cx| this.add(window, cx))),
                        ),
                    ),
            )
    }
}

/// A field with its label beside it, or above it when the dialog is narrow.
fn labeled(label: &'static str, field: impl IntoElement) -> impl IntoElement {
    h_flex()
        .w_full()
        .flex_wrap()
        .gap_x_2()
        .gap_y_1()
        .child(
            div()
                .w(scaled(px(64.)))
                .flex_none()
                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
        )
        .child(div().flex_1().min_w(scaled(px(200.))).flex().child(field))
}
