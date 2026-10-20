//! A browser block without a page: Silo's empty state and the recent
//! pages it offers, remembered per host across blocks and launches.

use serde::{Deserialize, Serialize};

/// Recent pages kept, over all hosts.
pub const KEPT: usize = 50;
/// Recent pages an empty block lists.
pub const LISTED: usize = 6;

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
