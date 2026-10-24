; Origin: tree-sitter-typescript 0.23.2 (crates.io), queries/highlights.scm
; https://github.com/tree-sitter/tree-sitter-typescript
; License: MIT, Copyright (c) 2017 Max Brunsfeld
; Used for typescript and tsx, layered with ../javascript/ as the grammar's
; tree-sitter.json does. Copied verbatim from the crate's queries/ directory; see ../NOTICE.

; Types

(type_identifier) @type
(predefined_type) @type.builtin

((identifier) @type
 (#match? @type "^[A-Z]"))

(type_arguments
  "<" @punctuation.bracket
  ">" @punctuation.bracket)

; Variables

(required_parameter (identifier) @variable.parameter)
(optional_parameter (identifier) @variable.parameter)

; Keywords

[ "abstract"
  "declare"
  "enum"
  "export"
  "implements"
  "interface"
  "keyof"
  "namespace"
  "private"
  "protected"
  "public"
  "type"
  "readonly"
  "override"
  "satisfies"
] @keyword
