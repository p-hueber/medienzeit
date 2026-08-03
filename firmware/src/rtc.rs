//! The onboard PCF85063A, on the shared I²C bus (IO47/IO48).
//!
//! This is the same bus the SHTC3 sits on at 0x70, and the same one a PN532 would
//! share at 0x24 — no address clashes, so it stays available for the reader.
//!
//! Its job is to make the clock survive a reboot. Without it, every restart parks the
//! firmware until the network comes up; with it, the ledger can start accounting
//! immediately and let SNTP correct it a moment later.

use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::Blocking;
use esp_println::println;
use medienzeit_core::rtc as codec;

/// Unix time at which this firmware was compiled; see `build.rs`.
const BUILD_UNIX: i64 = {
    let s = env!("MEDIENZEIT_BUILD_UNIX");
    // `const` parsing without a dependency: the value is always plain ASCII digits.
    let bytes = s.as_bytes();
    let mut acc = 0i64;
    let mut i = 0;
    while i < bytes.len() {
        acc = acc * 10 + (bytes[i] - b'0') as i64;
        i += 1;
    }
    acc
};

pub struct Rtc<'d> {
    i2c: I2c<'d, Blocking>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Bus,
    Codec(codec::Error),
}

impl<'d> Rtc<'d> {
    pub fn new(i2c: I2c<'d, Blocking>) -> Self {
        Self { i2c }
    }

    /// Read the current UTC unix timestamp.
    ///
    /// Returns [`codec::Error::ClockIntegrityLost`] when the oscillator-stop flag is
    /// set, which is what a fresh or power-cut board reports. That is a normal startup
    /// state, not a fault.
    pub fn now(&mut self) -> Result<i64, Error> {
        let mut regs = [0u8; codec::TIME_REGS];
        self.i2c
            .write_read(codec::ADDRESS, &[codec::REG_SECONDS], &mut regs)
            .map_err(|_| Error::Bus)?;
        codec::decode(&regs).map_err(Error::Codec)
    }

    /// Set the clock from a UTC unix timestamp. Clears the oscillator-stop flag.
    pub fn set(&mut self, utc: i64) -> Result<(), Error> {
        let regs = codec::encode(utc).map_err(Error::Codec)?;
        let mut buf = [0u8; codec::TIME_REGS + 1];
        buf[0] = codec::REG_SECONDS;
        buf[1..].copy_from_slice(&regs);
        self.i2c.write(codec::ADDRESS, &buf).map_err(|_| Error::Bus)
    }

    /// Best-effort read for startup: logs what happened and yields a timestamp only if
    /// the clock is genuinely trustworthy.
    pub fn startup_time(&mut self) -> Option<i64> {
        match self.now() {
            Ok(t) if t < BUILD_UNIX => {
                // Well-formed but older than this firmware, so it cannot be real. A
                // factory-tested board arrives exactly like this: oscillator running
                // since the production line, stop flag clear, time a year stale.
                println!("rtc: time {t} predates this build, ignoring");
                None
            }
            Ok(t) => {
                println!("rtc: holding time, unix {t}");
                Some(t)
            }
            Err(Error::Codec(codec::Error::ClockIntegrityLost)) => {
                println!("rtc: oscillator stopped, time unknown until SNTP");
                None
            }
            Err(e) => {
                println!("rtc: unusable ({e:?})");
                None
            }
        }
    }
}

/// Probe every 7-bit address and report which ones acknowledge.
///
/// Expect 0x51 (PCF85063A) and 0x70 (SHTC3) on this board.
pub fn scan(i2c: &mut I2c<'_, Blocking>) {
    // A 1-byte read, not a zero-length write: an empty write may be rejected by the
    // HAL before it ever drives the bus, which looks identical to "nothing is there".
    // A read is also non-destructive on devices we know nothing about.
    let mut found = 0;
    let mut buf = [0u8; 1];
    for addr in 0x08u8..=0x77 {
        if i2c.read(addr, &mut buf).is_ok() {
            println!("i2c: device at 0x{addr:02x}");
            found += 1;
        }
    }
    println!("i2c: scan complete, {found} device(s)");
}

/// Build the shared I²C bus. IO47 = SDA, IO48 = SCL, per Waveshare's `user_config.h`.
pub fn bus(
    periph: esp_hal::peripherals::I2C0<'static>,
    sda: esp_hal::peripherals::GPIO47<'static>,
    scl: esp_hal::peripherals::GPIO48<'static>,
) -> I2c<'static, Blocking> {
    I2c::new(
        periph,
        I2cConfig::default().with_frequency(esp_hal::time::Rate::from_khz(100)),
    )
        .expect("i2c init failed")
        .with_sda(sda)
        .with_scl(scl)
}
