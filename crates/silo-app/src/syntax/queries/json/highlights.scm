; Origin: tree-sitter-json 0.24.8 (crates.io), queries/highlights.scm
; https://github.com/tree-sitter/tree-sitter-json
; License: MIT, Copyright (c) 2014 Max Brunsfeld
; Copied verbatim from the crate's queries/ directory; see ../NOTICE.

(pair
  key: (_) @string.special.key)

(string) @string

(number) @number

[
  (null)
  (true)
  (false)
] @constant.builtin

(escape_sequence) @escape

(comment) @comment
