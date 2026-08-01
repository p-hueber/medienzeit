//! Civil-time helpers.
//!
//! Everything else in the crate works in UTC unix seconds. This module is the only
//! place that knows about Europe/Berlin and its DST rule, so it is also the only
//! place that can get the day boundary wrong.

pub const SECS_PER_DAY: i64 = 86_400;

/// Monday = 0 … Sunday = 6.
pub type Weekday = u8;

pub const MON: Weekday = 0;
pub const SAT: Weekday = 5;
pub const SUN: Weekday = 6;

/// A local wall-clock instant, already converted out of UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalDateTime {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub weekday: Weekday,
}

impl LocalDateTime {
    /// Minutes since local midnight — the unit away-windows are expressed in.
    pub fn minute_of_day(&self) -> u32 {
        self.hour * 60 + self.minute
    }
}

/// Days since the unix epoch for a proleptic-Gregorian date.
///
/// Howard Hinnant's `days_from_civil`; exact for any year we will ever see.
pub fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era as i64 * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`].
pub fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((if m <= 2 { y + 1 } else { y }) as i32, m, d)
}

/// Monday-based weekday for a day number. 1970-01-01 was a Thursday.
pub fn weekday_from_days(days: i64) -> Weekday {
    (days + 3).rem_euclid(7) as Weekday
}

fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        _ => unreachable!(),
    }
}

/// Day-of-month of the last Sunday in the given month.
fn last_sunday(y: i32, m: u32) -> u32 {
    let last = days_in_month(y, m);
    let wd = weekday_from_days(days_from_civil(y, m, last));
    // wd == SUN (6) -> subtract 0; wd == MON (0) -> subtract 1; etc.
    last - ((wd as u32 + 1) % 7)
}

/// UTC offset for Europe/Berlin at `utc` in seconds: +1 h CET or +2 h CEST.
///
/// The EU rule switches at 01:00 *UTC* on the last Sunday of March and October,
/// which is why this can be decided entirely in UTC — no chicken-and-egg between
/// the offset and the local time it would produce.
pub fn utc_offset(utc: i64) -> i64 {
    let (year, _, _) = civil_from_days(utc.div_euclid(SECS_PER_DAY));
    let start = days_from_civil(year, 3, last_sunday(year, 3)) * SECS_PER_DAY + 3_600;
    let end = days_from_civil(year, 10, last_sunday(year, 10)) * SECS_PER_DAY + 3_600;
    if utc >= start && utc < end {
        7_200
    } else {
        3_600
    }
}

/// Convert a UTC unix timestamp to Europe/Berlin wall-clock time.
pub fn local(utc: i64) -> LocalDateTime {
    let local_secs = utc + utc_offset(utc);
    let days = local_secs.div_euclid(SECS_PER_DAY);
    let tod = local_secs.rem_euclid(SECS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    LocalDateTime {
        year,
        month,
        day,
        hour: (tod / 3_600) as u32,
        minute: (tod % 3_600 / 60) as u32,
        second: (tod % 60) as u32,
        weekday: weekday_from_days(days),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc_of(y: i32, m: u32, d: u32, h: u32, mi: u32) -> i64 {
        days_from_civil(y, m, d) * SECS_PER_DAY + h as i64 * 3_600 + mi as i64 * 60
    }

    #[test]
    fn civil_roundtrips() {
        for days in -20_000..20_000 {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "roundtrip failed at {days}");
        }
    }

    #[test]
    fn epoch_is_a_thursday() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(weekday_from_days(0), 3);
    }

    #[test]
    fn known_dates() {
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        // 2024-01-01 was a Monday.
        assert_eq!(weekday_from_days(days_from_civil(2024, 1, 1)), MON);
        // 2026-08-01 is a Saturday.
        assert_eq!(weekday_from_days(days_from_civil(2026, 8, 1)), SAT);
    }

    #[test]
    fn leap_years() {
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2025, 2), 28);
        assert_eq!(days_in_month(2000, 2), 29);
        assert_eq!(days_in_month(1900, 2), 28);
    }

    #[test]
    fn last_sundays_are_sundays() {
        for y in 2024..2036 {
            for m in [3, 10] {
                let d = last_sunday(y, m);
                assert_eq!(weekday_from_days(days_from_civil(y, m, d)), SUN);
                // …and it really is the *last* one.
                assert!(d + 7 > days_in_month(y, m));
            }
        }
    }

    #[test]
    fn dst_spring_forward_2027() {
        // 2027-03-28 is the last Sunday in March.
        assert_eq!(last_sunday(2027, 3), 28);
        // 00:59 UTC is still CET (+1) => 01:59 local.
        let before = utc_of(2027, 3, 28, 0, 59);
        assert_eq!(utc_offset(before), 3_600);
        assert_eq!(local(before).hour, 1);
        assert_eq!(local(before).minute, 59);
        // 01:00 UTC flips to CEST (+2) => local jumps 02:00 -> 03:00.
        let after = utc_of(2027, 3, 28, 1, 0);
        assert_eq!(utc_offset(after), 7_200);
        assert_eq!(local(after).hour, 3);
        assert_eq!(local(after).minute, 0);
    }

    #[test]
    fn dst_fall_back_2027() {
        // 2027-10-31 is the last Sunday in October.
        assert_eq!(last_sunday(2027, 10), 31);
        let before = utc_of(2027, 10, 31, 0, 59);
        assert_eq!(utc_offset(before), 7_200);
        assert_eq!(local(before).hour, 2);
        // 01:00 UTC falls back to CET => local goes 03:00 -> 02:00.
        let after = utc_of(2027, 10, 31, 1, 0);
        assert_eq!(utc_offset(after), 3_600);
        assert_eq!(local(after).hour, 2);
        assert_eq!(local(after).minute, 0);
    }

    #[test]
    fn deep_winter_and_high_summer() {
        assert_eq!(utc_offset(utc_of(2027, 1, 15, 12, 0)), 3_600);
        assert_eq!(utc_offset(utc_of(2027, 7, 15, 12, 0)), 7_200);
    }

    #[test]
    fn local_conversion_is_monotonic_across_a_year() {
        // Guards against an offset lookup that flips back and forth.
        let start = utc_of(2027, 1, 1, 0, 0);
        let mut flips = 0;
        let mut prev = utc_offset(start);
        for h in 0..(365 * 24) {
            let off = utc_offset(start + h * 3_600);
            if off != prev {
                flips += 1;
                prev = off;
            }
        }
        assert_eq!(flips, 2, "expected exactly two DST transitions per year");
    }
}
