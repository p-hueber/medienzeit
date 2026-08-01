//! Berlin-local helpers for the simulator, so scenarios can be written in wall-clock
//! terms instead of unix seconds.

use medienzeit_core::civil::{self, days_from_civil, SECS_PER_DAY};

/// UTC timestamp for a Europe/Berlin wall-clock time.
pub fn berlin(y: i32, m: u32, d: u32, h: u32, mi: u32) -> i64 {
    let naive = days_from_civil(y, m, d) * SECS_PER_DAY + h as i64 * 3_600 + mi as i64 * 60;
    naive - civil::utc_offset(naive)
}
