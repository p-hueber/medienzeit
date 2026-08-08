//! PN5180 driver, enough of it for ISO 15693 inventory.
//!
//! Written from NXP's datasheet; there is no Rust driver for this part. Generic over
//! `embedded-hal`, so it compiles on the host as well as the target.
//!
//! # The BUSY line is the whole protocol
//!
//! Every exchange is framed by BUSY, and getting this wrong is the difference between
//! a working reader and one that returns plausible garbage:
//!
//! 1. Wait for BUSY **low** — the chip is idle.
//! 2. Pull NSS low, clock out the command, release NSS.
//! 3. BUSY goes **high** while the chip processes, then low again when done.
//! 4. For a command that returns data, do a *second* transfer to read it, framed the
//!    same way.
//!
//! Skipping step 3 reads back whatever was left in the chip's buffer, which looks like
//! data and is not. Every wait here is bounded: this board ties RST high because there
//! was no GPIO left for it, so a wedged chip cannot be hardware-reset and an unbounded
//! wait would hang the firmware permanently.
//!
//! # Running without BUSY
//!
//! The BUSY pin is optional here, and on this board it has to be: IO43 is the only
//! remaining header pin and the ROM bootloader drives it at reset, so anything wired
//! there stops the chip booting at all. Without BUSY the driver substitutes a fixed
//! settling delay, which is strictly worse — it is a guess where the pin was a fact —
//! and is the first thing to revisit if reads come back inconsistent.

#![no_std]

use embedded_hal::delay::DelayNs;
use embedded_hal::digital::InputPin;
use embedded_hal::spi::SpiDevice;

/// Direct commands, from the datasheet's instruction set.
mod cmd {
    pub const WRITE_REGISTER: u8 = 0x00;
    pub const READ_REGISTER: u8 = 0x04;
    pub const READ_EEPROM: u8 = 0x07;
}

/// EEPROM addresses worth reading.
pub mod eeprom {
    /// 16 bytes, unique per die.
    pub const DIE_IDENTIFIER: u8 = 0x00;
    /// 2 bytes, minor then major.
    pub const PRODUCT_VERSION: u8 = 0x10;
    /// 2 bytes, minor then major.
    pub const FIRMWARE_VERSION: u8 = 0x12;
    /// 2 bytes, minor then major.
    pub const EEPROM_VERSION: u8 = 0x14;
}

/// How long to wait for BUSY, in polling iterations of roughly 100 µs.
///
/// Generous enough for an RF transaction, short enough that a dead chip reports
/// rather than hangs.
const BUSY_TIMEOUT_STEPS: u32 = 5_000; // ~500 ms

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The SPI transfer itself failed.
    Spi,
    /// Could not read the BUSY pin.
    Pin,
    /// BUSY never returned low. With RST tied high the only recovery is a soft reset
    /// or a power cycle, so callers should surface this rather than retry forever.
    BusyTimeout,
    /// A response that cannot be right — all ones is what an absent or unpowered chip
    /// reads back.
    NoResponse,
}

pub struct Pn5180<SPI, BUSY, D> {
    spi: SPI,
    /// `None` when no BUSY pin is wired; see the module docs.
    busy: Option<BUSY>,
    delay: D,
}

/// Settling time used in place of the BUSY handshake. Generous: the datasheet's
/// worst-case command processing is far shorter, and this only costs latency.
const NO_BUSY_SETTLE_US: u32 = 5_000;

impl<SPI, BUSY, D> Pn5180<SPI, BUSY, D>
where
    SPI: SpiDevice,
    BUSY: InputPin,
    D: DelayNs,
{
    pub fn new(spi: SPI, busy: BUSY, delay: D) -> Self {
        Self { spi, busy: Some(busy), delay }
    }

    /// Construct without a BUSY line, substituting a fixed delay.
    pub fn without_busy(spi: SPI, delay: D) -> Self {
        Self { spi, busy: None, delay }
    }

    /// Block until BUSY is low, or give up.
    fn wait_idle(&mut self) -> Result<(), Error> {
        let Some(busy) = self.busy.as_mut() else {
            self.delay.delay_us(NO_BUSY_SETTLE_US);
            return Ok(());
        };
        for _ in 0..BUSY_TIMEOUT_STEPS {
            if busy.is_low().map_err(|_| Error::Pin)? {
                return Ok(());
            }
            self.delay.delay_us(100);
        }
        Err(Error::BusyTimeout)
    }

    /// Wait for BUSY to rise, which is how the chip acknowledges it has taken the
    /// command. Absence of a rise is not fatal — short commands can complete before we
    /// look — so this does not error, it just does not wait forever.
    fn wait_taken(&mut self) {
        let Some(busy) = self.busy.as_mut() else { return };
        for _ in 0..100 {
            match busy.is_high() {
                Ok(true) => return,
                Ok(false) => self.delay.delay_us(10),
                Err(_) => return,
            }
        }
    }

    /// Send one command frame.
    fn send(&mut self, frame: &[u8]) -> Result<(), Error> {
        self.wait_idle()?;
        self.spi.write(frame).map_err(|_| Error::Spi)?;
        self.wait_taken();
        self.wait_idle()
    }

    /// Read a response frame. Must follow the command it belongs to.
    fn recv(&mut self, buf: &mut [u8]) -> Result<(), Error> {
        self.wait_idle()?;
        self.spi.read(buf).map_err(|_| Error::Spi)?;
        self.wait_idle()
    }

    pub fn read_eeprom(&mut self, addr: u8, buf: &mut [u8]) -> Result<(), Error> {
        self.send(&[cmd::READ_EEPROM, addr, buf.len() as u8])?;
        self.recv(buf)
    }

    pub fn read_register(&mut self, reg: u8) -> Result<u32, Error> {
        self.send(&[cmd::READ_REGISTER, reg])?;
        let mut b = [0u8; 4];
        self.recv(&mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    pub fn write_register(&mut self, reg: u8, value: u32) -> Result<(), Error> {
        let v = value.to_le_bytes();
        self.send(&[cmd::WRITE_REGISTER, reg, v[0], v[1], v[2], v[3]])
    }

    /// (major, minor) of the product version.
    pub fn product_version(&mut self) -> Result<(u8, u8), Error> {
        self.version(eeprom::PRODUCT_VERSION)
    }

    /// (major, minor) of the firmware version.
    pub fn firmware_version(&mut self) -> Result<(u8, u8), Error> {
        self.version(eeprom::FIRMWARE_VERSION)
    }

    /// (major, minor) of the EEPROM version.
    pub fn eeprom_version(&mut self) -> Result<(u8, u8), Error> {
        self.version(eeprom::EEPROM_VERSION)
    }

    fn version(&mut self, addr: u8) -> Result<(u8, u8), Error> {
        let mut b = [0u8; 2];
        self.read_eeprom(addr, &mut b)?;
        // 0xFFFF is what an absent, unpowered or mis-wired chip returns. Treating it
        // as a version number would make a dead reader look alive.
        if b == [0xFF, 0xFF] {
            return Err(Error::NoResponse);
        }
        Ok((b[1], b[0]))
    }

    /// 16-byte die identifier, unique per chip.
    pub fn die_id(&mut self, buf: &mut [u8; 16]) -> Result<(), Error> {
        self.read_eeprom(eeprom::DIE_IDENTIFIER, buf)
    }
}
