//! Turning Unix seconds into something a person can act on.
//!
//! `history` printed raw epoch seconds and `schedule status` printed
//! `next check: unix 1788251400`. Both are exact and neither answers the
//! question actually being asked, which is "was that before or after the thing
//! I am debugging?" — so every reader pasted the number into another tool.
//!
//! Timestamps render as UTC rather than local time on purpose: this output is
//! compared across a fleet whose hosts are in different zones, and a log that
//! silently means something different on each machine is worse than one that
//! is uniformly not your zone. The relative form is what the eye actually uses.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch, now.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// `2026-09-01T08:30:00Z`.
pub fn format_unix(seconds: u64) -> String {
    let days = seconds / 86_400;
    let time_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (
        time_of_day / 3_600,
        (time_of_day % 3_600) / 60,
        time_of_day % 60,
    );
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// `2026-09-01T08:30:00Z (17m ago)` — the absolute value for the record, the
/// relative one for the reader.
pub fn describe_unix(seconds: u64) -> String {
    format!("{} ({})", format_unix(seconds), relative_to_now(seconds))
}

/// `17m ago`, `in 4h`, or `now`.
pub fn relative_to_now(seconds: u64) -> String {
    let current = now();
    if seconds >= current {
        match seconds - current {
            0 => "now".into(),
            delta => format!("in {}", humanize(delta)),
        }
    } else {
        format!("{} ago", humanize(current - seconds))
    }
}

/// The largest unit that keeps the number small, because a duration is read at
/// a glance or not at all.
pub fn humanize(seconds: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    match seconds {
        s if s < MINUTE => format!("{s}s"),
        s if s < HOUR => format!("{}m", s / MINUTE),
        s if s < DAY => format!("{}h{:02}m", s / HOUR, (s % HOUR) / MINUTE),
        s => format!("{}d{}h", s / DAY, (s % DAY) / HOUR),
    }
}

/// Days since the epoch to a civil date, by Howard Hinnant's `civil_from_days`
/// (<http://howardhinnant.github.io/date_algorithms.html>). Chosen over a date
/// crate because the whole point of this binary's dependency list is that a
/// release artifact stays small enough to download on first use.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_known_epoch_instants() {
        assert_eq!(format_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_unix(1_000_000_000), "2001-09-09T01:46:40Z");
        // A leap day, which is where a hand-rolled calendar goes wrong first.
        assert_eq!(format_unix(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(format_unix(1_756_710_000), "2025-09-01T07:00:00Z");
    }

    #[test]
    fn every_day_boundary_round_trips_for_a_century() {
        // Cheap exhaustive check: the date must advance by exactly one day for
        // every day between 1970 and 2070, which catches off-by-one era math.
        let mut previous = format_unix(0);
        for day in 1..36_525u64 {
            let current = format_unix(day * 86_400);
            assert!(current > previous, "{previous} -> {current}");
            previous = current;
        }
    }

    #[test]
    fn durations_read_at_a_glance() {
        assert_eq!(humanize(45), "45s");
        assert_eq!(humanize(90), "1m");
        assert_eq!(humanize(3_600 + 120), "1h02m");
        assert_eq!(humanize(86_400 * 3 + 3_600 * 5), "3d5h");
    }

    #[test]
    fn relative_time_names_its_direction() {
        assert!(relative_to_now(now() - 600).ends_with("ago"));
        assert!(relative_to_now(now() + 600).starts_with("in "));
    }
}
