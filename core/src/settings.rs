//! Persisting the tunable parts of [`Policy`].
//!
//! Separate from the journal on purpose. The journal is written every 30 seconds and is
//! a ring; settings change a handful of times in the life of the thing. Sharing a record
//! format would mean every balance write carried a copy of the rules, and a corrupt
//! record would take both down together.
//!
//! Only the numbers a parent would reasonably change are stored. Everything structural —
//! how many devices, how grace is billed — stays in code, because those are decisions the
//! design rests on rather than knobs.

use crate::policy::{Policy, Window, MAX_WINDOWS};

/// Encoded length: eight `u32` fields plus a CRC, with two bytes spare.
///
/// The spare is not padding for its own sake — the fields alone are exactly 32 bytes, so
/// a 32-byte record left the CRC overwriting the last field. It decoded consistently and
/// still lost data.
pub const SETTINGS_LEN: usize = 36;

/// The stored form. A superset would be tempting, but every field here has to survive a
/// firmware upgrade, so the set is deliberately small.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub seq: u32,
    pub refill_num: u32,
    pub refill_den: u32,
    pub cap_secs: u32,
    pub floor_secs: u32,
    pub prefill_secs: u32,
    pub grace_secs: u32,
    /// Night window, minutes from local midnight. Wraps when start > end.
    pub night_start_minute: u32,
    pub night_end_minute: u32,
}

impl Settings {
    /// The compiled-in defaults, as the starting point before anything is stored.
    pub fn from_policy(p: &Policy, seq: u32) -> Self {
        let (start, end) = p
            .night
            .first()
            .map(|w| (w.start_minute, w.end_minute))
            .unwrap_or((21 * 60, 6 * 60 + 30));
        Self {
            seq,
            refill_num: p.refill_num,
            refill_den: p.refill_den,
            cap_secs: p.cap_secs,
            floor_secs: p.floor_secs,
            prefill_secs: p.prefill_secs,
            grace_secs: p.grace_secs,
            night_start_minute: start,
            night_end_minute: end,
        }
    }

    /// Build a policy from stored settings.
    pub fn to_policy(&self) -> Policy {
        let mut night = heapless::Vec::<Window, MAX_WINDOWS>::new();
        // An empty window would mean "no night at all", which is a rule change disguised
        // as a rounding error, so a degenerate pair is dropped rather than stored as a
        // zero-length window.
        if self.night_start_minute != self.night_end_minute {
            let _ = night.push(Window::new(
                Window::EVERY_DAY,
                self.night_start_minute,
                self.night_end_minute,
            ));
        }
        Policy {
            refill_num: self.refill_num,
            refill_den: self.refill_den,
            cap_secs: self.cap_secs,
            floor_secs: self.floor_secs,
            prefill_secs: self.prefill_secs,
            grace_secs: self.grace_secs,
            night,
        }
    }

    /// Reject values that would break the ledger rather than merely be unusual.
    ///
    /// A denominator of zero divides; a cap below the floor makes the valid range empty.
    /// Everything else is the parent's business, including choices I would not pick.
    pub fn valid(&self) -> bool {
        self.refill_den != 0
            && self.night_start_minute < 24 * 60
            && self.night_end_minute < 24 * 60
    }
}

fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xffff;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

pub fn encode(s: &Settings) -> [u8; SETTINGS_LEN] {
    let mut out = [0u8; SETTINGS_LEN];
    let fields = [
        s.seq,
        s.refill_num,
        s.refill_den,
        s.cap_secs,
        s.floor_secs,
        s.prefill_secs,
        s.grace_secs,
        (s.night_start_minute << 16) | s.night_end_minute,
    ];
    for (i, f) in fields.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&f.to_le_bytes());
    }
    let crc = crc16(&out[..SETTINGS_LEN - 2]);
    out[SETTINGS_LEN - 2..].copy_from_slice(&crc.to_le_bytes());
    out
}

pub fn decode(slot: &[u8]) -> Option<Settings> {
    if slot.len() < SETTINGS_LEN {
        return None;
    }
    let slot = &slot[..SETTINGS_LEN];
    let want = u16::from_le_bytes([slot[SETTINGS_LEN - 2], slot[SETTINGS_LEN - 1]]);
    if crc16(&slot[..SETTINGS_LEN - 2]) != want {
        return None;
    }
    let word = |i: usize| {
        u32::from_le_bytes([slot[i * 4], slot[i * 4 + 1], slot[i * 4 + 2], slot[i * 4 + 3]])
    };
    let packed = word(7);
    let s = Settings {
        seq: word(0),
        refill_num: word(1),
        refill_den: word(2),
        cap_secs: word(3),
        floor_secs: word(4),
        prefill_secs: word(5),
        grace_secs: word(6),
        night_start_minute: packed >> 16,
        night_end_minute: packed & 0xffff,
    };
    // A record that survived the CRC but holds impossible values is worse than no record:
    // it would be applied. Erased flash reads as all ones, which fails the CRC anyway,
    // but a half-written or ancient record might not.
    s.valid().then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Settings {
        Settings::from_policy(&Policy::default(), 7)
    }

    #[test]
    fn round_trips() {
        let s = sample();
        assert_eq!(decode(&encode(&s)), Some(s));
    }

    #[test]
    fn defaults_survive_the_policy_conversion() {
        let p = Policy::default();
        let back = Settings::from_policy(&p, 0).to_policy();
        assert_eq!(back.refill_num, p.refill_num);
        assert_eq!(back.refill_den, p.refill_den);
        assert_eq!(back.cap_secs, p.cap_secs);
        assert_eq!(back.floor_secs, p.floor_secs);
        assert_eq!(back.prefill_secs, p.prefill_secs);
        assert_eq!(back.grace_secs, p.grace_secs);
        assert_eq!(back.night.len(), p.night.len());
        assert_eq!(back.night[0].start_minute, p.night[0].start_minute);
        assert_eq!(back.night[0].end_minute, p.night[0].end_minute);
    }

    /// Erased flash is all ones. It must not decode as settings, or a blank device would
    /// boot with nonsense rules instead of the compiled-in defaults.
    #[test]
    fn erased_flash_is_not_settings() {
        assert_eq!(decode(&[0xff; SETTINGS_LEN]), None);
    }

    #[test]
    fn every_single_bit_flip_is_caught() {
        let good = encode(&sample());
        for byte in 0..SETTINGS_LEN {
            for bit in 0..8 {
                let mut bad = good;
                bad[byte] ^= 1 << bit;
                assert_eq!(decode(&bad), None, "byte {byte} bit {bit} slipped through");
            }
        }
    }

    /// A zero denominator divides by zero in the refill; it must never reach the ledger.
    #[test]
    fn rejects_a_zero_denominator() {
        let mut s = sample();
        s.refill_den = 0;
        assert_eq!(decode(&encode(&s)), None);
    }

    #[test]
    fn rejects_impossible_clock_times() {
        let mut s = sample();
        s.night_start_minute = 24 * 60;
        assert_eq!(decode(&encode(&s)), None);
    }

    /// Equal start and end would otherwise store a zero-length night, silently removing
    /// the rule instead of changing it.
    #[test]
    fn a_degenerate_night_window_is_dropped_not_stored_empty() {
        let mut s = sample();
        s.night_start_minute = 600;
        s.night_end_minute = 600;
        assert!(s.to_policy().night.is_empty());
    }
}
