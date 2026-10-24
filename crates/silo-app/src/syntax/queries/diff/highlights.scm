; Origin: tree-sitter-diff 0.1.0 (crates.io), queries/highlights.scm
; https://github.com/the-mikedavis/tree-sitter-diff
; License: MIT, Copyright (c) 2021 Michael Davis
; Modified: additions and deletions capture @diff.plus / @diff.minus (Zed's theme
; names) instead of @string / @keyword; see ../NOTICE.

; These scopes are arbitrary and line up with good colors for the
; `tree-sitter highlight` command. Adapt them as you see fit.

[(addition) (new_file)] @diff.plus
[(deletion) (old_file)] @diff.minus

(commit) @constant
(location) @attribute
(command) @variable.builtin
