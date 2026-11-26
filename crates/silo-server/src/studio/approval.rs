//! An entity sheet's approval (docs/STUDIO.md §4.6): the frontmatter's
//! `approved: <RFC 3339 UTC time>`, and whether the sheet file changed
//! since; and whether its `voice` names a provider and voice.

use serde_json::Value;
use silo_proto::studio::Approval;

/// The frontmatter key.
pub const KEY: &str = "approved";

/// A sheet written this long after the approval still counts as approved
/// (the approval stamped while the file was being saved, coarse clocks).
pub const SLACK_MS: u64 = 2_000;

/// The approval of a sheet with mtime `sheet_ms` (unix ms; `None`: no
/// sheet) given the frontmatter's `approved` value. A time that doesn't
/// parse (`true`, `yes`, prose) is approved with nothing to compare;
/// `false`, `null` and an empty string are not approved.
#[must_use]
pub fn of(approved: Option<&Value>, sheet_ms: Option<u64>) -> Approval {
    let Some(sheet_ms) = sheet_ms else { return Approval::None };
    match approved {
        None | Some(Value::Null | Value::Bool(false)) => Approval::None,
        Some(Value::String(s)) if s.trim().is_empty() || matches!(s.trim(), "false" | "no") => Approval::None,
        Some(Value::String(s)) => match parse(s) {
            Some(at) if sheet_ms > at.saturating_add(SLACK_MS) => Approval::Changed,
            _ => Approval::Approved,
        },
        Some(_) => Approval::Approved,
    }
}

/// Whether a `voice` value names a provider and a voice
/// (`elevenlabs/JBFqnCBsd6RMkjVDRZzb`, `openai/alloy`): one token,
/// `<provider>/<voice>`, not prose such as `(pending, see notes)`.
#[must_use]
pub fn names_voice(voice: Option<&Value>) -> bool {
    let Some(Value::String(v)) = voice else { return false };
    let Some((provider, id)) = v.trim().split_once('/') else { return false };
    let provider_ok = !provider.is_empty() && provider.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    let id_ok = !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'));
    provider_ok && id_ok
}

/// Whether a `voice` value says the character doesn't speak (`none`,
/// `false`, `silent`, `mute`).
#[must_use]
pub fn speechless(voice: Option<&Value>) -> bool {
    match voice {
        Some(Value::Bool(false)) => true,
        Some(Value::String(s)) => matches!(s.trim().to_ascii_lowercase().as_str(), "none" | "false" | "no" | "silent" | "mute"),
        _ => false,
    }
}

/// Days since 1970-01-01 of a civil date (proleptic Gregorian).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date of a day count since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Unix ms as RFC 3339 in UTC, to the second (`2026-11-25T09:30:00Z`), as
/// `date -u +%Y-%m-%dT%H:%M:%SZ` writes it.
#[must_use]
pub fn rfc3339(ms: u64) -> String {
    let secs = i64::try_from(ms / 1000).unwrap_or(i64::MAX);
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rest / 3600, rest % 3600 / 60, rest % 60)
}

/// An RFC 3339 time (`2026-11-25T09:30:00Z`, with a fraction, an offset
/// `+05:30`, a space for the `T`, or a bare date: midnight UTC) as unix
/// ms; `None` when it isn't one or is before 1970.
#[must_use]
pub fn parse(text: &str) -> Option<u64> {
    let t = text.trim();
    let num = |s: &str| -> Option<u32> { (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok()).flatten() };
    let date = t.get(..10)?;
    let (y, m, d) = (num(date.get(..4)?)?, num(date.get(5..7)?)?, num(date.get(8..10)?)?);
    if date.as_bytes()[4] != b'-' || date.as_bytes()[7] != b'-' || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let days = days_from_civil(i64::from(y), m, d);
    let rest = &t[10..];
    let mut ms = days * 86_400_000;
    if !rest.is_empty() {
        let rest = rest.strip_prefix(['T', 't', ' '])?;
        let (h, mi, s) = (num(rest.get(..2)?)?, num(rest.get(3..5)?)?, num(rest.get(6..8)?)?);
        if rest.as_bytes()[2] != b':' || rest.as_bytes()[5] != b':' || h > 23 || mi > 59 || s > 60 {
            return None;
        }
        let mut zone = &rest[8..];
        let mut frac_ms = 0;
        if let Some(f) = zone.strip_prefix(['.', ',']) {
            let digits = f.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 {
                return None;
            }
            let first: String = f[..digits].chars().chain("000".chars()).take(3).collect();
            frac_ms = i64::from(num(&first)?);
            zone = &f[digits..];
        }
        let offset_min = match zone {
            "Z" | "z" => 0,
            z if z.len() == 6 && matches!(z.as_bytes()[0], b'+' | b'-') && z.as_bytes()[3] == b':' => {
                let mins = i64::from(num(z.get(1..3)?)?) * 60 + i64::from(num(z.get(4..6)?)?);
                if z.starts_with('-') { -mins } else { mins }
            }
            _ => return None,
        };
        ms += (i64::from(h) * 3600 + i64::from(mi) * 60 + i64::from(s) - offset_min * 60) * 1000 + frac_ms;
    }
    u64::try_from(ms).ok()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn times_round_trip_and_parse_leniently() {
        let at = parse("2026-11-25T09:30:05Z").unwrap();
        assert_eq!(rfc3339(at), "2026-11-25T09:30:05Z");
        assert_eq!(rfc3339(at + 999), "2026-11-25T09:30:05Z", "to the second");
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(parse("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse("2000-02-29T12:00:00Z"), Some(951_825_600_000));
        assert_eq!(rfc3339(951_825_600_000), "2000-02-29T12:00:00Z");
        assert_eq!(parse("2026-11-25T09:30:05.250Z"), Some(at + 250));
        assert_eq!(parse("2026-11-25T11:30:05+02:00"), Some(at));
        assert_eq!(parse("2026-11-25T04:00:05-05:30"), Some(at));
        assert_eq!(parse("2026-11-25 09:30:05z"), Some(at));
        assert_eq!(parse("2026-11-25"), Some(at - (9 * 3600 + 30 * 60 + 5) * 1000));
        assert_eq!(parse("  2026-11-25T09:30:05Z "), Some(at));
        for bad in ["", "yes", "2026-13-01", "2026-11-25T25:00:00Z", "2026-11-25T09:30:05", "2026/11/25", "1969-12-31T00:00:00Z", "2026-11-25T09:30Z"]
        {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
        // Every day for a few years survives the round trip.
        for day in (0..3 * 366).map(|d| at + d * 86_400_000) {
            assert_eq!(parse(&rfc3339(day)), Some(day / 1000 * 1000));
        }
    }

    #[test]
    fn approval_compares_the_sheet_with_the_stamp() {
        let at = parse("2026-11-25T09:30:00Z").unwrap();
        let stamp = json!("2026-11-25T09:30:00Z");
        assert_eq!(of(Some(&stamp), None), Approval::None, "no sheet");
        assert_eq!(of(None, Some(at)), Approval::None);
        assert_eq!(of(Some(&json!(null)), Some(at)), Approval::None);
        assert_eq!(of(Some(&json!(false)), Some(at)), Approval::None);
        assert_eq!(of(Some(&json!("")), Some(at)), Approval::None);
        assert_eq!(of(Some(&stamp), Some(at - 60_000)), Approval::Approved, "older sheet");
        assert_eq!(of(Some(&stamp), Some(at + SLACK_MS)), Approval::Approved, "within the slack");
        assert_eq!(of(Some(&stamp), Some(at + SLACK_MS + 1)), Approval::Changed);
        assert_eq!(of(Some(&json!(true)), Some(at)), Approval::Approved, "no time to compare");
        assert_eq!(of(Some(&json!("by the director")), Some(at)), Approval::Approved);
    }

    #[test]
    fn a_voice_names_a_provider_and_voice() {
        for yes in ["elevenlabs/JBFqnCBsd6RMkjVDRZzb", " openai/alloy ", "eleven-labs/voice_1.v2"] {
            assert!(names_voice(Some(&json!(yes))), "{yes:?}");
        }
        for no in [
            json!("(pending, see voices.md)"),
            json!("warm, low, a little hoarse"),
            json!("elevenlabs/"),
            json!("/JBF"),
            json!("ElevenLabs/JBF"),
            json!("elevenlabs/JBF and a note"),
            json!(""),
            json!(true),
            json!(null),
        ] {
            assert!(!names_voice(Some(&no)), "{no}");
        }
        assert!(!names_voice(None));
        assert!(speechless(Some(&json!("none"))) && speechless(Some(&json!(false))) && speechless(Some(&json!(" Silent "))));
        assert!(!speechless(Some(&json!("(pending)"))) && !speechless(None));
    }
}
