//! Skills in the composer (`docs/AGENT_BLOCK.md` §4g2): the `/` popup lists the project's skills (`skills.list`)
//! after its commands, and a `/` typed after whitespace later in the text lists them alone; a pick becomes a
//! `/name` chip sent as a `MentionKind::Skill` mention (otto tells the model to load it).

use std::ops::Range;

use otto_proto::SkillInfo;

use super::mention::Chip;

/// Most characters of a skill's description a row shows.
pub const ABOUT: usize = 64;

/// A `/query` after whitespace (not at the text's start, where the `/` menu with commands opens instead) with no
/// whitespace up to the caret, outside chips. Its range runs to the word's end; the query is what is typed before
/// the caret. A path (`/tmp/x`) is one only up to its first `/`, and matches no skill once typed past.
pub fn query<'a>(text: &'a str, caret: usize, chips: &[Chip]) -> Option<(Range<usize>, &'a str)> {
    let before = text.get(..caret)?;
    let at = before.rfind(|c: char| c == '/' || c.is_whitespace())?;
    if at == 0 || !before[at..].starts_with('/') || !before[..at].ends_with(char::is_whitespace) {
        return None;
    }
    if chips.iter().any(|c| c.range.start <= at && at < c.range.end) {
        return None;
    }
    let rest = &text[caret..];
    let mut end = caret + rest.find(char::is_whitespace).unwrap_or(rest.len());
    if let Some(next) = chips.iter().map(|c| c.range.start).filter(|s| *s >= caret).min() {
        end = end.min(next);
    }
    Some((at..end, &before[at + 1..]))
}

/// The leading `/word`'s range: from the start to the first whitespace (what a skill picked there replaces).
pub fn leading(text: &str) -> Range<usize> {
    0..text.find(char::is_whitespace).unwrap_or(text.len())
}

/// Skills matching `query` (fuzzy on the name, best first; all in order when empty); none once it has whitespace
/// or a `/` (a path).
pub fn matches<'a>(skills: &'a [SkillInfo], query: &str) -> Vec<&'a SkillInfo> {
    if query.contains(|c: char| c.is_whitespace() || c == '/') {
        return Vec::new();
    }
    let mut hits: Vec<(i32, usize, &SkillInfo)> =
        skills.iter().enumerate().filter_map(|(i, s)| crate::commands::fuzzy(query, &s.name).map(|(score, _)| (score, i, s))).collect();
    hits.sort_by_key(|(score, i, _)| (std::cmp::Reverse(*score), *i));
    hits.into_iter().map(|(.., s)| s).collect()
}

/// A description on one line, cut to [`ABOUT`] characters with an ellipsis.
pub fn about(description: &str) -> String {
    let line = description.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= ABOUT {
        return line;
    }
    let cut: String = line.chars().take(ABOUT - 1).collect();
    // At a word's end when one is near.
    let cut = match cut.rfind(' ') {
        Some(space) if space > ABOUT * 2 / 3 => &cut[..space],
        _ => cut.as_str(),
    };
    format!("{}…", cut.trim_end_matches([' ', ',', ';', ':', '.']))
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_proto::MentionKind;

    fn skill(name: &str) -> SkillInfo {
        SkillInfo { name: name.into(), description: String::new() }
    }

    #[test]
    fn a_slash_after_whitespace_is_a_skill_query() {
        assert_eq!(query("fight /act", 10, &[]), Some((6..10, "act")));
        assert_eq!(query("fight /", 7, &[]), Some((6..7, "")));
        assert_eq!(query("one\n/sh", 7, &[]), Some((4..7, "sh")));
        // The range runs to the word's end; the query stops at the caret.
        assert_eq!(query("a /action now", 5, &[]), Some((2..9, "ac")));
        // The text's start is the `/` menu's; not after a word; past a space; inside a path.
        assert_eq!(query("/act", 4, &[]), None);
        assert_eq!(query("and/or", 6, &[]), None);
        assert_eq!(query("a /act b", 8, &[]), None);
        assert_eq!(query("see /tmp/x", 10, &[]), None);
        assert_eq!(query("plain", 5, &[]), None);
        // A path's first segment is a query, but names no skill once it has a `/`.
        assert_eq!(query("see /tmp", 8, &[]), Some((4..8, "tmp")));
        let skills = [skill("action-choreography"), skill("shot-listing")];
        assert!(matches(&skills, "tmp").is_empty());
        assert!(matches(&skills, "tmp/x").is_empty());
        // Inside a chip there is nothing to complete.
        let chips = [Chip::new(2, MentionKind::Skill, "fight".into())];
        assert_eq!(query("a /fight", 8, &chips), None);
    }

    #[test]
    fn skills_match_by_name() {
        let skills = [skill("action-choreography"), skill("shot-listing"), skill("acting-direction")];
        let names = |q: &str| matches(&skills, q).into_iter().map(|s| s.name.as_str()).collect::<Vec<_>>();
        assert_eq!(names(""), ["action-choreography", "shot-listing", "acting-direction"]);
        assert_eq!(names("shot"), ["shot-listing"]);
        assert_eq!(names("actdir")[0], "acting-direction");
        assert!(names("mode auto").is_empty(), "a command's argument");
        assert_eq!(leading("/act on it"), 0..4);
        assert_eq!(leading("/act"), 0..4);
    }

    #[test]
    fn descriptions_fit_one_row() {
        assert_eq!(about("Fights.\n  Staged  well."), "Fights. Staged well.");
        let long = "Stage fights, chases and stunts beat by beat so every shot reads clearly and the geography holds.";
        let short = about(long);
        assert!(short.ends_with('…') && short.chars().count() <= ABOUT, "{short}");
        assert!(short.starts_with("Stage fights, chases and stunts beat by beat"), "{short}");
        assert_eq!(about(""), "");
    }
}
