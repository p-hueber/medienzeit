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
use medienzeit_core::settings::{self, Settings, SETTINGS_LEN};

/// Must match the `storage` partition in `partitions.csv`.
pub const OFFSET: u32 = 0x31_0000;
pub const SIZE: usize = 16 * 1024;

const SECTOR: usize = 4096;
const SLOTS_PER_SECTOR: usize = SECTOR / RECORD_LEN;
/// The journal ring gives up its last sector to settings, so adding stored settings did
/// not need the partition table changed on a device already holding a live balance.
/// Three sectors instead of four roughly doubles the per-sector wear, from about 15
/// erases a day to 20 — still centuries against a 100k-cycle rating.
const SECTORS: usize = SIZE / SECTOR - 1;
const SLOTS: usize = SLOTS_PER_SECTOR * SECTORS;

/// Settings live in the sector after the journal ring.
const SETTINGS_OFFSET: u32 = OFFSET + (SECTORS * SECTOR) as u32;
const SETTINGS_SLOTS: usize = SECTOR / SETTINGS_LEN;

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

    /// The shared flash handle, so settings can live in the same region.
    pub fn flash(&mut self) -> &mut FlashStorage<'a> {
        &mut self.flash
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

/// Stored [`Settings`], in their own sector.
///
/// Written like the journal rather than in place: settings change rarely, but a torn
/// write during a power cut would otherwise lose the rules entirely and silently fall
/// back to the compiled-in defaults. Slots are filled in turn and the newest valid one
/// wins, so the previous settings survive a failed save.
/// Holds only the bookkeeping: the flash handle belongs to [`Journal`], because the
/// peripheral can only be claimed once.
pub struct SettingsStore {
    newest: Option<usize>,
    seq: u32,
}

impl SettingsStore {
    /// Read the newest valid settings, if any have ever been stored.
    pub fn open(flash: &mut FlashStorage<'_>) -> (Self, Option<Settings>) {
        let mut best: Option<(usize, Settings)> = None;
        let mut buf = [0u8; SETTINGS_LEN];
        for slot in 0..SETTINGS_SLOTS {
            let addr = SETTINGS_OFFSET + (slot * SETTINGS_LEN) as u32;
            if flash.read(addr, &mut buf).is_err() {
                continue;
            }
            if let Some(s) = settings::decode(&buf) {
                if best.is_none_or(|(_, b)| s.seq > b.seq) {
                    best = Some((slot, s));
                }
            }
        }
        match best {
            Some((i, s)) => {
                println!("settings: loaded from slot {i}, seq {}", s.seq);
                (Self { newest: Some(i), seq: s.seq }, Some(s))
            }
            None => {
                println!("settings: none stored, using defaults");
                (Self { newest: None, seq: 0 }, None)
            }
        }
    }

    /// Store new settings. Refuses values that would break the ledger.
    pub fn save(&mut self, flash: &mut FlashStorage<'_>, mut s: Settings) -> bool {
        if !s.valid() {
            println!("settings: refused, invalid");
            return false;
        }
        self.seq = self.seq.wrapping_add(1);
        s.seq = self.seq;
        let slot = match self.newest {
            Some(i) => (i + 1) % SETTINGS_SLOTS,
            None => 0,
        };
        // Erasing on wrap is what makes the slots reusable; a sector must be erased
        // before any slot in it can be written again.
        if slot == 0 && flash.write(SETTINGS_OFFSET, &[0xff; SECTOR]).is_err() {
            println!("settings: erase failed");
            return false;
        }
        let addr = SETTINGS_OFFSET + (slot * SETTINGS_LEN) as u32;
        if flash.write(addr, &settings::encode(&s)).is_err() {
            println!("settings: write failed");
            return false;
        }
        self.newest = Some(slot);
        println!("settings: saved to slot {slot}, seq {}", s.seq);
        true
    }
}
