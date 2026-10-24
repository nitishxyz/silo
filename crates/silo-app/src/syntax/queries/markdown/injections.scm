; Origin: tree-sitter-md, Zed's fork at b596e737286780d7bfa9fcddceaeeb754574b352,
; tree-sitter-markdown/queries/injections.scm (the same rev Zed pins)
; https://github.com/zed-industries/tree-sitter-markdown
; License: MIT, Copyright (c) 2021 Matthias Deiml
; Copied verbatim from the crate's queries/ directory; see ../NOTICE.

(fenced_code_block
  (info_string
    (language) @injection.language)
  (code_fence_content) @injection.content)

((html_block) @injection.content (#set! injection.language "html"))

(document . (section . (thematic_break) (_) @injection.content (thematic_break)) (#set! injection.language "yaml"))

([(minus_metadata) (plus_metadata)] @injection.content (#set! injection.language "yml"))

((inline) @injection.content (#set! injection.language "markdown_inline"))
