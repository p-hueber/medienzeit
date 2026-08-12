//! Turning "what the reader can see" into "is this device put back".
//!
//! Generic over the identity type so this can live here rather than in the firmware,
//! which is the point: every bug this module has had was a logic bug, and logic bugs
//! belong somewhere `cargo test` can reach. It knows nothing about NFC — an identity is
//! anything that can be compared.
//!
//! # What happens when the reader fails
//!
//! A reader that stops answering holds the **last known** docking state and reports the
//! fault. Both alternatives are worse. Declaring everything undocked would drain the
//! balance for devices sitting untouched at the reader, which is the unfairness this
//! whole design exists to avoid, and inside the night window it would cut the internet
//! on a device that never moved. Declaring everything docked would hand out unmetered
//! screen time for as long as the fault lasted. Holding is wrong in neither direction
//! for the length of a glitch, and the alert is what makes a long fault visible.
//!
//! This has been got wrong twice in practice, both times by plumbing that turned a
//! failure into "the reader is fine and nothing is there". Hence the tests.

/// Reader failures tolerated before saying so. At one poll per second this is a few
/// seconds of silence, which distinguishes a wedged reader from a single bad transfer.
pub const DEFAULT_FAILURES_BEFORE_ALERT: u32 = 5;

/// What one poll concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Docked<T, const N: usize> {
    pub docked: [bool; N],
    /// An identity in the field belonging to no configured device, worth telling the
    /// parent about — a device carrying someone else's sticker looks exactly like this.
    pub unknown: Option<T>,
    /// The reader has been unresponsive long enough to be a fault, not a glitch.
    pub reader_fault: bool,
}

/// Maps identities in the field onto each device's docked flag.
///
/// A device with no configured identity falls back to whatever the caller passes, which
/// is how a button stands in before tags exist.
pub struct Docking<T, const N: usize> {
    known: [Option<T>; N],
    last: [bool; N],
    reported_unknown: Option<T>,
    consecutive_failures: u32,
    failures_before_alert: u32,
}

impl<T: Copy + PartialEq, const N: usize> Docking<T, N> {
    pub fn new(known: [Option<T>; N]) -> Self {
        Self::with_threshold(known, DEFAULT_FAILURES_BEFORE_ALERT)
    }

    pub fn with_threshold(known: [Option<T>; N], failures_before_alert: u32) -> Self {
        Self {
            known,
            // Start docked, matching the ledger's own fresh-state assumption: until
            // something is known, do not spend.
            last: [true; N],
            reported_unknown: None,
            consecutive_failures: 0,
            failures_before_alert,
        }
    }

    /// True when no device has an identity configured, so the reader cannot drive
    /// anything and the fallback is the only input.
    pub fn unconfigured(&self) -> bool {
        self.known.iter().all(|k| k.is_none())
    }

    /// Fold one reading in.
    ///
    /// `seen` is `None` when the reader failed — which is emphatically not the same as
    /// an empty slice, and collapsing the two is the bug this module keeps having.
    pub fn update(&mut self, seen: Option<&[T]>, fallback: [bool; N]) -> Docked<T, N> {
        let Some(seen) = seen else {
            self.consecutive_failures += 1;
            return Docked {
                docked: self.last,
                unknown: None,
                // Exactly at the threshold, so a persistent fault alerts once rather
                // than every second for as long as it lasts.
                reader_fault: self.consecutive_failures == self.failures_before_alert,
            };
        };
        self.consecutive_failures = 0;

        let mut docked = [false; N];
        for (i, slot) in docked.iter_mut().enumerate() {
            *slot = match self.known[i] {
                Some(id) => seen.contains(&id),
                None => fallback[i],
            };
        }
        self.last = docked;

        let unknown = seen
            .iter()
            .find(|id| !self.known.iter().any(|k| k.as_ref() == Some(*id)))
            .copied();
        // Report only when the unknown identity changes, so one strange tag left lying
        // at the reader does not alert every second.
        let report = match (unknown, self.reported_unknown) {
            (Some(u), Some(prev)) if u == prev => None,
            (Some(u), _) => Some(u),
            (None, _) => None,
        };
        self.reported_unknown = unknown;

        Docked { docked, unknown: report, reader_fault: false }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type D = Docking<u8, 2>;

    fn with_tags() -> D {
        Docking::new([Some(1), Some(2)])
    }

    #[test]
    fn a_tag_in_the_field_means_put_back() {
        let mut d = with_tags();
        assert_eq!(d.update(Some(&[1, 2]), [false; 2]).docked, [true, true]);
        assert_eq!(d.update(Some(&[1]), [false; 2]).docked, [true, false]);
        assert_eq!(d.update(Some(&[]), [false; 2]).docked, [false, false]);
    }

    /// Fresh state is docked, so a device is never charged for time before anything is
    /// known about it.
    #[test]
    fn it_starts_docked() {
        let mut d = with_tags();
        // A failure on the very first poll must report the starting assumption.
        assert_eq!(d.update(None, [false; 2]).docked, [true, true]);
    }

    /// The regression that has bitten twice: a failing reader must not read as "the
    /// devices have been taken away".
    #[test]
    fn a_reader_failure_holds_the_last_state() {
        let mut d = with_tags();
        d.update(Some(&[1, 2]), [false; 2]);
        for _ in 0..20 {
            assert_eq!(
                d.update(None, [false; 2]).docked,
                [true, true],
                "a failing reader must not look like a device being taken"
            );
        }
    }

    #[test]
    fn a_failure_holds_a_taken_device_taken_too() {
        let mut d = with_tags();
        d.update(Some(&[]), [false; 2]);
        assert_eq!(d.update(None, [false; 2]).docked, [false, false]);
    }

    /// An empty reading is a real reading: nothing is in the field.
    #[test]
    fn an_empty_reading_is_not_a_failure() {
        let mut d = with_tags();
        d.update(Some(&[1, 2]), [false; 2]);
        let r = d.update(Some(&[]), [false; 2]);
        assert_eq!(r.docked, [false, false]);
        assert!(!r.reader_fault);
    }

    #[test]
    fn the_fault_alert_fires_once_not_every_tick() {
        let mut d = Docking::<u8, 2>::with_threshold([Some(1), Some(2)], 3);
        assert!(!d.update(None, [false; 2]).reader_fault);
        assert!(!d.update(None, [false; 2]).reader_fault);
        assert!(d.update(None, [false; 2]).reader_fault, "fires at the threshold");
        for _ in 0..10 {
            assert!(
                !d.update(None, [false; 2]).reader_fault,
                "and not again while the same fault continues"
            );
        }
    }

    #[test]
    fn recovering_rearms_the_fault_alert() {
        let mut d = Docking::<u8, 2>::with_threshold([Some(1), Some(2)], 2);
        d.update(None, [false; 2]);
        assert!(d.update(None, [false; 2]).reader_fault);
        d.update(Some(&[1]), [false; 2]);
        d.update(None, [false; 2]);
        assert!(d.update(None, [false; 2]).reader_fault, "a second outage alerts again");
    }

    #[test]
    fn an_unknown_tag_is_reported_once_per_identity() {
        let mut d = with_tags();
        assert_eq!(d.update(Some(&[9]), [false; 2]).unknown, Some(9));
        for _ in 0..5 {
            assert_eq!(
                d.update(Some(&[9]), [false; 2]).unknown,
                None,
                "the same stray tag must not alert every second"
            );
        }
        assert_eq!(d.update(Some(&[8]), [false; 2]).unknown, Some(8));
    }

    #[test]
    fn a_known_tag_is_never_unknown() {
        let mut d = with_tags();
        assert_eq!(d.update(Some(&[1, 2]), [false; 2]).unknown, None);
    }

    /// Leaving and returning should alert again — it is a fresh event.
    #[test]
    fn an_unknown_tag_that_leaves_and_returns_alerts_again() {
        let mut d = with_tags();
        d.update(Some(&[9]), [false; 2]);
        d.update(Some(&[]), [false; 2]);
        assert_eq!(d.update(Some(&[9]), [false; 2]).unknown, Some(9));
    }

    #[test]
    fn a_device_without_an_identity_uses_the_fallback() {
        let mut d: D = Docking::new([Some(1), None]);
        assert_eq!(d.update(Some(&[1]), [false, true]).docked, [true, true]);
        assert_eq!(d.update(Some(&[1]), [false, false]).docked, [true, false]);
    }

    #[test]
    fn unconfigured_only_when_nothing_is_set() {
        let none: D = Docking::new([None, None]);
        assert!(none.unconfigured());
        let tag: D = Docking::new([Some(1), None]);
        assert!(!tag.unconfigured());
    }
}
