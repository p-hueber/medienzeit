//! Deciding whether to redraw the e-paper, and with which waveform.
//!
//! Pure, and here rather than in the firmware, for the same reason [`crate::docking`] is:
//! it is a decision, not a driver. E-paper updates are slow — a full refresh is the
//! better part of two seconds — and the panel has a finite number of them, so getting
//! this wrong is expensive in a way that is invisible until someone counts.
//!
//! The subtle case is the lockout. Going in or out of "blocked" inverts most of the
//! screen, and the quick waveform leaves the old image ghosted through the new one —
//! precisely when legibility matters most, because the screen is explaining why the
//! internet stopped.

use crate::{Flow, Snapshot};

/// Which waveform to drive the panel with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    /// Slow and flashing, but leaves no residue.
    Full,
    /// Fast, and accumulates ghosting.
    Quick,
}

/// Everything the screen actually shows. Redrawing on anything else wastes a refresh,
/// and the panel only has so many.
///
/// The balance is in whole minutes on purpose: it is displayed in minutes, so the other
/// 59 seconds of every minute are not a visible change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    balance_mins: i32,
    hour: u32,
    minute: u32,
    night: bool,
    flow: Flow,
    docked: [bool; 2],
}

impl Fingerprint {
    pub fn of(s: &Snapshot<2>) -> Self {
        Self {
            balance_mins: s.balance_secs / 60,
            hour: s.local.hour,
            minute: s.local.minute,
            night: s.night,
            flow: s.flow,
            docked: s.docked,
        }
    }

    /// True when the screen is in its "blocked" presentation, which is the transition
    /// that inverts it.
    fn locked_out(&self) -> bool {
        self.balance_mins <= 0
    }
}

/// Quick refreshes before forcing a full one to clear accumulated ghosting.
///
/// At one update per minute that is a flash every half hour, which is roughly the point
/// at which ghosting becomes noticeable on this panel.
pub const DEFAULT_QUICK_PER_FULL: u32 = 30;

/// Tracks what is on the panel and decides what to do next.
#[derive(Debug)]
pub struct Screen {
    last: Option<Fingerprint>,
    quick_since_full: u32,
    quick_per_full: u32,
}

impl Default for Screen {
    fn default() -> Self {
        Self::new(DEFAULT_QUICK_PER_FULL)
    }
}

impl Screen {
    pub fn new(quick_per_full: u32) -> Self {
        Self { last: None, quick_since_full: 0, quick_per_full }
    }

    /// What to draw, if anything. `None` means the panel already shows this.
    ///
    /// Call [`Screen::drawn`] once the refresh has actually happened, not before — a
    /// refresh that was decided but never performed would leave this believing the panel
    /// shows something it does not.
    pub fn decide(&self, now: Fingerprint) -> Option<Refresh> {
        let Some(previous) = self.last else {
            // First frame of the session: nothing on the panel can be trusted.
            return Some(Refresh::Full);
        };
        if previous == now {
            return None;
        }
        let inverted = previous.night != now.night || previous.locked_out() != now.locked_out();
        if inverted || self.quick_since_full >= self.quick_per_full {
            Some(Refresh::Full)
        } else {
            Some(Refresh::Quick)
        }
    }

    /// Record that the panel now shows `fp`, drawn with `mode`.
    pub fn drawn(&mut self, fp: Fingerprint, mode: Refresh) {
        self.last = Some(fp);
        self.quick_since_full = match mode {
            Refresh::Full => 0,
            Refresh::Quick => self.quick_since_full + 1,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::civil::LocalDateTime;

    fn snap(balance_secs: i32, hour: u32, minute: u32, night: bool) -> Snapshot<2> {
        Snapshot {
            balance_secs,
            cap_secs: 10_800,
            docked: [true, true],
            present: [true, true],
            flow: Flow::Filling,
            night,
            in_grace: false,
            grace_remaining_secs: 0,
            blocked: [false, false],
            local: LocalDateTime {
                year: 2026,
                month: 9,
                day: 6,
                hour,
                minute,
                second: 0,
                weekday: crate::civil::SUN,
            },
        }
    }

    fn fp(balance_secs: i32, hour: u32, minute: u32, night: bool) -> Fingerprint {
        Fingerprint::of(&snap(balance_secs, hour, minute, night))
    }

    #[test]
    fn the_first_frame_is_full_because_nothing_is_known() {
        let s = Screen::default();
        assert_eq!(s.decide(fp(600, 12, 0, false)), Some(Refresh::Full));
    }

    #[test]
    fn an_unchanged_screen_is_not_redrawn() {
        let mut s = Screen::default();
        let f = fp(600, 12, 0, false);
        s.drawn(f, Refresh::Full);
        assert_eq!(s.decide(f), None, "a refresh the panel does not need is one it never gets back");
    }

    /// Seconds are not shown, so draining through them is not a change. The bucket runs
    /// from a whole minute up to the next one, so 659 s and 600 s both display as 10.
    #[test]
    fn sub_minute_balance_movement_is_invisible() {
        let mut s = Screen::default();
        s.drawn(fp(659, 12, 0, false), Refresh::Full);
        assert_eq!(s.decide(fp(630, 12, 0, false)), None);
        assert_eq!(s.decide(fp(600, 12, 0, false)), None);
        assert_eq!(
            s.decide(fp(599, 12, 0, false)),
            Some(Refresh::Quick),
            "crossing to 9 is a digit the parent can see"
        );
    }

    #[test]
    fn an_ordinary_change_uses_the_quick_waveform() {
        let mut s = Screen::default();
        s.drawn(fp(600, 12, 0, false), Refresh::Full);
        assert_eq!(s.decide(fp(600, 12, 1, false)), Some(Refresh::Quick));
    }

    /// The case this module exists for: an inverted screen drawn quick is ghosted, and
    /// it is the screen explaining why the internet stopped.
    #[test]
    fn crossing_into_lockout_forces_a_full_refresh() {
        let mut s = Screen::default();
        s.drawn(fp(60, 12, 0, false), Refresh::Full);
        assert_eq!(s.decide(fp(0, 12, 1, false)), Some(Refresh::Full));
    }

    #[test]
    fn climbing_back_out_of_lockout_forces_one_too() {
        let mut s = Screen::default();
        s.drawn(fp(-30, 12, 0, false), Refresh::Full);
        assert_eq!(s.decide(fp(60, 12, 1, false)), Some(Refresh::Full));
    }

    #[test]
    fn staying_in_lockout_does_not_keep_flashing() {
        let mut s = Screen::default();
        s.drawn(fp(-60, 12, 0, false), Refresh::Full);
        assert_eq!(
            s.decide(fp(-120, 12, 1, false)),
            Some(Refresh::Quick),
            "already blocked and still blocked is not an inversion"
        );
    }

    #[test]
    fn entering_and_leaving_the_night_forces_a_full_refresh() {
        let mut s = Screen::default();
        s.drawn(fp(600, 20, 59, false), Refresh::Full);
        assert_eq!(s.decide(fp(600, 21, 0, true)), Some(Refresh::Full));
        s.drawn(fp(600, 21, 0, true), Refresh::Full);
        assert_eq!(s.decide(fp(600, 6, 30, false)), Some(Refresh::Full));
    }

    #[test]
    fn ghosting_is_cleared_on_a_schedule() {
        let mut s = Screen::new(3);
        s.drawn(fp(600, 12, 0, false), Refresh::Full);
        for i in 1..=3 {
            let f = fp(600, 12, i, false);
            assert_eq!(s.decide(f), Some(Refresh::Quick), "quick {i}");
            s.drawn(f, Refresh::Quick);
        }
        assert_eq!(
            s.decide(fp(600, 12, 4, false)),
            Some(Refresh::Full),
            "the fourth change clears what three quick refreshes left behind"
        );
    }

    #[test]
    fn a_full_refresh_resets_the_ghosting_count() {
        let mut s = Screen::new(2);
        s.drawn(fp(600, 12, 0, false), Refresh::Full);
        s.drawn(fp(600, 12, 1, false), Refresh::Quick);
        s.drawn(fp(600, 12, 2, false), Refresh::Full);
        assert_eq!(s.decide(fp(600, 12, 3, false)), Some(Refresh::Quick));
    }

    /// Docking is on the screen, so it has to be in the fingerprint — otherwise putting
    /// a device back would not show until the minute happened to change.
    #[test]
    fn docking_is_a_visible_change() {
        let mut s = Screen::default();
        let mut before = snap(600, 12, 0, false);
        before.docked = [true, true];
        let mut after = before.clone();
        after.docked = [true, false];
        s.drawn(Fingerprint::of(&before), Refresh::Full);
        assert_eq!(s.decide(Fingerprint::of(&after)), Some(Refresh::Quick));
    }

    #[test]
    fn a_flow_change_is_a_visible_change() {
        let mut s = Screen::default();
        let before = snap(600, 12, 0, false);
        let mut after = before.clone();
        after.flow = Flow::Draining;
        s.drawn(Fingerprint::of(&before), Refresh::Full);
        assert_eq!(s.decide(Fingerprint::of(&after)), Some(Refresh::Quick));
    }
}
