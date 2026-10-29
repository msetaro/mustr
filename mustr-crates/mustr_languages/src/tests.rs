use super::*;
use gpui::{AppContext as _, Hsla, TestAppContext};
use language::{Buffer, BufferSnapshot, LanguageAwareStyling, LanguageName};
use settings::SettingsStore;
use std::path::Path;
use theme::Theme;

/// A file of each language: its name, a line of it, a word in that line, and the theme style the
/// word is highlighted with (a keyword where the language has keywords).
struct Sample {
    language: &'static str,
    path: &'static str,
    text: &'static str,
    word: &'static str,
    style: &'static str,
}

const SAMPLES: &[Sample] = &[
    Sample {
        language: "CSharp",
        path: "src/Retry.cs",
        text: "public sealed class Retry { }\n",
        word: "class",
        style: "keyword",
    },
    Sample {
        language: "C# Project File",
        path: "src/Retry.csproj",
        text: "<Project Sdk=\"Microsoft.NET.Sdk\">\n  <PropertyGroup>\n    <Nullable>enable</Nullable>\n  </PropertyGroup>\n</Project>\n",
        word: "PropertyGroup",
        style: "tag",
    },
    Sample {
        language: "Dart",
        path: "lib/retry.dart",
        text: "import 'package:retry/retry.dart';\n",
        word: "import",
        style: "keyword",
    },
    Sample {
        language: "Dockerfile",
        path: "Dockerfile",
        text: "FROM ubuntu:24.04 AS build\n",
        word: "FROM",
        style: "keyword",
    },
    Sample {
        language: "Elixir",
        path: "lib/retry.ex",
        text: "defmodule Retry do\nend\n",
        word: "defmodule",
        style: "keyword",
    },
    Sample {
        language: "GraphQL",
        path: "schema.graphql",
        text: "type Query {\n  retry: Int\n}\n",
        word: "type",
        style: "keyword",
    },
    Sample {
        language: "Haskell",
        path: "src/Retry.hs",
        text: "retry n = if n > 3 then 0 else n\n",
        word: "if",
        style: "keyword",
    },
    Sample {
        language: "HCL",
        path: "config.hcl",
        text: "names = [for x in xs : x]\n",
        word: "for",
        style: "keyword",
    },
    Sample {
        language: "HTML",
        path: "index.html",
        text: "<div class=\"retry\">Hi</div>\n",
        word: "div",
        style: "tag",
    },
    Sample {
        language: "Java",
        path: "src/Retry.java",
        text: "public final class Retry {}\n",
        word: "class",
        style: "keyword",
    },
    Sample {
        language: "Kotlin",
        path: "src/Retry.kt",
        text: "fun retry(): Int = 1\n",
        word: "fun",
        style: "keyword",
    },
    Sample {
        language: "Lua",
        path: "retry.lua",
        text: "local n = 1\n",
        word: "local",
        style: "keyword",
    },
    Sample {
        language: "Make",
        path: "Makefile",
        text: "export RETRIES = 3\n",
        word: "export",
        style: "keyword",
    },
    Sample {
        language: "MSBuild File",
        path: "Directory.Build.props",
        text: "<Project>\n  <PropertyGroup>\n    <LangVersion>latest</LangVersion>\n  </PropertyGroup>\n</Project>\n",
        word: "PropertyGroup",
        style: "tag",
    },
    Sample {
        language: "Nix",
        path: "flake.nix",
        text: "let n = 3; in n\n",
        word: "let",
        style: "keyword",
    },
    Sample {
        language: "OCaml",
        path: "retry.ml",
        text: "let retry n = n + 1\n",
        word: "let",
        style: "keyword",
    },
    Sample {
        language: "OCaml Interface",
        path: "retry.mli",
        text: "val retry : int -> int\n",
        word: "val",
        style: "keyword",
    },
    Sample {
        language: "PHP",
        path: "retry.php",
        text: "<?php\nfunction retry() {}\n",
        word: "function",
        style: "keyword",
    },
    Sample {
        language: "Proto",
        path: "retry.proto",
        text: "message Retry {}\n",
        word: "message",
        style: "keyword",
    },
    Sample {
        language: "Ruby",
        path: "lib/retry.rb",
        text: "def retry\nend\n",
        word: "def",
        style: "keyword",
    },
    Sample {
        language: "Scala",
        path: "src/Retry.scala",
        text: "object Retry\n",
        word: "object",
        style: "keyword",
    },
    Sample {
        language: "C# Solution File",
        path: "Retry.slnx",
        text: "<Solution>\n  <Project Path=\"src/Retry.csproj\" />\n</Solution>\n",
        word: "Solution",
        style: "tag",
    },
    Sample {
        language: "SQL",
        path: "schema.sql",
        text: "SELECT id FROM users;\n",
        word: "SELECT",
        style: "keyword",
    },
    Sample {
        language: "Svelte",
        path: "src/Retry.svelte",
        text: "{#if ready}<p>Hi</p>{/if}\n",
        word: "if",
        style: "keyword",
    },
    Sample {
        language: "Swift",
        path: "Sources/Retry.swift",
        text: "func retry() {}\n",
        word: "func",
        style: "keyword",
    },
    Sample {
        language: "Terraform",
        path: "main.tf",
        text: "names = [for x in xs : x]\n",
        word: "for",
        style: "keyword",
    },
    Sample {
        language: "Terraform Vars",
        path: "prod.tfvars",
        text: "names = [for x in xs : x]\n",
        word: "for",
        style: "keyword",
    },
    Sample {
        language: "TOML",
        path: "Cargo.toml",
        text: "name = \"retry\"\n",
        word: "name",
        style: "property",
    },
    Sample {
        language: "Vue.js",
        path: "src/Retry.vue",
        text: "<template>\n  <p>{{ count }}</p>\n</template>\n",
        word: "template",
        style: "tag",
    },
    Sample {
        language: "XML",
        path: "config.xml",
        text: "<?xml version=\"1.0\"?>\n<retry/>\n",
        word: "xml",
        style: "keyword",
    },
    Sample {
        language: "Zig",
        path: "src/retry.zig",
        text: "const n = 1;\n",
        word: "const",
        style: "keyword",
    },
];

/// mustr's dark theme, which the app highlights with by default.
fn mustr_dark() -> Arc<Theme> {
    let family = theme_settings::refine_theme_family(mustr_ui::theme_family_content().unwrap());
    Arc::new(
        family
            .themes
            .into_iter()
            .find(|theme| theme.name == mustr_ui::DARK_THEME_NAME)
            .unwrap(),
    )
}

fn registry(cx: &mut TestAppContext) -> Arc<LanguageRegistry> {
    cx.update(|cx| {
        let settings = SettingsStore::test(cx);
        cx.set_global(settings);
    });
    let registry = Arc::new(LanguageRegistry::test(cx.executor()));
    registry.set_theme(mustr_dark());
    // Some of Zed's built-in languages, which these languages inject.
    registry.add(language::rust_lang());
    registry.add(language::json_lang());
    registry.add(language::markdown_lang());
    init(&registry);
    registry
}

/// The color the first `word` in `snapshot` is highlighted with.
fn color_of(snapshot: &BufferSnapshot, word: &str, theme: &Theme) -> Option<Hsla> {
    let start = snapshot
        .text()
        .find(word)
        .unwrap_or_else(|| panic!("{word:?} should be in the text"));
    let styling = LanguageAwareStyling {
        tree_sitter: true,
        diagnostics: false,
    };
    snapshot
        .chunks(start..start + word.len(), styling)
        .find_map(|chunk| chunk.syntax_highlight_id)
        .and_then(|id| theme.syntax().get(id))
        .and_then(|style| style.color)
}

#[test]
fn every_language_has_a_sample() {
    let configs: Vec<_> = LANGUAGES
        .iter()
        .map(|name| load_config(name).name.to_string())
        .collect();
    let samples: Vec<_> = SAMPLES
        .iter()
        .map(|sample| sample.language.to_string())
        .collect();
    assert_eq!(configs, samples, "one sample per language, in order");
}

#[gpui::test]
fn every_language_registers(cx: &mut TestAppContext) {
    let registry = registry(cx);
    let names = registry.language_names();
    for sample in SAMPLES {
        assert!(
            names.contains(&LanguageName::new_static(sample.language)),
            "{} isn't registered",
            sample.language
        );
    }
}

#[gpui::test]
fn languages_are_detected_by_file_name(cx: &mut TestAppContext) {
    let registry = registry(cx);
    for sample in SAMPLES {
        let language = registry
            .language_for_file_path(Path::new(sample.path))
            .and_then(|id| registry.language_name_for_id(id));
        assert_eq!(
            language,
            Some(LanguageName::new_static(sample.language)),
            "{}",
            sample.path
        );
    }
}

#[gpui::test]
async fn each_language_highlights_with_the_themes_colors(cx: &mut TestAppContext) {
    let registry = registry(cx);
    let theme = mustr_dark();
    let mut failures = Vec::new();
    for sample in SAMPLES {
        // Loading compiles the queries against the grammar, so a query the grammar doesn't
        // match fails here.
        let language = match registry.language_for_name(sample.language).await {
            Ok(language) => language,
            Err(error) => {
                failures.push(format!("{} doesn't load: {error:?}", sample.language));
                continue;
            }
        };
        let buffer = cx.new(|cx| Buffer::local(sample.text, cx).with_language(language, cx));
        cx.run_until_parked();
        let expected = theme
            .syntax()
            .style_for_name(sample.style)
            .and_then(|style| style.color);
        assert!(expected.is_some(), "mustr's theme colors {}", sample.style);
        let actual = buffer.read_with(cx, |buffer, _| {
            color_of(&buffer.snapshot(), sample.word, &theme)
        });
        if actual != expected {
            failures.push(format!(
                "{}: {:?} isn't highlighted as {}",
                sample.language, sample.word, sample.style
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Each query file compiles against the grammar its config names. (A language whose queries
/// don't compile fails to load, so its files would be plain text.)
#[test]
fn every_query_compiles_against_its_grammar() {
    let grammars: std::collections::HashMap<_, _> = grammars().into_iter().collect();
    let mut failures = Vec::new();
    for name in LANGUAGES {
        let config = load_config(name);
        let grammar_name = config
            .grammar
            .as_deref()
            .expect("every language has a grammar");
        let grammar = &grammars[grammar_name];
        let queries = load_queries(name);
        assert!(queries.highlights.is_some(), "{name} has no highlights");
        let files = [
            ("highlights", &queries.highlights),
            ("brackets", &queries.brackets),
            ("indents", &queries.indents),
            ("outline", &queries.outline),
            ("injections", &queries.injections),
            ("overrides", &queries.overrides),
        ];
        for (file, query) in files {
            if let Some(query) = query
                && let Err(error) = tree_sitter::Query::new(grammar, query)
            {
                failures.push(format!("{name}/{file}.scm: {error}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Each language parses its fixture file from `test_data/` (a realistic file that starts with a
/// comment) with tree-sitter's layer invariants checked, as debug builds check them.
#[gpui::test]
async fn realistic_files_parse(cx: &mut TestAppContext) {
    let registry = registry(cx);
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("test_data");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap();
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        let language_id = registry
            .language_for_file_path(&path)
            .unwrap_or_else(|| panic!("no language for {name}"));
        let language = registry.load_language(language_id).await.unwrap().unwrap();
        let buffer = cx.new(|cx| Buffer::local(text, cx).with_language(language, cx));
        cx.run_until_parked();
        buffer.read_with(cx, |buffer, _| {
            let snapshot = buffer.snapshot();
            let layers = snapshot.syntax_layers().count();
            assert!(layers > 0, "{name} wasn't parsed");
        });
        checked += 1;
    }
    assert!(checked > 0);
}

/// The T-SQL a C# shop's migrations and stored procedures use, one construct at a time.
const T_SQL: &[(&str, &str)] = &[
    (
        "bracketed names",
        "SELECT [Id], [Total] FROM [dbo].[Orders];",
    ),
    ("variables", "DECLARE @n INT = 1;\nSELECT @n;"),
    ("SET options", "SET NOCOUNT ON;"),
    ("GO", "SELECT 1;\nGO\nSELECT 2;"),
    (
        "procedures",
        "CREATE PROCEDURE dbo.p @a INT AS BEGIN SELECT @a; END",
    ),
    ("IF and BEGIN … END", "IF @a > 1\nBEGIN\n  SELECT 1;\nEND"),
    (
        "MERGE",
        "MERGE INTO t AS target USING s AS source ON target.id = source.id WHEN MATCHED THEN UPDATE SET target.v = source.v WHEN NOT MATCHED THEN INSERT (id, v) VALUES (source.id, source.v);",
    ),
    ("CTEs", "WITH c AS (SELECT 1 AS x) SELECT x FROM c;"),
    (
        "window functions",
        "SELECT ROW_NUMBER() OVER (PARTITION BY a ORDER BY b DESC) AS r FROM t;",
    ),
    (
        "window frames",
        "SELECT SUM(x) OVER (ORDER BY d ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t;",
    ),
    ("TOP", "SELECT TOP (10) a FROM t;"),
    (
        "TRY … CATCH",
        "BEGIN TRY\n  SELECT 1;\nEND TRY\nBEGIN CATCH\n  THROW;\nEND CATCH",
    ),
    ("transactions", "BEGIN TRANSACTION;\nCOMMIT TRANSACTION;"),
    ("N'' strings", "SELECT N'text';"),
    (
        "IDENTITY and DATETIME2",
        "CREATE TABLE t (id INT IDENTITY(1, 1) NOT NULL PRIMARY KEY, d DATETIME2(3) NOT NULL);",
    ),
    (
        "INCLUDE indexes",
        "CREATE NONCLUSTERED INDEX ix ON t (a) INCLUDE (b);",
    ),
    (
        "ANSI queries",
        "SELECT o.id, c.name FROM orders o JOIN customers c ON c.id = o.customer_id WHERE o.total > 10 GROUP BY c.name HAVING COUNT(*) > 1 ORDER BY 1;",
    ),
];

/// The lines of `text` that hold tree-sitter error or missing nodes.
fn error_lines(grammar: &tree_sitter::Language, text: &str) -> Vec<String> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(grammar).unwrap();
    let tree = parser.parse(text, None).unwrap();
    let mut cursor = tree.walk();
    let mut nodes = vec![tree.root_node()];
    let mut lines = Vec::new();
    while let Some(node) = nodes.pop() {
        if node.is_error() || node.is_missing() {
            let row = node.start_position().row;
            lines.push(format!(
                "{}: {}",
                row + 1,
                text.lines().nth(row).unwrap_or("")
            ));
        } else {
            nodes.extend(node.children(&mut cursor));
        }
    }
    lines
}

#[test]
fn t_sql_parses_without_errors() {
    let grammars: std::collections::HashMap<_, _> = grammars().into_iter().collect();
    let sql = &grammars[load_config("sql").grammar.as_deref().unwrap()];
    for (construct, text) in T_SQL {
        assert_eq!(error_lines(sql, text), Vec::<String>::new(), "{construct}");
    }
}

/// The realistic files in `test_data/` parse without errors: C#, T-SQL, and the XML a C# pull
/// request touches.
#[test]
fn c_sharp_and_t_sql_files_parse_without_errors() {
    let grammars: std::collections::HashMap<_, _> = grammars().into_iter().collect();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("test_data");
    for (file, language) in [
        ("RetryService.cs", "csharp"),
        ("V42__add_order_audit.sql", "sql"),
        ("usp_GetRecentOrders.sql", "sql"),
        ("Acme.Webhooks.csproj", "csproj"),
        ("Directory.Build.props", "msbuild"),
        ("App.config", "xml"),
    ] {
        let text = std::fs::read_to_string(dir.join(file)).unwrap();
        let grammar = &grammars[load_config(language).grammar.as_deref().unwrap()];
        assert_eq!(error_lines(grammar, &text), Vec::<String>::new(), "{file}");
    }
}

/// T-SQL's distinctive tokens get the theme's colors.
#[gpui::test]
async fn t_sql_highlights_with_the_themes_colors(cx: &mut TestAppContext) {
    let registry = registry(cx);
    let theme = mustr_dark();
    let text = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("test_data/usp_GetRecentOrders.sql"),
    )
    .unwrap();
    let language = registry.language_for_name("SQL").await.unwrap();
    let buffer = cx.new(|cx| Buffer::local(text, cx).with_language(language, cx));
    cx.run_until_parked();
    let color = |name: &str| {
        theme
            .syntax()
            .style_for_name(name)
            .and_then(|style| style.color)
    };
    buffer.read_with(cx, |buffer, _| {
        let snapshot = buffer.snapshot();
        for (word, style) in [
            ("PROCEDURE", "keyword"),
            ("SET", "keyword"),
            ("DECLARE", "keyword"),
            ("ROW_NUMBER", "function"),
            ("PARTITION", "keyword"),
            ("TRY", "keyword"),
            ("THROW", "keyword"),
            ("DATETIME2", "type"),
            ("-- Recent", "comment"),
        ] {
            assert_eq!(color_of(&snapshot, word, &theme), color(style), "{word}");
        }
    });
}
