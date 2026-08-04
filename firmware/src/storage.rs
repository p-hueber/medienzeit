//! The journal's home in flash.
//!
//! Lives in a dedicated `storage` data partition (see `partitions.csv`), never inside
//! the app image — an app that grows past its neighbour would otherwise quietly eat
//! the balance.
//!
//! Write cost, since this runs forever: one 20-byte record every 30 s while spending
//! is ~2 writes/minute. With 16 KB of ring that is one sector erase roughly every
//! 100 minutes, spread across four sectors — about 15 erases per sector per day, or
//! well over a century against a 100k-cycle rating.

use embedded_storage::{ReadStorage, Storage};
use esp_println::println;
use esp_storage::FlashStorage;
use medienzeit_core::journal::{self, Record, RECORD_LEN};

/// Must match the `storage` partition in `partitions.csv`.
pub const OFFSET: u32 = 0x31_0000;
pub const SIZE: usize = 16 * 1024;

const SECTOR: usize = 4096;
const SLOTS_PER_SECTOR: usize = SECTOR / RECORD_LEN;
const SECTORS: usize = SIZE / SECTOR;
const SLOTS: usize = SLOTS_PER_SECTOR * SECTORS;

pub struct Journal<'a> {
    flash: FlashStorage<'a>,
    newest_index: Option<usize>,
    seq: u32,
}

impl<'a> Journal<'a> {
    /// Open the region and recover the newest surviving record.
    pub fn open(flash: esp_hal::peripherals::FLASH<'a>) -> (Self, Option<Record>) {
        let mut flash = FlashStorage::new(flash);
        let mut best: Option<(usize, Record)> = None;

        let mut buf = [0u8; RECORD_LEN];
        for slot in 0..SLOTS {
            let addr = OFFSET + slot_offset(slot);
            if flash.read(addr, &mut buf).is_err() {
                continue;
            }
            if let Some(rec) = journal::decode(&buf) {
                if best.is_none_or(|(_, b)| rec.seq > b.seq) {
                    best = Some((slot, rec));
                }
            }
        }

        let (newest_index, seq, recovered) = match best {
            Some((i, rec)) => {
                println!(
                    "journal: recovered balance {}s from slot {i}, seq {}",
                    rec.balance_secs, rec.seq
                );
                (Some(i), rec.seq, Some(rec))
            }
            None => {
                println!("journal: empty, starting fresh");
                (None, 0, None)
            }
        };

        (Self { flash, newest_index, seq }, recovered)
    }

    /// Append one record. Erases the next sector when the ring advances into it.
    pub fn append(&mut self, balance_secs: i32, last_tick: i64) {
        let slot = journal::next_slot(self.newest_index, SLOTS);

        if journal::needs_erase(slot, SLOTS_PER_SECTOR) {
            let sector_start = OFFSET + (slot / SLOTS_PER_SECTOR * SECTOR) as u32;
            // `Storage::write` on esp-storage erases as needed, so a blank sector is
            // written rather than explicitly erased; writing 0xFF is the same thing.
            let blank = [0xFFu8; SECTOR];
            if let Err(e) = self.flash.write(sector_start, &blank) {
                println!("journal: sector erase failed ({e:?})");
                return;
            }
        }

        self.seq = self.seq.wrapping_add(1);
        let bytes = journal::encode(&Record {
            seq: self.seq,
            balance_secs,
            last_tick,
        });

        match self.flash.write(OFFSET + slot_offset(slot), &bytes) {
            Ok(()) => self.newest_index = Some(slot),
            // Leave `newest_index` alone so the next append retries the same slot
            // rather than silently skipping a position in the ring.
            Err(e) => println!("journal: write failed ({e:?})"),
        }
    }
}

fn slot_offset(slot: usize) -> u32 {
    (slot * RECORD_LEN) as u32
}
