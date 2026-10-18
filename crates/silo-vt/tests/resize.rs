//! Resize behaviour behind "content moves and gains extra space when the
//! window is resized": the terminal reflows soft-wrapped rows, and a prompt
//! line that fills the full width (starship's `$fill`, right prompts) is
//! such a row. Narrowing wraps it onto an extra row; the shell then redraws
//! its prompt below the reflowed copy. Explicit lines never move.

use silo_vt::{GridSize, TerminalCore};

fn grid(columns: u16, rows: u16) -> GridSize {
    GridSize { columns, rows, cell_width: 16, cell_height: 32 }
}

fn rows(vt: &mut TerminalCore) -> Vec<String> {
    let frame = vt.frame().unwrap();
    let columns = usize::from(frame.size.columns);
    frame
        .cells
        .chunks(columns)
        .map(|row| row.iter().map(|c| if c.text.is_empty() { " " } else { c.text.as_str() }).collect::<String>().trim_end().to_owned())
        .collect()
}

#[test]
fn explicit_lines_keep_their_cells_across_resizes() {
    let mut vt = TerminalCore::new(grid(30, 6)).unwrap();
    vt.write_output(b"0123456789\r\n|...|...|\r\n").unwrap();
    let before = rows(&mut vt);
    for columns in [24, 12, 18, 30, 40, 30] {
        vt.resize(grid(columns, 6)).unwrap();
        let now = rows(&mut vt);
        assert_eq!(now[0], before[0], "width {columns}");
        assert_eq!(now[1], before[1], "width {columns}");
    }
}

#[test]
fn a_full_width_prompt_line_reflows_onto_an_extra_row() {
    let mut vt = TerminalCore::new(grid(20, 6)).unwrap();
    // A starship-like prompt: a left part, `$fill` spaces to the right
    // edge, a right part, then the input line.
    vt.write_output(b"~/dev           12:00\r\n> ").unwrap();
    assert_eq!(rows(&mut vt)[..2], ["~/dev           12:0".to_owned(), "0".to_owned()]);
    let mut vt = TerminalCore::new(grid(21, 6)).unwrap();
    vt.write_output(b"~/dev           12:00\r\n> ").unwrap();
    assert_eq!(rows(&mut vt)[..2], ["~/dev           12:00".to_owned(), ">".to_owned()]);
    // Narrower: the soft-wrapped fill row now takes two rows, pushing the
    // input line down — this is the "extra space" seen during a resize
    // until the shell redraws its prompt.
    vt.resize(grid(15, 6)).unwrap();
    assert_eq!(rows(&mut vt)[..3], ["~/dev".to_owned(), " 12:00".to_owned(), ">".to_owned()]);
    // Back to the original width the row joins up again.
    vt.resize(grid(21, 6)).unwrap();
    assert_eq!(rows(&mut vt)[..2], ["~/dev           12:00".to_owned(), ">".to_owned()]);
}

#[test]
fn probe_marked_prompt() {
    let cases: [(&str, &[u8], u16, u16); 7] = [
        ("fill narrow redraw=1", b"out\r\n\x1b]133;A;redraw=1\x07~/dev           12:00\r\n\x1b]133;P;k=s\x07> \x1b]133;B\x07ls", 21, 15),
        ("short widen", b"out\r\n\x1b]133;A\x07> \x1b]133;B\x07ls", 21, 30),
        ("short narrow", b"out\r\n\x1b]133;A\x07> \x1b]133;B\x07ls", 21, 15),
        ("fill narrow no P", b"out\r\n\x1b]133;A\x07~/dev           12:00\r\n> \x1b]133;B\x07ls", 21, 15),
        ("fill narrow P", b"out\r\n\x1b]133;A\x07~/dev           12:00\r\n\x1b]133;P;k=s\x07> \x1b]133;B\x07ls", 21, 15),
        ("two-line narrow nofill", b"out\r\n\x1b]133;A\x07~/dev\r\n> \x1b]133;B\x07ls", 21, 15),
        ("two-line widen", b"out\r\n\x1b]133;A\x07~/dev\r\n> \x1b]133;B\x07ls", 21, 30),
    ];
    for (name, bytes, from, to) in cases {
        let mut vt = TerminalCore::new(grid(from, 6)).unwrap();
        vt.write_output(bytes).unwrap();
        vt.resize(grid(to, 6)).unwrap();
        eprintln!("{name:24} {:?}", rows(&mut vt));
    }
}
