//! Outside agents, as T3 has them (`docs/user/outside-agents.md`, Settings → Environments →
//! Copy MCP URL): Claude Code or Codex in your own terminal work with T3's projects and threads
//! through T3's MCP server, at the server's address followed by `/mcp`. Each signs in once on a
//! page the server serves (`/connect-agent`), which the agent's own login opens in the browser,
//! and you choose there what it may do. So mustr needs no page of its own: [`ConnectOutsideAgent`]
//! shows the MCP URL and T3's commands for each agent, to copy.

use crate::backend::ThreadBackend;
use gpui::{
    App, ClipboardItem, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, FontWeight,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Window, actions, div, px,
};
use mustr_ui::{palette, tokens, tokens::scaled};
use std::sync::Arc;
use ui::{
    Color, IconButton, IconName, IconSize, Label, LabelCommon as _, LabelSize, Tooltip, prelude::*,
};
use workspace::{ModalView, Workspace};

actions!(
    thread_chat,
    [
        /// Shows how Claude Code or Codex in your own terminal connect to T3's MCP server.
        ConnectOutsideAgent,
    ]
);

/// T3's MCP server at `server_url` (T3's "Copy MCP URL").
pub(crate) fn mcp_url(server_url: &str) -> String {
    format!("{}/mcp", server_url.trim_end_matches('/'))
}

/// T3's commands for an agent to add T3's MCP server and sign in to it, by agent.
pub(crate) fn connect_commands(mcp_url: &str) -> [(&'static str, String); 2] {
    [
        (
            "Claude Code",
            format!("claude mcp add --transport http t3 {mcp_url}\nclaude mcp login t3"),
        ),
        (
            "Codex",
            format!("codex mcp add t3 --url {mcp_url}\ncodex mcp login t3"),
        ),
    ]
}

pub(crate) struct OutsideAgentsModal {
    mcp_url: Option<String>,
    focus_handle: FocusHandle,
}

impl OutsideAgentsModal {
    pub(crate) fn new(backend: Option<Arc<dyn ThreadBackend>>, cx: &mut Context<Self>) -> Self {
        Self {
            mcp_url: backend
                .and_then(|backend| backend.server_url())
                .map(|url| mcp_url(&url)),
            focus_handle: cx.focus_handle(),
        }
    }

    #[cfg(test)]
    pub(crate) fn mcp_url(&self) -> Option<&str> {
        self.mcp_url.as_deref()
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for OutsideAgentsModal {}

impl ModalView for OutsideAgentsModal {}

impl Focusable for OutsideAgentsModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Text in the code font, with a copy button.
fn copyable(id: &'static str, text: String, cx: &App) -> AnyElement {
    let palette = palette(cx);
    h_flex()
        .w_full()
        .items_start()
        .gap_2()
        .px_2()
        .py_1()
        .rounded(scaled(tokens::CONTROL_RADIUS))
        .border(tokens::BORDER_WIDTH)
        .border_color(palette.border.hsla())
        .bg(palette.surface.hsla())
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .children(text.lines().map(|line| {
                    Label::new(SharedString::from(line.to_string()))
                        .size(LabelSize::Small)
                        .buffer_font(cx)
                })),
        )
        .child(
            IconButton::new(id, IconName::Copy)
                .icon_size(IconSize::XSmall)
                .icon_color(Color::Muted)
                .tooltip(Tooltip::text("Copy"))
                .on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(text.clone()))
                }),
        )
        .into_any_element()
}

impl Render for OutsideAgentsModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let note = |text: &'static str| Label::new(text).size(LabelSize::Small).color(Color::Muted);
        let body = match &self.mcp_url {
            Some(mcp_url) => {
                let commands = connect_commands(mcp_url);
                v_flex()
                    .gap_2()
                    .child(Label::new("T3's MCP server").size(LabelSize::Small))
                    .child(copyable("outside-agents-url", mcp_url.clone(), cx))
                    .children(
                        commands
                            .into_iter()
                            .enumerate()
                            .map(|(index, (agent, text))| {
                                v_flex()
                                    .gap_1()
                                    .child(Label::new(agent).size(LabelSize::Small))
                                    .child(copyable(
                                        if index == 0 {
                                            "outside-agents-claude"
                                        } else {
                                            "outside-agents-codex"
                                        },
                                        text,
                                        cx,
                                    ))
                            }),
                    )
                    .child(note(
                        "The login opens T3's sign-in page in your browser. Only approve a \
                         sign-in you just started, and pick what the agent may do there.",
                    ))
                    .child(note(
                        "Agents sign in only to localhost or over https. This is the address \
                         mustr reaches T3 at; mustr's own engine picks a new port each time it \
                         starts.",
                    ))
                    .into_any_element()
            }
            None => note("mustr isn't connected to a T3 server.").into_any_element(),
        };
        v_flex()
            .key_context("OutsideAgents")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .w(scaled(px(560.)))
            .elevation_2(cx)
            .p_3()
            .gap_3()
            .child(
                v_flex()
                    .gap_0p5()
                    .child(Label::new("Outside agents").weight(FontWeight::SEMIBOLD))
                    .child(div().child(note(
                        "Claude Code or Codex in your own terminal can read T3's projects \
                             and threads, and start and message threads, through T3's MCP \
                             server. Each signs in once.",
                    ))),
            )
            .child(body)
    }
}

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ConnectOutsideAgent, window, cx| {
            let backend = crate::backend(cx);
            workspace.toggle_modal(window, cx, move |_, cx| {
                OutsideAgentsModal::new(backend, cx)
            });
        });
    })
    .detach();
}
