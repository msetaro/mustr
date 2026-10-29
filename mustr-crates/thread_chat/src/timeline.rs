//! A thread's timeline: its rows, kept in step with the thread's state, with runs of work
//! grouped and collapsed, tool calls whose output loads on expand, and the Markdown messages,
//! reasoning, and plans show.

use crate::{
    attachments::{AssetKey, AssetState},
    backend::{Access, ThreadBackendExt as _},
    composer::LiveOutput,
    model::{
        ItemKind, ItemStatus, MessageFile, MessageImage, Row, TimelineItem, ToolKind,
        canonical_item,
    },
    thread_view::{CONTENT_MAX_WIDTH, Loadable, ThreadView},
};
use futures::StreamExt as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, FontWeight, Hsla, IntoElement, ObjectFit,
    ParentElement as _, Rems, SharedString, Styled as _, StyledImage as _, TextStyleRefinement,
    Window, div, img, px, relative,
};
use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use mustr_ui::{
    ButtonStyle as MustrButtonStyle, Card, TextButton, palette, tokens, tokens::scaled,
};
use t3_api::{
    rpc,
    types::{OrchestrationV2GetTurnItemInput, OrchestrationV2SubscribeCommandOutputInput},
};
use ui::{
    Color, Icon, IconName, IconSize, Indicator, Label, LabelCommon as _, LabelSize, prelude::*,
};

/// A message's thumbnails, two to a row as in T3's timeline (`grid-cols-2`, `aspect-[4/3]`).
const MESSAGE_IMAGE_WIDTH: f32 = 101.;
const MESSAGE_IMAGE_HEIGHT: f32 = 76.;
/// A tool's images, such as screenshots, in its expanded output.
const TOOL_IMAGE_WIDTH: f32 = 320.;
const TOOL_IMAGE_HEIGHT: f32 = 200.;

/// What a secret request's card shows.
struct SecretRequestView {
    title: Option<String>,
    label: String,
    reason: String,
    placeholder: Option<String>,
    status: String,
}

/// An image of the timeline, and how to show it.
struct Thumbnail {
    key: AssetKey,
    name: SharedString,
    selector: String,
    size: (Rems, Rems),
    fit: ObjectFit,
}

/// Tool calls shown as chips on a collapsed run of work.
const COLLAPSED_CHIPS: usize = 6;
const CHIP_MAX_CHARS: usize = 48;

impl ThreadView {
    /// Brings the list's rows in line with the thread, re-measuring only from the first row
    /// that changed. Streaming changes the last rows, so this stays cheap.
    pub(crate) fn sync_rows(&mut self, cx: &mut Context<Self>) {
        let Loadable::Loaded(state) = &self.state else {
            return;
        };
        let rows = state.rows();
        let revisions = rows
            .iter()
            .map(|row| state.row_revision(row))
            .collect::<Vec<_>>();
        let unchanged = rows
            .iter()
            .zip(&revisions)
            .zip(self.rows.iter().zip(&self.row_revisions))
            .take_while(|((row, revision), (old_row, old_revision))| {
                row == old_row && revision == old_revision
            })
            .count();
        // Only rows already shown have Markdown to update; the rest get theirs when they're first
        // rendered, so opening a long thread parses only what's on screen.
        let mut texts = Vec::new();
        for row in &rows[unchanged..] {
            let indices = match row {
                Row::Item(index) => std::slice::from_ref(index),
                Row::Work(indices) => indices.as_slice(),
            };
            texts.extend(
                indices
                    .iter()
                    .filter_map(|index| state.item(*index))
                    .filter(|item| self.markdown.contains_key(&item.id))
                    .filter_map(markdown_source),
            );
        }
        self.list_state
            .splice(unchanged..self.rows.len(), rows.len() - unchanged);
        self.rows = rows;
        self.row_revisions = revisions;
        for (id, text) in texts {
            self.update_markdown(id, text, cx);
        }
        // Where messages can be rewound to, and whether a rewind in flight has happened.
        self.sync_history();
    }

    fn update_markdown(&mut self, id: String, text: String, cx: &mut Context<Self>) {
        if let Some(markdown) = self.markdown.get(&id) {
            markdown.update(cx, |markdown, cx| {
                if markdown.source().as_ref() != text {
                    markdown.replace(text, cx);
                }
            });
            return;
        }
        let language_registry = self.language_registry.clone();
        let markdown = cx.new(|cx| Markdown::new(text.into(), language_registry, None, cx));
        self._subscriptions
            .push(cx.observe(&markdown, |_, _, cx| cx.notify()));
        self.markdown.insert(id, markdown);
    }

    /// Creates the Markdown of the items a row shows, if they don't have it yet.
    fn ensure_markdown(&mut self, row: &Row, cx: &mut Context<Self>) {
        let Loadable::Loaded(state) = &self.state else {
            return;
        };
        let indices = match row {
            Row::Item(index) => std::slice::from_ref(index),
            Row::Work(indices) => indices.as_slice(),
        };
        let missing = indices
            .iter()
            .filter_map(|index| state.item(*index))
            .filter(|item| !self.markdown.contains_key(&item.id))
            .filter_map(markdown_source)
            .collect::<Vec<_>>();
        for (id, text) in missing {
            self.update_markdown(id, text, cx);
        }
    }

    fn toggle_expanded(&mut self, key: String, cx: &mut Context<Self>) {
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
        // The row's height changes; rows keep their logical scroll position.
        self.list_state.remeasure();
        cx.notify();
    }

    /// The row that shows an item.
    fn row_of_item(&self, item_id: &str) -> Option<usize> {
        let state = self.thread_state()?;
        let index = state.items().iter().position(|item| item.id == item_id)?;
        self.rows.iter().position(|row| match row {
            Row::Item(item) => *item == index,
            Row::Work(items) => items.contains(&index),
        })
    }

    /// Follows a running command's output while it's expanded
    /// (`orchestration.subscribeCommandOutput`, as T3's `CommandOutputPanel` does), then loads
    /// the whole output once it ends.
    fn follow_command_output(&mut self, item_id: String, cx: &mut Context<Self>) {
        let Some(thread_id) = self.thread_id.clone() else {
            return;
        };
        let stream = self
            .backend
            .stream::<rpc::OrchestrationSubscribeCommandOutput>(
                OrchestrationV2SubscribeCommandOutputInput {
                    item_id: item_id.clone(),
                    thread_id,
                },
                Access::Read,
            );
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                log::warn!("couldn't follow the command's output: {error:#}");
                return;
            }
        };
        self.composer_state
            .live_outputs
            .insert(item_id.clone(), LiveOutput::default());
        let key = item_id.clone();
        let task = cx.spawn(async move |this, cx| {
            while let Some(frame) = stream.next().await {
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(error) => {
                        log::warn!("the command's output stream failed: {error:#}");
                        break;
                    }
                };
                let running = frame.running;
                let updated = this.update(cx, |this, cx| {
                    if let Some(output) = this.composer_state.live_outputs.get_mut(&item_id) {
                        output.apply(&frame);
                    }
                    if let Some(row) = this.row_of_item(&item_id) {
                        this.list_state.remeasure_items(row..row + 1);
                    }
                    cx.notify();
                });
                if updated.is_err() || !running {
                    break;
                }
            }
            this.update(cx, |this, cx| {
                this.composer_state.output_streams.remove(&item_id);
                if let Some(output) = this.composer_state.live_outputs.get_mut(&item_id) {
                    output.running = false;
                }
                cx.notify();
            })
            .ok();
        });
        self.composer_state.output_streams.insert(key, task);
    }

    /// Expands a tool call, fetching its output if T3 left it out, or following it while the
    /// command runs.
    pub(crate) fn toggle_tool(&mut self, item_id: String, cx: &mut Context<Self>) {
        let running_command = matches!(
            self.thread_state()
                .and_then(|state| state.items().iter().find(|item| item.id == item_id)),
            Some(TimelineItem {
                kind: ItemKind::Tool {
                    tool: ToolKind::Command,
                    ..
                },
                status,
                ..
            }) if status.is_in_progress()
        );
        if running_command {
            let expanding = !self.expanded.contains(&item_id);
            self.toggle_expanded(item_id.clone(), cx);
            if expanding {
                self.follow_command_output(item_id, cx);
            } else {
                self.composer_state.output_streams.remove(&item_id);
            }
            return;
        }
        let needs_output = matches!(
            self.thread_state()
                .and_then(|state| state.items().iter().find(|item| item.id == item_id))
                .map(|item| &item.kind),
            Some(ItemKind::Tool {
                output_omitted: true,
                ..
            })
        );
        let expanding = !self.expanded.contains(&item_id);
        self.toggle_expanded(item_id.clone(), cx);
        if !(expanding && needs_output) || !self.loading_outputs.insert(item_id.clone()) {
            return;
        }
        let Some(thread_id) = self.thread_id.clone() else {
            return;
        };
        let request = self.backend.turn_item(OrchestrationV2GetTurnItemInput {
            item_id: item_id.clone(),
            revision: None,
            thread_id,
        });
        self._tasks.push(cx.spawn(async move |this, cx| {
            let result = request.await;
            this.update(cx, |this, cx| {
                this.loading_outputs.remove(&item_id);
                let (output, images) = match result {
                    Ok(result) => {
                        let fetched = result
                            .item
                            .as_ref()
                            .and_then(canonical_item)
                            .and_then(|item| TimelineItem::from_turn_item(&item))
                            .and_then(|item| match item.kind {
                                ItemKind::Tool { output, images, .. } => Some((output, images)),
                                _ => None,
                            });
                        let (output, images) = fetched.unwrap_or_default();
                        (output.or_else(|| Some("No output.".to_string())), images)
                    }
                    Err(error) => (Some(format!("Couldn't load the output: {error:#}")), 0),
                };
                if let Loadable::Loaded(state) = &mut this.state
                    && state.set_output(&item_id, output, images).changed
                {
                    this.sync_rows(cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }
}

/// The Markdown an item shows, keyed by item id.
fn markdown_source(item: &TimelineItem) -> Option<(String, String)> {
    let text = match &item.kind {
        ItemKind::UserMessage { text, .. }
        | ItemKind::AssistantMessage { text, .. }
        | ItemKind::Reasoning { text } => text.clone(),
        ItemKind::Plan { markdown, .. } => markdown.clone(),
        _ => return None,
    };
    Some((item.id.clone(), text))
}

fn markdown_style(window: &Window, cx: &App) -> MarkdownStyle {
    let mut style = editor::hover_markdown_style(window, cx);
    style.base_text_style.refine(&TextStyleRefinement {
        font_size: Some(tokens::text_size(tokens::TEXT_SIZE_LARGE).into()),
        line_height: Some(relative(1.5)),
        ..Default::default()
    });
    style
}

fn muted_markdown_style(window: &Window, cx: &App) -> MarkdownStyle {
    let mut style = markdown_style(window, cx);
    style.base_text_style.refine(&TextStyleRefinement {
        color: Some(palette(cx).text_muted.hsla()),
        font_size: Some(tokens::text_size(tokens::TEXT_SIZE_MEDIUM).into()),
        ..Default::default()
    });
    style
}

/// The files a user message sent besides images, as chips with their names and sizes.
fn render_message_files(files: &[MessageFile], cx: &App) -> AnyElement {
    let palette = palette(cx);
    h_flex()
        .flex_wrap()
        .gap_1()
        .children(files.iter().map(|file| {
            let size = crate::attachments::format_size(file.size_bytes.max(0) as usize);
            let detail = if file.pasted_text {
                format!("{size} · pasted text")
            } else {
                size
            };
            h_flex()
                .gap_1()
                .px_1p5()
                .py_0p5()
                .rounded(scaled(tokens::CONTROL_RADIUS))
                .border(tokens::BORDER_WIDTH)
                .border_color(palette.border.hsla())
                .bg(palette.surface.hsla())
                .child(
                    Icon::new(if file.pasted_text {
                        IconName::TextSnippet
                    } else {
                        IconName::Paperclip
                    })
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
                )
                .child(Label::new(file.name.clone()).size(LabelSize::Small))
                .child(
                    Label::new(detail)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
        }))
        .into_any_element()
}

pub(crate) fn truncate(text: &str, max_chars: usize) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    match line.char_indices().nth(max_chars) {
        Some((index, _)) => format!("{}…", &line[..index]),
        None if text.lines().nth(1).is_some() => format!("{line}…"),
        None => line.to_string(),
    }
}

fn status_color(status: ItemStatus, cx: &App) -> Hsla {
    let palette = palette(cx);
    match status {
        ItemStatus::Failed => cx.theme().status().error,
        ItemStatus::Cancelled | ItemStatus::Interrupted => palette.warning.hsla(),
        ItemStatus::Pending | ItemStatus::Running | ItemStatus::Waiting => palette.accent.hsla(),
        ItemStatus::Completed => palette.ok.hsla(),
    }
}

fn tool_icon(tool: ToolKind) -> IconName {
    match tool {
        ToolKind::Command => IconName::ToolTerminal,
        ToolKind::FileSearch => IconName::ToolSearch,
        ToolKind::WebSearch => IconName::ToolWeb,
        ToolKind::Tool => IconName::ToolHammer,
        ToolKind::Subagent => IconName::Thread,
    }
}

/// "3 commands · 2 files edited (+12 −3) · thought", for a collapsed run of work.
fn work_summary(items: &[&TimelineItem]) -> String {
    let mut commands = 0;
    let mut searches = 0;
    let mut tools = 0;
    let mut files = 0;
    let mut additions = 0;
    let mut deletions = 0;
    let mut reasoning = false;
    for item in items {
        match &item.kind {
            ItemKind::Tool {
                tool: ToolKind::Command,
                ..
            } => commands += 1,
            ItemKind::Tool {
                tool: ToolKind::FileSearch | ToolKind::WebSearch,
                ..
            } => searches += 1,
            ItemKind::Tool { .. } => tools += 1,
            ItemKind::FileChange {
                additions: added,
                deletions: deleted,
                changes,
                ..
            } => {
                files += changes.len().max(1);
                additions += added.unwrap_or_default();
                deletions += deleted.unwrap_or_default();
            }
            ItemKind::Reasoning { .. } => reasoning = true,
            _ => {}
        }
    }
    let plural = |count: usize, one: &str, many: &str| {
        format!("{count} {}", if count == 1 { one } else { many })
    };
    let mut parts = Vec::new();
    if commands > 0 {
        parts.push(format!("Ran {}", plural(commands, "command", "commands")));
    }
    if files > 0 {
        let mut edited = format!("edited {}", plural(files, "file", "files"));
        if additions > 0 || deletions > 0 {
            edited.push_str(&format!(" (+{additions} −{deletions})"));
        }
        parts.push(edited);
    }
    if searches > 0 {
        parts.push(plural(searches, "search", "searches"));
    }
    if tools > 0 {
        parts.push(plural(tools, "tool call", "tool calls"));
    }
    if reasoning && parts.is_empty() {
        parts.push("Thought".to_string());
    }
    let summary = parts.join(" · ");
    let mut chars = summary.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => summary,
    }
}

/// One line for a work item: the command, the file, the search.
fn work_label(item: &TimelineItem) -> String {
    match &item.kind {
        ItemKind::Tool { summary, .. } => summary.clone(),
        ItemKind::FileChange {
            path,
            additions,
            deletions,
            changes,
            ..
        } => {
            let mut label = if changes.len() > 1 {
                format!("{path} and {} more", changes.len() - 1)
            } else {
                path.clone()
            };
            if additions.is_some() || deletions.is_some() {
                label.push_str(&format!(
                    "  +{} −{}",
                    additions.unwrap_or_default(),
                    deletions.unwrap_or_default()
                ));
            }
            label
        }
        ItemKind::Reasoning { .. } => "Thinking".to_string(),
        _ => String::new(),
    }
}

/// A small round avatar with a letter, as in the mockups' agent cards.
fn avatar(letter: &str, background: Hsla) -> impl IntoElement {
    div()
        .flex_none()
        .size(scaled(px(18.)))
        .mt(scaled(px(2.)))
        .rounded_full()
        .bg(background)
        .flex()
        .items_center()
        .justify_center()
        .text_color(gpui::white())
        .text_size(tokens::text_size(px(9.5)))
        .font_weight(FontWeight::BOLD)
        .child(letter.to_string())
}

impl ThreadView {
    fn agent_letter(&self) -> String {
        self.thread_state()
            .map(|state| state.thread.provider_instance_id.clone())
            .and_then(|provider| provider.chars().next())
            .map_or_else(
                || "A".to_string(),
                |letter| letter.to_uppercase().to_string(),
            )
    }

    pub(crate) fn render_row(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(row) = self.rows.get(index).cloned() else {
            return div().into_any_element();
        };
        self.ensure_markdown(&row, cx);
        let Loadable::Loaded(state) = &self.state else {
            return div().into_any_element();
        };
        let selector = match &row {
            Row::Item(index) => format!(
                "thread-row-{}",
                state.item(*index).map_or("", |item| item.id.as_str())
            ),
            Row::Work(indices) => format!(
                "thread-work-{}",
                indices
                    .first()
                    .and_then(|index| state.item(*index))
                    .map_or("", |item| item.id.as_str())
            ),
        };
        let content = match row {
            Row::Item(item_index) => match state.item(item_index).cloned() {
                Some(item) => self.render_item(&item, window, cx),
                None => div().into_any_element(),
            },
            Row::Work(indices) => {
                let items = indices
                    .iter()
                    .filter_map(|index| state.item(*index).cloned())
                    .collect::<Vec<_>>();
                self.render_work(items, window, cx)
            }
        };
        h_flex()
            .w_full()
            .justify_center()
            .px_4()
            .child(
                div()
                    .w_full()
                    .max_w(scaled(px(CONTENT_MAX_WIDTH)))
                    .py(scaled(px(6.)))
                    .debug_selector(|| selector)
                    .child(content),
            )
            .into_any_element()
    }

    fn render_markdown(&self, id: &str, muted: bool, window: &Window, cx: &App) -> AnyElement {
        match self.markdown.get(id) {
            Some(markdown) => MarkdownElement::new(
                markdown.clone(),
                if muted {
                    muted_markdown_style(window, cx)
                } else {
                    markdown_style(window, cx)
                },
            )
            .into_any_element(),
            None => div().into_any_element(),
        }
    }

    fn render_item(
        &mut self,
        item: &TimelineItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = palette(cx);
        match &item.kind {
            ItemKind::UserMessage {
                text,
                images,
                files,
                ..
            } => {
                let thumbnails =
                    (!images.is_empty()).then(|| self.render_message_images(&item.id, images, cx));
                let files = (!files.is_empty()).then(|| render_message_files(files, cx));
                let edit = self.render_edit_from_here(item, cx);
                let copy = self.render_copy_message(item, text, cx);
                h_flex()
                    .group(SharedString::from(format!("message-{}", item.id)))
                    .items_start()
                    .gap_2()
                    .child(avatar("Y", gpui::rgb(0x6e7781).into()))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1p5()
                            .px(scaled(px(10.)))
                            .py(scaled(px(6.)))
                            .rounded(scaled(tokens::CORNER_RADIUS))
                            .bg(palette.surface_2.hsla())
                            .when(!text.trim().is_empty(), |this| {
                                this.child(self.render_markdown(&item.id, false, window, cx))
                            })
                            .children(thumbnails)
                            .children(files),
                    )
                    .child(v_flex().flex_none().children(copy).children(edit))
                    .into_any_element()
            }
            ItemKind::AssistantMessage { streaming, text } => {
                let copy = (!*streaming)
                    .then(|| self.render_copy_message(item, text, cx))
                    .flatten();
                let fork = self.render_fork(item, cx);
                h_flex()
                    .group(SharedString::from(format!("message-{}", item.id)))
                    .items_start()
                    .gap_2()
                    .child(avatar(&self.agent_letter(), palette.agent.hsla()))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(self.render_markdown(&item.id, false, window, cx))
                            .when(*streaming, |this| {
                                this.child(
                                    div()
                                        .w(scaled(px(6.)))
                                        .h(scaled(px(12.)))
                                        .bg(palette.text.hsla())
                                        .opacity(0.7),
                                )
                            }),
                    )
                    .child(v_flex().flex_none().children(copy).children(fork))
                    .into_any_element()
            }
            ItemKind::Plan {
                title, markdown, ..
            } => {
                let actions = self.render_plan_actions(item, cx);
                let copied = self.composer_state.copied.as_deref() == Some(item.id.as_str());
                let copy_text = format!("{}\n", markdown.trim_end());
                let item_id = item.id.clone();
                Card::new()
                    .gap_1()
                    .border_color(palette.accent.hsla().opacity(0.45))
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(
                                Icon::new(IconName::ListTodo)
                                    .size(IconSize::Small)
                                    .color(Color::Accent),
                            )
                            .child(
                                Label::new(title.clone().unwrap_or_else(|| "Plan".to_string()))
                                    .size(LabelSize::Small)
                                    .weight(FontWeight::SEMIBOLD),
                            )
                            .child(div().flex_1())
                            .child(
                                div()
                                    .debug_selector(|| format!("thread-plan-copy-{item_id}"))
                                    .child(
                                        TextButton::new(
                                            SharedString::from(format!("copy-plan-{}", item.id)),
                                            if copied { "Copied" } else { "Copy" },
                                        )
                                        .tooltip("Copy the plan as Markdown")
                                        .on_click(
                                            cx.listener({
                                                let item_id = item.id.clone();
                                                move |this, _, _, cx| {
                                                    this.copy_text(
                                                        item_id.clone(),
                                                        copy_text.clone(),
                                                        cx,
                                                    )
                                                }
                                            }),
                                        ),
                                    ),
                            ),
                    )
                    .child(self.render_markdown(&item.id, false, window, cx))
                    .children(actions)
                    .into_any_element()
            }
            ItemKind::Todo { explanation, steps } => {
                let done = steps
                    .iter()
                    .filter(|step| step.status == "completed")
                    .count();
                Card::new()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(
                                Icon::new(IconName::ListTodo)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(format!("Tasks {done}/{}", steps.len()))
                                    .size(LabelSize::Small)
                                    .weight(FontWeight::SEMIBOLD),
                            ),
                    )
                    .when_some(explanation.clone(), |this, explanation| {
                        this.child(
                            Label::new(explanation)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .children(steps.iter().map(|step| {
                        let (icon, color) = match step.status.as_str() {
                            "completed" => (IconName::TodoComplete, Color::Success),
                            "running" => (IconName::TodoProgress, Color::Accent),
                            _ => (IconName::TodoPending, Color::Muted),
                        };
                        h_flex()
                            .gap_1p5()
                            .items_start()
                            .child(Icon::new(icon).size(IconSize::Small).color(color))
                            .child(
                                Label::new(step.text.clone())
                                    .size(LabelSize::Small)
                                    .when(step.status == "completed", |label| {
                                        label.color(Color::Muted)
                                    }),
                            )
                    }))
                    .into_any_element()
            }
            ItemKind::Approval {
                title,
                prompt,
                request_kind,
                ..
            } => {
                let outcome = match item.status {
                    status if status.is_in_progress() => "waiting for you".to_string(),
                    ItemStatus::Completed => "answered".to_string(),
                    status => status.label().to_string(),
                };
                Card::new()
                    .gap_1()
                    .border_color(palette.warning.hsla().opacity(0.5))
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(
                                Icon::new(IconName::Warning)
                                    .size(IconSize::Small)
                                    .color(Color::Warning),
                            )
                            .child(
                                Label::new(
                                    title
                                        .clone()
                                        .unwrap_or_else(|| format!("Approval: {request_kind}")),
                                )
                                .size(LabelSize::Small)
                                .weight(FontWeight::SEMIBOLD),
                            )
                            .child(div().flex_1())
                            .child(
                                Label::new(outcome)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .when_some(prompt.clone(), |this, prompt| {
                        this.child(
                            div()
                                .px_2()
                                .py_1()
                                .rounded(scaled(tokens::CONTROL_RADIUS))
                                .bg(palette.background.hsla())
                                .child(Label::new(prompt).size(LabelSize::Small).buffer_font(cx)),
                        )
                    })
                    .into_any_element()
            }
            ItemKind::Question {
                title, questions, ..
            } => {
                Card::new()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(
                                Icon::new(IconName::CircleHelp)
                                    .size(IconSize::Small)
                                    .color(Color::Accent),
                            )
                            .child(
                                Label::new(
                                    title
                                        .clone()
                                        .unwrap_or_else(|| "The agent asked".to_string()),
                                )
                                .size(LabelSize::Small)
                                .weight(FontWeight::SEMIBOLD),
                            ),
                    )
                    .children(questions.iter().map(|question| {
                        Label::new(question.question.clone()).size(LabelSize::Small)
                    }))
                    .into_any_element()
            }
            ItemKind::Error { message, .. } => Card::new()
                .border_color(cx.theme().status().error.opacity(0.6))
                .child(
                    h_flex()
                        .gap_1p5()
                        .items_start()
                        .child(
                            Icon::new(IconName::XCircle)
                                .size(IconSize::Small)
                                .color(Color::Error),
                        )
                        .child(Label::new(message.clone()).size(LabelSize::Small)),
                )
                .into_any_element(),
            ItemKind::SecretRequest {
                title,
                label,
                reason,
                placeholder,
                status,
            } => self.render_secret_request(
                item,
                SecretRequestView {
                    title: title.clone(),
                    label: label.clone(),
                    reason: reason.clone(),
                    placeholder: placeholder.clone(),
                    status: status.clone(),
                },
                window,
                cx,
            ),
            ItemKind::Notice { text } => h_flex()
                .gap_2()
                .child(div().flex_1().h(px(1.)).bg(palette.border.hsla()))
                .child(
                    Label::new(text.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(div().flex_1().h(px(1.)).bg(palette.border.hsla()))
                .into_any_element(),
            ItemKind::Tool { .. } | ItemKind::FileChange { .. } | ItemKind::Reasoning { .. } => {
                self.render_work(vec![item.clone()], window, cx)
            }
        }
    }

    /// The agent's request for a secret: a masked input with Save and Decline while it waits,
    /// then how it was answered (`chat/SecretRequestCard.tsx` in T3).
    fn render_secret_request(
        &mut self,
        item: &TimelineItem,
        request: SecretRequestView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = palette(cx);
        let pending = request.status == "pending";
        let heading = request
            .title
            .clone()
            .unwrap_or_else(|| format!("The agent asks for {}", request.label));
        let mut card = Card::new()
            .gap_1p5()
            .border_color(if pending {
                palette.warning.hsla().opacity(0.6)
            } else {
                palette.border.hsla()
            })
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::Lock)
                            .size(IconSize::Small)
                            .color(if pending {
                                Color::Warning
                            } else {
                                Color::Muted
                            }),
                    )
                    .child(
                        Label::new(heading)
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .child(div().flex_1())
                    .when(!pending, |this| {
                        this.child(
                            Label::new(request.status.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    }),
            )
            .when(!request.reason.trim().is_empty(), |this| {
                this.child(
                    Label::new(request.reason.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            });
        if pending && item.inherited {
            card = card.child(
                Label::new("Waiting for an answer in the original thread")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );
        } else if pending {
            let input = self.secret_input(&item.id, request.placeholder.as_deref(), window, cx);
            let busy = self.composer_state.answering_secrets.contains(&item.id)
                || self.backend.write_blocked_reason().is_some();
            let item_id = item.id.clone();
            card = card.child(
                h_flex()
                    .gap_1p5()
                    .child(
                        div()
                            .debug_selector(|| format!("thread-secret-input-{item_id}"))
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .py_1()
                            .rounded(scaled(tokens::CONTROL_RADIUS))
                            .border(tokens::BORDER_WIDTH)
                            .border_color(palette.border.hsla())
                            .bg(palette.background.hsla())
                            .child(input),
                    )
                    .child(
                        div()
                            .debug_selector({
                                let item_id = item.id.clone();
                                move || format!("thread-secret-save-{item_id}")
                            })
                            .child(
                                TextButton::new(
                                    SharedString::from(format!("secret-save-{}", item.id)),
                                    "Save",
                                )
                                .style(MustrButtonStyle::Primary)
                                .disabled(busy)
                                .tooltip("Give the agent this secret")
                                .on_click(cx.listener({
                                    let item_id = item.id.clone();
                                    move |this, _, window, cx| {
                                        this.answer_secret(item_id.clone(), true, window, cx)
                                    }
                                })),
                            ),
                    )
                    .child(
                        div()
                            .debug_selector({
                                let item_id = item.id.clone();
                                move || format!("thread-secret-decline-{item_id}")
                            })
                            .child(
                                TextButton::new(
                                    SharedString::from(format!("secret-decline-{}", item.id)),
                                    "Decline",
                                )
                                .disabled(busy)
                                .on_click(cx.listener({
                                    let item_id = item.id.clone();
                                    move |this, _, window, cx| {
                                        this.answer_secret(item_id.clone(), false, window, cx)
                                    }
                                })),
                            ),
                    ),
            );
        }
        card.into_any_element()
    }

    /// A message's copy button, shown on hover, as T3's `chat/MessageCopyButton.tsx`.
    fn render_copy_message(
        &self,
        item: &TimelineItem,
        text: &str,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if text.trim().is_empty() {
            return None;
        }
        let copied = self.composer_state.copied.as_deref() == Some(item.id.as_str());
        let item_id = item.id.clone();
        let text = text.to_string();
        let selector = format!("thread-copy-{}", item.id);
        Some(
            div()
                .flex_none()
                .debug_selector(|| selector)
                .when(!copied, |this| {
                    this.visible_on_hover(SharedString::from(format!("message-{}", item.id)))
                })
                .child(
                    ui::IconButton::new(
                        SharedString::from(format!("copy-message-{}", item.id)),
                        if copied {
                            IconName::Check
                        } else {
                            IconName::Copy
                        },
                    )
                    .icon_size(IconSize::XSmall)
                    .icon_color(if copied { Color::Success } else { Color::Muted })
                    .tooltip(ui::Tooltip::text(if copied {
                        "Copied"
                    } else {
                        "Copy message"
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.copy_text(item_id.clone(), text.clone(), cx)
                    })),
                )
                .into_any_element(),
        )
    }

    /// Implement, Implement in a new thread, and Refine, on the plan T3 would offer them for.
    fn render_plan_actions(
        &mut self,
        item: &TimelineItem,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let plan = self.thread_state()?.actionable_plan()?;
        if plan.item_id != item.id || !self.images.composer.is_empty() {
            return None;
        }
        let disabled = self.backend.write_blocked_reason().is_some() || self.sending;
        Some(
            h_flex()
                .pt_1()
                .gap_1p5()
                .child(
                    div()
                        .debug_selector(|| "thread-plan-implement".into())
                        .child(
                            TextButton::new("plan-implement", "Implement")
                                .style(MustrButtonStyle::Primary)
                                .disabled(disabled)
                                .tooltip("Leave plan mode and implement this plan")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.implement_plan(false, window, cx)
                                })),
                        ),
                )
                .child(
                    div()
                        .debug_selector(|| "thread-plan-implement-new".into())
                        .child(
                            TextButton::new("plan-implement-new", "Implement in a new thread")
                                .disabled(disabled)
                                .tooltip("Start a thread on the same branch that implements it")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.implement_plan(true, window, cx)
                                })),
                        ),
                )
                .child(
                    div().debug_selector(|| "thread-plan-refine".into()).child(
                        TextButton::new("plan-refine", "Refine")
                            .disabled(disabled)
                            .tooltip("Ask the agent to change the plan")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.refine_plan(window, cx)),
                            ),
                    ),
                )
                .into_any_element(),
        )
    }

    /// A message's images, as thumbnails that open full size.
    fn render_message_images(
        &mut self,
        item_id: &str,
        images: &[MessageImage],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .flex_wrap()
            .gap_2()
            .max_w(scaled(px(MESSAGE_IMAGE_WIDTH * 2. + 8.)))
            .children(images.iter().enumerate().map(|(index, image)| {
                let key = AssetKey::Attachment {
                    attachment_id: image.attachment_id.clone(),
                    mime_type: image.mime_type.clone(),
                };
                self.render_thumbnail(
                    Thumbnail {
                        key,
                        name: image.name.clone().into(),
                        selector: format!("thread-message-image-{item_id}-{index}"),
                        size: (
                            scaled(px(MESSAGE_IMAGE_WIDTH)),
                            scaled(px(MESSAGE_IMAGE_HEIGHT)),
                        ),
                        fit: ObjectFit::Cover,
                    },
                    cx,
                )
            }))
            .into_any_element()
    }

    /// An image of the timeline: a placeholder while it loads, then the image, which opens full
    /// size when clicked.
    fn render_thumbnail(&mut self, thumbnail: Thumbnail, cx: &mut Context<Self>) -> AnyElement {
        let Thumbnail {
            key,
            name,
            selector,
            size: (width, height),
            fit,
        } = thumbnail;
        let palette = palette(cx);
        let state = self.asset(&key, cx);
        let frame = div()
            .id(SharedString::from(selector.clone()))
            .debug_selector(|| selector)
            .flex_none()
            .w(width)
            .h(height)
            .rounded(scaled(tokens::CONTROL_RADIUS))
            .overflow_hidden()
            .border(tokens::BORDER_WIDTH)
            .border_color(palette.border.hsla())
            .bg(palette.background.hsla());
        match state {
            AssetState::Loaded(image) => {
                let preview = image.clone();
                frame
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_image_preview(name.clone(), preview.clone(), window, cx)
                    }))
                    .child(img(image).size_full().object_fit(fit))
                    .into_any_element()
            }
            AssetState::Loading => frame
                .flex()
                .items_center()
                .justify_center()
                .child(
                    Icon::new(IconName::Image)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            AssetState::Failed(error) => frame
                .flex()
                .items_center()
                .justify_center()
                .tooltip(ui::Tooltip::text(format!("Couldn't load {name}: {error}")))
                .child(
                    Icon::new(IconName::Warning)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
        }
    }

    fn render_work(
        &mut self,
        items: Vec<TimelineItem>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = palette(cx);
        let Some(first) = items.first() else {
            return div().into_any_element();
        };
        let key = first.id.clone();
        // Runs of work and tool calls share `expanded`, so a run's key mustn't be its first
        // item's id, or expanding the run would expand that item too.
        let expanded_key = format!("work:{key}");
        let expanded = self.expanded.contains(&expanded_key);
        let item_refs = items.iter().collect::<Vec<_>>();
        let in_progress = items.iter().any(|item| item.status.is_in_progress());
        let failed = items
            .iter()
            .filter(|item| item.status == ItemStatus::Failed)
            .count();
        let selector = format!("thread-work-header-{key}");
        let header = h_flex()
            .id(SharedString::from(format!("work-{key}")))
            .debug_selector(|| selector)
            .gap_1p5()
            .cursor_pointer()
            .on_click(
                cx.listener(move |this, _, _, cx| this.toggle_expanded(expanded_key.clone(), cx)),
            )
            .child(
                Icon::new(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(IconSize::XSmall)
                .color(Color::Muted),
            )
            .child(
                Label::new(work_summary(&item_refs))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .when(in_progress, |this| {
                this.child(Indicator::dot().color(Color::Accent)).child(
                    Label::new("working")
                        .size(LabelSize::XSmall)
                        .color(Color::Accent),
                )
            })
            .when(failed > 0, |this| {
                this.child(
                    Label::new(format!("{failed} failed"))
                        .size(LabelSize::XSmall)
                        .color(Color::Error),
                )
            });

        let body = if expanded {
            v_flex()
                .gap_0p5()
                .pl(scaled(px(14.)))
                .children(
                    items
                        .iter()
                        .map(|item| self.render_work_item(item, window, cx)),
                )
                .into_any_element()
        } else {
            let chips = items
                .iter()
                .filter(|item| !matches!(item.kind, ItemKind::Reasoning { .. }))
                .collect::<Vec<_>>();
            let hidden = chips.len().saturating_sub(COLLAPSED_CHIPS);
            h_flex()
                .flex_wrap()
                .gap(scaled(px(6.)))
                .pl(scaled(px(14.)))
                .children(chips.iter().take(COLLAPSED_CHIPS).map(|item| {
                    div()
                        .px(scaled(px(7.)))
                        .py(scaled(px(1.)))
                        .rounded_full()
                        .border_1()
                        .border_dashed()
                        .border_color(if item.status == ItemStatus::Failed {
                            cx.theme().status().error.opacity(0.6)
                        } else {
                            palette.border.hsla()
                        })
                        .text_size(tokens::text_size(tokens::TEXT_SIZE_XSMALL))
                        .text_color(palette.text_muted.hsla())
                        .font_buffer(cx)
                        .child(truncate(&work_label(item), CHIP_MAX_CHARS))
                }))
                .when(hidden > 0, |this| {
                    this.child(
                        Label::new(format!("+{hidden} more"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                })
                .into_any_element()
        };
        v_flex()
            .gap_1()
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_work_item(
        &mut self,
        item: &TimelineItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = palette(cx);
        let expanded = self.expanded.contains(&item.id);
        let icon = match &item.kind {
            ItemKind::Tool { tool, .. } => tool_icon(*tool),
            ItemKind::FileChange { .. } => IconName::ToolPencil,
            _ => IconName::ToolThink,
        };
        let detail: Option<AnyElement> = if expanded {
            match &item.kind {
                ItemKind::Tool {
                    input,
                    output,
                    images,
                    ..
                } => {
                    let loading = self.loading_outputs.contains(&item.id);
                    // A running command's streamed output, until T3 has the whole of it.
                    let live = self
                        .composer_state
                        .live_outputs
                        .get(&item.id)
                        .filter(|_| item.status.is_in_progress() || output.is_none())
                        .map(|live| {
                            if live.truncated {
                                format!("Earlier output not shown.\n{}", live.text)
                            } else {
                                live.text.clone()
                            }
                        });
                    let output = live.or_else(|| output.clone());
                    let text = [input.clone(), output]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    let item_id = item.id.clone();
                    let thumbnails = (0..*images)
                        .map(|index| {
                            self.render_thumbnail(
                                Thumbnail {
                                    key: AssetKey::ToolImage {
                                        item_id: item_id.clone(),
                                        index,
                                    },
                                    name: format!("Screenshot {}", index + 1).into(),
                                    selector: format!("thread-tool-image-{item_id}-{index}"),
                                    size: (
                                        scaled(px(TOOL_IMAGE_WIDTH)),
                                        scaled(px(TOOL_IMAGE_HEIGHT)),
                                    ),
                                    fit: ObjectFit::Contain,
                                },
                                cx,
                            )
                        })
                        .collect::<Vec<_>>();
                    Some(
                        v_flex()
                            .ml(scaled(px(20.)))
                            .gap_1()
                            .child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .rounded(scaled(tokens::CONTROL_RADIUS))
                                    .bg(palette.background.hsla())
                                    .border_1()
                                    .border_color(palette.border.hsla())
                                    .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
                                    .font_buffer(cx)
                                    .text_color(palette.text_muted.hsla())
                                    .child(if loading {
                                        "Loading…".to_string()
                                    } else if text.is_empty() {
                                        "No output.".to_string()
                                    } else {
                                        text
                                    }),
                            )
                            .when(!thumbnails.is_empty(), |this| {
                                this.child(h_flex().flex_wrap().gap_2().children(thumbnails))
                            })
                            .into_any_element(),
                    )
                }
                ItemKind::FileChange { changes, diff, .. } => Some(
                    v_flex()
                        .ml(scaled(px(20.)))
                        .gap_0p5()
                        .children(changes.iter().map(|change| {
                            Label::new(format!("{} {}", change.operation, change.path))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .buffer_font(cx)
                        }))
                        .when_some(diff.clone(), |this, diff| {
                            this.child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .rounded(scaled(tokens::CONTROL_RADIUS))
                                    .bg(palette.background.hsla())
                                    .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
                                    .font_buffer(cx)
                                    .child(diff),
                            )
                        })
                        .into_any_element(),
                ),
                ItemKind::Reasoning { .. } => Some(
                    div()
                        .ml(scaled(px(20.)))
                        .child(self.render_markdown(&item.id, true, window, cx))
                        .into_any_element(),
                ),
                _ => None,
            }
        } else {
            None
        };
        let item_id = item.id.clone();
        v_flex()
            .gap_0p5()
            .child(
                h_flex()
                    .id(SharedString::from(format!("work-item-{}", item.id)))
                    .debug_selector(|| format!("thread-work-item-{}", item.id))
                    .gap_1p5()
                    .cursor_pointer()
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.toggle_tool(item_id.clone(), cx)),
                    )
                    .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(tokens::text_size(tokens::TEXT_SIZE_SMALL))
                            .font_buffer(cx)
                            .child(truncate(&work_label(item), 160)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(tokens::text_size(tokens::TEXT_SIZE_XSMALL))
                            .text_color(status_color(item.status, cx))
                            .child(item.status.label()),
                    ),
            )
            .children(detail)
            .into_any_element()
    }
}
