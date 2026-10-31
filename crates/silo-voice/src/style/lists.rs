//! Spoken enumerations as Markdown lists: "first … second … third …" or
//! "one, … two, … three, …" numbered; "a few things: X, Y and Z" bulleted.
//! Three items at least, and only where it's clearly a list.

use super::{capitalize, ends_clause, ends_sentence, norm};

const ORDINALS: [&[&str]; 10] = [
    &["first", "firstly"],
    &["second", "secondly"],
    &["third", "thirdly"],
    &["fourth", "fourthly"],
    &["fifth", "fifthly"],
    &["sixth"],
    &["seventh"],
    &["eighth"],
    &["ninth"],
    &["tenth"],
];
const CARDINALS: [&str; 10] = ["one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten"];

/// Words before a number that make it plain ("the first time").
const NOT_AFTER: &[&str] = &[
    "a", "an", "the", "my", "your", "our", "their", "his", "her", "its", "this", "that", "at", "for", "in", "on", "of", "to", "by", "from", "with",
    "come", "came", "comes", "was", "is", "be", "been", "were", "are", "love", "only", "just", "than", "about", "around",
];
/// Words after it that do ("second hand", "one of").
const NOT_BEFORE: &[&str] = &[
    "time", "times", "place", "of", "half", "quarter", "hand", "person", "class", "rate", "floor", "grade", "round", "party", "parties", "edition",
    "version", "try", "attempt", "day", "days", "week", "weeks", "year", "years", "month", "one", "ones", "or", "and", "more", "less", "hundred",
    "thousand", "million", "percent", "thing", "opinion", "guess", "nature", "best", "last", "other", "too", "minute", "minutes", "hour", "hours",
    "second", "seconds", "way",
];
/// Nouns that announce a list ("three things: …").
const CUES: &[&str] = &[
    "things",
    "items",
    "steps",
    "points",
    "tasks",
    "changes",
    "options",
    "reasons",
    "ways",
    "ideas",
    "questions",
    "issues",
    "problems",
    "parts",
    "files",
    "names",
    "features",
    "bugs",
    "goals",
    "ingredients",
    "following",
];
const FILLER: &[&str] = &["and", "then", "so", "also"];

fn listed(words: &[&str], word: &str) -> bool {
    words.contains(&word)
}

/// The number said at `tokens[i]` as item `k` (from 0): where its item's
/// words start. Ordinals need a free spot; numbers ("one", "1") a clause's
/// start or "step"/"number" before them.
fn marker(tokens: &[&str], i: usize, k: usize, cardinal: bool) -> Option<usize> {
    let word = norm(tokens[i]);
    let before = i.checked_sub(1).map(|j| tokens[j]);
    let free = |b: Option<&str>| b.is_none_or(|b| ends_clause(b) || !listed(NOT_AFTER, &norm(b)));
    let after = tokens.get(i + 1).copied()?;
    if !cardinal && listed(ORDINALS[k], &word) {
        let of_all = k == 0 && norm(after) == "of" && tokens.get(i + 2).is_some_and(|t| norm(t) == "all");
        let content = if of_all { i + 3 } else { i + 1 };
        let plain = !of_all && !ends_clause(tokens[i]) && listed(NOT_BEFORE, &norm(after));
        return (free(before) && !plain && content < tokens.len()).then_some(content);
    }
    let number = word == CARDINALS[k] || word == (k + 1).to_string();
    let prefixed = before.is_some_and(|b| matches!(norm(b).as_str(), "step" | "number" | "item") && free(i.checked_sub(2).map(|j| tokens[j])));
    let start = before.is_none_or(|b| ends_clause(b) || listed(FILLER, &norm(b)));
    // "2." or "2)": written as a list already.
    let dotted = tokens[i].ends_with(['.', ')']) && word.chars().all(|c| c.is_ascii_digit());
    let plain = !ends_clause(tokens[i]) && listed(NOT_BEFORE, &norm(after));
    (cardinal && number && (prefixed || start || dotted) && !plain).then_some(i + 1)
}

/// An item's words, tidied: no filler around it, no trailing comma.
fn item(tokens: &[&str], vocabulary: &[String]) -> Option<String> {
    let skip = |t: &&str| norm(t).is_empty() || listed(FILLER, &norm(t)) || listed(&["step", "number", "item"], &norm(t));
    let start = tokens.iter().position(|t| !skip(t))?;
    let end = tokens.iter().rposition(|t| !(norm(t).is_empty() || listed(FILLER, &norm(t))))? + 1;
    let mut text = tokens.get(start..end)?.join(" ");
    text.truncate(text.trim_end_matches([',', ';', ':']).len());
    if text.ends_with('.') && text.matches(['.', '!', '?']).count() == 1 {
        text.pop();
    }
    (!text.is_empty()).then(|| capitalize(&text, vocabulary))
}

/// What comes before a list, ending in a colon.
fn preamble(tokens: &[&str]) -> String {
    let mut text = tokens.join(" ");
    text.truncate(text.trim_end_matches([',', ';']).len());
    if !text.is_empty() && !text.ends_with([':', '.', '!', '?']) {
        text.push(':');
    }
    text
}

/// The last item's words and what follows its sentence.
fn split_tail<'a>(tokens: &'a [&'a str]) -> (&'a [&'a str], &'a [&'a str]) {
    let end = tokens.iter().position(|t| ends_sentence(t)).map_or(tokens.len(), |i| i + 1);
    tokens.split_at(end)
}

fn render(head: &[&str], items: &[String], numbered: bool, tail: &[&str]) -> String {
    let mut out = preamble(head);
    for (n, item) in items.iter().enumerate() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&if numbered { format!("{}. {item}", n + 1) } else { format!("- {item}") });
    }
    if !tail.is_empty() {
        out.push_str("\n\n");
        out.push_str(&tail.join(" "));
    }
    out
}

/// "First … second … third …" (or numbers) as a numbered list.
fn numbered(tokens: &[&str], vocabulary: &[String], cardinal: bool) -> Option<String> {
    // (where the number was said, where its words start)
    let mut marks: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < tokens.len() && marks.len() < ORDINALS.len() {
        let k = marks.len();
        // The number and "step" or "number" before it.
        let said = |i: usize| if i > 0 && listed(&["step", "number", "item"], &norm(tokens[i - 1])) { i - 1 } else { i };
        match marker(tokens, i, k, cardinal).filter(|_| marks.last().is_none_or(|m| said(i) > m.1)) {
            Some(content) => {
                marks.push((said(i), content));
                i = content;
            }
            None => i += 1,
        }
    }
    if marks.len() < 2 {
        return None;
    }
    // "… and finally …" as the last item.
    let last = marks[marks.len() - 1].1;
    if let Some(at) = (last + 1..tokens.len()).find(|&j| matches!(norm(tokens[j]).as_str(), "finally" | "lastly") && j + 1 < tokens.len()) {
        marks.push((at, at + 1));
    }
    if marks.len() < 3 {
        return None;
    }
    let (last_words, tail) = split_tail(&tokens[marks[marks.len() - 1].1..]);
    let mut items = Vec::new();
    for (n, (_, content)) in marks.iter().enumerate() {
        let words = marks.get(n + 1).map_or(last_words, |next| &tokens[*content..next.0]);
        let text = item(words, vocabulary)?;
        if cardinal && text.split_whitespace().count() < 2 {
            return None;
        }
        items.push(text);
    }
    Some(render(&tokens[..marks[0].0], &items, true, tail))
}

/// "A few things: X, Y and Z" as a bulleted list.
fn bulleted(tokens: &[&str], vocabulary: &[String]) -> Option<String> {
    let cue = tokens.iter().position(|t| t.ends_with([':', ',']) && listed(CUES, &norm(t)))?;
    let (words, tail) = split_tail(&tokens[cue + 1..]);
    let mut items: Vec<Vec<&str>> = vec![Vec::new()];
    let mut last = false;
    for (n, token) in words.iter().enumerate() {
        let current = items.last_mut()?;
        if matches!(norm(token).as_str(), "and" | "or") && !last {
            if !current.is_empty() {
                items.push(Vec::new());
            }
            last = true;
        } else if token.ends_with(',') && n + 1 < words.len() {
            if last {
                return None;
            }
            current.push(token);
            items.push(Vec::new());
        } else {
            current.push(token);
        }
    }
    let ok = last && items.len() >= 3 && items.iter().all(|i| (1..=6).contains(&i.len()));
    let items: Vec<String> = items.iter().map(|i| item(i, vocabulary)).collect::<Option<_>>()?;
    ok.then(|| render(&tokens[..=cue], &items, false, tail))
}

/// `text` as a list, when it is one.
pub(super) fn detect(text: &str, vocabulary: &[String]) -> Option<String> {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    numbered(&tokens, vocabulary, false).or_else(|| numbered(&tokens, vocabulary, true)).or_else(|| bulleted(&tokens, vocabulary))
}
