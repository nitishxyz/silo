; Origin: tree-sitter-md, Zed's fork at b596e737286780d7bfa9fcddceaeeb754574b352,
; tree-sitter-markdown-inline/queries/highlights.scm (the same rev Zed pins)
; https://github.com/zed-industries/tree-sitter-markdown
; License: MIT, Copyright (c) 2021 Matthias Deiml
; Copied verbatim from the crate's queries/ directory; see ../NOTICE.

;; From nvim-treesitter/nvim-treesitter
[
  (code_span)
  (link_title)
] @text.literal

[
  (emphasis_delimiter)
  (code_span_delimiter)
] @punctuation.delimiter

(emphasis) @text.emphasis

(strong_emphasis) @text.strong

[
  (link_destination)
  (uri_autolink)
] @text.uri

[
  (link_label)
  (link_text)
  (image_description)
] @text.reference

[
  (backslash_escape)
  (hard_line_break)
] @string.escape

(image ["!" "[" "]" "(" ")"] @punctuation.delimiter)
(inline_link ["[" "]" "(" ")"] @punctuation.delimiter)
(shortcut_link ["[" "]"] @punctuation.delimiter)

; NOTE: extension not enabled by default
; (wiki_link ["[" "|" "]"] @punctuation.delimiter)
