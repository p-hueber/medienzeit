//! The accounting state machine.
//!
//! Deliberately a *wall clock*: the budget drains at 1x whenever **any** device is
//! undocked. Two devices out does not cost double. See [`Ledger::spending`].

use crate::civil::{self, LocalDateTime};
use crate::policy::Policy;
use heapless::Vec;

/// Warn this long before the budget runs out.
pub const WARNING_SECS: u32 = 5 * 60;

/// Largest gap between ticks that is treated as elapsed time. A longer gap means a
/// crash, a long sleep, or a clock correction, and is not charged to the budget.
pub const MAX_TICK_GAP_SECS: i64 = 300;

/// Edge-triggered things the firmware tasks need to react to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A new Medienzeit day began; the budget was reset.
    DayReset,
    /// Budget hit zero. Block the devices.
    Exhausted,
    /// Budget is positive again (new day, or a bonus grant). Unblock.
    Restored,
    /// Crossed below [`WARNING_SECS`] remaining.
    Warning,
    /// A tick gap larger than [`MAX_TICK_GAP_SECS`] was ignored.
    TimeJump { gap_secs: i64 },
}

pub const MAX_EVENTS: usize = 5;
pub type Events = Vec<Event, MAX_EVENTS>;

/// What the display and the web UI need, and nothing more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot<const N: usize> {
    pub remaining_secs: u32,
    pub allowance_secs: u32,
    pub spent_secs: u32,
    pub docked: [bool; N],
    /// True when an away-window is suppressing the clock right now.
    pub away: bool,
    /// True when the budget is actually draining this instant.
    pub spending: bool,
    /// True when a device is off its cradle but still inside the grace period, so
    /// nothing is being billed *yet*.
    pub in_grace: bool,
    /// Seconds of grace left before the clock starts — and bills retroactively.
    pub grace_remaining_secs: u32,
    pub exhausted: bool,
    pub local: LocalDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ledger<const N: usize> {
    day: i64,
    spent_secs: u32,
    /// Time accrued inside the current grace period. Flushed into `spent_secs` if the
    /// grace expires, discarded if she puts the device back first.
    pending_secs: u32,
    bonus_secs: u32,
    docked: [bool; N],
    last_tick: Option<i64>,
    /// When the current run of "would be spending" began, for grace accounting.
    spending_since: Option<i64>,
    was_exhausted: bool,
    warned: bool,
}

impl<const N: usize> Default for Ledger<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Ledger<N> {
    pub const fn new() -> Self {
        Self {
            day: i64::MIN,
            spent_secs: 0,
            pending_secs: 0,
            bonus_secs: 0,
            // Assume docked until a reader says otherwise: fail *closed* on the
            // clock, so a reader that never comes up cannot silently burn the day.
            docked: [true; N],
            last_tick: None,
            spending_since: None,
            was_exhausted: false,
            warned: false,
        }
    }

    /// The budget drains iff at least one device is away from its cradle.
    /// One boolean, never a per-device sum — this is what keeps it wall-clock.
    pub fn any_undocked(&self) -> bool {
        self.docked.iter().any(|d| !d)
    }

    pub fn allowance_secs(&self, policy: &Policy) -> u32 {
        policy.allowance_secs(self.day).saturating_add(self.bonus_secs)
    }

    pub fn remaining_secs(&self, policy: &Policy) -> u32 {
        self.allowance_secs(policy).saturating_sub(self.spent_secs)
    }

    pub fn exhausted(&self, policy: &Policy) -> bool {
        self.remaining_secs(policy) == 0
    }

    /// Seconds of grace left in the current pickup, 0 if not in one.
    pub fn grace_remaining_secs(&self, utc: i64, policy: &Policy) -> u32 {
        let Some(started) = self.spending_since else { return 0 };
        let elapsed = utc.saturating_sub(started).clamp(0, u32::MAX as i64) as u32;
        policy.grace_secs.saturating_sub(elapsed)
    }

    /// Grant extra time for today. Survives until the next day reset.
    pub fn grant_bonus(&mut self, secs: u32) {
        self.bonus_secs = self.bonus_secs.saturating_add(secs);
    }

    /// Advance to `utc`, with the current dock state of each device.
    ///
    /// Call this at ~1 Hz. It is idempotent-ish in the sense that a repeated call
    /// with the same timestamp charges nothing.
    pub fn tick(&mut self, utc: i64, docked: [bool; N], policy: &Policy) -> (Snapshot<N>, Events) {
        let mut events = Events::new();
        let local = civil::local(utc);
        let day = policy.day_key(utc);

        if day != self.day {
            // A backwards jump (NTP correcting a wildly wrong RTC) also lands here.
            // Resetting is the safe direction: it can hand back time, never steal it.
            self.day = day;
            self.spent_secs = 0;
            self.pending_secs = 0;
            self.bonus_secs = 0;
            self.warned = false;
            self.last_tick = None;
            self.spending_since = None;
            let _ = events.push(Event::DayReset);
        }

        self.docked = docked;
        let away = policy.is_away(&local);
        let running = self.any_undocked() && !away;

        // Grace tracks an unbroken run of "would be spending". Redocking — or an away
        // window starting — ends the run and forgives whatever had accrued.
        if running {
            if self.spending_since.is_none() {
                self.spending_since = Some(utc);
            }
        } else {
            self.spending_since = None;
            self.pending_secs = 0;
        }

        let grace_left = self.grace_remaining_secs(utc, policy);
        let in_grace = running && grace_left > 0;

        if let Some(last) = self.last_tick {
            let gap = utc - last;
            if gap > MAX_TICK_GAP_SECS {
                let _ = events.push(Event::TimeJump { gap_secs: gap });
            } else if gap > 0 && running {
                if in_grace {
                    self.pending_secs = self.pending_secs.saturating_add(gap as u32);
                } else {
                    // Grace has lapsed: bill this tick *and* everything held back
                    // during the grace period. Short pickups stay free; the moment
                    // one turns into real use, the whole pickup is charged.
                    self.spent_secs = self
                        .spent_secs
                        .saturating_add(self.pending_secs)
                        .saturating_add(gap as u32);
                    self.pending_secs = 0;
                }
            }
        }
        self.last_tick = Some(utc);
        let spending = running && !in_grace;

        let remaining = self.remaining_secs(policy);
        let exhausted = remaining == 0;

        if remaining <= WARNING_SECS && remaining > 0 && !self.warned {
            self.warned = true;
            let _ = events.push(Event::Warning);
        }
        if remaining > WARNING_SECS {
            self.warned = false;
        }
        if exhausted && !self.was_exhausted {
            let _ = events.push(Event::Exhausted);
        }
        if !exhausted && self.was_exhausted {
            let _ = events.push(Event::Restored);
        }
        self.was_exhausted = exhausted;

        let snapshot = Snapshot {
            remaining_secs: remaining,
            allowance_secs: self.allowance_secs(policy),
            spent_secs: self.spent_secs,
            docked: self.docked,
            away,
            spending,
            in_grace,
            grace_remaining_secs: grace_left,
            exhausted,
            local,
        };
        (snapshot, events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::civil::{days_from_civil, SECS_PER_DAY};
    use crate::policy::AwayWindow;

    fn berlin(y: i32, m: u32, d: u32, h: u32, mi: u32) -> i64 {
        let naive = days_from_civil(y, m, d) * SECS_PER_DAY + h as i64 * 3_600 + mi as i64 * 60;
        naive - civil::utc_offset(naive)
    }

    /// A policy with no away-windows and no grace, so tests opt into both explicitly.
    fn plain_policy() -> Policy {
        let mut p = Policy::default();
        p.away.clear();
        p.grace_secs = 0;
        p
    }

    /// Run one tick per second from `start` through `start + secs` inclusive, the way
    /// the firmware's 1 Hz loop does. Returns the final timestamp and snapshot.
    ///
    /// The tick at `start` charges nothing (there is no preceding interval), so a run
    /// of `secs` seconds while spending charges exactly `secs`.
    fn run(
        l: &mut Ledger<2>,
        p: &Policy,
        start: i64,
        secs: i64,
        docked: [bool; 2],
    ) -> (i64, Snapshot<2>) {
        let mut snap = l.tick(start, docked, p).0;
        for t in 1..=secs {
            snap = l.tick(start + t, docked, p).0;
        }
        (start + secs, snap)
    }

    const DOCKED: [bool; 2] = [true, true];
    const ONE_OUT: [bool; 2] = [false, true];
    const BOTH_OUT: [bool; 2] = [false, false];

    #[test]
    fn docked_devices_do_not_spend() {
        let p = plain_policy();
        let mut l = Ledger::<2>::new();
        let (_, snap) = run(&mut l, &p, berlin(2026, 8, 3, 17, 0), 600, DOCKED);
        assert_eq!(snap.spent_secs, 0);
        assert_eq!(snap.remaining_secs, 60 * 60);
        assert!(!snap.spending);
    }

    #[test]
    fn both_devices_out_still_drains_at_1x() {
        let p = plain_policy();
        let start = berlin(2026, 8, 3, 17, 0);

        let mut one = Ledger::<2>::new();
        let (_, one_snap) = run(&mut one, &p, start, 600, ONE_OUT);

        let mut both = Ledger::<2>::new();
        let (_, both_snap) = run(&mut both, &p, start, 600, BOTH_OUT);

        assert_eq!(one_snap.spent_secs, 600);
        assert_eq!(
            both_snap.spent_secs, one_snap.spent_secs,
            "two devices undocked must not drain faster than one"
        );
    }

    #[test]
    fn docking_one_of_two_keeps_the_clock_running() {
        let p = plain_policy();
        let mut l = Ledger::<2>::new();
        let (mid, _) = run(&mut l, &p, berlin(2026, 8, 3, 17, 0), 300, BOTH_OUT);
        let (_, snap) = run(&mut l, &p, mid, 300, ONE_OUT);
        assert_eq!(snap.spent_secs, 600, "rate is unchanged by docking one device");
        assert!(snap.spending);
    }

    #[test]
    fn away_window_suppresses_spending() {
        let mut p = plain_policy();
        let _ = p.away.push(AwayWindow::hm(AwayWindow::WEEKDAYS, 7, 30, 15, 0));
        let mut l = Ledger::<2>::new();
        // Monday 09:00, undocked the whole hour — at school.
        let (_, snap) = run(&mut l, &p, berlin(2026, 8, 3, 9, 0), 3_600, BOTH_OUT);
        assert_eq!(snap.spent_secs, 0);
        assert!(snap.away);
        assert!(!snap.spending);
    }

    #[test]
    fn spending_resumes_when_the_away_window_ends() {
        let mut p = plain_policy();
        let _ = p.away.push(AwayWindow::hm(AwayWindow::WEEKDAYS, 7, 30, 15, 0));
        let mut l = Ledger::<2>::new();
        // 14:55 -> 15:05: five minutes inside the window, five outside.
        let (_, snap) = run(&mut l, &p, berlin(2026, 8, 3, 14, 55), 600, ONE_OUT);

        // 301, not 300: each tick charges the second that *preceded* it at the rate
        // sampled *at* the tick. The tick landing exactly on 15:00 therefore bills the
        // final second of the away window. One second of slop per boundary crossing is
        // the price of reacting to an undock instantly rather than a tick late.
        assert_eq!(snap.spent_secs, 301);
    }

    #[test]
    fn exhaustion_fires_once_and_restores_on_bonus() {
        let mut p = plain_policy();
        p.weekday_secs = 10;
        let mut l = Ledger::<2>::new();
        let start = berlin(2026, 8, 3, 17, 0);

        // Run well past zero; Exhausted must not re-fire on every subsequent tick.
        let mut exhausted_count = 0;
        for t in 0..=29 {
            let (_, ev) = l.tick(start + t, ONE_OUT, &p);
            exhausted_count += ev.iter().filter(|e| **e == Event::Exhausted).count();
        }
        assert_eq!(exhausted_count, 1, "Exhausted is edge-triggered, not level-triggered");
        assert_eq!(l.spent_secs, 29);

        l.grant_bonus(60);
        let (snap, ev) = l.tick(start + 31, ONE_OUT, &p);
        assert!(ev.contains(&Event::Restored));
        assert!(!snap.exhausted);
        // Allowance 10 + 60 bonus = 70; spent 29 + the 2 s gap = 31.
        assert_eq!(snap.allowance_secs, 70);
        assert_eq!(snap.spent_secs, 31);
        assert_eq!(snap.remaining_secs, 39);
    }

    #[test]
    fn warning_fires_once_crossing_the_threshold() {
        let mut p = plain_policy();
        p.weekday_secs = WARNING_SECS + 5;
        let mut l = Ledger::<2>::new();
        let start = berlin(2026, 8, 3, 17, 0);

        let mut warnings = 0;
        for t in 0..10 {
            let (_, ev) = l.tick(start + t, ONE_OUT, &p);
            warnings += ev.iter().filter(|e| **e == Event::Warning).count();
        }
        assert_eq!(warnings, 1);
    }

    #[test]
    fn day_reset_clears_spend_and_bonus() {
        let p = plain_policy();
        let mut l = Ledger::<2>::new();
        // Spend 20 min on Monday evening.
        let (_, snap) = run(&mut l, &p, berlin(2026, 8, 3, 22, 0), 1_200, ONE_OUT);
        l.grant_bonus(600);
        assert_eq!(snap.spent_secs, 1_200);

        // Jump to Tuesday 04:00. The gap is huge, so it must not be charged.
        let next = berlin(2026, 8, 4, 4, 0);
        let (snap, ev) = l.tick(next, ONE_OUT, &p);
        assert!(ev.contains(&Event::DayReset));
        assert_eq!(snap.spent_secs, 0);
        assert_eq!(snap.allowance_secs, 60 * 60, "bonus does not carry over");
    }

    #[test]
    fn no_carryover_of_unused_time() {
        let p = plain_policy();
        let mut l = Ledger::<2>::new();
        // Monday: spend nothing at all.
        let (_, _) = l.tick(berlin(2026, 8, 3, 20, 0), DOCKED, &p);
        // Tuesday: still exactly one day's allowance.
        let (snap, _) = l.tick(berlin(2026, 8, 4, 20, 0), DOCKED, &p);
        assert_eq!(snap.allowance_secs, 60 * 60);
    }

    #[test]
    fn long_gap_is_not_charged() {
        let p = plain_policy();
        let mut l = Ledger::<2>::new();
        let start = berlin(2026, 8, 3, 17, 0);
        l.tick(start, ONE_OUT, &p);
        // Same day, but an hour later with no ticks in between: a crash or a sleep.
        let (snap, ev) = l.tick(start + 3_600, ONE_OUT, &p);
        assert!(matches!(ev[0], Event::TimeJump { .. }));
        assert_eq!(snap.spent_secs, 0);
    }

    #[test]
    fn a_normal_one_second_tick_is_charged() {
        let p = plain_policy();
        let mut l = Ledger::<2>::new();
        let start = berlin(2026, 8, 3, 17, 0);
        l.tick(start, ONE_OUT, &p);
        let (snap, _) = l.tick(start + 1, ONE_OUT, &p);
        assert_eq!(snap.spent_secs, 1);
    }

    #[test]
    fn starts_assuming_docked() {
        // If a PN532 never initialises we must not burn the whole day silently.
        let l = Ledger::<2>::new();
        assert!(!l.any_undocked());
    }

    /// Grace policy: 3 minutes, no away-windows.
    fn grace_policy() -> Policy {
        let mut p = plain_policy();
        p.grace_secs = 180;
        p
    }

    #[test]
    fn brief_pickup_within_grace_costs_nothing() {
        let p = grace_policy();
        let mut l = Ledger::<2>::new();
        let start = berlin(2026, 8, 3, 17, 0);

        // Picked up for 90 s to start a podcast, then put back.
        let (t, snap) = run(&mut l, &p, start, 90, ONE_OUT);
        assert_eq!(snap.spent_secs, 0, "nothing billed while inside grace");
        assert!(snap.in_grace);
        assert!(!snap.spending);
        assert_eq!(snap.grace_remaining_secs, 90);

        let (_, snap) = run(&mut l, &p, t, 5, DOCKED);
        assert_eq!(snap.spent_secs, 0, "redocking in time forgives the pickup");
        assert!(!snap.in_grace);
    }

    #[test]
    fn overrunning_grace_bills_the_whole_pickup_retroactively() {
        let p = grace_policy();
        let mut l = Ledger::<2>::new();
        // Out for 5 minutes: the 3 grace minutes are billed too, not just the excess.
        let (_, snap) = run(&mut l, &p, berlin(2026, 8, 3, 17, 0), 300, ONE_OUT);
        assert_eq!(snap.spent_secs, 300);
        assert!(snap.spending);
        assert!(!snap.in_grace);
    }

    #[test]
    fn grace_cannot_be_farmed_into_free_time() {
        // The exploit this guards against: undock, use for just under the grace
        // period, redock for a second, repeat forever. Retroactive billing means the
        // only way to stay free is to genuinely put the device back — but a *long*
        // session broken into chunks must still cost close to its real length.
        let p = grace_policy();
        let mut l = Ledger::<2>::new();
        let mut t = berlin(2026, 8, 3, 17, 0);

        // Ten cycles of "out for 170 s, back for 2 s" — 28 minutes of wall time.
        for _ in 0..10 {
            let (next, _) = run(&mut l, &p, t, 170, ONE_OUT);
            let (next, _) = run(&mut l, &p, next, 2, DOCKED);
            t = next;
        }
        // Each cycle really is under the grace period, so it really is free. That is
        // the intended deal: she is genuinely docking the device every three minutes.
        assert_eq!(l.spent_secs, 0);

        // But the moment one pickup runs long, the full pickup is billed — there is no
        // per-pickup free allowance carried into it.
        let (_, snap) = run(&mut l, &p, t, 600, ONE_OUT);
        assert_eq!(snap.spent_secs, 600);
    }

    #[test]
    fn grace_restarts_only_after_a_genuine_redock() {
        let p = grace_policy();
        let mut l = Ledger::<2>::new();
        let start = berlin(2026, 8, 3, 17, 0);

        // Out for 4 minutes: grace consumed, now billing.
        let (t, snap) = run(&mut l, &p, start, 240, ONE_OUT);
        assert_eq!(snap.spent_secs, 240);

        // Docking the *other* device changes nothing — the first is still out.
        let (t, snap) = run(&mut l, &p, t, 60, ONE_OUT);
        assert_eq!(snap.spent_secs, 300, "no fresh grace without docking everything");
        assert!(!snap.in_grace);

        // Both docked, then picked up again: a new pickup, fresh grace.
        let (t, _) = run(&mut l, &p, t, 5, DOCKED);
        let (_, snap) = run(&mut l, &p, t, 60, ONE_OUT);
        assert_eq!(snap.spent_secs, 300);
        assert!(snap.in_grace);
    }

    #[test]
    fn away_window_ending_mid_pickup_forgives_and_restarts_grace() {
        let mut p = grace_policy();
        let _ = p.away.push(AwayWindow::hm(AwayWindow::WEEKDAYS, 7, 30, 15, 0));
        let mut l = Ledger::<2>::new();
        // Undocked from 14:58, so the away window covers the first two minutes.
        let (_, snap) = run(&mut l, &p, berlin(2026, 8, 3, 14, 58), 240, ONE_OUT);
        // Grace starts at 15:00 when billing would otherwise begin, and 2 min later
        // it has not yet lapsed.
        assert!(snap.in_grace);
        assert_eq!(snap.spent_secs, 0);
    }

    #[test]
    fn grace_does_not_delay_exhaustion_once_billing_starts() {
        let mut p = grace_policy();
        p.weekday_secs = 240;
        let mut l = Ledger::<2>::new();
        let start = berlin(2026, 8, 3, 17, 0);

        let mut exhausted_at = None;
        for t in 0..=400 {
            let (snap, ev) = l.tick(start + t, ONE_OUT, &p);
            if ev.contains(&Event::Exhausted) {
                exhausted_at = Some((t, snap.spent_secs));
            }
        }
        // 240 s of budget, billed retroactively from the undock, so zero is reached at
        // t=240 — grace delays the *display*, never the total.
        assert_eq!(exhausted_at, Some((240, 240)));
    }

    #[test]
    fn dst_spring_forward_does_not_grant_or_steal_budget() {
        let p = plain_policy();
        let mut l = Ledger::<2>::new();
        // 2027-03-28 00:30 UTC: one real hour spanning the local 02:00 -> 03:00 jump.
        let start = days_from_civil(2027, 3, 28) * SECS_PER_DAY + 30 * 60;
        let (_, snap) = run(&mut l, &p, start, 3_600, ONE_OUT);
        assert_eq!(snap.spent_secs, 3_600, "one real hour is charged as one hour");
    }
}
