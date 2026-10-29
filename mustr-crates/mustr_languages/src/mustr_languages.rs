//! The languages mustr compiles in on top of Zed's built-in ones (`crates/languages`).
//!
//! Zed gets most languages (HTML, TOML, Swift, Java, Kotlin, Ruby and so on) from its extension
//! registry, which mustr leaves out (`mustr-docs/ZED-SERVICES.md`), so without this crate their
//! files would have no highlighting at all. This crate registers them natively, as
//! `crates/languages` registers Zed's: each grammar is compiled in (from crates.io, or vendored in
//! `grammars/` where no crate fits), and each language's `config.toml` and queries
//! (`languages/<name>/`) are the ones Zed's extension for it uses. `THIRD_PARTY_NOTICES.md` lists
//! each one's source, revision and license.
//!
//! It registers no language servers: these languages get highlighting, brackets, indentation,
//! outlines and injections (such as CSS and JavaScript inside HTML) only.

use std::{borrow::Cow, sync::Arc};

use anyhow::Context as _;
use futures::FutureExt as _;
use language::{
    LanguageConfig, LanguageQueries, LanguageRegistry, LoadedLanguage, QueryFile, QueryFileContents,
};
use tree_sitter_language::LanguageFn;

// Dev builds read the checkout's files at runtime, as Zed's `grammars` crate does.
util::fs_embed! {
    struct LanguageDir,
    crate_relative = "languages/",
    root_relative = "mustr-crates/mustr_languages/languages",
}

/// The languages this crate adds, by their directory under `languages/`.
pub const LANGUAGES: &[&str] = &[
    "csharp",
    "csproj",
    "dart",
    "dockerfile",
    "elixir",
    "graphql",
    "haskell",
    "hcl",
    "html",
    "java",
    "kotlin",
    "lua",
    "make",
    "msbuild",
    "nix",
    "ocaml",
    "ocaml-interface",
    "php",
    "proto",
    "ruby",
    "scala",
    "slnx",
    "sql",
    "svelte",
    "swift",
    "terraform",
    "terraform-vars",
    "toml",
    "vue",
    "xml",
    "zig",
];

// The vendored grammars (`build.rs`).
unsafe extern "C" {
    fn tree_sitter_dart() -> *const ();
    fn tree_sitter_dockerfile() -> *const ();
    fn tree_sitter_vue() -> *const ();
}

// SAFETY: each is the language function of a parser `build.rs` compiles from tree-sitter's
// generated C, which is what `LanguageFn` wraps.
const DART: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_dart) };
// SAFETY: as above.
const DOCKERFILE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_dockerfile) };
// SAFETY: as above.
const VUE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_vue) };

/// The grammars the languages' configs name.
fn grammars() -> Vec<(&'static str, tree_sitter::Language)> {
    vec![
        ("c_sharp", tree_sitter_c_sharp::LANGUAGE.into()),
        ("dart", DART.into()),
        ("dockerfile", DOCKERFILE.into()),
        ("elixir", tree_sitter_elixir::LANGUAGE.into()),
        ("graphql", tree_sitter_graphql::LANGUAGE.into()),
        ("haskell", tree_sitter_haskell::LANGUAGE.into()),
        ("hcl", tree_sitter_hcl::LANGUAGE.into()),
        ("html", tree_sitter_html::LANGUAGE.into()),
        ("java", tree_sitter_java::LANGUAGE.into()),
        ("kotlin", tree_sitter_kotlin_sg::LANGUAGE.into()),
        ("lua", tree_sitter_lua::LANGUAGE.into()),
        ("make", tree_sitter_make::LANGUAGE.into()),
        ("nix", tree_sitter_nix::LANGUAGE.into()),
        ("ocaml", tree_sitter_ocaml::LANGUAGE_OCAML.into()),
        (
            "ocaml_interface",
            tree_sitter_ocaml::LANGUAGE_OCAML_INTERFACE.into(),
        ),
        ("php", tree_sitter_php::LANGUAGE_PHP.into()),
        ("proto", tree_sitter_proto::LANGUAGE.into()),
        ("ruby", tree_sitter_ruby::LANGUAGE.into()),
        ("scala", tree_sitter_scala::LANGUAGE.into()),
        ("sql", tree_sitter_mssql::LANGUAGE.into()),
        ("svelte", tree_sitter_svelte_ng::LANGUAGE.into()),
        ("swift", tree_sitter_swift::LANGUAGE.into()),
        ("toml", tree_sitter_toml_ng::LANGUAGE.into()),
        ("vue", VUE.into()),
        ("xml", tree_sitter_xml::LANGUAGE_XML.into()),
        ("zig", tree_sitter_zig::LANGUAGE.into()),
    ]
}

/// Registers the grammars and languages with `languages`. Call it after `languages::init`, so a
/// language Zed builds in itself keeps Zed's registration.
pub fn init(languages: &LanguageRegistry) {
    languages.register_native_grammars(grammars());
    for name in LANGUAGES {
        let config = load_config(name);
        if languages.language_names().contains(&config.name) {
            log::info!(
                "{} is built into Zed now; mustr leaves it to Zed",
                config.name
            );
            continue;
        }
        languages.register_language(
            config.name.clone(),
            config.grammar.clone(),
            config.matcher.clone(),
            config.hidden,
            None,
            Arc::new(move || {
                let config = config.clone();
                async move {
                    Ok(LoadedLanguage {
                        config,
                        queries: load_queries(name),
                        context_provider: None,
                        toolchain_provider: None,
                        manifest_name: None,
                    })
                }
                .boxed()
            }),
        );
    }
}

/// A language's `config.toml`.
pub fn load_config(name: &str) -> LanguageConfig {
    let file = LanguageDir::get(&format!("{name}/config.toml"))
        .unwrap_or_else(|| panic!("mustr_languages has no config for {name:?}"));
    let config = std::str::from_utf8(&file.data)
        .with_context(|| format!("{name}/config.toml isn't UTF-8"))
        .unwrap();
    LanguageConfig::from_toml(config)
        .with_context(|| format!("loading {name}/config.toml"))
        .unwrap()
}

/// A language's queries (`highlights.scm` and so on).
pub fn load_queries(name: &str) -> LanguageQueries {
    LanguageQueries::from_files(LanguageDir::iter().filter_map(|path| {
        let file_name = path.strip_prefix(name)?.strip_prefix('/')?;
        let query_file = file_name.parse::<QueryFile>().ok()?;
        let contents = match LanguageDir::get(path.as_ref())?.data {
            Cow::Borrowed(bytes) => Cow::Borrowed(std::str::from_utf8(bytes).ok()?),
            Cow::Owned(bytes) => Cow::Owned(String::from_utf8(bytes).ok()?),
        };
        Some(QueryFileContents::new(query_file, contents))
    }))
}

#[cfg(test)]
mod tests;
