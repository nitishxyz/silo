; Origin: tree-sitter-javascript 0.25.0 (crates.io), queries/highlights-params.scm
; https://github.com/tree-sitter/tree-sitter-javascript
; License: MIT, Copyright (c) 2014 Max Brunsfeld
; Copied verbatim from the crate's queries/ directory; see ../NOTICE.

(formal_parameters
  [
    (identifier) @variable.parameter
    (array_pattern
      (identifier) @variable.parameter)
    (object_pattern
      [
        (pair_pattern value: (identifier) @variable.parameter)
        (shorthand_property_identifier_pattern) @variable.parameter
      ])
  ]
)
