//! PCF85063A register codec.
//!
//! The onboard RTC stores **UTC**, never local time. Storing local would mean the
//! stored value is ambiguous for one hour every October and impossible for one hour
//! every March — and the ledger's night window is defined in local time, so a
//! mis-resolved DST boundary would lock the devices out an hour early or late.
//! Conversion to local happens once, at the edge, in [`crate::civil`].
//!
//! The A variant has no century bit, so this covers 2000–2099.

use crate::civil::{civil_from_days, days_from_civil, SECS_PER_DAY};

/// Address of the PCF85063A on the shared I²C bus.
pub const ADDRESS: u8 = 0x51;

/// First of the seven consecutive time registers (Seconds … Years).
pub const REG_SECONDS: u8 = 0x04;

/// Number of time registers read or written in one burst.
pub const TIME_REGS: usize = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The oscillator-stop flag is set: the RTC lost power and its contents are
    /// meaningless. Not an error to paper over — it is the signal to go get the time
    /// from the network before doing any accounting.
    ClockIntegrityLost,
    /// A register held something that is not valid BCD, or a field out of range.
    Corrupt,
    /// Outside the 2000–2099 range the chip can represent.
    OutOfRange,
}

const fn from_bcd(v: u8) -> Option<u8> {
    let hi = v >> 4;
    let lo = v & 0x0f;
    if hi > 9 || lo > 9 {
        None
    } else {
        Some(hi * 10 + lo)
    }
}

const fn to_bcd(v: u8) -> u8 {
    ((v / 10) << 4) | (v % 10)
}

/// Encode a UTC unix timestamp into the seven time registers.
pub fn encode(utc: i64) -> Result<[u8; TIME_REGS], Error> {
    let days = utc.div_euclid(SECS_PER_DAY);
    let tod = utc.rem_euclid(SECS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    if !(2000..=2099).contains(&year) {
        return Err(Error::OutOfRange);
    }

    let hour = (tod / 3_600) as u8;
    let minute = (tod % 3_600 / 60) as u8;
    let second = (tod % 60) as u8;
    // Monday = 0 in `civil`; the PCF85063A counts Sunday = 0. We never read this field
    // back — the date is authoritative — but it should still be right on the wire.
    let weekday = ((crate::civil::weekday_from_days(days) + 1) % 7) as u8;

    Ok([
        to_bcd(second), // OS flag cleared by writing seconds
        to_bcd(minute),
        to_bcd(hour),
        to_bcd(day as u8),
        weekday,
        to_bcd(month as u8),
        to_bcd((year - 2000) as u8),
    ])
}

/// Decode the seven time registers into a UTC unix timestamp.
pub fn decode(regs: &[u8; TIME_REGS]) -> Result<i64, Error> {
    // Bit 7 of the seconds register is the oscillator-stop flag.
    if regs[0] & 0x80 != 0 {
        return Err(Error::ClockIntegrityLost);
    }

    let second = from_bcd(regs[0] & 0x7f).ok_or(Error::Corrupt)?;
    let minute = from_bcd(regs[1] & 0x7f).ok_or(Error::Corrupt)?;
    let hour = from_bcd(regs[2] & 0x3f).ok_or(Error::Corrupt)?;
    let day = from_bcd(regs[3] & 0x3f).ok_or(Error::Corrupt)?;
    let month = from_bcd(regs[5] & 0x1f).ok_or(Error::Corrupt)?;
    let year = from_bcd(regs[6]).ok_or(Error::Corrupt)? as i32 + 2000;

    if second > 59 || minute > 59 || hour > 23 {
        return Err(Error::Corrupt);
    }
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month as u32) {
        return Err(Error::Corrupt);
    }

    let days = days_from_civil(year, month as u32, day as u32);
    Ok(days * SECS_PER_DAY + hour as i64 * 3_600 + minute as i64 * 60 + second as i64)
}

fn days_in_month(y: i32, m: u32) -> u8 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_across_a_span_of_years() {
        // Every ~37 hours from 2026 to 2030, which crosses leap days, month ends and
        // year boundaries without taking all day to run.
        let start = days_from_civil(2026, 1, 1) * SECS_PER_DAY;
        let end = days_from_civil(2030, 1, 1) * SECS_PER_DAY;
        let mut t = start;
        while t < end {
            let regs = encode(t).expect("in range");
            assert_eq!(decode(&regs), Ok(t), "round trip failed at {t}");
            t += 133_777;
        }
    }

    #[test]
    fn oscillator_stop_flag_is_reported_not_ignored() {
        // A powered-down RTC holds plausible-looking garbage. Silently trusting it
        // would put the night window somewhere fictional.
        let mut regs = encode(days_from_civil(2026, 8, 3) * SECS_PER_DAY).unwrap();
        regs[0] |= 0x80;
        assert_eq!(decode(&regs), Err(Error::ClockIntegrityLost));
    }

    #[test]
    fn writing_seconds_clears_the_stop_flag() {
        // Bit 7 must be zero in anything we encode, or we would re-arm the flag on
        // every set and the clock would never be trusted again.
        for t in [0, 1_785_788_154, days_from_civil(2099, 12, 31) * SECS_PER_DAY] {
            if let Ok(regs) = encode(t) {
                assert_eq!(regs[0] & 0x80, 0, "stop flag set in encoded seconds");
            }
        }
    }

    #[test]
    fn known_timestamp_encodes_to_expected_bcd() {
        // 2026-08-03 20:15:54 UTC — the first time this firmware ever read off SNTP.
        let regs = encode(1_785_788_154).unwrap();
        assert_eq!(regs[0], 0x54, "seconds");
        assert_eq!(regs[1], 0x15, "minutes");
        assert_eq!(regs[2], 0x20, "hours");
        assert_eq!(regs[3], 0x03, "day");
        assert_eq!(regs[4], 1, "weekday: Monday, Sunday-based");
        assert_eq!(regs[5], 0x08, "month");
        assert_eq!(regs[6], 0x26, "year");
    }

    #[test]
    fn rejects_invalid_bcd_and_impossible_dates() {
        let good = encode(1_785_788_154).unwrap();

        let mut bad = good;
        bad[1] = 0x6A; // low nibble > 9
        assert_eq!(decode(&bad), Err(Error::Corrupt));

        let mut bad = good;
        bad[2] = 0x25; // hour 25
        assert_eq!(decode(&bad), Err(Error::Corrupt));

        let mut bad = good;
        bad[5] = 0x13; // month 13
        assert_eq!(decode(&bad), Err(Error::Corrupt));

        let mut bad = good;
        bad[3] = 0x00; // day 0
        assert_eq!(decode(&bad), Err(Error::Corrupt));

        // 30 February
        let mut bad = good;
        bad[3] = 0x30;
        bad[5] = 0x02;
        assert_eq!(decode(&bad), Err(Error::Corrupt));
    }

    #[test]
    fn accepts_a_real_leap_day() {
        let t = days_from_civil(2028, 2, 29) * SECS_PER_DAY + 12 * 3_600;
        assert_eq!(decode(&encode(t).unwrap()), Ok(t));
    }

    #[test]
    fn refuses_years_the_chip_cannot_hold() {
        assert_eq!(encode(days_from_civil(1999, 12, 31) * SECS_PER_DAY), Err(Error::OutOfRange));
        assert_eq!(encode(days_from_civil(2100, 1, 1) * SECS_PER_DAY), Err(Error::OutOfRange));
    }
}
