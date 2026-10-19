use silo_vt::{RestoreOptions, TerminalCore, snapshot};

/// Every row of scrollback plus the active area, as text with styling bits.
pub fn dump(vt: &mut TerminalCore) -> Vec<String> {
    let total = vt.scroll_state().unwrap().total;
    let mut lines = Vec::new();
    let mut row = 0;
    while row < total {
        vt.scroll_to_row(row);
        let frame = vt.frame().unwrap();
        let offset = frame.scroll.offset;
        let columns = usize::from(frame.size.columns);
        for (index, line) in frame.cells.chunks(columns).enumerate() {
            let absolute = offset + index as u64;
            if absolute < row {
                continue;
            }
            let mut text = String::new();
            for cell in line {
                text.push_str(if cell.text.is_empty() { " " } else { &cell.text });
                if cell.bold {
                    text.push('\u{1}');
                }
                text.push_str(&format!("{:06x}", cell.foreground));
            }
            lines.push(text);
        }
        row = offset + frame.scroll.len;
    }
    vt.scroll_to_row(u64::MAX);
    lines
}

/// Visible cells with colors, then cursor column/row/visible, then the
/// alternate screen and mouse tracking flags.
pub type Visible = (Vec<String>, (u16, u16, bool), (bool, bool));

pub fn visible(vt: &mut TerminalCore) -> Visible {
    let frame = vt.frame().unwrap();
    let columns = usize::from(frame.size.columns);
    let rows = frame
        .cells
        .chunks(columns)
        .map(|row| row.iter().map(|c| format!("{}|{:06x}|{:06x}|{}", c.text, c.foreground, c.background, c.bold)).collect())
        .collect();
    (rows, (frame.cursor.column, frame.cursor.row, frame.cursor.visible), (frame.alt_screen, frame.mouse_tracking))
}

pub fn restore_incrementally(bytes: &[u8], chunk: usize) -> TerminalCore {
    let head = snapshot::head_len(bytes).unwrap();
    let (mut vt, _) = TerminalCore::restore(&bytes[..head], RestoreOptions::default()).unwrap();
    let mut finished = false;
    for piece in bytes[head..].chunks(chunk) {
        finished = vt.restore_history(piece).unwrap().finished;
    }
    assert!(finished);
    assert!(!vt.restoring());
    vt
}
