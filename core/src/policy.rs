//! The rules: how much time per day, when the day starts, and when the clock is
//! allowed to run at all.

use crate::civil::{self, LocalDateTime, Weekday, SAT, SUN};
use heapless::Vec;

/// Maximum number of away-windows in a policy.
pub const MAX_AWAY_WINDOWS: usize = 8;

/// A recurring stretch of local time during which the budget does not drain —
/// school hours, typically. Times are local minutes-since-midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AwayWindow {
    /// Bit `n` set means the window applies on weekday `n` (Monday = bit 0).
    pub weekday_mask: u8,
    pub start_minute: u32,
    pub end_minute: u32,
}

impl AwayWindow {
    /// Mon–Fri.
    pub const WEEKDAYS: u8 = 0b0001_1111;
    /// Sat–Sun.
    pub const WEEKEND: u8 = 0b0110_0000;
    pub const EVERY_DAY: u8 = 0b0111_1111;

    pub const fn new(weekday_mask: u8, start_minute: u32, end_minute: u32) -> Self {
        Self { weekday_mask, start_minute, end_minute }
    }

    /// Convenience: `AwayWindow::daily(Mon–Fri, 7, 30, 15, 0)`.
    pub const fn hm(weekday_mask: u8, sh: u32, sm: u32, eh: u32, em: u32) -> Self {
        Self::new(weekday_mask, sh * 60 + sm, eh * 60 + em)
    }

    fn applies_on(&self, weekday: Weekday) -> bool {
        self.weekday_mask & (1 << weekday) != 0
    }

    /// Windows that wrap past midnight (start > end) are supported; the wrapped
    /// tail is attributed to the same weekday the window started on.
    pub fn contains(&self, now: &LocalDateTime) -> bool {
        let m = now.minute_of_day();
        if self.start_minute <= self.end_minute {
            self.applies_on(now.weekday) && m >= self.start_minute && m < self.end_minute
        } else {
            let yesterday = (now.weekday + 6) % 7;
            (self.applies_on(now.weekday) && m >= self.start_minute)
                || (self.applies_on(yesterday) && m < self.end_minute)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub weekday_secs: u32,
    pub weekend_secs: u32,
    /// Local hour at which the budget resets. 04:00 rather than midnight, so a late
    /// evening does not get a fresh allowance at the stroke of twelve.
    pub reset_hour: u32,
    /// How long a device may be off its cradle before the budget starts draining.
    ///
    /// Covers picking the phone up to skip a track, start a podcast or answer a
    /// message. Crucially the charge is **retroactive**: cross this threshold and the
    /// whole interval since undocking is billed, so short pickups cannot be farmed
    /// into free time. Set to 0 to disable.
    pub grace_secs: u32,
    pub away: Vec<AwayWindow, MAX_AWAY_WINDOWS>,
}

impl Default for Policy {
    fn default() -> Self {
        let mut away = Vec::new();
        let _ = away.push(AwayWindow::hm(AwayWindow::WEEKDAYS, 7, 30, 15, 0));
        Self {
            weekday_secs: 60 * 60,
            weekend_secs: 120 * 60,
            reset_hour: 4,
            grace_secs: 3 * 60,
            away,
        }
    }
}

impl Policy {
    /// Which "Medienzeit day" a UTC instant belongs to, as a day number.
    ///
    /// Shifting local time back by `reset_hour` before taking the date means
    /// 03:59 still belongs to yesterday and 04:00 starts today.
    pub fn day_key(&self, utc: i64) -> i64 {
        let shifted = utc + civil::utc_offset(utc) - self.reset_hour as i64 * 3_600;
        shifted.div_euclid(civil::SECS_PER_DAY)
    }

    /// The base allowance for a day, before any bonus.
    pub fn allowance_secs(&self, day_key: i64) -> u32 {
        match civil::weekday_from_days(day_key) {
            SAT | SUN => self.weekend_secs,
            _ => self.weekday_secs,
        }
    }

    pub fn is_away(&self, now: &LocalDateTime) -> bool {
        self.away.iter().any(|w| w.contains(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::civil::{days_from_civil, SECS_PER_DAY};

    fn utc_of(y: i32, m: u32, d: u32, h: u32, mi: u32) -> i64 {
        days_from_civil(y, m, d) * SECS_PER_DAY + h as i64 * 3_600 + mi as i64 * 60
    }

    /// Berlin local time -> UTC, for tests that want to talk in wall-clock terms.
    fn berlin(y: i32, m: u32, d: u32, h: u32, mi: u32) -> i64 {
        let naive = utc_of(y, m, d, h, mi);
        naive - civil::utc_offset(naive)
    }

    #[test]
    fn day_rolls_over_at_0400_not_midnight() {
        let p = Policy::default();
        // 2026-08-03 is a Monday.
        let late_sunday_night = p.day_key(berlin(2026, 8, 3, 1, 30));
        let early_monday = p.day_key(berlin(2026, 8, 3, 3, 59));
        let monday_proper = p.day_key(berlin(2026, 8, 3, 4, 0));
        let monday_evening = p.day_key(berlin(2026, 8, 3, 22, 0));

        assert_eq!(late_sunday_night, early_monday, "01:30 and 03:59 are the same day");
        assert_eq!(monday_proper, monday_evening, "04:00 and 22:00 are the same day");
        assert_eq!(monday_proper, early_monday + 1, "04:00 starts a new day");
    }

    #[test]
    fn weekend_gets_the_larger_allowance() {
        let p = Policy::default();
        // Saturday 2026-08-01 at 10:00 local.
        assert_eq!(p.allowance_secs(p.day_key(berlin(2026, 8, 1, 10, 0))), 120 * 60);
        // Monday 2026-08-03 at 10:00 local.
        assert_eq!(p.allowance_secs(p.day_key(berlin(2026, 8, 3, 10, 0))), 60 * 60);
    }

    #[test]
    fn friday_night_is_still_a_weekday_budget() {
        // Documenting the deliberate choice: the day that began Friday 04:00 keeps
        // the weekday allowance right through Friday night until Saturday 04:00.
        let p = Policy::default();
        assert_eq!(p.allowance_secs(p.day_key(berlin(2026, 7, 31, 23, 0))), 60 * 60);
        assert_eq!(p.allowance_secs(p.day_key(berlin(2026, 8, 1, 5, 0))), 120 * 60);
    }

    #[test]
    fn away_window_boundaries_are_half_open() {
        let p = Policy::default(); // Mon-Fri 07:30-15:00
        let at = |h, mi| p.is_away(&civil::local(berlin(2026, 8, 3, h, mi)));
        assert!(!at(7, 29));
        assert!(at(7, 30), "start is inclusive");
        assert!(at(14, 59));
        assert!(!at(15, 0), "end is exclusive");
    }

    #[test]
    fn away_window_does_not_apply_at_the_weekend() {
        let p = Policy::default();
        // Saturday 2026-08-01, 10:00 — inside the time range but not the day mask.
        assert!(!p.is_away(&civil::local(berlin(2026, 8, 1, 10, 0))));
    }

    #[test]
    fn wrapping_away_window() {
        let mut p = Policy::default();
        p.away.clear();
        // "Asleep": every day 21:00 -> 06:30.
        let _ = p.away.push(AwayWindow::hm(AwayWindow::EVERY_DAY, 21, 0, 6, 30));
        let at = |h, mi| p.is_away(&civil::local(berlin(2026, 8, 3, h, mi)));
        assert!(at(22, 0));
        assert!(at(2, 0));
        assert!(at(6, 29));
        assert!(!at(6, 30));
        assert!(!at(20, 59));
    }
}
