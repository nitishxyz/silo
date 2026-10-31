//! Spoken commands: "new line", "new paragraph", "bullet" / "next bullet",
//! "numbered list", "period", "comma", "question mark", "exclamation mark"
//! and "scratch that". Each counts only where it can't be a plain word:
//! not after "a", "the", "short"…, not before "of", "separated"…

use super::{ends_clause, ends_sentence, norm};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    Line,
    Paragraph,
    Bullet,
    Numbered,
    Mark(char),
    Scratch,
}

const COMMANDS: &[(&[&str], Command)] = &[
    (&["new", "line"], Command::Line),
    (&["newline"], Command::Line),
    (&["new", "paragraph"], Command::Paragraph),
    (&["next", "bullet"], Command::Bullet),
    (&["bullet", "point"], Command::Bullet),
    (&["bullet"], Command::Bullet),
    (&["numbered", "list"], Command::Numbered),
    (&["period"], Command::Mark('.')),
    (&["full", "stop"], Command::Mark('.')),
    (&["comma"], Command::Mark(',')),
    (&["question", "mark"], Command::Mark('?')),
    (&["exclamation", "mark"], Command::Mark('!')),
    (&["exclamation", "point"], Command::Mark('!')),
    (&["scratch", "that"], Command::Scratch),
];

/// Words before which a command word is a plain word ("a new line").
const NOT_AFTER: &[&str] = &[
    "a", "an", "the", "this", "that", "these", "those", "each", "every", "any", "some", "no", "one", "another", "my", "your", "our", "their", "his",
    "her", "its", "per", "extra", "empty", "blank", "same", "first", "last", "trial", "grace", "time", "short", "long", "given", "certain",
    "waiting", "billing", "oxford", "use", "type", "insert", "add", "put", "say", "said", "says", "called", "named", "word", "with", "without",
    "single", "double", "silver", "magic",
];
/// And before "scratch that" ("you can scratch that").
const NOT_SCRATCHED_AFTER: &[&str] = &["i", "you", "we", "they", "he", "she", "can", "could", "will", "would", "should", "to", "just", "please"];

/// Words after which it is one too ("new line character").
const NOT_BEFORE: &[&str] = &[
    "of",
    "is",
    "was",
    "are",
    "were",
    "be",
    "been",
    "character",
    "characters",
    "key",
    "keys",
    "sign",
    "symbol",
    "separated",
    "delimited",
    "between",
    "before",
    "after",
    "at",
    "in",
    "on",
    "for",
    "by",
    "with",
    "from",
    "to",
    "points",
    "proof",
    "train",
    "hole",
    "holes",
    "wound",
    "journal",
    "style",
    "itch",
    "idea",
    "off",
    "out",
];

/// Whether `tokens[at..at + len]` is said as a command (`fresh`: right
/// after another; `text_empty`: nothing typed yet).
fn said(tokens: &[&str], at: usize, len: usize, command: Command, fresh: bool, text_empty: bool) -> bool {
    // No punctuation inside a command ("new, line").
    if tokens[at..at + len - 1].iter().any(|t| t.ends_with(|c: char| !c.is_alphanumeric())) {
        return false;
    }
    let before = at.checked_sub(1).filter(|_| !fresh).map(|i| tokens[i]);
    let after = tokens.get(at + len).copied();
    let last = tokens[at + len - 1];
    let free_before = before.is_none_or(|b| ends_clause(b) || !NOT_AFTER.contains(&norm(b).as_str()));
    let free_after = ends_clause(last) || after.is_none_or(|a| !NOT_BEFORE.contains(&norm(a).as_str()));
    match command {
        // Something to punctuate; "period" also only at the end or before
        // a capitalized word ("the period ends" stays).
        Command::Mark(mark) => {
            let next_capital = after.is_some_and(|a| a.starts_with(char::is_uppercase) && norm(a) != "i");
            before.is_some_and(|b| !ends_sentence(b))
                && free_before
                && free_after
                && (mark != '.' || after.is_none() || ends_clause(last) || next_capital)
        }
        // At a phrase's end or start, not "I scratch that itch".
        Command::Scratch => {
            !text_empty
                && (ends_clause(last) || after.is_none() || before.is_none_or(ends_clause))
                && free_before
                && free_after
                && before.is_none_or(|b| ends_clause(b) || !NOT_SCRATCHED_AFTER.contains(&norm(b).as_str()))
        }
        _ => free_before && free_after,
    }
}

/// Text typed so far, and the list being dictated.
struct Out {
    text: String,
    multiline: bool,
    /// The last numbered item.
    numbered: Option<u32>,
    capital: bool,
}

impl Out {
    fn word(&mut self, word: &str) {
        if self.text.ends_with(|c: char| !c.is_whitespace()) {
            self.text.push(' ');
        }
        if std::mem::take(&mut self.capital) {
            self.text.push_str(&super::capitalize(word, &[]));
        } else {
            self.text.push_str(word);
        }
    }

    /// End the line (`blank`: and leave an empty one).
    fn line(&mut self, blank: bool) {
        self.text.truncate(self.text.trim_end().len());
        if self.multiline && !self.text.is_empty() {
            self.text.push_str(if blank { "\n\n" } else { "\n" });
        }
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Line | Command::Paragraph => {
                self.numbered = None;
                self.line(command == Command::Paragraph);
            }
            Command::Bullet | Command::Numbered => {
                self.drop_empty_item();
                self.line(false);
                self.numbered = match (command, self.numbered) {
                    (Command::Numbered, _) => Some(1),
                    (_, Some(n)) => Some(n + 1),
                    _ => None,
                };
                if self.multiline {
                    self.text.push_str(&self.numbered.map_or_else(|| "- ".to_owned(), |n| format!("{n}. ")));
                }
            }
            Command::Mark(mark) => {
                self.text.truncate(self.text.trim_end_matches(|c: char| c.is_whitespace() || ",.;:!?".contains(c)).len());
                self.text.push(mark);
                self.capital = mark != ',';
            }
            Command::Scratch => self.scratch(),
        }
    }

    /// An item marker with nothing after it goes (its number with it).
    fn drop_empty_item(&mut self) {
        let last = self.text.rsplit('\n').next().unwrap_or_default().trim_end();
        let number = last.strip_suffix('.').and_then(|n| n.parse::<u32>().ok());
        if last == "-" || number.is_some() {
            self.text.truncate(self.text.len() - self.text.rsplit('\n').next().unwrap_or_default().len());
            self.text.truncate(self.text.trim_end().len());
            if let Some(n) = number {
                self.numbered = n.checked_sub(1);
            }
        }
    }

    /// Drop the last sentence or line (keeping a list item's marker).
    fn scratch(&mut self) {
        let body = self.text.trim_end().trim_end_matches(['.', '!', '?', ',', ';', ':']);
        let boundary =
            body.char_indices().rev().find(|&(i, c)| c == '\n' || (matches!(c, '.' | '!' | '?') && body[i + 1..].starts_with(char::is_whitespace)));
        let mut cut = boundary.map_or(0, |(i, c)| i + c.len_utf8());
        let rest = &body[cut..];
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if rest.starts_with("- ") {
            cut += 2;
        } else if digits > 0 && rest[digits..].starts_with(". ") {
            cut += digits + 2;
        }
        self.text.truncate(cut);
        self.capital = false;
    }
}

/// `text` with its spoken commands carried out; one line unless `multiline`.
pub(super) fn apply(text: &str, multiline: bool) -> String {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let mut out = Out { text: String::new(), multiline, numbered: None, capital: false };
    let (mut i, mut fresh) = (0, false);
    'next: while i < tokens.len() {
        for (words, command) in COMMANDS {
            let fits = tokens.len() - i >= words.len() && words.iter().enumerate().all(|(k, w)| norm(tokens[i + k]) == *w);
            if fits && said(&tokens, i, words.len(), *command, fresh, out.text.is_empty()) {
                out.command(*command);
                i += words.len();
                fresh = true;
                continue 'next;
            }
        }
        out.word(tokens[i]);
        (i, fresh) = (i + 1, false);
    }
    // No empty item left at the end.
    out.drop_empty_item();
    out.text.trim_end().to_owned()
}
