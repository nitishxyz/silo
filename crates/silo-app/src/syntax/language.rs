//! The languages the highlighter knows, and detecting one for a file.

/// A language with a bundled grammar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Language {
    Bash,
    C,
    Cpp,
    Css,
    Diff,
    Go,
    Html,
    JavaScript,
    Json,
    Markdown,
    /// Markdown's inline grammar (emphasis, code spans, links). Only ever
    /// injected into [`Language::Markdown`]; detection never returns it.
    MarkdownInline,
    Python,
    Rust,
    Toml,
    Tsx,
    TypeScript,
    Yaml,
}

impl Language {
    pub const ALL: [Self; 17] = [
        Self::Bash,
        Self::C,
        Self::Cpp,
        Self::Css,
        Self::Diff,
        Self::Go,
        Self::Html,
        Self::JavaScript,
        Self::Json,
        Self::Markdown,
        Self::MarkdownInline,
        Self::Python,
        Self::Rust,
        Self::Toml,
        Self::Tsx,
        Self::TypeScript,
        Self::Yaml,
    ];

    /// Index into [`Language::ALL`].
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::Css => "css",
            Self::Diff => "diff",
            Self::Go => "go",
            Self::Html => "html",
            Self::JavaScript => "javascript",
            Self::Json => "json",
            Self::Markdown => "markdown",
            Self::MarkdownInline => "markdown_inline",
            Self::Python => "python",
            Self::Rust => "rust",
            Self::Toml => "toml",
            Self::Tsx => "tsx",
            Self::TypeScript => "typescript",
            Self::Yaml => "yaml",
        }
    }

    /// The language a file is written in: by file name, then extension,
    /// then the shebang on `first_line`. `None` is plain text.
    #[must_use]
    pub fn detect(path: &str, first_line: Option<&str>) -> Option<Self> {
        let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
        by_file_name(name)
            .or_else(|| {
                let (stem, ext) = name.rsplit_once('.')?;
                if stem.is_empty() {
                    return None; // a dotfile's name is not an extension
                }
                by_extension(&ext.to_ascii_lowercase())
            })
            .or_else(|| first_line.and_then(by_shebang))
    }

    /// The language a markdown fence info string or an injection query
    /// names: `rust`, `rs`, `ts`, `sh`, `c++`, `rust,ignore`, `{.python}`…
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        let token = name
            .trim()
            .trim_start_matches(['{', '.'])
            .split(|c: char| c.is_whitespace() || matches!(c, ',' | '{' | '}' | '=' | ':'))
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        Some(match token.as_str() {
            "bash" | "sh" | "shell" | "zsh" | "ksh" | "dash" | "console" | "shellsession" | "make" | "makefile" => Self::Bash,
            "c" | "h" => Self::C,
            "cpp" | "c++" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Self::Cpp,
            "css" => Self::Css,
            "diff" | "patch" | "udiff" => Self::Diff,
            "go" | "golang" => Self::Go,
            "html" | "htm" | "xhtml" => Self::Html,
            "javascript" | "js" | "jsx" | "mjs" | "cjs" | "node" => Self::JavaScript,
            "json" | "jsonc" | "json5" => Self::Json,
            "markdown" | "md" => Self::Markdown,
            "markdown_inline" => Self::MarkdownInline,
            "python" | "py" | "python3" | "py3" => Self::Python,
            "rust" | "rs" => Self::Rust,
            "toml" => Self::Toml,
            "tsx" => Self::Tsx,
            "typescript" | "ts" | "mts" | "cts" => Self::TypeScript,
            "yaml" | "yml" => Self::Yaml,
            _ => return None,
        })
    }
}

fn by_file_name(name: &str) -> Option<Language> {
    Some(match name {
        "Cargo.lock" | "uv.lock" | "poetry.lock" | "Pipfile" | "pdm.lock" | ".rustfmt.toml" | "rustfmt.toml" => Language::Toml,
        "Makefile" | "makefile" | "GNUmakefile" | "PKGBUILD" | "APKBUILD" | ".bashrc" | ".bash_profile" | ".bash_login" | ".bash_logout"
        | ".bash_aliases" | ".profile" | ".zshrc" | ".zshenv" | ".zprofile" | ".zlogin" | ".zlogout" | ".envrc" => Language::Bash,
        "package-lock.json" | ".babelrc" | ".eslintrc" | ".prettierrc" | "flake.lock" => Language::Json,
        ".clang-format" | ".clang-tidy" => Language::Yaml,
        _ => return None,
    })
}

fn by_extension(ext: &str) -> Option<Language> {
    Some(match ext {
        "sh" | "bash" | "zsh" | "ksh" | "command" | "mk" | "ebuild" => Language::Bash,
        "c" | "h" => Language::C,
        "cpp" | "cc" | "cxx" | "c++" | "hpp" | "hh" | "hxx" | "h++" | "ipp" | "inl" | "tpp" | "cu" | "mm" => Language::Cpp,
        "css" => Language::Css,
        "diff" | "patch" | "rej" => Language::Diff,
        "go" => Language::Go,
        "html" | "htm" | "xhtml" | "shtml" => Language::Html,
        "js" | "mjs" | "cjs" | "jsx" => Language::JavaScript,
        "json" | "jsonc" | "json5" | "jsonl" | "ndjson" | "geojson" | "webmanifest" => Language::Json,
        "md" | "markdown" | "mdown" | "mkd" | "mdx" => Language::Markdown,
        "py" | "pyi" | "pyw" => Language::Python,
        "rs" => Language::Rust,
        "toml" => Language::Toml,
        "tsx" => Language::Tsx,
        "ts" | "mts" | "cts" => Language::TypeScript,
        "yaml" | "yml" => Language::Yaml,
        _ => return None,
    })
}

/// `#!/bin/sh`, `#!/usr/bin/env python3`, `#!/usr/bin/env -S node --flag`.
fn by_shebang(line: &str) -> Option<Language> {
    let mut words = line.strip_prefix("#!")?.split_whitespace();
    let mut program = words.next()?.rsplit('/').next()?;
    if program == "env" {
        program = words.find(|w| !w.starts_with('-') && !w.contains('='))?.rsplit('/').next()?;
    }
    // python3.12, bash5 → python, bash.
    let base = program.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    Some(match base {
        "sh" | "bash" | "zsh" | "ksh" | "dash" | "ash" | "mksh" => Language::Bash,
        "python" | "pypy" | "uv" => Language::Python,
        "node" | "nodejs" | "bun" | "qjs" => Language::JavaScript,
        "deno" | "ts-node" | "tsx" => Language::TypeScript,
        _ => return None,
    })
}
