//! A browser block without a page: Silo's empty state and the recent
//! pages it offers, remembered per host across blocks and launches. The
//! address bar completes what is typed from the same pages.

use serde::{Deserialize, Serialize};

/// Recent pages kept, over all hosts.
pub const KEPT: usize = 200;
/// Recent pages an empty block lists.
pub const LISTED: usize = 6;
/// Pages the address bar suggests while typing.
pub const SUGGESTED: usize = 6;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentPage {
    pub host: String,
    pub url: String,
    #[serde(default)]
    pub title: String,
}

fn remembered(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

/// A visit to `url` on `host`: it moves first, once; the newest [`KEPT`] stay.
pub fn visit(list: &mut Vec<RecentPage>, host: &str, url: &str, title: &str) {
    if !remembered(url) {
        return;
    }
    let old = list.iter().position(|p| p.host == host && p.url == url).map(|i| list.remove(i));
    let title = if title.is_empty() { old.map(|p| p.title).unwrap_or_default() } else { title.to_owned() };
    list.insert(0, RecentPage { host: host.to_owned(), url: url.to_owned(), title });
    list.truncate(KEPT);
}

/// The page `url` on `host` got its title; its place stays.
pub fn retitle(list: &mut [RecentPage], host: &str, url: &str, title: &str) {
    if let Some(page) = list.iter_mut().find(|p| p.host == host && p.url == url) {
        title.clone_into(&mut page.title);
    }
}

/// The recent pages an empty block on `host` lists, newest first.
#[must_use]
pub fn listed(list: &[RecentPage], host: &str) -> Vec<RecentPage> {
    list.iter().filter(|p| p.host == host).take(LISTED).cloned().collect()
}

/// `s` starts with `prefix`, ASCII case aside.
fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// The ways `url` may be typed, shortest first: without scheme and
/// `www.`, without scheme, and (only when `typed` has a `:`) whole.
fn forms<'a>(url: &'a str, typed: &str) -> impl Iterator<Item = &'a str> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let bare = rest.strip_prefix("www.").unwrap_or(rest);
    let whole = typed.contains(':').then_some(url);
    [Some(bare), (bare != rest).then_some(rest), whole].into_iter().flatten()
}

/// The pages on `host` typing `typed` suggests, best first: those whose
/// address starts with it, then those whose address or title holds each
/// of its words; newest first within each.
#[must_use]
pub fn suggest(list: &[RecentPage], host: &str, typed: &str) -> Vec<RecentPage> {
    let typed = typed.trim();
    if typed.is_empty() {
        return Vec::new();
    }
    let words: Vec<String> = typed.split_whitespace().map(str::to_lowercase).collect();
    let (mut prefixed, mut holding) = (Vec::new(), Vec::new());
    for page in list.iter().filter(|p| p.host == host) {
        if forms(&page.url, typed).any(|f| starts_with_ci(f, typed)) {
            prefixed.push(page);
        } else {
            let (url, title) = (page.url.to_lowercase(), page.title.to_lowercase());
            if words.iter().all(|w| url.contains(w.as_str()) || title.contains(w.as_str())) {
                holding.push(page);
            }
        }
    }
    prefixed.into_iter().chain(holding).take(SUGGESTED).cloned().collect()
}

/// What the address bar adds after the typed text, selected, and the page
/// accepting it opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    pub suffix: String,
    pub url: String,
}

/// The inline completion of `typed` from `pages` (best first): the rest of
/// the first matching page's host while `typed` is within it (opening that
/// host's root), else the rest of its address. Nothing once `typed` is a
/// whole host or address, or holds a space (a search).
#[must_use]
pub fn complete(pages: &[RecentPage], typed: &str) -> Option<Completion> {
    if typed.is_empty() || typed.contains(char::is_whitespace) {
        return None;
    }
    pages.iter().find_map(|page| {
        let form = forms(&page.url, typed).find(|f| starts_with_ci(f, typed))?;
        let after_scheme = form.find("://").map_or(0, |i| i + 3);
        let host_end = after_scheme + form[after_scheme..].find(['/', '?', '#']).unwrap_or(form.len() - after_scheme);
        if typed.len() < host_end {
            let url_scheme = page.url.find("://").map_or(0, |i| i + 3);
            let authority = url_scheme + page.url[url_scheme..].find(['/', '?', '#']).unwrap_or(page.url.len() - url_scheme);
            return Some(Completion { suffix: form[typed.len()..host_end].to_owned(), url: format!("{}/", &page.url[..authority]) });
        }
        if typed.len() == host_end {
            return None;
        }
        let full = form.strip_suffix('/').unwrap_or(form);
        let suffix = full.get(typed.len()..).filter(|s| !s.is_empty())?;
        Some(Completion { suffix: suffix.to_owned(), url: page.url.clone() })
    })
}

/// ↓ (`1`) / ↑ (`-1`) from the address bar (`None`) through `len` rows:
/// ↑ from the first row returns to the address bar, ↓ stops at the last.
#[must_use]
pub fn select(current: Option<usize>, len: usize, delta: isize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    match current {
        None => (delta > 0).then_some(0),
        Some(i) => i.checked_add_signed(delta).map(|i| i.min(len - 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(list: &[RecentPage]) -> Vec<&str> {
        list.iter().map(|p| p.url.as_str()).collect()
    }

    #[test]
    fn visits_dedup_newest_first() {
        let mut list = Vec::new();
        visit(&mut list, "local", "http://a/", "A");
        visit(&mut list, "local", "http://b/", "B");
        visit(&mut list, "local", "http://a/", "");
        assert_eq!(urls(&list), ["http://a/", "http://b/"]);
        assert_eq!(list[0].title, "A", "an untitled revisit keeps the title");
        visit(&mut list, "remote", "http://a/", "Other");
        assert_eq!(list.len(), 3, "hosts are separate");
        retitle(&mut list, "local", "http://b/", "Bee");
        assert_eq!((list[2].url.as_str(), list[2].title.as_str()), ("http://b/", "Bee"));
    }

    #[test]
    fn only_web_pages_are_remembered() {
        let mut list = Vec::new();
        for url in ["about:blank", "", "data:text/html,hi", "ws://x/"] {
            visit(&mut list, "local", url, "");
        }
        assert!(list.is_empty());
    }

    #[test]
    fn capped_and_listed_per_host() {
        let mut list = Vec::new();
        for i in 0..KEPT + 5 {
            visit(&mut list, if i % 2 == 0 { "local" } else { "remote" }, &format!("http://p{i}/"), "");
        }
        assert_eq!(list.len(), KEPT);
        assert_eq!(list[0].url, format!("http://p{}/", KEPT + 4));
        let local = listed(&list, "local");
        assert_eq!(local.len(), LISTED);
        assert!(local.iter().all(|p| p.host == "local"));
        assert_eq!(local[0].url, format!("http://p{}/", KEPT + 4));
        assert_eq!(local[1].url, format!("http://p{}/", KEPT + 2));
    }

    fn page(url: &str, title: &str) -> RecentPage {
        RecentPage { host: "local".into(), url: url.into(), title: title.into() }
    }

    #[test]
    fn suggestions_prefer_address_prefixes() {
        let list = vec![
            page("https://docs.rs/gpui", "gpui - Rust"),
            page("https://www.github.com/nitishxyz/term", "term"),
            page("https://news.ycombinator.com/", "Hacker News"),
            page("https://gist.github.com/", "Gists"),
            RecentPage { host: "remote".into(), url: "https://github.io/".into(), title: String::new() },
        ];
        let urls = |typed| suggest(&list, "local", typed).into_iter().map(|p| p.url).collect::<Vec<_>>();
        assert_eq!(urls("gi"), ["https://www.github.com/nitishxyz/term", "https://gist.github.com/"]);
        assert_eq!(urls("GitHub"), ["https://www.github.com/nitishxyz/term", "https://gist.github.com/"], "then any match");
        assert_eq!(urls("www.git"), ["https://www.github.com/nitishxyz/term"]);
        assert_eq!(urls("hacker news"), ["https://news.ycombinator.com/"], "titles, every word");
        assert_eq!(urls("rust"), ["https://docs.rs/gpui"]);
        assert!(urls("  ").is_empty());
        assert!(urls("github.io").is_empty(), "other hosts' pages");
    }

    #[test]
    fn inline_completion_finishes_the_host_then_the_address() {
        let pages = [page("https://www.github.com/nitishxyz/term", ""), page("http://localhost:5173/app/", "")];
        let c = |typed| complete(&pages, typed).map(|c| (c.suffix, c.url));
        assert_eq!(c("git"), Some(("hub.com".into(), "https://www.github.com/".into())));
        assert_eq!(c("GiT"), Some(("hub.com".into(), "https://www.github.com/".into())), "typed case stays");
        assert_eq!(c("www.g"), Some(("ithub.com".into(), "https://www.github.com/".into())));
        assert_eq!(c("github.com"), None, "a whole host");
        assert_eq!(c("github.com/n"), Some(("itishxyz/term".into(), "https://www.github.com/nitishxyz/term".into())));
        assert_eq!(c("github.com/nitishxyz/term"), None);
        assert_eq!(c("local"), Some(("host:5173".into(), "http://localhost:5173/".into())), "the page's scheme");
        assert_eq!(c("localhost:5173/a"), Some(("pp".into(), "http://localhost:5173/app/".into())), "no trailing slash");
        assert_eq!(c("https://gi"), None, "the scheme form is www's");
        assert_eq!(c("https://www.gi"), Some(("thub.com".into(), "https://www.github.com/".into())));
        assert_eq!(c("git hub"), None, "a search");
        assert_eq!(c(""), None);
        assert_eq!(complete(&[page("https://é.fr/", "")], "é"), Some(Completion { suffix: ".fr".into(), url: "https://é.fr/".into() }));
    }

    #[test]
    fn keyboard_selection() {
        assert_eq!(select(None, 0, 1), None);
        assert_eq!(select(None, 3, -1), None);
        assert_eq!(select(None, 3, 1), Some(0));
        assert_eq!(select(Some(0), 3, 1), Some(1));
        assert_eq!(select(Some(2), 3, 1), Some(2));
        assert_eq!(select(Some(1), 3, -1), Some(0));
        assert_eq!(select(Some(0), 3, -1), None);
        assert_eq!(select(Some(5), 2, -1), Some(1), "a shrunk list clamps");
    }
}
