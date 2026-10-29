//! What agents render for the user, which T3 shows in a web view and mustr, without one, opens
//! in the system browser:
//! - An HTML render (T3's `html_render` tool, `chat/HtmlRenderFrame.tsx`): a chart, a table, a
//!   page the agent made. T3 keeps it as an attachment; mustr shows a card, and Open in browser
//!   loads it (`assets.createUrl`, as T3's frame does) into a temporary file the browser opens.
//! - A page the agent opened in T3's preview browser (`preview_open`, `preview_navigate`): its
//!   tool call has Open in browser for the page's address.

use crate::{
    backend::{Access, ThreadBackendExt as _},
    thread_view::ThreadView,
};
use anyhow::{Context as _, anyhow};
use gpui::{
    AnyElement, Context, FontWeight, IntoElement, ParentElement as _, SharedString, Styled as _,
};
use mustr_ui::{Card, TextButton, palette};
use serde_json::Value;
use t3_api::{
    rpc,
    tagged::AssetResource,
    types::{
        AssetCreateUrlInput, AssetResourceAttachment, AssetResourceAttachmentDisposition,
        AssetResourceAttachmentTag,
    },
};
use ui::{Color, Icon, IconName, IconSize, Label, LabelCommon as _, LabelSize, prelude::*};

/// T3's longest title for a render (`HTML_RENDER_MAX_TITLE_LENGTH`).
const MAX_TITLE_CHARS: usize = 120;
/// The biggest render mustr loads.
const MAX_RENDER_BYTES: usize = 20 * 1024 * 1024;

/// The tool a T3 tool call names, without the MCP server's prefix (`resolveT3McpToolName`):
/// `mcp__t3-code__html_render`, `t3-code.html_render` and `html_render` are all `html_render`.
pub(crate) fn t3_tool_name(name: &str) -> Option<&str> {
    let name = name.trim();
    if let Some(rest) = name.strip_prefix("mcp__") {
        let (server, tool) = rest.split_once("__")?;
        return is_t3_server(server).then_some(tool);
    }
    for separator in ['.', ':', '/'] {
        if let Some((server, tool)) = name.split_once(separator)
            && is_t3_server(server)
        {
            return Some(tool);
        }
    }
    Some(name)
}

fn is_t3_server(server: &str) -> bool {
    matches!(
        server.to_ascii_lowercase().as_str(),
        "t3-code" | "t3_code" | "t3code" | "t3"
    )
}

/// The data a tool's result carries, as T3 reads it (`readResult`): JSON text, a list of
/// content blocks, or an object whose `structuredContent` (or `content`) holds it. Also
/// whether the result says the call failed.
fn read_result(value: &Value, depth: usize) -> (Option<Value>, bool) {
    if depth > 8 {
        return (None, false);
    }
    match value {
        Value::String(text) => serde_json::from_str::<Value>(text)
            .map(|parsed| read_result(&parsed, depth + 1))
            .unwrap_or((None, false)),
        Value::Array(blocks) => {
            let mut data = None;
            let mut failed = false;
            for block in blocks {
                let text = &block["text"];
                let text = if text.is_object() {
                    text.get("text").unwrap_or(text)
                } else {
                    text
                };
                let (found, block_failed) = read_result(text, depth + 1);
                data = data.or(found);
                failed |= block_failed;
            }
            (data, failed)
        }
        Value::Object(fields) => {
            let failed = value["isError"] == Value::Bool(true)
                || value["is_error"] == Value::Bool(true)
                || value["_tag"] == "OrchestratorMcpFailure"
                || fields.get("error").is_some_and(|error| !error.is_null());
            match fields
                .get("structuredContent")
                .or_else(|| fields.get("content"))
            {
                Some(content) => {
                    let (data, nested_failed) = read_result(content, depth + 1);
                    (data, failed || nested_failed)
                }
                None => (Some(value.clone()), failed),
            }
        }
        _ => (None, false),
    }
}

/// An HTML render a finished `html_render` call made: its attachment and title
/// (`htmlRenderFromToolItem`).
pub(crate) fn html_render_of(
    tool_name: Option<&str>,
    output: Option<&Value>,
) -> Option<(String, String)> {
    if tool_name.and_then(t3_tool_name) != Some("html_render") {
        return None;
    }
    let (data, failed) = read_result(output?, 0);
    if failed {
        return None;
    }
    let render = &data?["htmlRender"];
    let attachment_id = render["attachmentId"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 256)?;
    let title = render["title"].as_str()?;
    render["height"].as_f64()?;
    let title = title
        .trim()
        .chars()
        .take(MAX_TITLE_CHARS)
        .collect::<String>();
    let title = if title.trim().is_empty() {
        "HTML".to_string()
    } else {
        title.trim().to_string()
    };
    Some((attachment_id.to_string(), title))
}

/// The page a preview tool call opened, for Open in browser.
pub(crate) fn preview_link_of(tool_name: Option<&str>, input: &Value) -> Option<String> {
    let tool = tool_name.and_then(t3_tool_name)?;
    let tool = tool.strip_prefix("t3_").unwrap_or(tool);
    if !matches!(tool, "preview_open" | "preview_navigate") {
        return None;
    }
    let input = match input {
        Value::String(text) => serde_json::from_str(text).ok()?,
        other => other.clone(),
    };
    input["url"]
        .as_str()
        .map(str::trim)
        .filter(|url| url.starts_with("http://") || url.starts_with("https://"))
        .map(str::to_string)
}

/// The file a render saves as (`htmlRenderFileName`): its title without characters files
/// can't have.
pub(crate) fn render_file_name(title: &str) -> String {
    let cleaned = title
        .chars()
        .map(|c| {
            if matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect::<String>();
    let name = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let name = name.chars().take(120).collect::<String>();
    let name = name.trim();
    format!("{}.html", if name.is_empty() { "Page" } else { name })
}

impl ThreadView {
    /// An HTML render's card: its title, and Open in browser.
    pub(crate) fn render_html_render(
        &self,
        item_id: &str,
        attachment_id: &str,
        title: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = palette(cx);
        let opening = self.composer_state.opening_renders.contains(attachment_id);
        let attachment_id = attachment_id.to_string();
        let open_title = title.to_string();
        Card::new()
            .gap_1()
            .border_color(palette.accent.hsla().opacity(0.35))
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::FileCode)
                            .size(IconSize::Small)
                            .color(Color::Accent),
                    )
                    .child(
                        Label::new(title.to_string())
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD)
                            .truncate(),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .debug_selector({
                                let item_id = item_id.to_string();
                                move || format!("thread-html-open-{item_id}")
                            })
                            .child(
                                TextButton::new(
                                    SharedString::from(format!("html-open-{item_id}")),
                                    if opening {
                                        "Opening…"
                                    } else {
                                        "Open in browser"
                                    },
                                )
                                .disabled(opening)
                                .tooltip("Open the page the agent made in your browser")
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.open_html_render(
                                            attachment_id.clone(),
                                            open_title.clone(),
                                            cx,
                                        )
                                    },
                                )),
                            ),
                    ),
            )
            .child(
                Label::new("The agent made a page to show you. mustr opens it in your browser.")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    /// Loads a render (`assets.createUrl`, as T3's frame asks for it), saves it in a temporary
    /// folder, and opens it in the system browser.
    pub(crate) fn open_html_render(
        &mut self,
        attachment_id: String,
        title: String,
        cx: &mut Context<Self>,
    ) {
        if !self
            .composer_state
            .opening_renders
            .insert(attachment_id.clone())
        {
            return;
        }
        let file_name = render_file_name(&title);
        let signed = self.backend.call::<rpc::AssetsCreateUrl>(
            AssetCreateUrlInput {
                resource: AssetResource::Attachment(AssetResourceAttachment {
                    attachment_id: attachment_id.clone(),
                    disposition: Some(AssetResourceAttachmentDisposition::Inline),
                    file_name: Some(file_name.clone()),
                    mime_type: Some("text/html".to_string()),
                    render_intent: None,
                    tag: AssetResourceAttachmentTag::Attachment,
                }),
            },
            Access::Read,
        );
        let backend = self.backend.clone();
        let folder = std::env::temp_dir().join("mustr-html-renders").join(
            crate::mentions::kind_scoped_context_id("render", &attachment_id),
        );
        cx.notify();
        self._tasks.push(cx.spawn(async move |this, cx| {
            let saved = async {
                let signed = signed.await.context("signing the page's URL")?;
                let bytes = backend.fetch_asset(&signed.relative_url).await?;
                if bytes.len() > MAX_RENDER_BYTES {
                    return Err(anyhow!("the page is too big to open"));
                }
                let path = folder.join(&file_name);
                std::fs::create_dir_all(&folder)?;
                std::fs::write(&path, bytes)?;
                anyhow::Ok(path)
            }
            .await;
            this.update(cx, |this, cx| {
                this.composer_state.opening_renders.remove(&attachment_id);
                match saved {
                    Ok(path) => cx.open_url(&format!("file://{}", path.display())),
                    Err(error) => {
                        this.error = Some(format!("Couldn't open the page: {error:#}").into())
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn renders_are_found_as_t3_finds_them() {
        let render = json!({
            "htmlRender": { "attachmentId": "att-1", "title": " Latency by region ", "height": 420 },
            "message": "Rendered."
        });
        for name in [
            "mcp__t3-code__html_render",
            "t3-code.html_render",
            "html_render",
        ] {
            assert_eq!(
                html_render_of(Some(name), Some(&json!({ "structuredContent": render }))),
                Some(("att-1".into(), "Latency by region".into())),
                "{name}"
            );
        }
        // As MCP content blocks of JSON text.
        let blocks = json!({ "content": [{ "type": "text", "text": render.to_string() }] });
        assert_eq!(
            html_render_of(Some("mcp__t3code__html_render"), Some(&blocks)),
            Some(("att-1".into(), "Latency by region".into()))
        );
        // Another server's tool, a failure, and a reference without a height aren't renders.
        assert_eq!(
            html_render_of(Some("mcp__other__html_render"), Some(&render)),
            None
        );
        assert_eq!(
            html_render_of(
                Some("html_render"),
                Some(&json!({ "isError": true, "structuredContent": render }))
            ),
            None
        );
        assert_eq!(
            html_render_of(
                Some("html_render"),
                Some(&json!({ "htmlRender": { "attachmentId": "a", "title": "t" } }))
            ),
            None
        );
        assert_eq!(render_file_name("Q3: p95 / p99?"), "Q3 p95 p99.html");
        assert_eq!(render_file_name("  "), "Page.html");
    }

    #[test]
    fn previews_open_their_page() {
        let input = json!({ "url": "http://localhost:5173/settings" });
        assert_eq!(
            preview_link_of(Some("mcp__t3-code__preview_open"), &input).as_deref(),
            Some("http://localhost:5173/settings")
        );
        assert_eq!(
            preview_link_of(Some("t3_preview_navigate"), &input.to_string().into()).as_deref(),
            Some("http://localhost:5173/settings")
        );
        assert_eq!(preview_link_of(Some("preview_click"), &input), None);
        assert_eq!(
            preview_link_of(
                Some("preview_open"),
                &json!({ "url": "file:///etc/passwd" })
            ),
            None
        );
    }
}
