//! The rules: how the balance refills, how far it may run in either direction, and
//! when the devices are locked out entirely.

use crate::civil::{LocalDateTime, Weekday};
use heapless::Vec;

pub const MAX_WINDOWS: usize = 4;

/// A recurring stretch of local time, given as minutes since midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// Bit `n` set means the window applies on weekday `n` (Monday = bit 0).
    pub weekday_mask: u8,
    pub start_minute: u32,
    pub end_minute: u32,
}

impl Window {
    pub const WEEKDAYS: u8 = 0b0001_1111;
    pub const WEEKEND: u8 = 0b0110_0000;
    pub const EVERY_DAY: u8 = 0b0111_1111;

    pub const fn new(weekday_mask: u8, start_minute: u32, end_minute: u32) -> Self {
        Self { weekday_mask, start_minute, end_minute }
    }

    /// `Window::hm(EVERY_DAY, 21, 0, 6, 30)` — 21:00 to 06:30.
    pub const fn hm(weekday_mask: u8, sh: u32, sm: u32, eh: u32, em: u32) -> Self {
        Self::new(weekday_mask, sh * 60 + sm, eh * 60 + em)
    }

    fn applies_on(&self, weekday: Weekday) -> bool {
        self.weekday_mask & (1 << weekday) != 0
    }

    /// Windows that wrap past midnight (start > end) are supported, which the night
    /// window always does; the wrapped tail belongs to the day the window started on.
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
    /// Refill ratio as an exact fraction: `refill_num` seconds of balance are earned
    /// for every `refill_den` seconds spent not using the devices.
    ///
    /// The default 1:10 means "ten minutes not using earns one minute of screen time",
    /// so a 21:00–07:00 night within the reader's field funds an hour the next day. A ratio explains
    /// far better to a child than a rate, and keeping it rational keeps the accounting
    /// integer-exact — no drift from repeated float rounding.
    pub refill_num: u32,
    pub refill_den: u32,
    /// Most balance she can bank. Caps hoarding without ever capping a single session.
    pub cap_secs: u32,
    /// How far below zero the balance may go, as a positive magnitude.
    ///
    /// This is load-bearing rather than a nicety: once the balance hits zero, debt is
    /// the only remaining reason to put a device back, because leaving it out keeps
    /// digging. Keep it shallow enough to clear in one night.
    pub floor_secs: u32,
    /// Balance a freshly provisioned ledger starts with, so day one is not spent
    /// staring at an empty bank.
    pub prefill_secs: u32,
    /// How long a device may be away from the reader before the balance starts draining.
    ///
    /// Covers picking a phone up to skip a track or answer a message. Billing is
    /// **retroactive**: cross the threshold and the whole pickup is charged, so short
    /// pickups cannot be farmed into free time.
    pub grace_secs: u32,
    /// Devices are locked out entirely during these windows, regardless of balance.
    pub night: Vec<Window, MAX_WINDOWS>,
}

impl Default for Policy {
    fn default() -> Self {
        let mut night = Vec::new();
        let _ = night.push(Window::hm(Window::EVERY_DAY, 21, 0, 6, 30));
        Self {
            refill_num: 1,
            refill_den: 10,
            cap_secs: 3 * 60 * 60,
            floor_secs: 30 * 60,
            prefill_secs: 60 * 60,
            grace_secs: 3 * 60,
            night,
        }
    }
}

impl Policy {
    pub fn is_night(&self, now: &LocalDateTime) -> bool {
        self.night.iter().any(|w| w.contains(now))
    }

    pub fn floor(&self) -> i32 {
        -(self.floor_secs as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::civil::{self, days_from_civil, SECS_PER_DAY};

    fn berlin(y: i32, m: u32, d: u32, h: u32, mi: u32) -> i64 {
        let naive = days_from_civil(y, m, d) * SECS_PER_DAY + h as i64 * 3_600 + mi as i64 * 60;
        naive - civil::utc_offset(naive)
    }

    fn at(p: &Policy, h: u32, mi: u32) -> bool {
        p.is_night(&civil::local(berlin(2026, 8, 3, h, mi)))
    }

    #[test]
    fn night_window_wraps_past_midnight() {
        let p = Policy::default(); // 21:00 -> 06:30
        assert!(!at(&p, 20, 59));
        assert!(at(&p, 21, 0), "start is inclusive");
        assert!(at(&p, 23, 30));
        assert!(at(&p, 3, 0));
        assert!(at(&p, 6, 29));
        assert!(!at(&p, 6, 30), "end is exclusive");
        assert!(!at(&p, 12, 0));
    }

    #[test]
    fn a_weekday_only_window_does_not_leak_into_the_weekend() {
        let mut p = Policy::default();
        p.night.clear();
        let _ = p.night.push(Window::hm(Window::WEEKDAYS, 21, 0, 6, 30));
        // Saturday 2026-08-01 22:00 — inside the time range, outside the day mask.
        assert!(!p.is_night(&civil::local(berlin(2026, 8, 1, 22, 0))));
        // Monday 2026-08-03 22:00 — inside both.
        assert!(p.is_night(&civil::local(berlin(2026, 8, 3, 22, 0))));
    }

    #[test]
    fn floor_is_negative() {
        assert_eq!(Policy::default().floor(), -1_800);
    }
}
