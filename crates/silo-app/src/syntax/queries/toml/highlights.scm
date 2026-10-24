; Origin: tree-sitter-toml-ng 0.7.0 (crates.io), queries/highlights.scm
; https://github.com/tree-sitter-grammars/tree-sitter-toml
; License: MIT, Copyright (c) Ika <ikatyang@gmail.com>
; Modified: a pair's bare key captures @property; upstream captured the whole
; pair, so the key kept @type. See ../NOTICE.

; Properties
;-----------

(bare_key) @type

(quoted_key) @string

(pair
  (bare_key) @property)

(pair
  (dotted_key
    (bare_key) @property))

; Literals
;---------

(boolean) @boolean

(comment) @comment

(string) @string

[
  (integer)
  (float)
] @number

[
  (offset_date_time)
  (local_date_time)
  (local_date)
  (local_time)
] @string.special

; Punctuation
;------------

[
  "."
  ","
] @punctuation.delimiter

"=" @operator

[
  "["
  "]"
  "[["
  "]]"
  "{"
  "}"
] @punctuation.bracket
