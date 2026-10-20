//! The browser address bar's reading of a page: its security state, how
//! its URL is shown when not edited, and its zoom.

/// What the security indicator shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Security {
    /// https with a valid certificate and no insecure content.
    Secure,
    /// Plain http to a public host, or a certificate error.
    NotSecure,
    /// Local or special pages, insecure content, or not known yet.
    Neutral,
}

fn split_scheme(url: &str) -> Option<(String, &str)> {
    let (scheme, rest) = url.split_once(':')?;
    Some((scheme.to_ascii_lowercase(), rest))
}

/// The host (and port) of a web URL.
fn authority(rest: &str) -> &str {
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    rest.split(['/', '?', '#']).next().unwrap_or(rest)
}

fn hostname(authority: &str) -> &str {
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(v6),
        None => authority.split(':').next().unwrap_or(authority),
    }
}

/// The indicator for `url`, given Chromium's facts about its connection
/// (`None`, or facts for another URL, while unknown).
#[must_use]
pub fn security(url: &str, info: Option<&silo_web::Security>) -> Security {
    let info = info.filter(|i| i.url == url);
    if info.is_some_and(|i| i.cert_error) {
        return Security::NotSecure;
    }
    let Some((scheme, rest)) = split_scheme(url) else { return Security::Neutral };
    let web = matches!(scheme.as_str(), "http" | "https" | "ws" | "wss");
    if !web || silo_proto::model::private_host(hostname(authority(rest))) {
        return Security::Neutral;
    }
    match (scheme.as_str(), info) {
        ("http" | "ws", _) => Security::NotSecure,
        (_, Some(i)) if i.secure && !i.mixed => Security::Secure,
        _ => Security::Neutral,
    }
}

/// The security popover's first line.
#[must_use]
pub fn headline(url: &str, info: Option<&silo_web::Security>) -> &'static str {
    let info = info.filter(|i| i.url == url);
    match security(url, info) {
        Security::Secure => "Connection is secure",
        Security::NotSecure => "Connection is not secure",
        Security::Neutral if info.is_some_and(|i| i.mixed) => "Parts of this page are not secure",
        Security::Neutral if info.is_none() && url.starts_with("https://") && !local(url) => "Checking connection…",
        Security::Neutral => "Local page",
    }
}

fn local(url: &str) -> bool {
    split_scheme(url).is_none_or(|(_, rest)| silo_proto::model::private_host(hostname(authority(rest))))
}

/// A URL as shown at rest: `scheme` and `path` muted around the `host`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shown {
    pub scheme: String,
    pub host: String,
    pub path: String,
}

/// `https://` and a leading `www.` dropped, `http://` kept (muted), the
/// host in Unicode; `about:blank` shows nothing.
#[must_use]
pub fn shown(url: &str) -> Shown {
    if silo_web::is_blank(url) {
        return Shown::default();
    }
    let Some((scheme, rest)) = split_scheme(url).filter(|(s, r)| matches!(s.as_str(), "http" | "https") && r.starts_with("//")) else {
        return Shown { host: url.to_owned(), ..Shown::default() };
    };
    let host = authority(rest);
    let path = &rest[2 + host.len()..];
    let bare = host.strip_prefix("www.").filter(|h| h.contains('.')).unwrap_or(host);
    let host = if bare.contains("xn--") { idna::domain_to_unicode(bare).0 } else { bare.to_owned() };
    Shown {
        scheme: if scheme == "http" { "http://".into() } else { String::new() },
        host,
        path: if path == "/" { String::new() } else { path.to_owned() },
    }
}

/// The page zoom as a percentage when it is not 100% (CEF levels are
/// powers of 1.2).
#[must_use]
pub fn zoom_label(level: f32) -> Option<String> {
    let percent = (1.2f64.powf(f64::from(level)) * 100.0).round();
    (percent != 100.0).then(|| format!("{percent}%"))
}

/// A Unix time as `YYYY-MM-DD` (UTC).
#[must_use]
pub fn date(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(url: &str, secure: bool, cert_error: bool, mixed: bool) -> silo_web::Security {
        silo_web::Security { url: url.into(), secure, cert_error, mixed, ..Default::default() }
    }

    #[test]
    fn security_states() {
        let x = "https://x.com/home";
        assert_eq!(security(x, Some(&facts(x, true, false, false))), Security::Secure);
        assert_eq!(security(x, Some(&facts(x, true, false, true))), Security::Neutral, "mixed content");
        assert_eq!(security(x, Some(&facts(x, true, true, false))), Security::NotSecure, "certificate error");
        assert_eq!(security(x, None), Security::Neutral, "not known yet");
        assert_eq!(security(x, Some(&facts("https://y.com/", true, false, false))), Security::Neutral, "facts of another page");
        assert_eq!(security("http://neverssl.com/", None), Security::NotSecure);
        for local in ["http://localhost:3000/", "http://192.168.1.4/", "http://[::1]:8080/", "about:blank", "data:text/html,hi", "file:///tmp/a"] {
            assert_eq!(security(local, None), Security::Neutral, "{local}");
        }
        let self_signed = "https://localhost:8443/";
        assert_eq!(security(self_signed, Some(&facts(self_signed, false, true, false))), Security::NotSecure, "errors beat local");
    }

    #[test]
    fn headlines() {
        let x = "https://x.com/";
        assert_eq!(headline(x, Some(&facts(x, true, false, false))), "Connection is secure");
        assert_eq!(headline(x, Some(&facts(x, true, false, true))), "Parts of this page are not secure");
        assert_eq!(headline(x, None), "Checking connection…");
        assert_eq!(headline("http://neverssl.com/", None), "Connection is not secure");
        assert_eq!(headline("http://localhost:8765/", None), "Local page");
    }

    #[test]
    fn urls_show_host_then_muted_path() {
        let s = |scheme: &str, host: &str, path: &str| Shown { scheme: scheme.into(), host: host.into(), path: path.into() };
        assert_eq!(shown("https://x.com/home"), s("", "x.com", "/home"));
        assert_eq!(shown("https://www.example.com/"), s("", "example.com", ""));
        assert_eq!(shown("https://www.example.com/a?b#c"), s("", "example.com", "/a?b#c"));
        assert_eq!(shown("http://neverssl.com/"), s("http://", "neverssl.com", ""));
        assert_eq!(shown("http://localhost:8765/Fav.html"), s("http://", "localhost:8765", "/Fav.html"));
        assert_eq!(shown("https://xn--bcher-kva.example/"), s("", "bücher.example", ""));
        assert_eq!(shown("https://www/"), s("", "www", ""), "a host named www stays");
        assert_eq!(shown("about:blank"), Shown::default());
        assert_eq!(shown("data:text/html,hi"), s("", "data:text/html,hi", ""));
    }

    #[test]
    fn zoom_pill_only_off_100() {
        assert_eq!(zoom_label(0.0), None);
        assert_eq!(zoom_label(0.000_1), None);
        assert_eq!(zoom_label(1.0), Some("120%".into()));
        assert_eq!(zoom_label(-1.0), Some("83%".into()));
    }

    #[test]
    fn dates() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_798_675_200), "2026-12-31");
        assert_eq!(date(951_782_400), "2000-02-29");
    }
}
