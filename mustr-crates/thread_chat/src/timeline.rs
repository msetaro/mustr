//! A thread's timeline: its rows, kept in step with the thread's state, with runs of work
//! grouped and collapsed, tool calls whose output loads on expand, and the Markdown messages,
//! reasoning, and plans show.

use crate::{
    model::{ItemKind, ItemStatus, Row, TimelineItem, ToolKind, canonical_item},
    thread_view::{CONTENT_MAX_WIDTH, Loadable, ThreadView},
};
use gpui::{
    AnyElement, App, AppContext as _, Context, FontWeight, Hsla, IntoElement, ParentElement as _,
    SharedString, Styled as _, TextStyleRefinement, Window, div, px, relative,
};
use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use mustr_ui::{Card, palette, tokens};
use t3_api::types::OrchestrationV2GetTurnItemInput;
use ui::{
    Color, Icon, IconName, IconSize, Indicator, Label, LabelCommon as _, LabelSize, prelude::*,
};

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

    /// Expands a tool call, fetching its output if T3 left it out.
    pub(crate) fn toggle_tool(&mut self, item_id: String, cx: &mut Context<Self>) {
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
                let output = match result {
                    Ok(result) => result
                        .item
                        .as_ref()
                        .and_then(canonical_item)
                        .and_then(|item| TimelineItem::from_turn_item(&item))
                        .and_then(|item| match item.kind {
                            ItemKind::Tool { output, .. } => output,
                            _ => None,
                        })
                        .or_else(|| Some("No output.".to_string())),
                    Err(error) => Some(format!("Couldn't load the output: {error:#}")),
                };
                if let Loadable::Loaded(state) = &mut this.state
                    && state.set_output(&item_id, output).changed
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
        .size(px(18.))
        .mt(px(2.))
        .rounded_full()
        .bg(background)
        .flex()
        .items_center()
        .justify_center()
        .text_color(gpui::white())
        .text_size(px(9.5))
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
                    .max_w(px(CONTENT_MAX_WIDTH))
                    .py(px(6.))
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
            ItemKind::UserMessage { .. } => h_flex()
                .items_start()
                .gap_2()
                .child(avatar("Y", gpui::rgb(0x6e7781).into()))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .px(px(10.))
                        .py(px(6.))
                        .rounded(tokens::CORNER_RADIUS)
                        .bg(palette.surface_2.hsla())
                        .child(self.render_markdown(&item.id, false, window, cx)),
                )
                .into_any_element(),
            ItemKind::AssistantMessage { streaming, .. } => h_flex()
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
                                    .w(px(6.))
                                    .h(px(12.))
                                    .bg(palette.text.hsla())
                                    .opacity(0.7),
                            )
                        }),
                )
                .into_any_element(),
            ItemKind::Plan { title, .. } => Card::new()
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
                        ),
                )
                .child(self.render_markdown(&item.id, false, window, cx))
                .into_any_element(),
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
                                .rounded(tokens::CONTROL_RADIUS)
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
            ItemKind::Error { message } => Card::new()
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
        let expanded = self.expanded.contains(&key);
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
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_expanded(key.clone(), cx)))
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
                .pl(px(14.))
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
                .gap(px(6.))
                .pl(px(14.))
                .children(chips.iter().take(COLLAPSED_CHIPS).map(|item| {
                    div()
                        .px(px(7.))
                        .py(px(1.))
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
                ItemKind::Tool { input, output, .. } => {
                    let loading = self.loading_outputs.contains(&item.id);
                    let text = [input.clone(), output.clone()]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    Some(
                        div()
                            .ml(px(20.))
                            .px_2()
                            .py_1()
                            .rounded(tokens::CONTROL_RADIUS)
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
                            })
                            .into_any_element(),
                    )
                }
                ItemKind::FileChange { changes, diff, .. } => Some(
                    v_flex()
                        .ml(px(20.))
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
                                    .rounded(tokens::CONTROL_RADIUS)
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
                        .ml(px(20.))
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
