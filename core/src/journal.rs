//! Crash-safe balance journal.
//!
//! An append-only ring of fixed-size records across a few flash sectors. The newest
//! valid record wins on boot, so a power cut costs at most one write interval.
//!
//! Everything here is pure: slots are byte slices, and the caller decides where they
//! live. That keeps the parts that are actually easy to get wrong — integrity, picking
//! the newest record, and knowing when a sector must be erased — testable on the host.
//!
//! Deliberately hand-rolled rather than pulling in a key-value store: the payload is
//! twenty bytes with one writer, and the failure mode that matters (a record torn
//! half-written by a power cut) is handled by the CRC either way.

/// `MZ`, little-endian.
const MAGIC: u16 = 0x5A4D;

/// Fixed on-flash record size. A multiple of four, which flash writes require.
pub const RECORD_LEN: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    /// Monotonic. At one write per 30 s a `u32` lasts several thousand years, so
    /// wraparound is not handled and does not need to be.
    pub seq: u32,
    pub balance_secs: i32,
    /// Wall clock at the time of writing, used to detect how long the unit was off.
    pub last_tick: i64,
}

/// CRC-16/CCITT-FALSE.
fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for byte in data {
        crc ^= (*byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

pub fn encode(rec: &Record) -> [u8; RECORD_LEN] {
    let mut b = [0u8; RECORD_LEN];
    b[0..2].copy_from_slice(&MAGIC.to_le_bytes());
    b[2..6].copy_from_slice(&rec.seq.to_le_bytes());
    b[6..10].copy_from_slice(&rec.balance_secs.to_le_bytes());
    b[10..18].copy_from_slice(&rec.last_tick.to_le_bytes());
    let crc = crc16(&b[0..18]);
    b[18..20].copy_from_slice(&crc.to_le_bytes());
    b
}

/// Decode one slot. Returns `None` for erased flash, a torn write, or corruption —
/// all of which are indistinguishable from the caller's point of view and all of which
/// mean the same thing: skip this slot.
pub fn decode(slot: &[u8]) -> Option<Record> {
    if slot.len() < RECORD_LEN {
        return None;
    }
    if u16::from_le_bytes([slot[0], slot[1]]) != MAGIC {
        return None;
    }
    let stored = u16::from_le_bytes([slot[18], slot[19]]);
    if stored != crc16(&slot[0..18]) {
        return None;
    }
    Some(Record {
        seq: u32::from_le_bytes([slot[2], slot[3], slot[4], slot[5]]),
        balance_secs: i32::from_le_bytes([slot[6], slot[7], slot[8], slot[9]]),
        last_tick: i64::from_le_bytes([
            slot[10], slot[11], slot[12], slot[13], slot[14], slot[15], slot[16], slot[17],
        ]),
    })
}

/// The newest valid record in the region, with its slot index.
pub fn newest<'a>(slots: impl Iterator<Item = &'a [u8]>) -> Option<(usize, Record)> {
    let mut best: Option<(usize, Record)> = None;
    for (i, slot) in slots.enumerate() {
        if let Some(rec) = decode(slot) {
            if best.is_none_or(|(_, b)| rec.seq > b.seq) {
                best = Some((i, rec));
            }
        }
    }
    best
}

/// Where the next record goes, given where the newest one is.
pub fn next_slot(newest_index: Option<usize>, total_slots: usize) -> usize {
    match newest_index {
        Some(i) => (i + 1) % total_slots,
        None => 0,
    }
}

/// Whether writing `slot` requires erasing its sector first.
///
/// Flash can only clear bits by erasing a whole sector, so the ring erases one sector
/// ahead of itself as it advances. Only true at a sector boundary, which is what keeps
/// the wear spread out instead of hammering one sector.
pub fn needs_erase(slot: usize, slots_per_sector: usize) -> bool {
    slots_per_sector != 0 && slot % slots_per_sector == 0
}

/// A period during which the unit was not running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outage {
    pub from: i64,
    pub to: i64,
}

impl Outage {
    pub fn secs(&self) -> i64 {
        self.to - self.from
    }
}

/// Decide whether the gap between the last journalled tick and now is worth reporting.
///
/// The gap itself is never billed — a genuine power cut must not cost her the evening —
/// but it is worth telling the parent about, because "unplug the unit" is otherwise the
/// obvious way to stop the clock.
///
/// A backwards clock returns `None`: that is a time correction, not an outage.
pub fn detect_outage(last_tick: i64, now: i64, min_secs: i64) -> Option<Outage> {
    if now <= last_tick {
        return None;
    }
    let gap = now - last_tick;
    (gap >= min_secs).then_some(Outage { from: last_tick, to: now })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ERASED: [u8; RECORD_LEN] = [0xFF; RECORD_LEN];

    fn rec(seq: u32, balance: i32, tick: i64) -> Record {
        Record { seq, balance_secs: balance, last_tick: tick }
    }

    #[test]
    fn round_trips() {
        for r in [
            rec(0, 0, 0),
            rec(1, 3_600, 1_785_788_154),
            rec(u32::MAX, i32::MIN, i64::MAX),
            rec(7, -1_800, -1),
        ] {
            assert_eq!(decode(&encode(&r)), Some(r));
        }
    }

    #[test]
    fn erased_flash_is_not_a_record() {
        // A fresh region is all 0xFF. Reading that as data would resurrect a balance
        // out of nothing.
        assert_eq!(decode(&ERASED), None);
        assert_eq!(decode(&[0u8; RECORD_LEN]), None);
    }

    #[test]
    fn a_torn_write_is_rejected() {
        // Power lost mid-write: the header landed, the tail did not.
        let mut slot = encode(&rec(4, 1_234, 1_785_788_154));
        slot[14..20].copy_from_slice(&[0xFF; 6]);
        assert_eq!(decode(&slot), None);
    }

    #[test]
    fn every_single_bit_flip_is_caught() {
        let good = encode(&rec(9, 4_242, 1_785_788_154));
        for byte in 0..RECORD_LEN {
            for bit in 0..8 {
                let mut bad = good;
                bad[byte] ^= 1 << bit;
                assert_eq!(decode(&bad), None, "bit {bit} of byte {byte} slipped through");
            }
        }
    }

    #[test]
    fn short_slots_do_not_panic() {
        for n in 0..RECORD_LEN {
            assert_eq!(decode(&ERASED[..n]), None);
        }
    }

    #[test]
    fn newest_wins_regardless_of_position() {
        let a = encode(&rec(3, 100, 10));
        let b = encode(&rec(9, 200, 20));
        let c = encode(&rec(5, 300, 30));
        let slots: [&[u8]; 4] = [&a, &b, &c, &ERASED];
        assert_eq!(newest(slots.into_iter()), Some((1, rec(9, 200, 20))));
    }

    #[test]
    fn newest_skips_corruption_rather_than_giving_up() {
        let mut broken = encode(&rec(99, 1, 1));
        broken[7] ^= 0xFF;
        let good = encode(&rec(4, 500, 50));
        let slots: [&[u8]; 3] = [&broken, &good, &ERASED];
        // The corrupt record has the highest sequence number, and must not win.
        assert_eq!(newest(slots.into_iter()), Some((1, rec(4, 500, 50))));
    }

    #[test]
    fn an_empty_region_has_no_newest() {
        let slots: [&[u8]; 2] = [&ERASED, &ERASED];
        assert_eq!(newest(slots.into_iter()), None);
    }

    #[test]
    fn the_ring_wraps() {
        assert_eq!(next_slot(None, 8), 0);
        assert_eq!(next_slot(Some(0), 8), 1);
        assert_eq!(next_slot(Some(7), 8), 0);
    }

    #[test]
    fn erase_happens_only_at_sector_boundaries() {
        let per_sector = 204;
        assert!(needs_erase(0, per_sector));
        assert!(!needs_erase(1, per_sector));
        assert!(!needs_erase(203, per_sector));
        assert!(needs_erase(204, per_sector));
        assert!(!needs_erase(205, per_sector));
    }

    #[test]
    fn a_full_lap_of_the_ring_keeps_the_newest_record() {
        // Simulates the region as it actually behaves: write in order, erase a sector
        // when entering it, and confirm the newest record is always recoverable.
        const SLOTS: usize = 12;
        const PER_SECTOR: usize = 4;
        let mut region = [[0xFFu8; RECORD_LEN]; SLOTS];
        let mut newest_index = None;

        for seq in 1..=40u32 {
            let slot = next_slot(newest_index, SLOTS);
            if needs_erase(slot, PER_SECTOR) {
                let sector = slot / PER_SECTOR;
                for s in &mut region[sector * PER_SECTOR..(sector + 1) * PER_SECTOR] {
                    *s = [0xFF; RECORD_LEN];
                }
            }
            region[slot] = encode(&rec(seq, seq as i32 * 10, seq as i64));
            newest_index = Some(slot);

            let view: heapless::Vec<&[u8], SLOTS> =
                region.iter().map(|s| s.as_slice()).collect();
            let (idx, found) = newest(view.into_iter()).expect("something must survive");
            assert_eq!(found.seq, seq, "lost the newest record at seq {seq}");
            assert_eq!(idx, slot);
        }
    }

    // ---- outage ----------------------------------------------------------

    #[test]
    fn a_long_gap_is_an_outage() {
        let o = detect_outage(1_000, 1_000 + 3_600, 300).expect("should report");
        assert_eq!(o.secs(), 3_600);
        assert_eq!(o.from, 1_000);
    }

    #[test]
    fn a_short_gap_is_just_a_reboot() {
        assert_eq!(detect_outage(1_000, 1_010, 300), None);
    }

    #[test]
    fn a_backwards_clock_is_a_correction_not_an_outage() {
        // SNTP moving the clock back must not be reported as her unplugging the unit.
        assert_eq!(detect_outage(2_000, 1_000, 300), None);
        assert_eq!(detect_outage(1_000, 1_000, 300), None);
    }
}
