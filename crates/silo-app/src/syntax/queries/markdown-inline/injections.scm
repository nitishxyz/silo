; Origin: tree-sitter-md, Zed's fork at b596e737286780d7bfa9fcddceaeeb754574b352,
; tree-sitter-markdown-inline/queries/injections.scm (the same rev Zed pins)
; https://github.com/zed-industries/tree-sitter-markdown
; License: MIT, Copyright (c) 2021 Matthias Deiml
; Copied verbatim from the crate's queries/ directory; see ../NOTICE.

((html_tag) @injection.content (#set! injection.language "html"))
((latex_block) @injection.content (#set! injection.language "latex"))
