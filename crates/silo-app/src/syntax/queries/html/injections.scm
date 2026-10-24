; Origin: tree-sitter-html 0.23.2 (crates.io), queries/injections.scm
; https://github.com/tree-sitter/tree-sitter-html
; License: MIT, Copyright (c) 2014 Max Brunsfeld
; Copied verbatim from the crate's queries/ directory; see ../NOTICE.

((script_element
  (raw_text) @injection.content)
 (#set! injection.language "javascript"))

((style_element
  (raw_text) @injection.content)
 (#set! injection.language "css"))
