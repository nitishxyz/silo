//! Per-language grammar and compiled queries, built on first use.

use std::sync::OnceLock;

use tree_sitter::Query;

use super::{capture, language::Language};

/// A grammar with its highlights query, and an injections query where the
/// language embeds others.
pub(super) struct Config {
    pub language: tree_sitter::Language,
    pub highlights: Query,
    pub injections: Option<Query>,
    /// Per highlights capture index: whether it paints (it maps to a theme
    /// key and is not a predicate helper such as `@_name`).
    pub paints: Vec<bool>,
    /// For one node, the first matching pattern wins instead of the last.
    /// The upstream queries were written for both: most put general
    /// patterns first (`(identifier) @variable`, then calls), as
    /// `tree-sitter highlight` and Zed expect today; Go's puts them last.
    pub first_wins: bool,
}

macro_rules! query {
    ($lang:literal, $file:literal) => {
        include_str!(concat!("queries/", $lang, "/", $file, ".scm"))
    };
}

/// The grammar and query sources for `lang`. For one node the later
/// pattern wins, so layered queries go general to specific: C before C++
/// (as tree-sitter-cpp's tree-sitter.json lists them), and JavaScript
/// before TypeScript before JSX. tree-sitter-typescript's tree-sitter.json
/// lists JavaScript's last, which under that rule would let its
/// `(identifier) @variable` hide TypeScript's and JSX's captures.
fn sources(lang: Language) -> (tree_sitter::Language, Vec<&'static str>, Option<&'static str>) {
    let js_injections = Some(query!("javascript", "injections"));
    match lang {
        Language::Bash => (tree_sitter_bash::LANGUAGE.into(), vec![query!("bash", "highlights")], None),
        Language::C => (tree_sitter_c::LANGUAGE.into(), vec![query!("c", "highlights")], None),
        Language::Cpp => {
            (tree_sitter_cpp::LANGUAGE.into(), vec![query!("c", "highlights"), query!("cpp", "highlights")], Some(query!("cpp", "injections")))
        }
        Language::Css => (tree_sitter_css::LANGUAGE.into(), vec![query!("css", "highlights")], None),
        Language::Diff => (tree_sitter_diff::LANGUAGE.into(), vec![query!("diff", "highlights")], None),
        Language::Go => (tree_sitter_go::LANGUAGE.into(), vec![query!("go", "highlights")], None),
        Language::Html => (tree_sitter_html::LANGUAGE.into(), vec![query!("html", "highlights")], Some(query!("html", "injections"))),
        Language::JavaScript => (
            tree_sitter_javascript::LANGUAGE.into(),
            vec![query!("javascript", "highlights"), query!("javascript", "highlights-jsx"), query!("javascript", "highlights-params")],
            js_injections,
        ),
        Language::Json => (tree_sitter_json::LANGUAGE.into(), vec![query!("json", "highlights")], None),
        Language::Markdown => (tree_sitter_md::LANGUAGE.into(), vec![query!("markdown", "highlights")], Some(query!("markdown", "injections"))),
        Language::MarkdownInline => {
            (tree_sitter_md::INLINE_LANGUAGE.into(), vec![query!("markdown-inline", "highlights")], Some(query!("markdown-inline", "injections")))
        }
        Language::Python => (tree_sitter_python::LANGUAGE.into(), vec![query!("python", "highlights")], None),
        Language::Rust => (tree_sitter_rust::LANGUAGE.into(), vec![query!("rust", "highlights")], Some(query!("rust", "injections"))),
        Language::Toml => (tree_sitter_toml_ng::LANGUAGE.into(), vec![query!("toml", "highlights")], None),
        Language::Tsx => (
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            vec![query!("javascript", "highlights"), query!("typescript", "highlights"), query!("javascript", "highlights-jsx")],
            js_injections,
        ),
        Language::TypeScript => (
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            vec![query!("javascript", "highlights"), query!("typescript", "highlights")],
            js_injections,
        ),
        Language::Yaml => (tree_sitter_yaml::LANGUAGE.into(), vec![query!("yaml", "highlights")], None),
    }
}

/// Compile `lang`'s queries. An error names the language and query.
pub(super) fn build(lang: Language) -> Result<Config, String> {
    let (language, highlights, injections) = sources(lang);
    let highlights = Query::new(&language, &highlights.join("\n")).map_err(|e| format!("{} highlights: {e}", lang.name()))?;
    let injections = injections.map(|source| Query::new(&language, source).map_err(|e| format!("{} injections: {e}", lang.name()))).transpose()?;
    let paints = highlights.capture_names().iter().map(|name| capture::paints(name)).collect();
    Ok(Config { language, highlights, injections, paints, first_wins: lang == Language::Go })
}

/// `lang`'s config, compiled once; `None` (logged) if a query does not
/// compile against its grammar, so that language is plain text.
pub(super) fn config(lang: Language) -> Option<&'static Config> {
    static CONFIGS: [OnceLock<Option<Config>>; Language::ALL.len()] = [const { OnceLock::new() }; Language::ALL.len()];
    CONFIGS[lang.index()]
        .get_or_init(|| build(lang).map_err(|error| tracing::warn!(%error, "syntax: query does not compile; highlighting off for the language")).ok())
        .as_ref()
}
