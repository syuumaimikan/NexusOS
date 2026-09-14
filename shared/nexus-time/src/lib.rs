//! Dates, and the arithmetic that turns one representation into the other.
//!
//! The kernel reads a wall clock once at boot and counts ticks afterwards;
//! everything else — a log line, a clock on the desktop, a package's date, a
//! timezone — needs to turn that into a year, a month and a day, or back.
//!
//! It is a shared crate rather than a corner of the kernel because the
//! conversion is pure arithmetic with no machine in it, which means it can be
//! checked against dates somebody has already worked out. A calendar that is
//! only ever exercised by being displayed is a calendar nobody has checked.
//!
//! # The algorithm
//!
//! Days-from-civil, in the form Howard Hinnant published: shift the year so
//! that March is its first month, which moves the leap day to the *end* of the
//! year and removes every special case from the day-of-year calculation except
//! the four-hundred-year era. What is left is exact integer arithmetic with no
//! table and no loop, and it is correct for any year this system will see.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::string::String;

/// Seconds in a day.
pub const DAY: i64 = 86_400;

/// A moment, in whatever zone the caller is thinking in.
///
/// There is no zone *in* this type on purpose. A `Time` is a set of fields; what
/// they mean is decided by whoever produced them, and a type that carried a zone
/// would invite arithmetic between two of them that disagreed about it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Time {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl Time {
    /// Seconds since the start of 1970.
    #[must_use]
    pub const fn unix_seconds(self) -> i64 {
        let year = self.year as i64 - if self.month <= 2 { 1 } else { 0 };
        let era = if year >= 0 { year } else { year - 399 } / 400;
        let year_of_era = year - era * 400;
        let month = self.month as i64;
        let day_of_year =
            (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + self.day as i64 - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        let days = era * 146_097 + day_of_era - 719_468;

        days * DAY + self.hour as i64 * 3_600 + self.minute as i64 * 60 + self.second as i64
    }

    /// The moment `seconds` after the start of 1970.
    ///
    /// The exact inverse of [`Time::unix_seconds`], which is what the tests
    /// check: a round trip through both has to land where it started, for every
    /// day in a century.
    #[must_use]
    pub const fn from_unix(seconds: i64) -> Self {
        // Floor division, not truncation. A negative second count is a moment
        // before 1970, and rounding it towards zero would put it on the wrong
        // day by one.
        let days = if seconds >= 0 {
            seconds / DAY
        } else {
            (seconds - (DAY - 1)) / DAY
        };
        let mut rest = seconds - days * DAY;
        if rest < 0 {
            rest += DAY;
        }

        let shifted = days + 719_468;
        let era = if shifted >= 0 {
            shifted
        } else {
            shifted - 146_096
        } / 146_097;
        let day_of_era = shifted - era * 146_097;
        let year_of_era =
            (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let year = year_of_era + era * 400;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let shifted_month = (5 * day_of_year + 2) / 153;
        let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
        let month = shifted_month + if shifted_month < 10 { 3 } else { -9 };

        Self {
            year: (year + if month <= 2 { 1 } else { 0 }) as u16,
            month: month as u8,
            day: day as u8,
            hour: (rest / 3_600) as u8,
            minute: ((rest % 3_600) / 60) as u8,
            second: (rest % 60) as u8,
        }
    }

    /// `YYYY-MM-DD HH:MM:SS`, which sorts the same way it reads.
    #[must_use]
    pub fn to_text(self) -> String {
        alloc::format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year,
            self.month,
            self.day,
            self.hour,
            self.minute,
            self.second
        )
    }

    /// `HH:MM`, for somewhere there is no room for the rest.
    #[must_use]
    pub fn to_clock(self) -> String {
        alloc::format!("{:02}:{:02}", self.hour, self.minute)
    }
}

/// Turn binary-coded decimal into a number.
///
/// The clock chip stores 25 as `0x25` unless told otherwise, and a reader that
/// assumed either encoding would be right on some machines and wrong on
/// others — the kind of bug that works on the developer's machine for a year.
#[must_use]
pub const fn from_bcd(value: u8) -> u8 {
    (value & 0x0F) + ((value >> 4) * 10)
}

/// A place's offset from UTC, in minutes.
///
/// Minutes rather than hours because not every zone is a whole number of them:
/// India is five and a half hours ahead and Nepal is five and three quarters.
/// A system that stored hours would be a system that cannot be used in either.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Zone {
    /// What it is called, for showing.
    pub name: &'static str,
    /// Minutes to add to UTC.
    pub offset_minutes: i32,
}

/// The zones this system offers to choose between.
///
/// A short list, not a database. The real thing is a table of rules that change
/// several times a year and has to be updated with the world; what this is, it
/// is honestly — a fixed offset, chosen once, with no daylight saving in it.
/// Anything more would be a table pretending to be current.
pub const ZONES: &[Zone] = &[
    Zone {
        name: "UTC",
        offset_minutes: 0,
    },
    Zone {
        name: "Japan (UTC+9)",
        offset_minutes: 9 * 60,
    },
    Zone {
        name: "Central Europe (UTC+1)",
        offset_minutes: 60,
    },
    Zone {
        name: "India (UTC+5:30)",
        offset_minutes: 5 * 60 + 30,
    },
    Zone {
        name: "US Eastern (UTC-5)",
        offset_minutes: -5 * 60,
    },
    Zone {
        name: "US Pacific (UTC-8)",
        offset_minutes: -8 * 60,
    },
];

/// The zone with this name, if it is one this system offers.
#[must_use]
pub fn zone_by_name(name: &str) -> Option<&'static Zone> {
    ZONES.iter().find(|zone| zone.name == name)
}

/// What a moment looks like in a particular zone.
#[must_use]
pub fn local(unix_seconds: i64, zone: &Zone) -> Time {
    Time::from_unix(unix_seconds + i64::from(zone.offset_minutes) * 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epoch_is_zero() {
        let time = Time {
            year: 1970,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
        };
        assert_eq!(time.unix_seconds(), 0);
        assert_eq!(Time::from_unix(0), time);
    }

    #[test]
    fn a_thousand_million_seconds() {
        // 2001-09-09 01:46:40 UTC, which is the one date everybody has a number
        // for.
        let time = Time {
            year: 2001,
            month: 9,
            day: 9,
            hour: 1,
            minute: 46,
            second: 40,
        };
        assert_eq!(time.unix_seconds(), 1_000_000_000);
        assert_eq!(Time::from_unix(1_000_000_000), time);
    }

    #[test]
    fn two_thousand_was_a_leap_year() {
        // The century rule alone would deny it: divisible by a hundred, but
        // also by four hundred.
        let before = Time {
            year: 2000,
            month: 2,
            day: 28,
            hour: 0,
            minute: 0,
            second: 0,
        };
        let after = Time {
            year: 2000,
            month: 3,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
        };
        assert_eq!(after.unix_seconds() - before.unix_seconds(), 2 * DAY);
        assert_eq!(
            Time::from_unix(before.unix_seconds() + DAY),
            Time {
                year: 2000,
                month: 2,
                day: 29,
                hour: 0,
                minute: 0,
                second: 0
            }
        );
    }

    #[test]
    fn nineteen_hundred_was_not() {
        let before = Time {
            year: 1900,
            month: 2,
            day: 28,
            hour: 0,
            minute: 0,
            second: 0,
        };
        let after = Time {
            year: 1900,
            month: 3,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
        };
        assert_eq!(after.unix_seconds() - before.unix_seconds(), DAY);
    }

    #[test]
    fn every_day_of_a_century_survives_the_round_trip() {
        // The strongest check there is: convert both ways for every day from
        // 1970 to 2070 and require the answer to be where it started. An
        // off-by-one anywhere in the era arithmetic shows up within a year.
        let mut seconds = 0i64;
        let end = 100 * 366 * DAY;
        while seconds < end {
            let time = Time::from_unix(seconds);
            assert_eq!(time.unix_seconds(), seconds, "at {seconds}");
            seconds += DAY + 3_599; // a day and a bit, so the time of day moves too
        }
    }

    #[test]
    fn moments_before_the_epoch_round_down() {
        // One second before 1970 is the last second of 1969, not the first of
        // 1970. Truncating division towards zero gets this wrong by a day.
        let time = Time::from_unix(-1);
        assert_eq!(
            time,
            Time {
                year: 1969,
                month: 12,
                day: 31,
                hour: 23,
                minute: 59,
                second: 59
            }
        );
        assert_eq!(time.unix_seconds(), -1);
    }

    #[test]
    fn packed_decimal_reads_as_decimal() {
        assert_eq!(from_bcd(0x25), 25);
        assert_eq!(from_bcd(0x00), 0);
        assert_eq!(from_bcd(0x59), 59);
    }

    #[test]
    fn a_zone_moves_the_clock_and_can_move_the_day() {
        let midnight = Time {
            year: 2026,
            month: 1,
            day: 1,
            hour: 0,
            minute: 30,
            second: 0,
        }
        .unix_seconds();

        let japan = zone_by_name("Japan (UTC+9)").expect("a zone this offers");
        let there = local(midnight, japan);
        assert_eq!(there.hour, 9);
        assert_eq!(there.day, 1);

        // And backwards over midnight, which is the case a naive
        // implementation gets wrong: half past midnight in UTC is half past
        // seven the *previous* evening in New York.
        let eastern = zone_by_name("US Eastern (UTC-5)").expect("a zone this offers");
        let then = local(midnight, eastern);
        assert_eq!(then.year, 2025);
        assert_eq!(then.month, 12);
        assert_eq!(then.day, 31);
        assert_eq!(then.hour, 19);
        assert_eq!(then.minute, 30);
    }

    #[test]
    fn a_zone_can_be_half_an_hour() {
        let noon = Time {
            year: 2026,
            month: 6,
            day: 1,
            hour: 12,
            minute: 0,
            second: 0,
        }
        .unix_seconds();
        let india = zone_by_name("India (UTC+5:30)").expect("a zone this offers");
        let there = local(noon, india);
        assert_eq!((there.hour, there.minute), (17, 30));
    }

    #[test]
    fn text_sorts_the_way_it_reads() {
        let earlier = Time::from_unix(1_000_000_000).to_text();
        let later = Time::from_unix(1_000_000_001).to_text();
        assert!(earlier < later);
        assert_eq!(earlier, "2001-09-09 01:46:40");
    }
}
