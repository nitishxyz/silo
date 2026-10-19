//! Drop directory names, `<yyyymmdd-hhmmss>-<6 hex>` (UTC), shared by the
//! server's uploads and clients saving files locally; the server's garbage
//! collection ages drops by the stamp.

use std::time::{SystemTime, UNIX_EPOCH};

/// A fresh drop directory name for now: `yyyymmdd-hhmmss-xxxxxx`.
#[must_use]
pub fn name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let mut x = (now.as_nanos() as u64) ^ (u64::from(std::process::id()) << 32) ^ NEXT.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    format!("{}-{:06x}", stamp(now.as_secs()), (x ^ (x >> 31)) & 0xff_ffff)
}

/// `yyyymmdd-hhmmss` (UTC) of `secs` since the epoch.
#[must_use]
pub fn stamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil(days);
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", rem / 3600, rem / 60 % 60, rem % 60)
}

/// Seconds since the epoch of a drop directory name's stamp.
#[must_use]
pub fn parse(name: &str) -> Option<u64> {
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit()).then(|| s.parse::<u64>().ok()).flatten();
    let (date, time) = (name.get(..8)?, name.get(9..15)?);
    if name.as_bytes().get(8) != Some(&b'-') {
        return None;
    }
    let (y, m, d) = (digits(&date[..4])?, digits(&date[4..6])?, digits(&date[6..])?);
    let (h, min, s) = (digits(&time[..2])?, digits(&time[2..4])?, digits(&time[4..])?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || h > 23 || min > 59 || s > 59 {
        return None;
    }
    let days = days_from_civil(y as i64, m as i64, d as i64);
    Some(u64::try_from(days).ok()? * 86_400 + h * 3600 + min * 60 + s)
}

/// Howard Hinnant's `civil_from_days`.
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `bytes` for people: `512 B`, `1.5 KB`, `2 GB` (1024-based).
#[must_use]
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    let text = if unit == 0 { bytes.to_string() } else { format!("{value:.1}") };
    format!("{} {}", text.strip_suffix(".0").unwrap_or(&text), UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_round_trip() {
        assert_eq!(stamp(0), "19700101-000000");
        assert_eq!(stamp(1_792_454_400 + 3661), "20261020-010101");
        for secs in [0, 951_782_400, 1_792_454_400 + 86_399, 4_102_444_800] {
            assert_eq!(parse(&format!("{}-abc123", stamp(secs))), Some(secs));
        }
        assert_eq!(parse("notes"), None);
        assert_eq!(parse("20261399-000000-x"), None);
        let (a, b) = (name(), name());
        assert!(a != b && a.len() == 22 && parse(&a).is_some());
    }

    #[test]
    fn sizes_read_well() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(2 << 30), "2 GB");
        assert_eq!(human_size((2 << 30) + (110 << 20)), "2.1 GB");
    }
}
