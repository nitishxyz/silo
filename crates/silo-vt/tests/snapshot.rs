mod common;

use common::{dump, restore_incrementally, visible};
use silo_vt::{GridSize, RestoreOptions, TerminalCore, snapshot};

fn grid(columns: u16, rows: u16) -> GridSize {
    GridSize::new(columns, rows).unwrap()
}

fn history(lines: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..lines {
        out.extend_from_slice(format!("\x1b[3{}mline {i:06} \x1b[1mbold\x1b[0m tail\r\n", i % 8).as_bytes());
    }
    out
}

#[test]
fn round_trip_with_history_and_unfinished_sequence() {
    let mut source = TerminalCore::new(grid(80, 24)).unwrap();
    source.write_output(&history(3000)).unwrap();
    // Leave an SGR sequence unfinished; its continuation must survive.
    source.write_output(b"prompt$ \x1b[3").unwrap();
    assert!(!source.at_ground().unwrap());
    let bytes = source.snapshot().unwrap();
    assert_eq!(snapshot::version(&bytes).unwrap(), 1);

    let mut restored = restore_incrementally(&bytes, 4096);
    assert_eq!(restored.size().columns, 80);
    assert_eq!(dump(&mut source), dump(&mut restored));

    // Finishing the sequence must behave identically on both.
    for vt in [&mut source, &mut restored] {
        vt.write_output(b"1mred\x1b[0m done\r\n").unwrap();
    }
    assert_eq!(visible(&mut source), visible(&mut restored));
    assert_eq!(dump(&mut source), dump(&mut restored));
}

#[test]
fn modes_alt_screen_and_keyboard_survive() {
    let mut source = TerminalCore::new(grid(60, 20)).unwrap();
    source.write_output(&history(200)).unwrap();
    // Alt screen, kitty keyboard flags, mouse SGR tracking, bracketed paste,
    // application cursor keys, focus events, cursor style.
    source.write_output(b"\x1b[?1049h\x1b[H\x1b[>1u\x1b[?1002h\x1b[?1006h\x1b[?2004h\x1b[?1h\x1b[?1004h\x1b[5 q\x1b[2;3Hhi").unwrap();
    let bytes = source.snapshot().unwrap();
    let mut restored = restore_incrementally(&bytes, 1 << 20);
    assert_eq!(visible(&mut source), visible(&mut restored));

    let key = silo_vt::KeyEvent { key: "up".into(), text: String::new(), modifiers: silo_vt::Modifiers::default(), release: false };
    assert_eq!(source.encode_key(&key).unwrap(), restored.encode_key(&key).unwrap());
    let ctrl_c = silo_vt::KeyEvent {
        key: "c".into(),
        text: String::new(),
        modifiers: silo_vt::Modifiers { control: true, ..Default::default() },
        release: false,
    };
    assert_eq!(source.encode_key(&ctrl_c).unwrap(), restored.encode_key(&ctrl_c).unwrap());
    assert_eq!(source.paste("x").unwrap(), restored.paste("x").unwrap());
    assert_eq!(source.encode_focus(true).unwrap(), restored.encode_focus(true).unwrap());
    let click = silo_vt::MouseEvent {
        action: silo_vt::MouseAction::Press,
        button: silo_vt::MouseButton::Left,
        x: 20,
        y: 40,
        modifiers: silo_vt::Modifiers::default(),
    };
    assert_eq!(source.encode_mouse(click).unwrap(), restored.encode_mouse(click).unwrap());

    for vt in [&mut source, &mut restored] {
        vt.write_output(b"\x1b[?1049l").unwrap();
    }
    assert_eq!(dump(&mut source), dump(&mut restored));
}

#[test]
fn history_prepends_while_live_output_arrives() {
    let mut source = TerminalCore::new(grid(100, 30)).unwrap();
    source.write_output(&history(5000)).unwrap();
    let bytes = source.snapshot().unwrap();
    let head = snapshot::head_len(&bytes).unwrap();

    let (mut restored, history_rows) =
        TerminalCore::restore(&bytes[..head], RestoreOptions { compress_history: true, ..Default::default() }).unwrap();
    assert!(history_rows > 0);
    assert!(restored.restoring());

    // Interleave: each history chunk is followed by live output that scrolls
    // new rows into history at the same time older rows are prepended.
    let mut live = 0;
    for piece in bytes[head..].chunks(8192) {
        let out = format!("live {live}\r\n");
        live += 1;
        source.write_output(out.as_bytes()).unwrap();
        restored.write_output(out.as_bytes()).unwrap();
        restored.restore_history(piece).unwrap();
        let _ = restored.frame().unwrap();
    }
    assert!(!restored.restoring());
    assert!(live > 2, "expected several history chunks, got {live}");
    assert_eq!(visible(&mut source), visible(&mut restored));
    assert_eq!(dump(&mut source), dump(&mut restored));
}

#[test]
fn resize_after_restore_reflows_identically() {
    let mut source = TerminalCore::new(grid(80, 24)).unwrap();
    source.write_output(&history(500)).unwrap();
    source.write_output(&[b'x'; 300]).unwrap();
    let bytes = source.snapshot().unwrap();
    let mut restored = restore_incrementally(&bytes, 65536);
    for vt in [&mut source, &mut restored] {
        vt.resize(grid(50, 20)).unwrap();
        vt.write_output(b"after\r\n").unwrap();
    }
    assert_eq!(dump(&mut source), dump(&mut restored));
}

#[test]
fn encode_time_for_large_history() {
    for lines in [10_000usize, 100_000] {
        let mut source = TerminalCore::new(grid(120, 40)).unwrap();
        source
            .set_defaults(&silo_vt::TerminalDefaults {
                scrollback_lines: lines as u64 + 100,
                scrollback_bytes: 512 * 1024 * 1024,
                ..Default::default()
            })
            .unwrap();
        source.write_output(&history(lines)).unwrap();
        let start = std::time::Instant::now();
        let bytes = source.snapshot().unwrap();
        let encode = start.elapsed();
        let start = std::time::Instant::now();
        let mut restored = restore_incrementally(&bytes, 1 << 16);
        let decode = start.elapsed();
        let rows = restored.scroll_state().unwrap().total;
        eprintln!("{lines} lines: {} KiB, encode {encode:?}, decode {decode:?}, rows {rows}", bytes.len() / 1024);
        assert!(rows as usize >= lines);
    }
}
