//! Reviews show the active theme's full syntax colors: the new side and the deleted side of a
//! diff look exactly as the same file does in the editor, in every theme, and switching the theme
//! recolors an open review.

use super::tests::{lhs_editor, rel_path, rhs_editor, test_workspace};
use super::*;
use collections::HashMap;
use editor::display_map::DisplayRow;
use gpui::{Hsla, TestAppContext, VisualTestContext};
use language::{LanguageAwareStyling, rust_lang};
use project::FakeFs;
use serde_json::json;
use settings::DiffViewStyle;
use std::path::Path;
use theme::{GlobalTheme, Theme};

const CS_BASE: &str = "namespace Acme;\n\npublic class RetryService\n{\n    public int Retry(int attempt) => attempt + 1;\n    private string Name = \"once\";\n}\n";
const CS_HEAD: &str = "namespace Acme;\n\npublic sealed record RetryPlan(Guid Id, int Attempt);\n\npublic class RetryService\n{\n    public async Task<int> RetryAsync(int? attempt) => await Task.FromResult(attempt ?? 0);\n    private string Name = $\"twice {Attempt}\";\n}\n";
const SQL_BASE: &str = "-- Orders\nCREATE TABLE [dbo].[Orders] ([Id] INT NOT NULL);\nGO\n";
const SQL_HEAD: &str = "-- Orders with totals\nCREATE TABLE [dbo].[Orders] ([Id] INT NOT NULL, [Total] DECIMAL(18, 2) NULL);\nGO\nCREATE OR ALTER PROCEDURE [dbo].[usp_Orders] @Top INT AS\nBEGIN\n    SELECT TOP (@Top) [Id] FROM [dbo].[Orders];\nEND\nGO\n";
const RS_BASE: &str =
    "// Retries\nfn retry(n: u32) -> u32 {\n    let s = \"once\";\n    n + 1\n}\n";
const RS_HEAD: &str = "// Retries with backoff\npub fn retry(n: u32) -> u32 {\n    let s = \"twice\";\n    n * 2\n}\n";

const FILES: [(&str, &str, &str); 3] = [
    ("src/RetryService.cs", CS_BASE, CS_HEAD),
    ("db/orders.sql", SQL_BASE, SQL_HEAD),
    ("src/retry.rs", RS_BASE, RS_HEAD),
];

/// mustr's three themes and one of Zed's.
fn themes() -> Vec<Arc<Theme>> {
    let mut families = vec![mustr_ui::theme_family_content().unwrap()];
    families.push(
        theme_settings::deserialize_user_theme(
            include_str!("../../../assets/themes/one/one.json").as_bytes(),
        )
        .unwrap(),
    );
    families
        .into_iter()
        .flat_map(|family| theme_settings::refine_theme_family(family).themes)
        .filter(|theme| theme.name.starts_with("mustr") || theme.name == "One Dark")
        .map(Arc::new)
        .collect()
}

/// Each line an editor shows, with the color it draws each non-blank character in.
type Colored = Vec<(String, Vec<Option<Hsla>>)>;

fn colored_lines(editor: &Entity<Editor>, theme: &Theme, cx: &mut VisualTestContext) -> Colored {
    editor.update_in(cx, |editor, window, cx| {
        let snapshot = editor.snapshot(window, cx);
        let style = editor.style(cx).clone();
        assert!(
            Arc::ptr_eq(&style.syntax, theme.syntax()),
            "the editor draws with {}'s syntax colors",
            theme.name
        );
        let rows = DisplayRow(0)..DisplayRow(snapshot.max_point().row().0 + 1);
        let styling = LanguageAwareStyling {
            tree_sitter: true,
            diagnostics: false,
        };
        let mut lines: Colored = vec![(String::new(), Vec::new())];
        for chunk in snapshot.highlighted_chunks(rows, styling, &style) {
            let color = chunk
                .style
                .and_then(|style| style.color)
                .unwrap_or(style.text.color);
            for (index, part) in chunk.text.split('\n').enumerate() {
                if index > 0 {
                    lines.push((String::new(), Vec::new()));
                }
                let (text, colors) = lines.last_mut().unwrap();
                text.push_str(part);
                colors.extend(part.chars().map(|c| (!c.is_whitespace()).then_some(color)));
            }
        }
        lines
    })
}

/// The colors of `text` in a plain editor, by line.
async fn editor_colors(
    path: &str,
    text: &str,
    workspace: &Entity<Workspace>,
    theme: &Theme,
    cx: &mut VisualTestContext,
) -> HashMap<String, Vec<Option<Hsla>>> {
    let project = workspace.read_with(cx, |workspace, _| workspace.project().clone());
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    let language = languages
        .language_for_file_path(Path::new(path))
        .unwrap_or_else(|| panic!("{path} has a language"));
    let language = languages.load_language(language).await.unwrap().unwrap();
    let editor = workspace.update_in(cx, |_, window, cx| {
        let buffer = cx.new(|cx| Buffer::local(text, cx).with_language(language, cx));
        cx.new(|cx| Editor::for_buffer(buffer, Some(project), window, cx))
    });
    cx.run_until_parked();
    colored_lines(&editor, theme, cx).into_iter().collect()
}

async fn check_review(diff_view_style: DiffViewStyle, local: bool, cx: &mut TestAppContext) {
    let fs = FakeFs::new(cx.executor());
    let mut tree = serde_json::Map::new();
    for (path, _, head) in FILES {
        tree.insert(path.to_string(), json!(head));
    }
    fs.insert_tree("/worktree", nested(tree)).await;
    let contents = ReviewContents {
        // An agent review of a worktree on this machine shows the real files; a pull request's
        // review (or an agent review of another machine's worktree) shows in-memory ones.
        worktree: local.then(|| PathBuf::from("/worktree")),
        files: FILES
            .iter()
            .map(|(path, base, head)| ReviewFileContents {
                path: rel_path(path),
                old_path: None,
                base_text: Some(base.to_string()),
                head_text: Some(head.to_string()),
                commentable: None,
            })
            .collect(),
        ..ReviewContents::default()
    };
    let themes = themes();
    assert_eq!(themes.len(), 4);
    let first = themes[0].clone();
    let (workspace, cx) = test_workspace(
        diff_view_style,
        fs,
        move |languages| {
            languages.set_theme(first);
            languages.add(rust_lang());
            mustr_languages::init(languages);
        },
        cx,
    )
    .await;
    let languages = workspace.read_with(cx, |workspace, cx| {
        workspace.project().read(cx).languages().clone()
    });
    let theme = themes[0].clone();
    cx.update(|_, cx| GlobalTheme::update_theme(cx, theme));
    let review_view = workspace
        .update_in(cx, |workspace, window, cx| {
            ReviewView::open(contents, workspace, window, cx)
        })
        .await
        .expect("review view should open");
    cx.run_until_parked();

    // Switching the theme, as mustr's settings do, while the review stays open.
    for theme in &themes {
        languages.set_theme(theme.clone());
        let switched = theme.clone();
        cx.update(|window, cx| {
            GlobalTheme::update_theme(cx, switched);
            window.refresh();
        });
        cx.run_until_parked();

        let keyword = theme
            .syntax()
            .style_for_name("keyword")
            .and_then(|s| s.color);
        let mut sides = vec![("new", rhs_editor(&review_view, cx))];
        if diff_view_style == DiffViewStyle::Split {
            sides.push(("deleted", lhs_editor(&review_view, cx)));
        }
        let mut expected = HashMap::default();
        let mut keywords_seen = 0;
        for (path, base, head) in FILES {
            let base = editor_colors(path, base, &workspace, theme, cx).await;
            let head = editor_colors(path, head, &workspace, theme, cx).await;
            for (line, colors) in base.into_iter().chain(head) {
                if line.trim().len() > 3 {
                    expected.insert(line, colors);
                }
            }
        }
        for (side, editor) in sides {
            let mut compared = 0;
            for (line, colors) in colored_lines(&editor, theme, cx) {
                let Some(expected) = expected.get(&line) else {
                    continue;
                };
                assert_eq!(
                    &colors, expected,
                    "{} ({side} side, {diff_view_style:?}): {line:?} is colored as in the editor",
                    theme.name
                );
                keywords_seen += colors.iter().filter(|c| **c == keyword).count();
                compared += 1;
            }
            assert!(
                compared >= 10,
                "{side} side: only {compared} lines compared"
            );
        }
        assert!(keywords_seen > 0, "{}: keywords are colored", theme.name);
    }
}

/// `{"src/a.cs": text}` as nested directories, for `FakeFs::insert_tree`.
fn nested(files: serde_json::Map<String, serde_json::Value>) -> serde_json::Value {
    let mut root = serde_json::Map::new();
    for (path, text) in files {
        let mut parts = path.split('/').collect::<Vec<_>>();
        let file = parts.pop().unwrap();
        let mut dir = &mut root;
        for part in parts {
            dir = dir
                .entry(part.to_string())
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .unwrap();
        }
        dir.insert(file.to_string(), text);
    }
    serde_json::Value::Object(root)
}

#[gpui::test]
async fn test_split_agent_review_colors_match_the_editor(cx: &mut TestAppContext) {
    check_review(DiffViewStyle::Split, true, cx).await;
}

#[gpui::test]
async fn test_unified_agent_review_colors_match_the_editor(cx: &mut TestAppContext) {
    check_review(DiffViewStyle::Unified, true, cx).await;
}

#[gpui::test]
async fn test_split_pull_request_review_colors_match_the_editor(cx: &mut TestAppContext) {
    check_review(DiffViewStyle::Split, false, cx).await;
}

#[gpui::test]
async fn test_unified_pull_request_review_colors_match_the_editor(cx: &mut TestAppContext) {
    check_review(DiffViewStyle::Unified, false, cx).await;
}
