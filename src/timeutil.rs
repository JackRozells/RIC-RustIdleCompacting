//! Time without a time library: wall-clock milliseconds, the RFC 3339
//! timestamps Claude Code writes, and durations for people to read.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `2026-10-23T15:04:05.123Z` or `...+02:00` to epoch milliseconds.
pub fn parse_rfc3339_ms(text: &str) -> Option<i64> {
    let b = text.trim().as_bytes();
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || !(b[10] == b'T' || b[10] == b't' || b[10] == b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let num = |s: &[u8]| -> Option<i64> {
        let mut n = 0i64;
        for &c in s {
            if !c.is_ascii_digit() {
                return None;
            }
            n = n * 10 + (c - b'0') as i64;
        }
        Some(n)
    };
    let (year, month, day) = (num(&b[0..4])?, num(&b[5..7])?, num(&b[8..10])?);
    let (hour, minute, second) = (num(&b[11..13])?, num(&b[14..16])?, num(&b[17..19])?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut i = 19;
    let mut millis = 0i64;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let frac = &b[start..i];
        let mut scale = 100;
        for &c in frac.iter().take(3) {
            millis += (c - b'0') as i64 * scale;
            scale /= 10;
        }
    }
    // No zone (read as UTC) or `Z`: no offset.
    let offset_min = if i >= b.len() || b[i] == b'Z' || b[i] == b'z' {
        0
    } else if (b[i] == b'+' || b[i] == b'-') && b.len() >= i + 6 && b[i + 3] == b':' {
        let sign = if b[i] == b'-' { -1 } else { 1 };
        sign * (num(&b[i + 1..i + 3])? * 60 + num(&b[i + 4..i + 6])?)
    } else {
        return None;
    };
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_min * 60;
    Some(secs * 1_000 + millis)
}

/// Epoch milliseconds to `2026-10-23T15:04:05Z` (UTC), for logs.
pub fn format_rfc3339(ms: i64) -> String {
    let secs = ms.div_euclid(1_000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // civil_from_days (Howard Hinnant)
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3_600, (rem % 3_600) / 60, rem % 60)
}

/// 90_000 -> "1m 30s", 7_380_000 -> "2h 3m".
pub fn human_duration(ms: i64) -> String {
    let s = (ms.max(0) + 500) / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3_600 {
        let (m, r) = (s / 60, s % 60);
        if r == 0 || m >= 10 {
            format!("{m}m")
        } else {
            format!("{m}m {r}s")
        }
    } else if s < 86_400 {
        let (h, m) = (s / 3_600, (s % 3_600) / 60);
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h {m}m")
        }
    } else {
        let (d, h) = (s / 86_400, (s % 86_400) / 3_600);
        if h == 0 {
            format!("{d}d")
        } else {
            format!("{d}d {h}h")
        }
    }
}

/// 412_880 -> "413k", 1_250_000 -> "1.25M".
pub fn human_tokens(n: i64) -> String {
    if n >= 1_000_000 {
        let text = format!("{:.2}", n as f64 / 1_000_000.0);
        let text = text.trim_end_matches('0').trim_end_matches('.');
        format!("{text}M")
    } else if n >= 1_000 {
        format!("{}k", (n + 500) / 1_000)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claude_timestamps() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:01.250Z"), Some(1_250));
        assert_eq!(parse_rfc3339_ms("2026-10-23T15:04:05.123Z"), Some(1_792_767_845_123));
        assert_eq!(parse_rfc3339_ms("2026-10-23T17:04:05.123+02:00"), Some(1_792_767_845_123));
        assert_eq!(parse_rfc3339_ms("2000-02-29T12:00:00Z"), Some(951_825_600_000));
        assert_eq!(parse_rfc3339_ms("not a time"), None);
        assert_eq!(parse_rfc3339_ms("2026-13-01T00:00:00Z"), None);
    }

    #[test]
    fn formats_round_trip() {
        for ms in [0, 951_825_600_000, 1_792_767_845_000, 4_102_444_800_000] {
            assert_eq!(parse_rfc3339_ms(&format_rfc3339(ms)), Some(ms));
        }
        assert_eq!(format_rfc3339(1_792_767_845_123), "2026-10-23T15:04:05Z");
    }

    #[test]
    fn reads_durations_and_tokens() {
        assert_eq!(human_duration(90_000), "1m 30s");
        assert_eq!(human_duration(54 * 60_000), "54m");
        assert_eq!(human_duration(7_380_000), "2h 3m");
        assert_eq!(human_tokens(412_880), "413k");
        assert_eq!(human_tokens(1_250_000), "1.25M");
        assert_eq!(human_tokens(2_000_000), "2M");
        assert_eq!(human_tokens(512), "512");
    }
}
