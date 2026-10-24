; Origin: tree-sitter-rust 0.24.2 (crates.io), queries/injections.scm
; https://github.com/tree-sitter/tree-sitter-rust
; License: MIT, Copyright (c) 2017 Maxim Sokolov
; Copied verbatim from the crate's queries/ directory; see ../NOTICE.

((macro_invocation
  (token_tree) @injection.content)
 (#set! injection.language "rust")
 (#set! injection.include-children))

((macro_rule
  (token_tree) @injection.content)
 (#set! injection.language "rust")
 (#set! injection.include-children))
