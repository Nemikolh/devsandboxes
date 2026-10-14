//! How Inbox times read: a card's short age, the pane's stamp, the local
//! UTC offset both shift by. Shared by the dashboard and `devsandbox inbox`,
//! and outside `src/tui/` because the dashboard is unix-only while the CLI
//! isn't.

/// Past this age a card shows a date instead of a relative age.
const RELATIVE_FOR: u64 = 7 * 86_400;

/// A card's age, as short as it gets (`now`, `5m`, `3h`, `2d`), then the
/// date alone (`Oct 12`) once it's a week old: a card's corner has no room
/// for words, and the pane shows the full [`stamp`]. A timestamp
/// ahead of `now` (clock skew) reads `now`.
pub fn short_age(at: u64, now: u64, utc_offset: i64) -> String {
    match now.saturating_sub(at) {
        a if a < 60 => "now".into(),
        a if a < 3600 => format!("{}m", a / 60),
        a if a < 86_400 => format!("{}h", a / 3600),
        a if a < RELATIVE_FOR => format!("{}d", a / 86_400),
        _ => {
            let (month, day) = month_day((at as i64 + utc_offset).div_euclid(86_400));
            format!("{} {day}", MONTHS[month as usize - 1])
        }
    }
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// `Oct 2 14:32` of unix time `at`, shifted by `utc_offset` seconds (no year:
/// the Inbox only holds recent notifications).
pub fn stamp(at: u64, utc_offset: i64) -> String {
    let local = at as i64 + utc_offset;
    let (days, secs) = (local.div_euclid(86_400), local.rem_euclid(86_400));
    let (month, day) = month_day(days);
    format!("{} {day} {:02}:{:02}", MONTHS[month as usize - 1], secs / 3600, secs % 3600 / 60)
}

/// (month 1-12, day 1-31) of `days` since 1970-01-01 in the proleptic
/// Gregorian calendar (Howard Hinnant's `civil_from_days`; std has no dates).
fn month_day(days: i64) -> (i64, i64) {
    let z = days + 719_468;
    let doe = z - z.div_euclid(146_097) * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    (if mp < 10 { mp + 3 } else { mp - 9 }, day)
}

/// Parse `date +%z` output (`+0200`, `-0530`) into seconds east of UTC.
fn parse_utc_offset(s: &str) -> Option<i64> {
    let s = s.trim();
    let (sign, digits) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hours: i64 = digits[..2].parse().ok()?;
    let minutes: i64 = digits[2..].parse().ok()?;
    Some(sign * (hours * 3600 + minutes * 60))
}

/// Seconds east of UTC for the Inbox clock, from `date +%z` (std has no
/// timezone support and no tz crate is in the tree). 0 (UTC) when `date` is
/// missing or odd; read once, so a DST switch mid-session isn't picked up.
pub fn local_utc_offset() -> i64 {
    std::process::Command::new("date")
        .arg("+%z")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .and_then(|o| parse_utc_offset(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_age_then_date() {
        let now = 1_000_000_000;
        assert_eq!(short_age(now, now, 0), "now");
        assert_eq!(short_age(now + 5, now, 0), "now", "skewed clock");
        assert_eq!(short_age(now - 59, now, 0), "now");
        assert_eq!(short_age(now - 60, now, 0), "1m");
        assert_eq!(short_age(now - 5 * 60 - 30, now, 0), "5m");
        assert_eq!(short_age(now - 3599, now, 0), "59m");
        assert_eq!(short_age(now - 3 * 3600, now, 0), "3h");
        assert_eq!(short_age(now - 86_399, now, 0), "23h");
        assert_eq!(short_age(now - 2 * 86_400, now, 0), "2d");
        assert_eq!(short_age(now - (7 * 86_400 - 1), now, 0), "6d");
        // 1_000_000_000 is 2001-09-09 01:46:40 UTC: a week back is Sep 2,
        // and the date follows the local offset across midnight.
        assert_eq!(short_age(now - 7 * 86_400, now, 0), "Sep 2");
        assert_eq!(short_age(now - 7 * 86_400, now, -2 * 3600), "Sep 1");
    }

    #[test]
    fn stamp_and_offset() {
        assert_eq!(stamp(0, 0), "Jan 1 00:00");
        assert_eq!(stamp(13 * 3600 + 7 * 60 + 59, 0), "Jan 1 13:07");
        assert_eq!(stamp(23 * 3600, 2 * 3600), "Jan 2 01:00");
        assert_eq!(stamp(3600, -2 * 3600), "Dec 31 23:00");
        // Leap day, and the day after (2024-02-29 / 03-01, noon UTC).
        assert_eq!(stamp(1_709_208_000, 0), "Feb 29 12:00");
        assert_eq!(stamp(1_709_294_400, 0), "Mar 1 12:00");
        assert_eq!(stamp(1_000_000_000, 0), "Sep 9 01:46");
        assert_eq!(parse_utc_offset("+0200\n"), Some(7200));
        assert_eq!(parse_utc_offset("-0530"), Some(-(5 * 3600 + 30 * 60)));
        assert_eq!(parse_utc_offset("CEST"), None);
        assert_eq!(parse_utc_offset("+02"), None);
        assert_eq!(parse_utc_offset(""), None);
    }
}
