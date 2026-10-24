use std::ops::Range;

use super::*;

/// Every query compiles against its grammar.
#[test]
fn queries_compile() {
    for lang in Language::ALL {
        if let Err(e) = config::build(lang) {
            panic!("{e}");
        }
    }
}

fn at(spans: &[(Range<usize>, Capture)], offset: usize) -> Option<Capture> {
    spans.iter().find(|(r, _)| r.contains(&offset)).map(|(_, c)| *c)
}

/// Each `(needle, key)`: every byte of the first `needle` in `source` has a
/// capture whose name is `key` or under it (`function` takes
/// `function.method`).
#[track_caller]
fn check(lang: Language, source: &str, expected: &[(&str, &str)]) {
    let spans = highlight(lang, source);
    for w in spans.windows(2) {
        assert!(w[0].0.end <= w[1].0.start, "{lang:?}: overlapping or unsorted {w:?}");
    }
    for &(needle, key) in expected {
        let start = source.find(needle).unwrap_or_else(|| panic!("{needle:?} not in sample"));
        for i in start..start + needle.len() {
            let capture = at(&spans, i);
            let ok = capture.is_some_and(|c| c.name() == key || c.name().starts_with(&format!("{key}.")));
            assert!(
                ok,
                "{lang:?}: {needle:?} byte {} is {capture:?}, want {key}\nspans: {:?}",
                i - start,
                spans.iter().map(|(r, c)| (&source[r.clone()], c.name())).collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn rust() {
    let src = "// say hi\nfn greet(name: &str) -> String {\n    let n = 42;\n    format!(\"hi {name}\")\n}\nconst MAX: u8 = 1;\n";
    check(
        Language::Rust,
        src,
        &[
            ("// say hi", "comment"),
            ("fn", "keyword"),
            ("greet", "function"),
            ("String", "type"),
            ("let", "keyword"),
            ("42", "constant"),
            ("\"hi {name}\"", "string"),
            ("MAX", "constant"),
        ],
    );
}

#[test]
fn toml() {
    let src = "# deps\n[package]\nname = \"silo\"\nversion = 3\nenabled = true\n";
    check(Language::Toml, src, &[("# deps", "comment"), ("name", "property"), ("\"silo\"", "string"), ("3", "number"), ("true", "boolean")]);
}

#[test]
fn json() {
    let src = "{\"key\": \"value\", \"n\": 1.5, \"ok\": true, \"none\": null}";
    check(Language::Json, src, &[("\"key\"", "string"), ("\"value\"", "string"), ("1.5", "number"), ("true", "constant"), ("null", "constant")]);
}

#[test]
fn markdown() {
    let src = "# Title\n\nSome *emph* and `code` with [link](https://x.dev).\n\n- item\n";
    check(
        Language::Markdown,
        src,
        &[
            ("Title", "text.title"),
            ("#", "punctuation.special"),
            ("emph", "text.emphasis"),
            ("code", "text.literal"),
            ("https://x.dev", "text.uri"),
            ("-", "punctuation.special"),
        ],
    );
}

#[test]
fn javascript() {
    let src = "// note\nfunction add(a, b) { return a + b; }\nconst s = 'str';\nconsole.log(s);\n";
    check(
        Language::JavaScript,
        src,
        &[("// note", "comment"), ("function", "keyword"), ("add", "function"), ("return", "keyword"), ("'str'", "string"), ("log", "function")],
    );
}

#[test]
fn typescript() {
    let src = "// t\ninterface Point { x: number }\nfunction len(p: Point): number { return p.x; }\nconst s: string = \"a\";\n";
    check(
        Language::TypeScript,
        src,
        &[("// t", "comment"), ("interface", "keyword"), ("Point", "type"), ("number", "type"), ("len", "function"), ("\"a\"", "string")],
    );
}

#[test]
fn tsx() {
    let src = "// c\nexport function app(): JSX.Element { return <div className=\"a\">hi</div>; }\n";
    check(
        Language::Tsx,
        src,
        &[("// c", "comment"), ("export", "keyword"), ("app", "function"), ("div", "tag"), ("className", "attribute"), ("\"a\"", "string")],
    );
}

#[test]
fn python() {
    let src = "# c\ndef area(r):\n    return 3.14 * r * r\nprint(\"x\")\n";
    check(
        Language::Python,
        src,
        &[
            ("# c", "comment"),
            ("def", "keyword"),
            ("area", "function"),
            ("return", "keyword"),
            ("3.14", "number"),
            ("\"x\"", "string"),
            ("print", "function"),
        ],
    );
}

#[test]
fn go() {
    let src = "// c\npackage main\nfunc add(a int) int { return a }\nfunc main() { fmt.Println(\"hi\"); add(1) }\n";
    check(
        Language::Go,
        src,
        &[
            ("// c", "comment"),
            ("package", "keyword"),
            ("func", "keyword"),
            ("add", "function"),
            ("int", "type"),
            ("Println", "function"),
            ("\"hi\"", "string"),
        ],
    );
}

#[test]
fn bash() {
    let src = "#!/bin/bash\n# c\nif [ -n \"$X\" ]; then\n  echo 'hi'\nfi\ngreet() { ls; }\n";
    check(
        Language::Bash,
        src,
        &[("# c", "comment"), ("if", "keyword"), ("echo", "function"), ("'hi'", "string"), ("greet", "function"), ("fi", "keyword")],
    );
}

#[test]
fn c() {
    let src = "/* c */\n#include <stdio.h>\nint main(void) { printf(\"hi\"); return 0; }\n";
    check(
        Language::C,
        src,
        &[
            ("/* c */", "comment"),
            ("#include", "keyword"),
            ("int", "type"),
            ("main", "function"),
            ("\"hi\"", "string"),
            ("return", "keyword"),
            ("0", "number"),
        ],
    );
}

#[test]
fn cpp() {
    let src = "// c\nnamespace app { class Box {}; }\nint main() { auto s = std::string(\"x\"); return 0; }\n";
    check(
        Language::Cpp,
        src,
        &[("// c", "comment"), ("namespace", "keyword"), ("class", "keyword"), ("main", "function"), ("\"x\"", "string"), ("return", "keyword")],
    );
}

#[test]
fn css() {
    let src = "/* c */\n.btn > a { color: red; margin: 4px; }\n@media screen { p { width: calc(1px); } }\n";
    check(
        Language::Css,
        src,
        &[
            ("/* c */", "comment"),
            ("btn", "property"),
            ("color", "property"),
            ("4", "number"),
            ("@media", "keyword"),
            ("calc", "function"),
            ("a", "tag"),
        ],
    );
}

#[test]
fn html() {
    let src = "<!-- c -->\n<a href=\"/x\">link</a>\n";
    check(Language::Html, src, &[("<!-- c -->", "comment"), ("a", "tag"), ("href", "attribute"), ("/x", "string")]);
}

#[test]
fn yaml() {
    let src = "# c\nname: silo\ncount: 3\nok: true\nlist:\n  - \"a\"\n";
    check(
        Language::Yaml,
        src,
        &[("# c", "comment"), ("name", "property"), ("silo", "string"), ("3", "number"), ("true", "boolean"), ("\"a\"", "string")],
    );
}

#[test]
fn diff() {
    let src = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n";
    check(Language::Diff, src, &[("-old", "diff.minus"), ("+new", "diff.plus"), ("@@ -1 +1 @@", "attribute")]);
}

#[test]
fn markdown_fence_injects_the_fence_language() {
    let src = "Text\n\n```rust\nfn main() { let s = \"x\"; }\n```\n\n```python title=\"a\"\ndef f(): pass\n```\n\n```nope\nfn x\n```\n";
    check(Language::Markdown, src, &[("fn", "keyword"), ("main", "function"), ("\"x\"", "string"), ("def", "keyword")]);
    // An unknown language leaves the code plain (the fence's `@none`).
    let spans = highlight(Language::Markdown, src);
    let unknown = src.find("fn x").unwrap();
    assert_eq!(at(&spans, unknown), None);
}

#[test]
fn markdown_fence_inside_a_quote_skips_the_markers() {
    let src = "> ```rust\n> let a = 1;\n> ```\n";
    check(Language::Markdown, src, &[("let", "keyword")]);
    let spans = highlight(Language::Markdown, src);
    let marker = src.find("> let").unwrap();
    assert_ne!(at(&spans, marker).and_then(Capture::key), Some("keyword"));
}

#[test]
fn html_injects_script_and_style() {
    let src = "<script>\nconst x = 'js';\n</script>\n<style>\np { color: red; }\n</style>\n";
    check(Language::Html, src, &[("script", "tag"), ("const", "keyword"), ("'js'", "string"), ("color", "property")]);
}

#[test]
fn detects_by_name_extension_and_shebang() {
    let cases: &[(&str, Option<&str>, Option<Language>)] = &[
        ("src/main.rs", None, Some(Language::Rust)),
        ("/a/Cargo.toml", None, Some(Language::Toml)),
        ("Cargo.lock", None, Some(Language::Toml)),
        ("Makefile", None, Some(Language::Bash)),
        ("x/run.sh", None, Some(Language::Bash)),
        (".zshrc", None, Some(Language::Bash)),
        ("README.md", None, Some(Language::Markdown)),
        ("README.MD", None, Some(Language::Markdown)),
        ("a.json", None, Some(Language::Json)),
        ("a.mjs", None, Some(Language::JavaScript)),
        ("a.jsx", None, Some(Language::JavaScript)),
        ("a.ts", None, Some(Language::TypeScript)),
        ("a.tsx", None, Some(Language::Tsx)),
        ("a.py", None, Some(Language::Python)),
        ("a.go", None, Some(Language::Go)),
        ("a.c", None, Some(Language::C)),
        ("a.h", None, Some(Language::C)),
        ("a.hpp", None, Some(Language::Cpp)),
        ("a.cc", None, Some(Language::Cpp)),
        ("a.css", None, Some(Language::Css)),
        ("index.html", None, Some(Language::Html)),
        ("ci.yml", None, Some(Language::Yaml)),
        ("fix.patch", None, Some(Language::Diff)),
        ("notes.txt", None, None),
        (".gitignore", None, None),
        ("script", Some("#!/usr/bin/env python3"), Some(Language::Python)),
        ("script", Some("#!/bin/sh -e"), Some(Language::Bash)),
        ("script", Some("#!/usr/bin/env -S node --no-warnings"), Some(Language::JavaScript)),
        ("script", Some("#!/usr/bin/env -S uv run --script"), Some(Language::Python)),
        ("script", Some("#!/usr/bin/perl"), None),
        ("script", Some("no shebang"), None),
        ("a.rs", Some("#!/bin/sh"), Some(Language::Rust)),
    ];
    for &(path, first, want) in cases {
        assert_eq!(Language::detect(path, first), want, "{path} {first:?}");
    }
}

#[test]
fn fence_names_map_to_languages() {
    for (name, want) in [
        ("rust", Some(Language::Rust)),
        ("rs", Some(Language::Rust)),
        ("rust,ignore", Some(Language::Rust)),
        ("{.python}", Some(Language::Python)),
        ("TS", Some(Language::TypeScript)),
        ("c++", Some(Language::Cpp)),
        ("sh", Some(Language::Bash)),
        ("yml", Some(Language::Yaml)),
        ("text", None),
        ("", None),
    ] {
        assert_eq!(Language::from_name(name), want, "{name:?}");
    }
}

#[test]
fn captures_take_the_longest_dotted_prefix() {
    assert_eq!(key_for("function"), Some("function"));
    assert_eq!(key_for("function.method"), Some("function"));
    assert_eq!(key_for("function.method.call"), Some("function"));
    assert_eq!(key_for("string.special.key"), Some("string.special"));
    assert_eq!(key_for("string.escape"), Some("string.escape"));
    assert_eq!(key_for("punctuation.bracket"), Some("punctuation"));
    assert_eq!(key_for("comment.documentation"), Some("comment.documentation"));
    assert_eq!(key_for("none"), Some(NONE));
    assert_eq!(key_for("spell"), None);
    assert_eq!(key_for("functional"), None, "a prefix ends at a dot");
    assert_eq!(key_for(""), None);
}

#[test]
fn unknown_captures_do_not_paint() {
    assert!(!capture::paints("_name"));
    assert!(!capture::paints("injection.content"));
    assert!(!capture::paints("glimmer"));
    assert!(capture::paints("keyword"));
}

#[test]
fn guards_skip_large_files_and_long_lines() {
    assert!(highlightable("fn main() {}"));
    let long_line = format!("let x = \"{}\";", "a".repeat(MAX_LINE_CHARS));
    assert!(!highlightable(&long_line));
    assert!(highlight(Language::Rust, &long_line).is_empty());
    // 10k multi-byte characters is more bytes but not more characters.
    assert!(highlightable(&"é".repeat(MAX_LINE_CHARS)));
    let big = "let x = 1;\n".repeat(MAX_BYTES / 11 + 1);
    assert!(big.len() > MAX_BYTES);
    assert!(highlight(Language::Rust, &big).is_empty());
    let fits = "let x = 1;\n".repeat(MAX_BYTES / 11 - 1);
    assert!(!highlight(Language::Rust, &fits).is_empty());
}

#[test]
fn empty_and_broken_sources_are_fine() {
    assert!(highlight(Language::Rust, "").is_empty());
    for lang in Language::ALL {
        let _ = highlight(lang, "}}} ((( \"unterminated\n<<<");
    }
    // Deep block quotes overflowed the upstream markdown scanner.
    let _ = highlight(Language::Markdown, &">".repeat(300));
}

#[test]
fn by_line_splits_spans_at_newlines() {
    let src = "/* a\nb */ x\n";
    let spans = highlight(Language::C, src);
    let lines = by_line(src, &spans);
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0].iter().map(|(r, c)| (r.clone(), c.name())).collect::<Vec<_>>(), [(0..4, "comment")]);
    assert_eq!(lines[1][0].0, 0..4);
    assert!(lines[2].is_empty());
}

#[test]
fn every_key_reads_on_every_theme() {
    use crate::{terminal_prefs::THEMES, theme};
    for t in THEMES {
        for &(key, style) in theme::SYNTAX {
            let color = theme::syntax_color_in(&t.ansi, t.foreground, t.background, style.ink, t.background);
            let ratio = theme::contrast(color, t.background);
            assert!(ratio >= theme::SYNTAX_CONTRAST - 0.01, "{} {key}: {color:06x} {ratio}", t.id);
        }
    }
}

#[test]
fn styles_follow_the_theme() {
    let src = "fn main() {}";
    let styled = highlight_styles(Language::Rust, src, crate::theme::background());
    let keyword = styled.iter().find(|(r, _)| *r == (0..2)).expect("fn is styled");
    assert!(keyword.1.color.is_some());
    let title = capture_style(Capture("text.title"), 0).unwrap();
    assert_eq!(title.font_weight, Some(gpui::FontWeight::BOLD));
    assert_eq!(capture_style(Capture("none"), 0), None);
    assert_eq!(capture_style(Capture("spell"), 0), None);
}
