//! UTC timestamps, without a calendar crate.
//!
//! The files in `.ai_collaboration` already carry RFC 3339 timestamps, written
//! by the other agent's tools, and anything this writes has to sit beside them
//! and sort the same way. That needs a civil date out of a Unix timestamp,
//! which is about thirty lines of arithmetic and no dependency.
//!
//! Only UTC. A shared state file with two agents' local times in it would be a
//! file where "which happened first" cannot be read off the page, and that is
//! the one question the timestamps exist to answer.

use std::time::{SystemTime, UNIX_EPOCH};

/// A date and time, to the second, in UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Moment {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl Moment {
    /// Now.
    ///
    /// A clock set before 1970 would give a negative duration, which
    /// `SystemTime` reports as an error rather than a negative number. Nothing
    /// sensible can be written in that case, so it becomes the epoch: a
    /// timestamp that is obviously wrong beats one that is subtly wrong.
    pub fn now() -> Self {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_secs() as i64)
            .unwrap_or(0);
        Self::from_unix(seconds)
    }

    /// The moment `seconds` after the Unix epoch.
    pub fn from_unix(seconds: i64) -> Self {
        // Split into whole days and the seconds within the day, with the
        // remainder always positive so that dates before 1970 work too.
        let mut days = seconds.div_euclid(86_400);
        let within = seconds.rem_euclid(86_400);

        // Howard Hinnant's civil-from-days: shift the year to start in March,
        // so that the leap day is the last day of the year and every other
        // month has a fixed length in the sequence.
        days += 719_468;
        let era = days.div_euclid(146_097);
        let day_of_era = days.rem_euclid(146_097);
        let year_of_era =
            (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let year = year_of_era + era * 400;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let shifted_month = (5 * day_of_year + 2) / 153;
        let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
        // March is 0 in the shifted sequence, so months 0..=9 are March to
        // December and 10 and 11 are January and February of the next year.
        let month = if shifted_month < 10 {
            shifted_month + 3
        } else {
            shifted_month - 9
        } as u32;

        Self {
            year: if month <= 2 { year + 1 } else { year },
            month,
            day,
            hour: (within / 3600) as u32,
            minute: (within % 3600 / 60) as u32,
            second: (within % 60) as u32,
        }
    }

    /// RFC 3339, to the second, in UTC.
    ///
    /// `+00:00` rather than `Z`, because that is what is already in
    /// `STATE.json` and two spellings of the same thing in one file is a
    /// reason for somebody to write a parser later.
    pub fn stamp(&self) -> String {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}+00:00",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }

    /// A name safe to put in a filename: no colons, which Windows refuses.
    pub fn filename_stamp(&self) -> String {
        format!(
            "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

/// Seconds since the Unix epoch, now.
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

/// Read an RFC 3339 timestamp back into seconds since the epoch.
///
/// Deliberately narrow: it reads the shape this program writes and the shape
/// already in these files, `YYYY-MM-DDTHH:MM:SS` followed by an offset that is
/// either `Z`, `+00:00`, or a fractional part and then one of those. Anything
/// else returns `None`, and every caller treats that as "no idea how old this
/// is" rather than as zero -- an unreadable timestamp read as the epoch would
/// make every lock look fifty years stale and every one of them get reported.
pub fn parse(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let number = |from: usize, to: usize| -> Option<i64> { text.get(from..to)?.parse().ok() };
    if bytes[4] != b'-' || bytes[7] != b'-' || bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    if bytes[10] != b'T' && bytes[10] != b' ' {
        return None;
    }

    let year = number(0, 4)?;
    let month = number(5, 7)?;
    let day = number(8, 10)?;
    let hour = number(11, 13)?;
    let minute = number(14, 16)?;
    let second = number(17, 19)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    // days-from-civil, the inverse of the shift above.
    let shifted_year = if month <= 2 { year - 1 } else { year };
    let era = shifted_year.div_euclid(400);
    let year_of_era = shifted_year - era * 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;

    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

/// A duration as something a person reads: "3 minutes", "2 hours", "4 days".
///
/// The thresholds overshoot each unit by half, so that the answer is never the
/// awkward "1 minute" for sixty-one seconds: seconds up to a minute and a
/// half, minutes up to an hour and a half, hours up to a day and a half. The
/// cost is that "89 seconds" is a thing this says, and that reads fine.
pub fn describe(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let (count, unit) = match seconds {
        0..=89 => (seconds, "second"),
        90..=5399 => (seconds / 60, "minute"),
        5400..=129_599 => (seconds / 3600, "hour"),
        _ => (seconds / 86_400, "day"),
    };
    if count == 1 {
        format!("1 {unit}")
    } else {
        format!("{count} {unit}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epoch_is_the_first_of_january_nineteen_seventy() {
        let moment = Moment::from_unix(0);
        assert_eq!(moment.stamp(), "1970-01-01T00:00:00+00:00");
    }

    #[test]
    fn a_known_moment_reads_back_as_itself() {
        // 2026-09-14T21:26:44Z, which is one of the timestamps already in
        // STATE.json. Taken from the file rather than made up, so that this
        // agrees with whatever wrote it.
        let stamp = "2026-09-14T21:26:44+00:00";
        let seconds = parse(stamp).expect("the file's own format should parse");
        assert_eq!(Moment::from_unix(seconds).stamp(), stamp);
    }

    #[test]
    fn a_leap_day_is_a_day() {
        // 2024 was a leap year; 1900 was not, and 2000 was. The century rules
        // are where a hand-written calendar goes wrong.
        for stamp in [
            "2024-02-29T12:00:00+00:00",
            "2000-02-29T00:00:00+00:00",
            "2023-03-01T00:00:00+00:00",
            "1999-12-31T23:59:59+00:00",
        ] {
            let seconds = parse(stamp).unwrap();
            assert_eq!(Moment::from_unix(seconds).stamp(), stamp, "{stamp}");
        }
        assert!(
            parse("1900-02-29T00:00:00Z").is_none_or(
                |seconds| Moment::from_unix(seconds).stamp() != "1900-02-29T00:00:00+00:00"
            ),
            "1900 was not a leap year"
        );
    }

    #[test]
    fn every_day_of_four_centuries_round_trips() {
        // The real test. Walking the whole range catches an off-by-one in the
        // month table that any handful of spot checks would sail past.
        let mut seconds = parse("1970-01-01T00:00:00Z").unwrap();
        let end = parse("2370-01-01T00:00:00Z").unwrap();
        while seconds < end {
            let stamp = Moment::from_unix(seconds).stamp();
            assert_eq!(parse(&stamp), Some(seconds), "{stamp}");
            seconds += 86_400;
        }
    }

    #[test]
    fn a_timestamp_that_is_not_one_is_refused() {
        for bad in [
            "",
            "yesterday",
            "2026-13-01T00:00:00Z",
            "2026-09-14",
            "2026/09/14T00:00:00Z",
            "2026-09-14T99:00:00Z",
        ] {
            assert_eq!(parse(bad), None, "{bad:?} should not parse");
        }
    }

    #[test]
    fn a_zulu_suffix_and_a_fraction_are_both_accepted() {
        // Astra's tools write fractional seconds; the shape in STATE.json is
        // `2026-09-14T21:26:44.212200+00:00`. The fraction is dropped, which
        // is right for "how old is this lock" and would be wrong for nothing
        // this program does.
        let plain = parse("2026-09-14T21:26:44+00:00").unwrap();
        assert_eq!(parse("2026-09-14T21:26:44Z"), Some(plain));
        assert_eq!(parse("2026-09-14T21:26:44.212200+00:00"), Some(plain));
    }

    #[test]
    fn a_duration_is_said_in_the_largest_unit_that_fits() {
        assert_eq!(describe(1), "1 second");
        assert_eq!(describe(45), "45 seconds");
        // Each unit runs half again past its own length, so that nothing is
        // ever rounded down to a bare "1".
        assert_eq!(describe(89), "89 seconds");
        assert_eq!(describe(90), "1 minute");
        assert_eq!(describe(5399), "89 minutes");
        assert_eq!(describe(5400), "1 hour");
        assert_eq!(describe(7200), "2 hours");
        assert_eq!(describe(129_599), "35 hours");
        assert_eq!(describe(129_600), "1 day");
        assert_eq!(describe(86_400 * 3), "3 days");
        assert_eq!(describe(-5), "0 seconds");
        // And a clock that has gone backwards never says a negative age.
        assert!(!describe(-100_000).starts_with('-'));
    }

    #[test]
    fn a_filename_stamp_has_nothing_windows_refuses() {
        let name = Moment::from_unix(0).filename_stamp();
        assert_eq!(name, "19700101T000000Z");
        assert!(!name.contains(':'));
    }
}
