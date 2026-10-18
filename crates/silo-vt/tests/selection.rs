//! Pointer selection: press/drag/release select the dragged cells, frames
//! report them, and copy returns the text.

use std::time::{Duration, Instant};

use silo_vt::{GridSize, Modifiers, MouseAction, MouseButton, MouseEvent, RestoreOptions, TerminalCore};

const CW: u16 = 16;
const CH: u16 = 32;

fn grid(columns: u16, rows: u16) -> GridSize {
    GridSize { columns, rows, cell_width: CW, cell_height: CH }
}

fn mouse(action: MouseAction, column: u16, row: u16, shift: bool) -> MouseEvent {
    MouseEvent {
        action,
        button: MouseButton::Left,
        // Press at the cell's left edge, drags at its right edge, so the
        // cell is inside the selection (Ghostty's half-cell rule).
        x: if matches!(action, MouseAction::Press) { column * CW + 1 } else { (column + 1) * CW - 1 },
        y: row * CH + CH / 2,
        modifiers: Modifiers { shift, ..Modifiers::default() },
    }
}

fn selected_rows(vt: &mut TerminalCore) -> Vec<(usize, String)> {
    let frame = vt.frame().unwrap();
    let columns = usize::from(frame.size.columns);
    frame
        .cells
        .chunks(columns)
        .enumerate()
        .filter_map(|(row, cells)| {
            let text: String = cells.iter().filter(|c| c.selected).map(|c| if c.text.is_empty() { " " } else { c.text.as_str() }).collect();
            (!text.is_empty()).then_some((row, text))
        })
        .collect()
}

fn drag(vt: &mut TerminalCore, from: (u16, u16), to: (u16, u16), t: Instant) {
    vt.encode_mouse_at(mouse(MouseAction::Press, from.0, from.1, false), t).unwrap();
    vt.encode_mouse_at(mouse(MouseAction::Motion, to.0, to.1, false), t + Duration::from_millis(20)).unwrap();
    vt.encode_mouse_at(mouse(MouseAction::Release, to.0, to.1, false), t + Duration::from_millis(40)).unwrap();
}

fn check_drag(vt: &mut TerminalCore) {
    let t = Instant::now();
    // Frame once first, as the renderer does, so row caching is exercised.
    let _ = vt.frame().unwrap();
    drag(vt, (2, 1), (6, 2), t);
    let rows = selected_rows(vt);
    assert_eq!(rows.first().map(|r| r.0), Some(1), "{rows:?}");
    assert_eq!(rows.last().map(|r| r.0), Some(2), "{rows:?}");
    assert_eq!(vt.copy_selection().unwrap(), "cond line\nthird l");
    // A new drag elsewhere moves the highlight.
    drag(vt, (0, 0), (4, 0), t + Duration::from_secs(2));
    let rows = selected_rows(vt);
    assert_eq!(rows, vec![(0, "first".to_owned())]);
}

#[test]
fn drag_selects_and_follows() {
    let mut vt = TerminalCore::new(grid(40, 6)).unwrap();
    vt.write_output(b"first line\r\nsecond line\r\nthird line\r\n").unwrap();
    check_drag(&mut vt);
}

#[test]
fn drag_selects_on_restored_terminal() {
    let mut server = TerminalCore::new(GridSize { columns: 40, rows: 6, cell_width: 8, cell_height: 16 }).unwrap();
    server.write_output(b"first line\r\nsecond line\r\nthird line\r\n").unwrap();
    let snapshot = server.snapshot().unwrap();
    let head = silo_vt::snapshot::head_len(&snapshot).unwrap();
    let (mut vt, _) = TerminalCore::restore(&snapshot[..head], RestoreOptions { retain_continuation: false, compress_history: true }).unwrap();
    vt.restore_history(&snapshot[head..]).unwrap();
    vt.resize(grid(40, 6)).unwrap();
    check_drag(&mut vt);
}

#[test]
fn double_and_triple_click() {
    let mut vt = TerminalCore::new(grid(40, 6)).unwrap();
    vt.write_output(b"alpha beta gamma\r\n").unwrap();
    let t = Instant::now();
    let click = |vt: &mut TerminalCore, at: Instant| {
        vt.encode_mouse_at(mouse(MouseAction::Press, 7, 0, false), at).unwrap();
        vt.encode_mouse_at(mouse(MouseAction::Release, 7, 0, false), at + Duration::from_millis(10)).unwrap();
    };
    click(&mut vt, t);
    click(&mut vt, t + Duration::from_millis(100));
    assert_eq!(vt.copy_selection().unwrap(), "beta");
    click(&mut vt, t + Duration::from_millis(200));
    assert_eq!(vt.copy_selection().unwrap(), "alpha beta gamma");
}

#[test]
fn shift_click_extends_and_clear_drops() {
    let mut vt = TerminalCore::new(grid(40, 6)).unwrap();
    vt.write_output(b"first line\r\nsecond line\r\nthird line\r\n").unwrap();
    let t = Instant::now();
    drag(&mut vt, (0, 0), (4, 0), t);
    assert_eq!(vt.copy_selection().unwrap(), "first");
    vt.encode_mouse_at(mouse(MouseAction::Press, 5, 1, true), t + Duration::from_secs(1)).unwrap();
    vt.encode_mouse_at(mouse(MouseAction::Release, 5, 1, true), t + Duration::from_secs(1)).unwrap();
    assert_eq!(vt.copy_selection().unwrap(), "first line\nsecond");
    assert!(vt.clear_selection());
    assert!(selected_rows(&mut vt).is_empty());
    assert!(!vt.clear_selection());
}

#[test]
fn tracking_programs_get_plain_drags_shift_selects() {
    let mut vt = TerminalCore::new(grid(40, 6)).unwrap();
    vt.write_output(b"hello world\r\n\x1b[?1002h\x1b[?1006h").unwrap();
    let t = Instant::now();
    let bytes = vt.encode_mouse_at(mouse(MouseAction::Press, 1, 0, false), t).unwrap();
    assert!(bytes.starts_with(b"\x1b[<0;2;1M"), "{:?}", String::from_utf8_lossy(&bytes));
    vt.encode_mouse_at(mouse(MouseAction::Release, 1, 0, false), t).unwrap();
    let bytes = vt.encode_mouse_at(mouse(MouseAction::Press, 0, 0, true), t + Duration::from_secs(1)).unwrap();
    assert!(bytes.is_empty());
    vt.encode_mouse_at(mouse(MouseAction::Motion, 4, 0, true), t + Duration::from_secs(1)).unwrap();
    vt.encode_mouse_at(mouse(MouseAction::Release, 4, 0, true), t + Duration::from_secs(1)).unwrap();
    assert_eq!(vt.copy_selection().unwrap(), "hello");
}
