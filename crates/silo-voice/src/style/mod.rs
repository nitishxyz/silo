//! Dictated text tidied for where it goes, on this Mac and deterministic:
//! the user's replacements, spoken commands ([`commands`]), then lists
//! ([`lists`]). Conservative: a word that may be meant literally stays.

mod commands;
mod lists;
#[cfg(test)]
mod tests;

use crate::prefs::Replacement;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Style {
    /// Line breaks and lists may be typed (not a terminal).
    pub multiline: bool,
    /// Turn spoken enumerations into lists.
    pub lists: bool,
    /// Words kept as written at a list item's start (the vocabulary).
    pub vocabulary: Vec<String>,
}

/// `text` as it should be typed.
#[must_use]
pub fn format(text: &str, replacements: &[Replacement], style: &Style) -> String {
    let text = replace(text, replacements);
    let text = commands::apply(&text, style.multiline);
    if style.multiline
        && style.lists
        && !text.contains('\n')
        && let Some(list) = lists::detect(&text, &style.vocabulary)
    {
        return list;
    }
    text
}

/// A token without its surrounding punctuation, lowercased.
fn norm(token: &str) -> String {
    token.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase()
}

/// Ends with a full stop, question or exclamation mark.
fn ends_sentence(token: &str) -> bool {
    token.trim_end_matches(['"', '\'', ')', '\u{201d}']).ends_with(['.', '!', '?'])
}

/// Ends a clause: a sentence, or a comma, colon or semicolon.
fn ends_clause(token: &str) -> bool {
    ends_sentence(token) || token.ends_with([',', ':', ';'])
}

/// The first letter in upper case, unless the word is written in the
/// vocabulary starting in lower case ("otto").
fn capitalize(text: &str, vocabulary: &[String]) -> String {
    let first = text.split_whitespace().next().unwrap_or_default().trim_end_matches(|c: char| !c.is_alphanumeric());
    if vocabulary.iter().any(|v| v == first) {
        return text.to_owned();
    }
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

/// Word runs (letters, digits, `-`, `_`) and what lies between them.
fn runs(text: &str) -> Vec<(bool, &str)> {
    let word = |c: char| c.is_alphanumeric() || c == '-' || c == '_';
    let mut out = Vec::new();
    let mut start = 0;
    let mut kind = None;
    for (i, c) in text.char_indices() {
        let w = word(c);
        if kind.is_some_and(|k| k != w) {
            out.push((kind.unwrap_or(w), &text[start..i]));
            start = i;
        }
        kind = Some(w);
    }
    if let Some(k) = kind {
        out.push((k, &text[start..]));
    }
    out
}

/// `write` in the case `heard` was said in: all capitals, capitalized, or
/// as written.
fn match_case(heard: &str, write: &str) -> String {
    let letters: Vec<char> = heard.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.len() > 1 && letters.iter().all(|c| c.is_uppercase()) {
        write.to_uppercase()
    } else if letters.first().is_some_and(|c| c.is_uppercase()) {
        capitalize(write, &[])
    } else {
        write.to_owned()
    }
}

/// Apply `rules`: whole words in any case, longest first, the case kept.
#[must_use]
pub fn replace(text: &str, rules: &[Replacement]) -> String {
    let mut rules: Vec<(Vec<String>, &str)> = rules
        .iter()
        .filter_map(|r| {
            let heard = runs(&r.heard);
            let ok = heard.iter().all(|(w, s)| *w || s.trim().is_empty()) && heard.iter().any(|(w, _)| *w);
            ok.then(|| (heard.iter().filter(|(w, _)| *w).map(|(_, s)| s.to_lowercase()).collect(), r.write.trim()))
        })
        .collect();
    rules.sort_by_key(|(words, _)| std::cmp::Reverse(words.len()));
    let runs = runs(text);
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    'next: while i < runs.len() {
        if runs[i].0 {
            for (words, write) in &rules {
                let end = i + words.len() * 2 - 1;
                let matched = end <= runs.len()
                    && words.iter().enumerate().all(|(k, w)| runs[i + k * 2].1.to_lowercase() == *w)
                    && (1..words.len()).all(|k| runs[i + k * 2 - 1].1.trim().is_empty());
                if matched {
                    out.push_str(&match_case(runs[i].1, write));
                    i = end;
                    continue 'next;
                }
            }
        }
        out.push_str(runs[i].1);
        i += 1;
    }
    out
}
