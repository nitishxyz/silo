//! Host offers inside terminal output: a private OSC,
//! `ESC ] 7717 ; offer ; <ticket> ; <name> (BEL | ESC \)`, printed by
//! `silo` on a machine reached over SSH so the local server can offer to
//! link it. [`OfferScanner`] finds and strips it from a byte stream, with
//! the program resume reports of OSC 7577 (docs/RESUME.md), which the
//! server takes for itself the same way.

/// The OSC number.
pub const OSC: &str = "7717";
/// Longest sequence (introducer to terminator) recognised; longer ones are
/// passed through untouched. OSC 7577 asks for at least 2048 bytes.
pub const MAX_LEN: usize = 4096;

const PREFIX: &[u8] = b"\x1b]7717;";
const RESUME_PREFIX: &[u8] = b"\x1b]7577;";
/// Both prefixes have this length.
const PREFIX_LEN: usize = 7;
/// CAN and SUB abort any escape sequence.
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;

/// The sequence offering `ticket` (a `silo1…` pairing ticket) under `name`.
/// Control characters in either are dropped; `;` is dropped from the ticket.
#[must_use]
pub fn osc(ticket: &str, name: &str) -> String {
    let ticket: String = ticket.chars().filter(|c| !c.is_control() && *c != ';').collect();
    let name: String = name.chars().filter(|c| !c.is_control()).collect();
    format!("\x1b]{OSC};offer;{ticket};{name}\x07")
}

/// A parsed offer; the ticket is not yet validated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub ticket: String,
    pub name: String,
}

/// A sequence the scanner took out of the stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Found {
    Offer(Offer),
    /// The body of an OSC 7577 report, unparsed (its `mac` covers the
    /// exact bytes).
    Resume(Vec<u8>),
}

/// Streaming scanner: feed output chunks, get them back without any
/// `OSC 7717` or `OSC 7577` sequence (even split across chunks) and what
/// they held.
#[derive(Default)]
pub struct OfferScanner {
    /// Bytes held back: a prefix of [`PREFIX`], or a whole sequence so far.
    held: Vec<u8>,
}

impl OfferScanner {
    /// Nothing is held back: output so far has been passed on whole.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.held.is_empty()
    }

    /// Give up on held bytes, appending them to `out` unchanged.
    pub fn release(&mut self, out: &mut Vec<u8>) {
        out.append(&mut self.held);
    }

    /// Append `input` minus any OSC 7717 and 7577 sequences to `out`. Bytes
    /// that may start a sequence are held until the next call decides them.
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) -> Vec<Found> {
        let mut offers = Vec::new();
        let mut i = 0;
        while i < input.len() {
            let byte = input[i];
            if self.held.is_empty() {
                // Fast path: copy everything up to the next ESC.
                let end = input[i..].iter().position(|&b| b == 0x1b).map_or(input.len(), |p| i + p);
                out.extend_from_slice(&input[i..end]);
                i = end;
                if i < input.len() {
                    self.held.push(0x1b);
                    i += 1;
                }
                continue;
            }
            if self.held.len() < PREFIX_LEN {
                let at = self.held.len();
                let fits = |prefix: &[u8]| prefix[..at] == self.held[..] && prefix[at] == byte;
                if fits(PREFIX) || fits(RESUME_PREFIX) {
                    self.held.push(byte);
                    i += 1;
                } else {
                    // Not ours: release and rescan this byte from ground.
                    out.append(&mut self.held);
                }
                continue;
            }
            let after_esc = self.held.last() == Some(&0x1b);
            match byte {
                CAN | SUB => {
                    // Aborted: release it all and pass the byte on.
                    out.append(&mut self.held);
                    continue;
                }
                0x07 if !after_esc => {
                    offers.extend(found(&self.held, self.held.len()));
                    self.held.clear();
                }
                b'\\' if after_esc => {
                    offers.extend(found(&self.held, self.held.len() - 1));
                    self.held.clear();
                }
                _ if after_esc => {
                    // ESC cancels the OSC: release all but that ESC, which
                    // may start the next sequence.
                    self.held.pop();
                    out.append(&mut self.held);
                    self.held.push(0x1b);
                    continue;
                }
                _ => self.held.push(byte),
            }
            i += 1;
            if self.held.len() >= MAX_LEN {
                out.append(&mut self.held);
            }
        }
        offers
    }
}

/// What the held sequence (body ending at `end`) holds.
fn found(held: &[u8], end: usize) -> Option<Found> {
    let body = &held[PREFIX_LEN..end];
    if held.starts_with(RESUME_PREFIX) {
        return Some(Found::Resume(body.to_vec()));
    }
    parse(body).map(Found::Offer)
}

fn parse(body: &[u8]) -> Option<Offer> {
    let body = std::str::from_utf8(body).ok()?;
    let mut parts = body.splitn(3, ';');
    if parts.next()? != "offer" {
        return None;
    }
    let ticket = parts.next()?.trim();
    let name = parts.next()?.trim();
    (!ticket.is_empty()).then(|| Offer { ticket: ticket.to_owned(), name: name.to_owned() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(chunks: &[&[u8]]) -> (Vec<u8>, Vec<Offer>) {
        let (out, found) = scan_all(chunks);
        let offers = found
            .into_iter()
            .filter_map(|f| match f {
                Found::Offer(offer) => Some(offer),
                Found::Resume(_) => None,
            })
            .collect();
        (out, offers)
    }

    fn scan_all(chunks: &[&[u8]]) -> (Vec<u8>, Vec<Found>) {
        let mut scanner = OfferScanner::default();
        let (mut out, mut found) = (Vec::new(), Vec::new());
        for chunk in chunks {
            found.extend(scanner.feed(chunk, &mut out));
        }
        (out, found)
    }

    #[test]
    fn resume_reports_are_taken_out_whole() {
        let input = b"a\x1b]7577;pid=1:time=2:clear=1:mac=ab\x1b\\b\x1b]7577;x=1\x07c\x1b]757;n\x07";
        for cut in 0..=input.len() {
            let (out, found) = scan_all(&[&input[..cut], &input[cut..]]);
            assert_eq!(out, b"abc\x1b]757;n\x07", "cut {cut}");
            assert_eq!(found, vec![Found::Resume(b"pid=1:time=2:clear=1:mac=ab".to_vec()), Found::Resume(b"x=1".to_vec())]);
        }
        // One of 2048 bytes in all is still read.
        let body = "k=".to_owned() + &"v".repeat(2048 - 8 - 2);
        let seq = format!("\x1b]7577;{body}\x07");
        assert_eq!(seq.len(), 2048);
        assert_eq!(scan_all(&[seq.as_bytes()]).1, vec![Found::Resume(body.into_bytes())]);
    }

    fn offer(ticket: &str, name: &str) -> Offer {
        Offer { ticket: ticket.into(), name: name.into() }
    }

    #[test]
    fn encodes_and_strips_with_either_terminator() {
        let seq = osc("silo1abc", "mini\x07;box");
        assert_eq!(seq, "\x1b]7717;offer;silo1abc;mini;box\x07");
        let input = format!("a{seq}b");
        assert_eq!(scan(&[input.as_bytes()]), (b"ab".to_vec(), vec![offer("silo1abc", "mini;box")]));
        let st = b"x\x1b]7717;offer;silo1t;host\x1b\\y";
        assert_eq!(scan(&[st]), (b"xy".to_vec(), vec![offer("silo1t", "host")]));
    }

    #[test]
    fn split_across_every_boundary() {
        let input = format!("\x1b[1mhi{}\x1b]0;title\x07end", osc("silo1zz", "host"));
        let bytes = input.as_bytes();
        for cut in 0..=bytes.len() {
            for cut2 in cut..=bytes.len() {
                let (out, offers) = scan(&[&bytes[..cut], &bytes[cut..cut2], &bytes[cut2..]]);
                assert_eq!(out, b"\x1b[1mhi\x1b]0;title\x07end", "cuts {cut} {cut2}");
                assert_eq!(offers, vec![offer("silo1zz", "host")]);
            }
        }
        let one_by_one: Vec<&[u8]> = bytes.chunks(1).collect();
        assert_eq!(scan(&one_by_one).1.len(), 1);
    }

    #[test]
    fn other_sequences_pass_through() {
        let input: &[u8] = b"\x1b]77;x\x07\x1b]7718;y\x07\x1b[31m\x1b\x1b]7717;offer;silo1q;n\x07z";
        let (out, offers) = scan(&[input]);
        assert_eq!(out, b"\x1b]77;x\x07\x1b]7718;y\x07\x1b[31m\x1bz");
        assert_eq!(offers, vec![offer("silo1q", "n")]);
        // Malformed bodies are stripped without an offer.
        assert_eq!(scan(&[b"a\x1b]7717;other;x\x07b", b"\x1b]7717;offer;\x07c"]), (b"abc".to_vec(), vec![]));
        // ESC that is not ST cancels: the partial sequence is released.
        let (out, offers) = scan(&[b"\x1b]7717;offer;t\x1b[0m"]);
        assert_eq!(out, b"\x1b]7717;offer;t\x1b[0m");
        assert!(offers.is_empty());
        // CAN and SUB abort it the same way, and reach the terminal.
        for abort in [b"\x18", b"\x1a"] {
            let input = [&b"\x1b]7717;offer;t"[..], abort, b"x\x1b]7717;offer;silo1c;n\x07"].concat();
            let (out, offers) = scan(&[&input]);
            assert_eq!(out, [&b"\x1b]7717;offer;t"[..], abort, b"x"].concat());
            assert_eq!(offers, vec![offer("silo1c", "n")]);
        }
    }

    #[test]
    fn idle_until_a_sequence_may_start_and_release_gives_it_back() {
        let mut scanner = OfferScanner::default();
        let mut out = Vec::new();
        scanner.feed(b"plain", &mut out);
        assert!(scanner.is_idle());
        scanner.feed(b"a\x1b]77", &mut out);
        assert!(!scanner.is_idle());
        scanner.release(&mut out);
        assert!(scanner.is_idle());
        assert_eq!(out, b"plaina\x1b]77");
    }

    #[test]
    fn overflow_is_released_unparsed() {
        let long = format!("\x1b]7717;offer;{};n\x07tail", "a".repeat(MAX_LEN));
        let (out, offers) = scan(&[long.as_bytes()]);
        assert!(offers.is_empty());
        assert_eq!(out, long.as_bytes());
        let (out, offers) = scan(&[b"\x1b]7717;offer;", &[b'a'; MAX_LEN], b"\x1b]7717;offer;silo1k;h\x07"]);
        assert_eq!(offers, vec![offer("silo1k", "h")]);
        assert_eq!(out.len(), PREFIX_LEN + 6 + MAX_LEN);
    }
}
