; Origin: tree-sitter-cpp 0.23.4 (crates.io), queries/injections.scm
; https://github.com/tree-sitter/tree-sitter-cpp
; License: MIT, Copyright (c) 2014 Max Brunsfeld
; Copied verbatim from the crate's queries/ directory; see ../NOTICE.

(raw_string_literal
  delimiter: (raw_string_delimiter) @injection.language
  (raw_string_content) @injection.content)
