//! The accounting state machine.
//!
//! A single balance that **fills whenever the devices are not being used at home** and
//! **drains at 1× while any of them is**. There is no daily reset and no allowance: the
//! bucket is the whole model.
//!
//! Two properties are deliberate and easy to break by accident:
//!
//! - It is a **wall clock**. Two devices out drains at 1×, not 2×.
//! - Being **out of the house earns exactly what being docked earns**. Anything else
//!   penalises her for going outside, which is the opposite of the point.

use crate::civil::{self, LocalDateTime};
use crate::policy::Policy;
use heapless::Vec;

/// Warn this long before the balance runs out.
pub const WARNING_SECS: i32 = 5 * 60;

/// Largest gap between ticks treated as elapsed time. Anything longer is a crash, a
/// sleep or a clock correction, and is neither charged nor credited.
pub const MAX_TICK_GAP_SECS: i64 = 300;

/// What the balance is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Not in use at home: docked, or out of the house.
    Filling,
    /// In use at home, past grace.
    Draining,
    /// Neither — inside the grace period, or undocked during the night window.
    Held,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Balance reached zero. Block whatever is undocked.
    Exhausted,
    /// Balance is positive again. Unblock.
    Restored,
    /// Crossed below [`WARNING_SECS`] remaining.
    Warning,
    NightBegan,
    NightEnded,
    /// A device left its cradle during the night window. The router block does
    /// nothing about offline use, so this alert is the actual enforcement.
    UndockedAtNight { device: usize },
    /// A tick gap larger than [`MAX_TICK_GAP_SECS`] was ignored.
    TimeJump { gap_secs: i64 },
}

pub const MAX_EVENTS: usize = 6;
pub type Events = Vec<Event, MAX_EVENTS>;

/// Everything the display, the web UI and the enforcement task need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot<const N: usize> {
    /// May be negative, down to [`Policy::floor`].
    pub balance_secs: i32,
    pub cap_secs: u32,
    pub docked: [bool; N],
    /// Whether the FRITZ!Box currently sees each device on the home network.
    pub present: [bool; N],
    pub flow: Flow,
    pub night: bool,
    pub in_grace: bool,
    pub grace_remaining_secs: u32,
    /// Per device. A docked device is never blocked — that is what keeps a reason to
    /// put it back once the balance is gone.
    pub blocked: [bool; N],
    pub local: LocalDateTime,
}

impl<const N: usize> Snapshot<N> {
    pub fn exhausted(&self) -> bool {
        self.balance_secs <= 0
    }
    pub fn any_blocked(&self) -> bool {
        self.blocked.iter().any(|b| *b)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ledger<const N: usize> {
    balance_secs: i32,
    /// Time accrued inside the current grace period, flushed into the balance if grace
    /// lapses and discarded if she puts the device back first.
    pending_secs: u32,
    /// Sub-second remainder of the refill fraction, carried between ticks so a 1:10
    /// ratio credits exactly one second per ten and never drifts.
    refill_remainder: u32,
    docked: [bool; N],
    present: [bool; N],
    last_tick: Option<i64>,
    /// When the current unbroken run of "would be draining" began, for grace.
    draining_since: Option<i64>,
    was_exhausted: bool,
    was_night: bool,
    /// Per device: was it already out of the box during the night window last tick?
    night_offence: [bool; N],
    warned: bool,
}

impl<const N: usize> Ledger<N> {
    /// A freshly provisioned ledger, seeded with [`Policy::prefill_secs`].
    pub fn new(policy: &Policy) -> Self {
        Self::with_balance(policy.prefill_secs as i32)
    }

    /// Restore a persisted balance, or start a test from a known point.
    pub const fn with_balance(balance_secs: i32) -> Self {
        Self {
            balance_secs,
            pending_secs: 0,
            refill_remainder: 0,
            // Assume docked until a reader says otherwise: fail *closed* on the clock,
            // so a reader that never comes up cannot silently drain the balance.
            docked: [true; N],
            // Assume absent until the FRITZ!Box says otherwise, for the same reason —
            // draining requires positive evidence that she is home.
            present: [false; N],
            last_tick: None,
            draining_since: None,
            was_exhausted: false,
            was_night: false,
            night_offence: [false; N],
            warned: false,
        }
    }

    pub fn balance_secs(&self) -> i32 {
        self.balance_secs
    }

    /// Grant extra time. The cap still applies.
    pub fn grant_bonus(&mut self, secs: u32, policy: &Policy) {
        self.balance_secs = (self.balance_secs + secs as i32).min(policy.cap_secs as i32);
    }

    /// A device counts as in use when it is off its cradle *and* the box can see it on
    /// the home network. Off-network means out of the house, which is free.
    fn in_use_at_home(&self) -> bool {
        (0..N).any(|i| !self.docked[i] && self.present[i])
    }

    fn any_undocked(&self) -> bool {
        self.docked.iter().any(|d| !*d)
    }

    /// Seconds of grace left in the current pickup, 0 if not in one.
    pub fn grace_remaining_secs(&self, utc: i64, policy: &Policy) -> u32 {
        let Some(started) = self.draining_since else { return 0 };
        let elapsed = utc.saturating_sub(started).clamp(0, u32::MAX as i64) as u32;
        policy.grace_secs.saturating_sub(elapsed)
    }

    /// Credit `secs` of not-using at the policy's refill ratio, carrying the remainder.
    fn refill(&mut self, secs: u32, policy: &Policy) {
        if policy.refill_den == 0 {
            return;
        }
        let total = self.refill_remainder as u64 + secs as u64 * policy.refill_num as u64;
        let earned = (total / policy.refill_den as u64) as i32;
        self.refill_remainder = (total % policy.refill_den as u64) as u32;
        self.balance_secs = (self.balance_secs + earned).min(policy.cap_secs as i32);
    }

    /// Advance to `utc`. Call at ~1 Hz.
    ///
    /// `docked` comes from the NFC reader, `present` from the FRITZ!Box host table.
    pub fn tick(
        &mut self,
        utc: i64,
        docked: [bool; N],
        present: [bool; N],
        policy: &Policy,
    ) -> (Snapshot<N>, Events) {
        let mut events = Events::new();
        let local = civil::local(utc);

        self.docked = docked;
        self.present = present;

        let night = policy.is_night(&local);
        if night != self.was_night {
            let _ = events.push(if night { Event::NightBegan } else { Event::NightEnded });
            self.was_night = night;
        }

        let in_use = self.in_use_at_home();
        let any_undocked = self.any_undocked();

        // Grace tracks an unbroken run of in-use-at-home. Docking, leaving the house,
        // or the night window starting all end the run and forgive what had accrued.
        let would_drain = in_use && !night;
        if would_drain {
            if self.draining_since.is_none() {
                self.draining_since = Some(utc);
            }
        } else {
            self.draining_since = None;
            self.pending_secs = 0;
        }

        let grace_left = self.grace_remaining_secs(utc, policy);
        let in_grace = would_drain && grace_left > 0;

        let flow = if night {
            // Taking a device out at night neither earns nor costs; it is simply
            // locked, and the alert below is what actually matters.
            if any_undocked { Flow::Held } else { Flow::Filling }
        } else if would_drain {
            if in_grace { Flow::Held } else { Flow::Draining }
        } else {
            Flow::Filling
        };

        if let Some(last) = self.last_tick {
            let gap = utc - last;
            if gap > MAX_TICK_GAP_SECS {
                let _ = events.push(Event::TimeJump { gap_secs: gap });
            } else if gap > 0 {
                let g = gap as u32;
                match flow {
                    Flow::Filling => self.refill(g, policy),
                    Flow::Held => {
                        if in_grace {
                            self.pending_secs = self.pending_secs.saturating_add(g);
                        }
                    }
                    Flow::Draining => {
                        // Grace has lapsed: bill this tick and everything held back
                        // during the grace period.
                        let owed = self.pending_secs.saturating_add(g) as i32;
                        self.pending_secs = 0;
                        self.balance_secs = (self.balance_secs - owed).max(policy.floor());
                    }
                }
            }
        }
        self.last_tick = Some(utc);

        // A docked device is never blocked, so docking always restores something.
        let mut blocked = [false; N];
        let lock_out = night || self.balance_secs <= 0;
        for (i, b) in blocked.iter_mut().enumerate() {
            let out = !self.docked[i];
            *b = lock_out && out;
            // Edge-triggered, or it would alert once per second all night.
            let offence = night && out;
            if offence && !self.night_offence[i] {
                let _ = events.push(Event::UndockedAtNight { device: i });
            }
            self.night_offence[i] = offence;
        }

        let exhausted = self.balance_secs <= 0;
        if self.balance_secs <= WARNING_SECS && !exhausted && !self.warned {
            self.warned = true;
            let _ = events.push(Event::Warning);
        }
        if self.balance_secs > WARNING_SECS {
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
            balance_secs: self.balance_secs,
            cap_secs: policy.cap_secs,
            docked: self.docked,
            present: self.present,
            flow,
            night,
            in_grace,
            grace_remaining_secs: grace_left,
            blocked,
            local,
        };
        (snapshot, events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::civil::{days_from_civil, SECS_PER_DAY};
    use crate::policy::Window;

    fn berlin(y: i32, m: u32, d: u32, h: u32, mi: u32) -> i64 {
        let naive = days_from_civil(y, m, d) * SECS_PER_DAY + h as i64 * 3_600 + mi as i64 * 60;
        naive - civil::utc_offset(naive)
    }

    /// No night window and no grace, so tests opt into both explicitly.
    fn plain() -> Policy {
        let mut p = Policy::default();
        p.night.clear();
        p.grace_secs = 0;
        p.prefill_secs = 0;
        p
    }

    const DOCKED: [bool; 2] = [true, true];
    const ONE_OUT: [bool; 2] = [false, true];
    const BOTH_OUT: [bool; 2] = [false, false];
    const HOME: [bool; 2] = [true, true];
    const AWAY: [bool; 2] = [false, false];

    /// One tick per second from `start` through `start + secs` inclusive.
    fn run(
        l: &mut Ledger<2>,
        p: &Policy,
        start: i64,
        secs: i64,
        docked: [bool; 2],
        present: [bool; 2],
    ) -> (i64, Snapshot<2>) {
        let mut snap = l.tick(start, docked, present, p).0;
        for t in 1..=secs {
            snap = l.tick(start + t, docked, present, p).0;
        }
        (start + secs, snap)
    }

    // ---- refill ----------------------------------------------------------

    #[test]
    fn docked_at_home_fills_at_the_policy_ratio() {
        let p = plain(); // 1:10
        let mut l = Ledger::<2>::with_balance(0);
        let (_, snap) = run(&mut l, &p, berlin(2026, 8, 3, 16, 0), 600, DOCKED, HOME);
        assert_eq!(snap.balance_secs, 60, "ten minutes docked earns one minute");
        assert_eq!(snap.flow, Flow::Filling);
    }

    #[test]
    fn being_out_earns_exactly_as_much_as_being_docked() {
        // The whole point of the redesign: going outside must never cost her.
        let p = plain();
        let start = berlin(2026, 8, 3, 16, 0);

        let mut home = Ledger::<2>::with_balance(0);
        run(&mut home, &p, start, 3_600, DOCKED, HOME);

        let mut out = Ledger::<2>::with_balance(0);
        run(&mut out, &p, start, 3_600, BOTH_OUT, AWAY);

        assert_eq!(out.balance_secs, home.balance_secs);
        assert_eq!(out.balance_secs, 360);
    }

    #[test]
    fn refill_remainder_does_not_drift() {
        // Ticking one second at a time through a 1:10 ratio must still credit exactly
        // 1/10th, not round to zero every tick.
        let p = plain();
        let mut l = Ledger::<2>::with_balance(0);
        let (_, snap) = run(&mut l, &p, berlin(2026, 8, 3, 16, 0), 3_600, DOCKED, HOME);
        assert_eq!(snap.balance_secs, 360);
    }

    #[test]
    fn refill_stops_at_the_cap() {
        let mut p = plain();
        p.cap_secs = 100;
        let mut l = Ledger::<2>::with_balance(0);
        let (_, snap) = run(&mut l, &p, berlin(2026, 8, 3, 16, 0), 5_000, DOCKED, HOME);
        assert_eq!(snap.balance_secs, 100);
    }

    // ---- drain -----------------------------------------------------------

    #[test]
    fn undocked_at_home_drains_at_one_times() {
        let p = plain();
        let mut l = Ledger::<2>::with_balance(3_600);
        let (_, snap) = run(&mut l, &p, berlin(2026, 8, 3, 16, 0), 600, ONE_OUT, HOME);
        assert_eq!(snap.balance_secs, 3_000);
        assert_eq!(snap.flow, Flow::Draining);
    }

    #[test]
    fn two_devices_out_still_drains_at_one_times() {
        let p = plain();
        let start = berlin(2026, 8, 3, 16, 0);

        let mut one = Ledger::<2>::with_balance(3_600);
        run(&mut one, &p, start, 600, ONE_OUT, HOME);
        let mut both = Ledger::<2>::with_balance(3_600);
        run(&mut both, &p, start, 600, BOTH_OUT, HOME);

        assert_eq!(both.balance_secs, one.balance_secs);
    }

    #[test]
    fn a_device_on_her_desk_at_home_still_drains() {
        // The strict edge of the design: undocked is undocked, touched or not.
        let p = plain();
        let mut l = Ledger::<2>::with_balance(3_600);
        let (_, snap) = run(&mut l, &p, berlin(2026, 8, 3, 16, 0), 300, ONE_OUT, HOME);
        assert_eq!(snap.balance_secs, 3_300);
    }

    #[test]
    fn undocked_but_off_network_fills_rather_than_drains() {
        // Row 11 of the scenario table, and the known open hole: Wi-Fi off at home is
        // indistinguishable from being out, so it earns. Pinned so the behaviour is a
        // decision on the record rather than an accident.
        let p = plain();
        let mut l = Ledger::<2>::with_balance(0);
        let (_, snap) = run(&mut l, &p, berlin(2026, 8, 3, 16, 0), 600, BOTH_OUT, AWAY);
        assert_eq!(snap.balance_secs, 60);
        assert_eq!(snap.flow, Flow::Filling);
    }

    #[test]
    fn drain_stops_at_the_floor() {
        let mut p = plain();
        p.floor_secs = 120;
        let mut l = Ledger::<2>::with_balance(60);
        let (_, snap) = run(&mut l, &p, berlin(2026, 8, 3, 16, 0), 3_600, ONE_OUT, HOME);
        assert_eq!(snap.balance_secs, -120);
    }

    #[test]
    fn debt_refills_back_out_of_the_hole() {
        let p = plain();
        let mut l = Ledger::<2>::with_balance(-60);
        let (_, snap) = run(&mut l, &p, berlin(2026, 8, 3, 16, 0), 1_200, DOCKED, HOME);
        assert_eq!(snap.balance_secs, 60, "20 min docked at 1:10 climbs 120 s");
    }

    // ---- grace -----------------------------------------------------------

    #[test]
    fn a_brief_pickup_costs_nothing_and_is_forgiven_on_redock() {
        let mut p = plain();
        p.grace_secs = 180;
        let mut l = Ledger::<2>::with_balance(3_600);
        let start = berlin(2026, 8, 3, 16, 0);

        let (t, snap) = run(&mut l, &p, start, 90, ONE_OUT, HOME);
        assert_eq!(snap.balance_secs, 3_600);
        assert_eq!(snap.flow, Flow::Held);
        assert!(snap.in_grace);

        let (_, snap) = run(&mut l, &p, t, 5, DOCKED, HOME);
        assert_eq!(snap.balance_secs, 3_600, "grace does not fill either");
        assert!(!snap.in_grace);
    }

    #[test]
    fn overrunning_grace_bills_the_whole_pickup() {
        let mut p = plain();
        p.grace_secs = 180;
        let mut l = Ledger::<2>::with_balance(3_600);
        let (_, snap) = run(&mut l, &p, berlin(2026, 8, 3, 16, 0), 300, ONE_OUT, HOME);
        assert_eq!(snap.balance_secs, 3_300, "all 300 s billed, not just the excess");
    }

    #[test]
    fn leaving_the_house_mid_pickup_forgives_the_grace() {
        let mut p = plain();
        p.grace_secs = 180;
        let mut l = Ledger::<2>::with_balance(3_600);
        let start = berlin(2026, 8, 3, 16, 0);
        let (t, _) = run(&mut l, &p, start, 60, ONE_OUT, HOME);
        let (_, snap) = run(&mut l, &p, t, 600, ONE_OUT, AWAY);
        assert!(snap.balance_secs > 3_600, "out of the house, so it fills");
    }

    // ---- night -----------------------------------------------------------

    #[test]
    fn night_blocks_undocked_devices_and_holds_the_balance() {
        let mut p = plain();
        let _ = p.night.push(Window::hm(Window::EVERY_DAY, 21, 0, 6, 30));
        let mut l = Ledger::<2>::with_balance(3_600);
        let (_, snap) = run(&mut l, &p, berlin(2026, 8, 3, 23, 0), 600, ONE_OUT, HOME);

        assert!(snap.night);
        assert_eq!(snap.flow, Flow::Held);
        assert_eq!(snap.balance_secs, 3_600, "night neither earns nor costs");
        assert_eq!(snap.blocked, [true, false]);
    }

    #[test]
    fn docked_devices_fill_through_the_night_and_are_never_blocked() {
        let mut p = plain();
        let _ = p.night.push(Window::hm(Window::EVERY_DAY, 21, 0, 6, 30));
        let mut l = Ledger::<2>::with_balance(0);
        let (_, snap) = run(&mut l, &p, berlin(2026, 8, 3, 22, 0), 3_600, DOCKED, HOME);
        assert_eq!(snap.balance_secs, 360);
        assert_eq!(snap.blocked, [false, false]);
    }

    #[test]
    fn undocking_at_night_alerts_once_not_once_per_second() {
        let mut p = plain();
        let _ = p.night.push(Window::hm(Window::EVERY_DAY, 21, 0, 6, 30));
        let mut l = Ledger::<2>::with_balance(3_600);
        let t = berlin(2026, 8, 3, 23, 0);
        l.tick(t, DOCKED, HOME, &p);

        let mut alerts = 0;
        for i in 1..=600 {
            let (_, ev) = l.tick(t + i, ONE_OUT, HOME, &p);
            alerts += ev
                .iter()
                .filter(|e| matches!(e, Event::UndockedAtNight { device: 0 }))
                .count();
        }
        assert_eq!(alerts, 1, "edge-triggered, or it would alert all night long");

        // Putting it back and taking it out again is a fresh offence.
        l.tick(t + 601, DOCKED, HOME, &p);
        let (_, ev) = l.tick(t + 602, ONE_OUT, HOME, &p);
        assert!(ev.contains(&Event::UndockedAtNight { device: 0 }));
    }

    #[test]
    fn a_full_night_in_the_box_funds_the_next_day() {
        // The calibration claim behind the 1:10 ratio: 21:00 -> 07:00 earns an hour.
        let p = Policy::default();
        let mut l = Ledger::<2>::with_balance(0);
        let (_, snap) = run(&mut l, &p, berlin(2026, 8, 3, 21, 0), 10 * 3_600, DOCKED, HOME);
        assert_eq!(snap.balance_secs, 3_600);
    }

    // ---- blocking --------------------------------------------------------

    #[test]
    fn a_docked_device_is_never_blocked_even_at_zero() {
        let p = plain();
        let mut l = Ledger::<2>::with_balance(0);
        let (_, snap) = run(&mut l, &p, berlin(2026, 8, 3, 16, 0), 5, DOCKED, HOME);
        assert!(snap.exhausted());
        assert_eq!(snap.blocked, [false, false]);
    }

    #[test]
    fn only_the_undocked_device_is_blocked() {
        let p = plain();
        let mut l = Ledger::<2>::with_balance(0);
        let (_, snap) = run(&mut l, &p, berlin(2026, 8, 3, 16, 0), 5, ONE_OUT, HOME);
        assert_eq!(snap.blocked, [true, false]);
    }

    #[test]
    fn exhausted_and_restored_are_edge_triggered() {
        let p = plain();
        let mut l = Ledger::<2>::with_balance(3);
        let start = berlin(2026, 8, 3, 16, 0);

        let mut exhausted = 0;
        for t in 0..=20 {
            let (_, ev) = l.tick(start + t, ONE_OUT, HOME, &p);
            exhausted += ev.iter().filter(|e| **e == Event::Exhausted).count();
        }
        assert_eq!(exhausted, 1);

        l.grant_bonus(600, &p);
        let (_, ev) = l.tick(start + 21, ONE_OUT, HOME, &p);
        assert!(ev.contains(&Event::Restored));
    }

    #[test]
    fn warning_fires_once_on_the_way_down() {
        let p = plain();
        let mut l = Ledger::<2>::with_balance(WARNING_SECS + 5);
        let start = berlin(2026, 8, 3, 16, 0);
        let mut warnings = 0;
        for t in 0..=10 {
            let (_, ev) = l.tick(start + t, ONE_OUT, HOME, &p);
            warnings += ev.iter().filter(|e| **e == Event::Warning).count();
        }
        assert_eq!(warnings, 1);
    }

    // ---- robustness ------------------------------------------------------

    #[test]
    fn prefill_seeds_a_fresh_ledger() {
        let mut p = plain();
        p.prefill_secs = 1_800;
        let l = Ledger::<2>::new(&p);
        assert_eq!(l.balance_secs(), 1_800);
    }

    #[test]
    fn a_fresh_ledger_assumes_docked_and_absent() {
        // Fail closed: neither a dead reader nor an unreachable FRITZ!Box may drain.
        let p = plain();
        let mut l = Ledger::<2>::with_balance(600);
        let (snap, _) = l.tick(berlin(2026, 8, 3, 16, 0), DOCKED, AWAY, &p);
        assert_eq!(snap.flow, Flow::Filling);
        assert_eq!(snap.balance_secs, 600);
    }

    #[test]
    fn a_long_gap_is_neither_charged_nor_credited() {
        let p = plain();
        let mut l = Ledger::<2>::with_balance(600);
        let start = berlin(2026, 8, 3, 16, 0);
        l.tick(start, ONE_OUT, HOME, &p);
        let (snap, ev) = l.tick(start + 3_600, ONE_OUT, HOME, &p);
        assert!(matches!(ev[0], Event::TimeJump { .. }));
        assert_eq!(snap.balance_secs, 600);
    }

    #[test]
    fn dst_spring_forward_neither_grants_nor_steals() {
        let p = plain();
        let mut l = Ledger::<2>::with_balance(7_200);
        // 2027-03-28 00:30 UTC: a real hour spanning the local 02:00 -> 03:00 jump.
        let start = days_from_civil(2027, 3, 28) * SECS_PER_DAY + 30 * 60;
        let (_, snap) = run(&mut l, &p, start, 3_600, ONE_OUT, HOME);
        assert_eq!(snap.balance_secs, 3_600, "one real hour costs one hour");
    }
}
