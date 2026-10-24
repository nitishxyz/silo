; Origin: tree-sitter-javascript 0.25.0 (crates.io), queries/highlights-jsx.scm
; https://github.com/tree-sitter/tree-sitter-javascript
; License: MIT, Copyright (c) 2014 Max Brunsfeld
; Copied verbatim from the crate's queries/ directory; see ../NOTICE.

(jsx_opening_element (identifier) @tag (#match? @tag "^[a-z][^.]*$"))
(jsx_closing_element (identifier) @tag (#match? @tag "^[a-z][^.]*$"))
(jsx_self_closing_element (identifier) @tag (#match? @tag "^[a-z][^.]*$"))

(jsx_attribute (property_identifier) @attribute)
(jsx_opening_element (["<" ">"]) @punctuation.bracket)
(jsx_closing_element (["</" ">"]) @punctuation.bracket)
(jsx_self_closing_element (["<" "/>"]) @punctuation.bracket)
