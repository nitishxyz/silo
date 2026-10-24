; Origin: tree-sitter-html 0.23.2 (crates.io), queries/highlights.scm
; https://github.com/tree-sitter/tree-sitter-html
; License: MIT, Copyright (c) 2014 Max Brunsfeld
; Copied verbatim from the crate's queries/ directory; see ../NOTICE.

(tag_name) @tag
(erroneous_end_tag_name) @tag.error
(doctype) @constant
(attribute_name) @attribute
(attribute_value) @string
(comment) @comment

[
  "<"
  ">"
  "</"
  "/>"
] @punctuation.bracket
