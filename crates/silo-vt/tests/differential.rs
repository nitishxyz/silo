//! Differential fuzz: a terminal restored from a snapshot must stay
//! indistinguishable from its source under the same random VT input.

mod common;

use common::{dump, restore_incrementally, visible};
use silo_vt::{GridSize, KeyEvent, Modifiers, RestoreOptions, TerminalCore, TerminalDefaults, snapshot};
use std::fmt::Write as _;
use std::panic::{AssertUnwindSafe, catch_unwind};

const DEFAULT_SEED: u64 = 0x5110;
const DEFAULT_CASES: u64 = 200;
const LONG_CASES: u64 = 20_000;
const MAX_BYTES: usize = 8 * 1024;

/// Seeds pinned out of `Bytes::Small` runs for a known upstream bug; see
/// `byte_limited_history_after_reflow`.
// upstream ghostty: with a scrollback byte limit below ~16 MiB, reflow resizes
// prune history differently on a restored terminal than on its live source.
// Which seeds hit it depends on the OS page size (e.g. 0x5177 and 0x5188 only
// diverge with 4 KiB pages), so the default run only draws `Bytes::Large`.
const UPSTREAM: &[u64] = &[0x57c1, 0x5a4f, 0x5e24, 0x5fd1];

/// Range the scrollback byte limit is drawn from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Bytes {
    /// 16..=64 MiB: never trips the upstream byte-limit pruning bug.
    Large,
    /// 64 KiB..=16 MiB: exercises byte-limited pruning, upstream bug included.
    Small,
}

/// splitmix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).unwrap()
    }

    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

const FINALS: &[u8] = b"ABCDHJKLMPSTXZdfghlmnqrsu";
const MODES: &[u16] = &[1049, 1, 2004, 1002, 1006, 25];
const TEXT: &[&str] = &["中", "文", "😀", "👍🏽", "e\u{301}", "a\u{308}\u{332}", "é", "ß", "│", "\u{200d}"];
const C0: &[&str] = &["\r", "\n", "\r\n", "\x08", "\t"];

fn params(rng: &mut Rng, out: &mut String, max: usize) {
    for i in 0..rng.below(max + 1) {
        if i > 0 {
            out.push(';');
        }
        write!(out, "{}", rng.below(120)).unwrap();
    }
}

fn sgr(rng: &mut Rng, out: &mut String) {
    out.push_str("\x1b[");
    for i in 0..rng.range(1, 3) {
        if i > 0 {
            out.push(';');
        }
        let n = rng.below(256);
        match rng.below(8) {
            0 => write!(out, "{}", rng.below(10)),
            1 => write!(out, "{}", 30 + rng.below(10)),
            2 => write!(out, "{}", 40 + rng.below(10)),
            3 => write!(out, "{}", 90 + rng.below(8)),
            4 => write!(out, "38;5;{n}"),
            5 => write!(out, "48;5;{n}"),
            6 => write!(out, "38;2;{n};{};{}", rng.below(256), rng.below(256)),
            _ => write!(out, "48;2;{n};{};{}", rng.below(256), rng.below(256)),
        }
        .unwrap();
    }
    out.push('m');
}

/// One grammar-biased piece, sometimes cut at a random byte so that
/// snapshots land mid-sequence.
fn piece(rng: &mut Rng) -> Vec<u8> {
    let mut s = String::new();
    match rng.below(100) {
        0..30 => {
            for _ in 0..rng.range(1, 40) {
                s.push(char::from(b' ' + u8::try_from(rng.below(95)).unwrap()));
            }
        }
        30..38 => {
            for _ in 0..rng.range(1, 6) {
                s.push_str(rng.pick(TEXT));
            }
        }
        38..57 => s.push_str(rng.pick(C0)),
        57..69 => {
            s.push_str("\x1b[");
            params(rng, &mut s, 3);
            s.push(char::from(*rng.pick(FINALS)));
        }
        69..75 => {
            let set = if rng.chance(50) { 'h' } else { 'l' };
            write!(s, "\x1b[?{}{set}", rng.pick(MODES)).unwrap();
        }
        75..85 => sgr(rng, &mut s),
        85..90 => {
            match rng.below(4) {
                0 => write!(s, "\x1b]0;title {}", rng.below(1000)),
                1 => write!(s, "\x1b]2;name {}", rng.below(1000)),
                2 => write!(s, "\x1b]7;file://host/tmp/{}", rng.below(1000)),
                _ if rng.chance(50) => write!(s, "\x1b]8;;https://x.test/{}", rng.below(1000)),
                _ => write!(s, "\x1b]8;;"),
            }
            .unwrap();
            s.push_str(if rng.chance(50) { "\x07" } else { "\x1b\\" });
        }
        90..93 => s.push_str(if rng.chance(50) { "\x1b7" } else { "\x1b8" }),
        93..95 => {
            if rng.chance(50) {
                write!(s, "\x1b[>{}u", rng.below(32)).unwrap();
            } else {
                s.push_str("\x1b[<u");
            }
        }
        _ => {
            return (0..rng.range(1, 8)).map(|_| u8::try_from(rng.below(256)).unwrap()).collect();
        }
    }
    let mut bytes = s.into_bytes();
    if bytes.len() > 1 && rng.chance(10) {
        bytes.truncate(rng.below(bytes.len()));
    }
    bytes
}

fn stream(rng: &mut Rng) -> Vec<u8> {
    let len = rng.below(MAX_BYTES + 1);
    let mut out = Vec::with_capacity(len + 64);
    while out.len() < len {
        out.extend(piece(rng));
    }
    out.truncate(len);
    out
}

fn chunks(rng: &mut Rng, bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < bytes.len() {
        let end = (start + rng.range(1, 512)).min(bytes.len());
        out.push((start, end));
        start = end;
    }
    out
}

fn grid(rng: &mut Rng) -> GridSize {
    let columns = u16::try_from(rng.range(20, 120)).unwrap();
    let rows = u16::try_from(rng.range(5, 50)).unwrap();
    GridSize::new(columns, rows).unwrap()
}

/// What the failing case was doing, for the panic report.
#[derive(Default)]
struct Trace {
    fed: Vec<u8>,
    stage: String,
}

fn feed(a: &mut TerminalCore, b: &mut TerminalCore, bytes: &[u8], trace: &mut Trace) {
    trace.fed.extend_from_slice(bytes);
    let replies = a.write_output(bytes).unwrap();
    assert_eq!(replies, b.write_output(bytes).unwrap(), "replies");
}

fn check(a: &mut TerminalCore, b: &mut TerminalCore) {
    assert_eq!(visible(a), visible(b), "visible");
    let up = KeyEvent { key: "up".into(), text: String::new(), modifiers: Modifiers::default(), release: false };
    assert_eq!(a.encode_key(&up).unwrap(), b.encode_key(&up).unwrap(), "encode_key(up)");
    assert_eq!(a.paste("x").unwrap(), b.paste("x").unwrap(), "paste");
    assert_eq!(dump(a), dump(b), "dump");
}

fn case(seed: u64, bytes: Bytes, trace: &mut Trace) {
    let mut rng = Rng(seed);
    let mut a = TerminalCore::new(grid(&mut rng)).unwrap();
    let scrollback_lines = rng.range(1, 2000) as u64;
    let scrollback_bytes = match bytes {
        Bytes::Large => rng.range(16 << 20, 64 << 20),
        Bytes::Small => rng.range(64 << 10, 16 << 20),
    } as u64;
    a.set_defaults(&TerminalDefaults { scrollback_lines, scrollback_bytes, ..Default::default() }).unwrap();

    let before = stream(&mut rng);
    for (index, (start, end)) in chunks(&mut rng, &before).into_iter().enumerate() {
        trace.stage = format!("before chunk {index}");
        trace.fed.extend_from_slice(&before[start..end]);
        a.write_output(&before[start..end]).unwrap();
    }
    trace.stage = "snapshot".into();
    let bytes = a.snapshot().unwrap();

    let after = stream(&mut rng);
    let mut pieces = chunks(&mut rng, &after).into_iter().enumerate();
    trace.stage = "restore".into();
    let mut b = if seed.is_multiple_of(3) {
        let head = snapshot::head_len(&bytes).unwrap();
        let options = RestoreOptions { compress_history: rng.chance(50), ..Default::default() };
        let (mut b, _) = TerminalCore::restore(&bytes[..head], options).unwrap();
        for part in bytes[head..].chunks(rng.range(1, 16 << 10)) {
            if let Some((index, (start, end))) = pieces.next() {
                trace.stage = format!("after chunk {index} (restoring)");
                feed(&mut a, &mut b, &after[start..end], trace);
            }
            b.restore_history(part).unwrap();
        }
        assert!(!b.restoring(), "history restore did not finish");
        b
    } else {
        restore_incrementally(&bytes, rng.range(1, 64 << 10))
    };
    check(&mut a, &mut b);

    for (index, (start, end)) in pieces {
        trace.stage = format!("after chunk {index}");
        if rng.chance(5) {
            let size = grid(&mut rng);
            a.resize(size).unwrap();
            b.resize(size).unwrap();
        }
        feed(&mut a, &mut b, &after[start..end], trace);
        check(&mut a, &mut b);
    }
    trace.stage = "end".into();
    check(&mut a, &mut b);
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        write!(out, "{byte:02x} ").unwrap();
        out
    })
}

fn run(base: u64, cases: u64, limits: &[Bytes]) {
    let mut failures = Vec::new();
    for index in 0..cases {
        let seed = base.wrapping_add(index);
        for &bytes in limits {
            if bytes == Bytes::Small && UPSTREAM.contains(&seed) {
                eprintln!("skipping pinned upstream seed {seed:#x} ({bytes:?})");
                continue;
            }
            let mut trace = Trace::default();
            if catch_unwind(AssertUnwindSafe(|| case(seed, bytes, &mut trace))).is_err() {
                let tail = &trace.fed[trace.fed.len().saturating_sub(64)..];
                let failure = format!(
                    "seed {seed:#x} ({bytes:?}, case {index} from base {base:#x}) at {}\nlast {} bytes fed: {}",
                    trace.stage,
                    tail.len(),
                    hex(tail),
                );
                eprintln!("divergence: {failure}");
                failures.push(failure);
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {cases} cases diverged:\n{}\nreproduce one: SILO_FUZZ_SEED=<seed> SILO_FUZZ_CASES=1 \
         cargo test -p silo-vt --test differential -- --ignored differential_long",
        failures.len(),
        failures.join("\n"),
    );
}

fn env_u64(name: &str) -> Option<u64> {
    let value = std::env::var(name).ok()?;
    let parsed = match value.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => value.parse(),
    };
    Some(parsed.unwrap_or_else(|_| panic!("{name}={value} is not a number")))
}

#[test]
fn differential() {
    run(DEFAULT_SEED, DEFAULT_CASES, &[Bytes::Large]);
}

#[test]
#[ignore = "long fuzz run, small byte limits included; SILO_FUZZ_CASES and SILO_FUZZ_SEED"]
fn differential_long() {
    let seed = env_u64("SILO_FUZZ_SEED").unwrap_or_else(|| {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
        u64::try_from(now.as_nanos() & u128::from(u64::MAX)).unwrap()
    });
    let cases = env_u64("SILO_FUZZ_CASES").unwrap_or(LONG_CASES);
    eprintln!("differential_long: SILO_FUZZ_SEED={seed:#x} SILO_FUZZ_CASES={cases}");
    run(seed, cases, &[Bytes::Large, Bytes::Small]);
}

#[test]
fn osc8_with_invalid_utf8_uri_keeps_frame_working() {
    let mut vt = TerminalCore::new(GridSize::new(32, 16).unwrap()).unwrap();
    vt.write_output(b"\x1b]8;;\xc1\x1bA\\").unwrap();
    vt.frame().unwrap();
    let mut restored = restore_incrementally(&vt.snapshot().unwrap(), 1 << 20);
    assert_eq!(visible(&mut vt), visible(&mut restored));
}

/// Minimized from seed 0x5fd1. Asserts the known upstream divergence so a fixed
/// libghostty fails here: then flip to `assert_eq!` and empty `UPSTREAM`.
// upstream ghostty: byte-limited scrollback prunes differently after restore + reflow.
#[test]
fn byte_limited_history_after_reflow() {
    let grid = |columns, rows| GridSize::new(columns, rows).unwrap();
    let mut a = TerminalCore::new(grid(56, 43)).unwrap();
    a.set_defaults(&TerminalDefaults { scrollback_bytes: 258_201, ..Default::default() }).unwrap();
    a.write_output(b"\x1b[65Hh\x1b[d").unwrap();
    let mut b = restore_incrementally(&a.snapshot().unwrap(), 1 << 20);
    assert_eq!(dump(&mut a), dump(&mut b));
    for size in [grid(104, 10), grid(85, 27)] {
        a.resize(size).unwrap();
        b.resize(size).unwrap();
    }
    assert_ne!(dump(&mut a).len(), dump(&mut b).len(), "upstream fixed: remove the pins");
}
