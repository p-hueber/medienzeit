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
//! data and is not. Every wait here is bounded regardless, because an unbounded wait on
//! a chip that has stopped answering hangs the firmware permanently — and the ledger
//! stops ticking with it.
//!
//! # Running without BUSY
//!
//! The BUSY pin is optional here, substituted by a fixed settling delay. That is
//! strictly worse — a guess where the pin was a fact — and exists only as an escape
//! hatch for a board that genuinely has no GPIO to spare. Wire BUSY if you can.

#![no_std]

use embedded_hal::delay::DelayNs;
use embedded_hal::digital::InputPin;
use embedded_hal::spi::SpiDevice;

/// Direct commands, from the datasheet's instruction set.
mod cmd {
    pub const WRITE_REGISTER: u8 = 0x00;
    pub const WRITE_REGISTER_OR_MASK: u8 = 0x01;
    pub const WRITE_REGISTER_AND_MASK: u8 = 0x02;
    pub const READ_REGISTER: u8 = 0x04;
    pub const READ_EEPROM: u8 = 0x07;
    pub const SEND_DATA: u8 = 0x09;
    pub const READ_DATA: u8 = 0x0a;
    pub const LOAD_RF_CONFIG: u8 = 0x11;
    pub const RF_ON: u8 = 0x16;
    pub const RF_OFF: u8 = 0x17;
}

/// Registers we touch.
pub mod reg {
    /// Bits 2:0 select the transceive command.
    pub const SYSTEM_CONFIG: u8 = 0x00;
    pub const IRQ_STATUS: u8 = 0x02;
    pub const IRQ_CLEAR: u8 = 0x03;
    /// Bits 8:0 are the number of bytes received.
    pub const RX_STATUS: u8 = 0x13;
    /// Bits 26:24 are the transceive state.
    pub const RF_STATUS: u8 = 0x1d;
    /// Bit 0 enables the receiver's CRC check.
    pub const CRC_RX_CONFIG: u8 = 0x12;
    /// Bit 0 enables the transmitter's CRC.
    pub const CRC_TX_CONFIG: u8 = 0x19;
    /// Transmitter framing. Clearing bits 6, 7 and 10 makes it send a bare EOF.
    pub const TX_CONFIG: u8 = 0x18;
}

/// Transceive state meaning "ready for the host to hand over a frame".
const TS_WAIT_TRANSMIT: u32 = 1;
/// Transceive state meaning the machine has finished and is doing nothing.
const TS_IDLE: u32 = 0;

/// Clears the TX_CONFIG bits that carry data framing, leaving the transmitter emitting
/// only an EOF. That is how an ISO 15693 anticollision round steps to the next slot:
/// a zero-length `SEND_DATA` on its own transmits nothing at all.
const TX_CONFIG_EOF_ONLY: u32 = 0xffff_fb3f;

/// Requests per poll before a card counts as absent. Cheap: each is a ~20 ms timeout,
/// and only a genuinely absent card pays the full cost.
const CARD_ATTEMPTS: u32 = 3;

/// Extract the transceive state from `RF_STATUS`.
pub fn transceive_state(rf_status: u32) -> u32 {
    (rf_status >> 24) & 0x07
}

/// `IRQ_STATUS` bit for "a frame has been received".
const RX_IRQ: u32 = 1 << 0;
/// Every IRQ bit, for clearing.
const ALL_IRQS: u32 = 0x000f_ffff;

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
    /// The chip reported more received bytes than the caller's buffer can hold.
    Overflow,
    /// The transceiver never reached WaitTransmit, so a frame could not be handed over.
    NotReadyToTransmit,
}

/// An ISO 15693 unique identifier, in transmission order (least significant byte first,
/// which is how the tag sends it).
///
/// Kept in wire order deliberately: the display order everyone quotes is the reverse,
/// and converting once at the edge is less error-prone than two representations that
/// look alike. Use [`Uid::display_order`] when showing it to a human.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Uid(pub [u8; 8]);

impl Uid {
    /// Most-significant byte first, as printed on datasheets and tag labels. ISO 15693
    /// UIDs always start `E0` in this order.
    pub fn display_order(&self) -> [u8; 8] {
        let mut out = self.0;
        out.reverse();
        out
    }

    /// The manufacturer byte — `0x04` for NXP, i.e. ICODE.
    pub fn manufacturer(&self) -> u8 {
        self.0[6]
    }

    /// Parse a UID written the way it is printed and quoted: most significant byte
    /// first, starting `E0`. Separators (`:`, `-`, space) are ignored, case does not
    /// matter.
    ///
    /// Rejects anything that is not a well-formed ISO 15693 UID rather than accepting it
    /// quietly. A typo in configuration would otherwise produce a device that can never
    /// be recognised, and the symptom — the clock always running — would look like a
    /// hardware fault rather than a wrong character.
    pub fn from_display_hex(s: &str) -> Option<Self> {
        let mut bytes = [0u8; 8];
        let mut n = 0;
        let mut hi: Option<u8> = None;
        for c in s.chars() {
            if matches!(c, ':' | '-' | ' ' | '_') {
                continue;
            }
            let d = c.to_digit(16)? as u8;
            match hi {
                None => hi = Some(d),
                Some(h) => {
                    if n == 8 {
                        return None;
                    }
                    bytes[n] = (h << 4) | d;
                    n += 1;
                    hi = None;
                }
            }
        }
        if n != 8 || hi.is_some() || bytes[0] != 0xe0 {
            return None;
        }
        bytes.reverse();
        Some(Uid(bytes))
    }
}

/// Decode an inventory response: flags, DSFID, then eight UID bytes.
///
/// Pure so it can be tested on the host, which matters more than it looks — this is the
/// step where a length or ordering mistake produces a UID that is stable, plausible and
/// wrong, and a wrong-but-stable UID would silently identify the wrong device forever.
pub fn parse_inventory_response(buf: &[u8]) -> Option<Uid> {
    // flags(1) + DSFID(1) + UID(8). The CRC is checked and stripped by the PN5180.
    if buf.len() < 10 {
        return None;
    }
    // Bit 0 of the response flags is the error flag; a tag reporting an error has not
    // given us a usable identity.
    if buf[0] & 0x01 != 0 {
        return None;
    }
    let mut uid = [0u8; 8];
    uid.copy_from_slice(&buf[2..10]);
    // Every ISO 15693 UID begins 0xE0 in display order, i.e. the last wire byte. This
    // rejects framing slips that would otherwise look like a valid tag.
    if uid[7] != 0xe0 {
        return None;
    }
    Some(Uid(uid))
}

/// An ISO 14443A 4-byte card identifier, in the order the card sends it — which is also
/// the order it is printed.
///
/// A separate type from [`Uid`] on purpose. These are a different protocol, they are
/// four bytes not eight, and 4-byte card UIDs are **not** reliably unique across cheap
/// batches. Keeping them distinct means the two can never be compared or confused, and
/// the weaker guarantee stays visible in the type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CardUid(pub [u8; 4]);

impl CardUid {
    /// Parse from printed hex, most significant byte first. Separators are ignored.
    pub fn from_hex(s: &str) -> Option<Self> {
        let mut bytes = [0u8; 4];
        let mut n = 0;
        let mut hi: Option<u8> = None;
        for c in s.chars() {
            if matches!(c, ':' | '-' | ' ' | '_') {
                continue;
            }
            let d = c.to_digit(16)? as u8;
            match hi {
                None => hi = Some(d),
                Some(h) => {
                    if n == 4 {
                        return None;
                    }
                    bytes[n] = (h << 4) | d;
                    n += 1;
                    hi = None;
                }
            }
        }
        if n != 4 || hi.is_some() {
            return None;
        }
        Some(CardUid(bytes))
    }
}

/// Decode an ISO 14443A anticollision response: four UID bytes then the BCC.
///
/// The BCC is an XOR checksum over the UID, and checking it is the only guard against a
/// misframed read here — unlike ISO 15693 there is no `0xE0` prefix to sanity-check, so
/// without it any five bytes would be accepted as an identity.
pub fn parse_anticollision(buf: &[u8]) -> Option<CardUid> {
    if buf.len() < 5 {
        return None;
    }
    let uid = [buf[0], buf[1], buf[2], buf[3]];
    if uid[0] ^ uid[1] ^ uid[2] ^ uid[3] != buf[4] {
        return None;
    }
    // 0x88 is the cascade tag, meaning this is really a 7- or 10-byte UID and what we
    // have is only its first fragment. Accepting it would key a device on a partial
    // identity that a second cascade level would contradict.
    if uid[0] == 0x88 {
        return None;
    }
    Some(CardUid(uid))
}


/// Smooths tag presence over consecutive inventory rounds.
///
/// A tag is believed present the moment it is seen, and believed gone only after it has
/// been missing from several rounds in a row. Tracking that **per tag** rather than per
/// count is the point: with two tags in the field, a round that happens to see only one
/// of them keeps the count at one, so a count-based check sees "still one tag" and
/// silently swaps which device is present. Observed on hardware — putting one sticker
/// back flipped both devices at once, and the untouched one read as taken.
pub struct TagSet<const N: usize> {
    tags: [Uid; N],
    /// Rounds since each tag was last seen.
    missing: [u32; N],
    len: usize,
    limit: u32,
}

impl<const N: usize> TagSet<N> {
    /// `limit` is how many consecutive rounds a tag may be missing before it counts as
    /// gone. Zero means believe every round exactly as it comes.
    pub fn new(limit: u32) -> Self {
        Self { tags: [Uid::default(); N], missing: [0; N], len: 0, limit }
    }

    pub fn tags(&self) -> &[Uid] {
        &self.tags[..self.len]
    }

    pub fn contains(&self, uid: &Uid) -> bool {
        self.tags().contains(uid)
    }

    /// Fold one round's reading in, and return the believed set.
    pub fn update(&mut self, seen: &[Uid]) -> &[Uid] {
        // Age existing entries, dropping those missing for too long. Iterating downwards
        // keeps the swap-remove from skipping an entry.
        let mut i = self.len;
        while i > 0 {
            i -= 1;
            if seen.contains(&self.tags[i]) {
                self.missing[i] = 0;
            } else {
                self.missing[i] += 1;
                if self.missing[i] > self.limit {
                    self.len -= 1;
                    self.tags[i] = self.tags[self.len];
                    self.missing[i] = self.missing[self.len];
                }
            }
        }
        // Newly seen tags are believed at once: a device being picked up should register
        // immediately, where a device going quiet deserves the benefit of the doubt.
        for uid in seen {
            if !self.tags[..self.len].contains(uid) && self.len < N {
                self.tags[self.len] = *uid;
                self.missing[self.len] = 0;
                self.len += 1;
            }
        }
        self.tags()
    }
}

pub struct Pn5180<SPI, BUSY, D> {
    spi: SPI,
    /// `None` when no BUSY pin is wired; see the module docs.
    busy: Option<BUSY>,
    delay: D,
    /// The protocol most recently configured, so a wedged transceiver can be recovered
    /// without the caller having to know what it was running.
    protocol: Option<(u8, u8)>,
    /// How many times the transceiver had to be recovered by cycling the field.
    recoveries: u32,
    /// Whether BUSY has ever been observed high.
    ///
    /// Bring-up diagnostic, and a sharp one: a chip that is unpowered, held in reset or
    /// wired to the wrong pin holds BUSY low forever, which is indistinguishable from a
    /// chip so fast we always miss the rise. If this is still false after a few
    /// commands, the handshake is not happening and every read is fiction.
    saw_busy_high: bool,
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
        Self { spi, busy: Some(busy), delay, protocol: None, recoveries: 0, saw_busy_high: false }
    }

    /// Construct without a BUSY line, substituting a fixed delay.
    pub fn without_busy(spi: SPI, delay: D) -> Self {
        Self { spi, busy: None, delay, protocol: None, recoveries: 0, saw_busy_high: false }
    }

    /// Has BUSY ever been seen high? See the field docs — false after several commands
    /// means the chip is not acknowledging anything.
    pub fn saw_busy_high(&self) -> bool {
        self.saw_busy_high
    }

    /// Current BUSY level, or `None` if no BUSY pin is wired or it cannot be read.
    pub fn busy_level(&mut self) -> Option<bool> {
        self.busy.as_mut()?.is_high().ok()
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
                Ok(true) => {
                    self.saw_busy_high = true;
                    return;
                }
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

    // --- ISO 15693 ------------------------------------------------------------------

    pub fn write_register_or_mask(&mut self, reg: u8, mask: u32) -> Result<(), Error> {
        let m = mask.to_le_bytes();
        self.send(&[cmd::WRITE_REGISTER_OR_MASK, reg, m[0], m[1], m[2], m[3]])
    }

    pub fn write_register_and_mask(&mut self, reg: u8, mask: u32) -> Result<(), Error> {
        let m = mask.to_le_bytes();
        self.send(&[cmd::WRITE_REGISTER_AND_MASK, reg, m[0], m[1], m[2], m[3]])
    }

    /// Select a protocol's transmitter and receiver configuration from EEPROM.
    pub fn load_rf_config(&mut self, tx: u8, rx: u8) -> Result<(), Error> {
        self.send(&[cmd::LOAD_RF_CONFIG, tx, rx])
    }

    /// Energise the antenna. Nothing before this point draws RF current.
    pub fn field_on(&mut self) -> Result<(), Error> {
        self.send(&[cmd::RF_ON, 0x00])
    }

    pub fn field_off(&mut self) -> Result<(), Error> {
        self.send(&[cmd::RF_OFF, 0x00])
    }

    /// Blocking delay, so callers can pace probes without owning a second delay source.
    pub fn delay_ms(&mut self, ms: u32) {
        self.delay.delay_us(ms * 1000);
    }

    /// Clear every interrupt status bit.
    pub fn clear_all_irqs(&mut self) -> Result<(), Error> {
        self.clear_irqs()
    }

    fn clear_irqs(&mut self) -> Result<(), Error> {
        self.write_register(reg::IRQ_CLEAR, ALL_IRQS)
    }

    /// Put the transceiver into Transceive so the next `SEND_DATA` actually goes out.
    ///
    /// Required before *every* frame: after a transaction the state machine falls back
    /// to Idle, and a `SEND_DATA` issued from Idle is accepted without complaint and
    /// never transmitted.
    /// Snapshot of the three registers worth seeing when RF is not working:
    /// `(RF_STATUS, IRQ_STATUS, RX_STATUS)`.
    pub fn rf_debug(&mut self) -> Result<(u32, u32, u32), Error> {
        Ok((
            self.read_register(reg::RF_STATUS)?,
            self.read_register(reg::IRQ_STATUS)?,
            self.read_register(reg::RX_STATUS)?,
        ))
    }

    /// Ask the transceiver to return to Idle, and report whether it got there.
    fn settle_to_idle(&mut self) -> Result<bool, Error> {
        self.write_register_and_mask(reg::SYSTEM_CONFIG, 0xffff_fff8)?;
        for _ in 0..500 {
            if transceive_state(self.read_register(reg::RF_STATUS)?) == TS_IDLE {
                return Ok(true);
            }
            self.delay.delay_us(100);
        }
        Ok(false)
    }

    /// Arm the transceiver, recovering the front end if it will not arm.
    ///
    /// Reaching Idle is not sufficient — observed on hardware: after a poll that finds
    /// nothing, the machine settles to Idle and then refuses to advance to WaitTransmit,
    /// so every later frame fails until the field is cycled. Configuring the protocol
    /// per poll used to hide this by cycling the field every time. So the recovery is
    /// triggered by the failure it actually fixes, not by a proxy for it.
    fn begin_transceive(&mut self) -> Result<(), Error> {
        if self.try_arm()? {
            return Ok(());
        }
        if let Some((tx, rx)) = self.protocol {
            self.recoveries += 1;
            self.begin_protocol(tx, rx)?;
            if self.try_arm()? {
                return Ok(());
            }
        }
        Err(Error::NotReadyToTransmit)
    }

    /// One attempt at getting to WaitTransmit. `false` means it did not get there.
    fn try_arm(&mut self) -> Result<bool, Error> {
        self.settle_to_idle()?;
        self.write_register_or_mask(reg::SYSTEM_CONFIG, 0x0000_0003)?;
        // Short: this is the fast path, and the recovery behind it is what handles the
        // slow case. Waiting long here would just delay every failed poll.
        for _ in 0..500 {
            if transceive_state(self.read_register(reg::RF_STATUS)?) == TS_WAIT_TRANSMIT {
                return Ok(true);
            }
            self.delay.delay_us(100);
        }
        Ok(false)
    }

    /// Transmit a frame. `data` may be empty, which sends a bare EOF — that is how
    /// ISO 15693 advances to the next anticollision slot.
    fn send_data(&mut self, data: &[u8]) -> Result<(), Error> {
        self.send_data_bits(data, 0)
    }

    /// As [`Self::send_data`], but with an explicit count of valid bits in the final
    /// byte. ISO 14443A's short frames need 7, which is the only reason this exists.
    fn send_data_bits(&mut self, data: &[u8], valid_bits: u8) -> Result<(), Error> {
        self.begin_transceive()?;
        let mut frame = [0u8; 2 + MAX_FRAME];
        frame[0] = cmd::SEND_DATA;
        frame[1] = valid_bits;
        frame[2..2 + data.len()].copy_from_slice(data);
        self.send(&frame[..2 + data.len()])
    }

    /// How many bytes the receiver is holding.
    fn rx_len(&mut self) -> Result<usize, Error> {
        Ok((self.read_register(reg::RX_STATUS)? & 0x1ff) as usize)
    }

    /// Wait for a frame, and return its length. `None` means nothing answered, which for
    /// an inventory slot is the ordinary case rather than an error.
    fn await_frame(&mut self, timeout_us: u32) -> Result<Option<usize>, Error> {
        let steps = timeout_us / 100;
        for _ in 0..steps {
            if self.read_register(reg::IRQ_STATUS)? & RX_IRQ != 0 {
                return Ok(Some(self.rx_len()?));
            }
            self.delay.delay_us(100);
        }
        Ok(None)
    }

    fn read_data(&mut self, buf: &mut [u8]) -> Result<(), Error> {
        self.send(&[cmd::READ_DATA, 0x00])?;
        self.recv(buf)
    }

    /// Configure the RF front end for ISO 15693 and switch the field on.
    ///
    /// Separate from [`Self::inventory`] because the field should stay up between polls:
    /// tags need time to power up from the field, so cycling it every second would cost
    /// read reliability for no benefit.
    pub fn begin_iso15693(&mut self) -> Result<(), Error> {
        // 0x0d / 0x8d are the ISO 15693 ASK100 26 kbit/s transmitter and receiver
        // profiles in the stock EEPROM configuration.
        self.begin_protocol(0x0d, 0x8d)
    }

    /// Switch the front end to a protocol, from a known state.
    ///
    /// Reconfiguring while the field is up and the transceiver mid-flight wedges the
    /// state machine: every subsequent `begin_transceive` times out waiting for
    /// WaitTransmit, in *both* protocols, until a reset. Observed when alternating
    /// ISO 15693 and ISO 14443A once a second. Dropping the field and returning the
    /// command field to Idle first is what makes the switch survivable.
    /// How many times the transceiver has needed recovering. Worth surfacing: a rising
    /// count means the RF front end is unhappy even though reads still succeed.
    pub fn recoveries(&self) -> u32 {
        self.recoveries
    }

    pub fn begin_protocol(&mut self, tx: u8, rx: u8) -> Result<(), Error> {
        self.protocol = Some((tx, rx));
        self.write_register_and_mask(reg::SYSTEM_CONFIG, 0xffff_fff8)?;
        self.field_off()?;
        self.delay.delay_us(10_000);
        self.load_rf_config(tx, rx)?;
        self.field_on()?;
        // Let the field ramp before anything tries to transmit into it.
        self.delay.delay_us(10_000);
        Ok(())
    }

    /// One single-slot inventory, returning the raw response length and bytes.
    ///
    /// Bring-up counterpart to [`Self::inventory`]: one slot, no EOF stepping, no
    /// parsing. It separates "no tag is answering" from "the anticollision round is
    /// wrong", which the 16-slot version cannot distinguish because both look like zero
    /// tags found. `data_rate_high` is worth trying both ways — some tags are markedly
    /// more reliable at the low rate.
    pub fn inventory_single_raw(
        &mut self,
        buf: &mut [u8],
        data_rate_high: bool,
    ) -> Result<usize, Error> {
        // 0x04 inventory + 0x20 one slot, plus 0x02 for the high data rate.
        let flags = 0x24 | if data_rate_high { 0x02 } else { 0x00 };
        self.clear_irqs()?;
        self.send_data(&[flags, 0x01, 0x00])?;
        match self.await_frame(20_000)? {
            None => Ok(0),
            Some(len) => {
                let len = len.min(buf.len());
                if len > 0 {
                    self.read_data(&mut buf[..len])?;
                }
                Ok(len)
            }
        }
    }

    /// Diagnostic only: send an ISO 14443A `REQA` and return the card's ATQA.
    ///
    /// This project needs ISO 15693 for its range, not 14443A. The value here is purely
    /// as a control: a card answering `REQA` proves the antenna radiates and the receive
    /// path works, which separates "the reader is broken" from "that tag speaks a
    /// different protocol" — indistinguishable otherwise, since both are silence.
    pub fn probe_iso14443a(&mut self) -> Result<Option<[u8; 2]>, Error> {
        self.begin_iso14443a()?;
        self.reqa_or_wupa(0x26)
    }

    /// Configure the front end for ISO 14443A and switch the field on.
    ///
    /// Called once, like [`Self::begin_iso15693`]. Reconfiguring per poll would cycle
    /// the field every second, which both costs read reliability — a passive card needs
    /// time in the field to power up — and risks wedging the transceiver.
    pub fn begin_iso14443a(&mut self) -> Result<(), Error> {
        self.begin_protocol(0x00, 0x80)?;
        // ISO 14443A's short frames carry no CRC in either direction.
        self.write_register_and_mask(reg::CRC_TX_CONFIG, 0xffff_fffe)?;
        self.write_register_and_mask(reg::CRC_RX_CONFIG, 0xffff_fffe)
    }

    /// `REQA` (0x26) or `WUPA` (0x52), returning the card's ATQA.
    ///
    /// Repeat polling must use WUPA. A card that has been through anticollision is no
    /// longer in IDLE, and only WUPA wakes it from there — poll with REQA and the card
    /// answers once and then appears to vanish while sitting on the coil.
    fn reqa_or_wupa(&mut self, cmd: u8) -> Result<Option<[u8; 2]>, Error> {
        self.clear_irqs()?;
        // Seven valid bits: these are short frames.
        self.send_data_bits(&[cmd], 7)?;
        match self.await_frame(20_000)? {
            Some(len) if len >= 2 => {
                let mut atqa = [0u8; 2];
                self.read_data(&mut atqa)?;
                Ok(Some(atqa))
            }
            _ => Ok(None),
        }
    }

    /// Identify a single ISO 14443A card: `REQA`, then one anticollision level.
    ///
    /// Deliberately no anticollision *loop* — one card at a time is all this is for, as
    /// an interim while the ISO 15693 stickers are in the post. Two cards in the field
    /// will collide and read as nothing, which is the safe way to fail: no identity
    /// rather than a wrong one.
    ///
    /// Reconfigures the front end on every call, and uses `REQA` rather than `WUPA`.
    ///
    /// Both look wasteful and both are deliberate. This exact sequence read a card
    /// reliably on the bench; replacing it with one-time setup plus `WUPA` — two changes
    /// at once — wedged the transceiver on most polls and never recovered. Cycling the
    /// field per poll evidently resets something that returning the command field to
    /// Idle does not. Change one of these at a time, and verify against a real card.
    pub fn iso14443a_uid(&mut self) -> Result<Option<CardUid>, Error> {
        self.begin_iso14443a()?;
        // Several attempts before concluding the card is gone. The field is cycled once
        // per poll, so a card sitting perfectly still has to power up from cold every
        // time, and it does not always make the first request. One attempt per poll made
        // a stationary card appear to come and go.
        for _ in 0..CARD_ATTEMPTS {
            if self.reqa_or_wupa(0x26)?.is_none() {
                continue;
            }
            // ANTICOLLISION, cascade level 1: 0x93 0x20, no CRC, whole bytes.
            self.clear_irqs()?;
            self.send_data_bits(&[0x93, 0x20], 0)?;
            if let Some(len) = self.await_frame(20_000)? {
                if len >= 5 {
                    let mut buf = [0u8; 5];
                    self.read_data(&mut buf)?;
                    if let Some(uid) = parse_anticollision(&buf) {
                        return Ok(Some(uid));
                    }
                }
            }
        }
        Ok(None)
    }

    /// Find every tag in the field, with anticollision.
    ///
    /// Verified against two ICODE SLIX2 stickers: both UIDs, every round. The
    /// single-slot alternative reads *nothing* with two tags present — they collide and
    /// neither answers — which is why this has to be the default. That failure is
    /// silent and would run the clock on both devices at once.
    pub fn inventory(&mut self, out: &mut [Uid]) -> Result<usize, Error> {
        self.inventory_16slot(out)
    }

    /// Run one 16-slot inventory round, collecting every tag that answers.
    ///
    /// Sixteen slots rather than one is the entire reason a single reader can serve both
    /// devices: with one slot, two tags answering together collide and *neither* is read,
    /// which would look exactly like both devices being absent — the most damaging
    /// possible failure, since it silently stops the clock.
    ///
    /// **Known not to work.** Reads nothing against a tag that single-slot inventory
    /// reads every time, so the slot stepping is wrong: `send_data(&[])` calls
    /// `begin_transceive`, which resets the state machine and almost certainly ends the
    /// round rather than advancing it. The PN5180 wants an EOF sent within the same
    /// transceive session, configured through `TX_CONFIG`.
    ///
    /// Returns the number of UIDs written to `out`. Slots that stay silent, or that
    /// collide, are skipped rather than retried: the caller polls again in a second, and
    /// the firmware requires several consecutive misses before believing a tag is gone.
    pub fn inventory_16slot(&mut self, out: &mut [Uid]) -> Result<usize, Error> {
        // Flags: 0x02 high data rate, 0x04 inventory. Bit 5 clear selects 16 slots.
        const FLAGS: u8 = 0x06;
        const CMD_INVENTORY: u8 = 0x01;

        // Start from a known transmitter configuration: a previous round leaves it in
        // EOF-only mode, and a data frame sent in that state goes out as nothing.
        self.load_rf_config(0x0d, 0x8d)?;
        self.clear_irqs()?;
        // Mask length zero: no prefix filter, every tag participates.
        self.send_data(&[FLAGS, CMD_INVENTORY, 0x00])?;

        let mut found = 0;
        for slot in 0..16 {
            // A tag replies within about 4 ms at 26 kbit/s; 10 ms is slack for one at
            // the edge of the field without making a full round unreasonably slow.
            if let Some(len) = self.await_frame(10_000)? {
                if (10..=MAX_FRAME).contains(&len) {
                    let mut buf = [0u8; MAX_FRAME];
                    self.read_data(&mut buf[..len])?;
                    if let Some(uid) = parse_inventory_response(&buf[..len]) {
                        // Deduplicate: a tag can answer more than once if the field
                        // flickers, and a duplicate would look like a third device.
                        if !out[..found].contains(&uid) {
                            if found == out.len() {
                                return Err(Error::Overflow);
                            }
                            out[found] = uid;
                            found += 1;
                        }
                    }
                }
            }
            // Step to the next slot with a bare EOF. Not needed after the last one.
            if slot < 15 {
                self.write_register_and_mask(reg::TX_CONFIG, TX_CONFIG_EOF_ONLY)?;
                self.clear_irqs()?;
                self.send_data(&[])?;
            }
        }
        // Leave the transmitter able to send data again.
        self.load_rf_config(0x0d, 0x8d)?;
        Ok(found)
    }
}

/// Longest frame we will send or receive. Inventory responses are 10 bytes; the margin
/// covers the longer ICODE commands this will grow into.
pub const MAX_FRAME: usize = 64;

#[cfg(test)]
mod tests {
    use super::*;

    /// A real ICODE SLIX response: flags, DSFID, then the UID least significant byte
    /// first. `e0` is the last wire byte because it is the first in display order.
    fn slix() -> [u8; 10] {
        [0x00, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x04, 0xe0]
    }

    #[test]
    fn decodes_a_tag() {
        let uid = parse_inventory_response(&slix()).expect("valid response");
        assert_eq!(uid.0, [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x04, 0xe0]);
        assert_eq!(uid.manufacturer(), 0x04, "NXP");
    }

    #[test]
    fn display_order_starts_with_e0() {
        let uid = parse_inventory_response(&slix()).unwrap();
        assert_eq!(
            uid.display_order(),
            [0xe0, 0x04, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11]
        );
    }

    #[test]
    fn rejects_short_frames() {
        let full = slix();
        for n in 0..10 {
            assert_eq!(parse_inventory_response(&full[..n]), None, "len {n}");
        }
    }

    #[test]
    fn rejects_the_error_flag() {
        let mut r = slix();
        r[0] = 0x01;
        assert_eq!(parse_inventory_response(&r), None);
    }

    /// The failure this guard exists for: a frame that is the right length and looks
    /// like data, but is offset by a byte. Without the `e0` check it would yield a
    /// stable, plausible UID — and stably identify the wrong device forever.
    #[test]
    fn rejects_a_shifted_frame() {
        let mut shifted = [0u8; 10];
        shifted[..9].copy_from_slice(&slix()[1..]);
        assert_eq!(parse_inventory_response(&shifted), None);
    }


    #[test]
    fn parses_a_printed_uid() {
        let uid = Uid::from_display_hex("E004010203040506").expect("valid");
        assert_eq!(uid.display_order(), [0xe0, 0x04, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);
        assert_eq!(uid.manufacturer(), 0x04);
    }

    #[test]
    fn parsing_ignores_separators_and_case() {
        let a = Uid::from_display_hex("E004010203040506").unwrap();
        for s in [
            "e0:04:01:02:03:04:05:06",
            "E0-04-01-02-03-04-05-06",
            "e0 04 01 02 03 04 05 06",
            "E0_04_01_02_03_04_05_06",
        ] {
            assert_eq!(Uid::from_display_hex(s), Some(a), "{s}");
        }
    }

    #[test]
    fn parsing_rejects_malformed_input() {
        for s in [
            "",                          // empty
            "E00401020304",              // too short
            "E0040102030405060708",      // too long
            "E00401020304050",           // odd digit count
            "E004010203040g06",          // not hex
            "12004010203040506",         // does not start E0
            "1004010203040506",          // wrong manufacturer prefix
        ] {
            assert_eq!(Uid::from_display_hex(s), None, "{s:?} should be rejected");
        }
    }

    /// Parsing and display must be exact inverses, or a UID configured from a log line
    /// would not match the tag it was copied from.
    #[test]
    fn parse_and_display_round_trip() {
        let uid = parse_inventory_response(&slix()).unwrap();
        let mut text = hex16(&uid.display_order());
        let lower = core::str::from_utf8(&text).unwrap();
        assert_eq!(Uid::from_display_hex(lower), Some(uid));
        text.make_ascii_uppercase();
        let upper = core::str::from_utf8(&text).unwrap();
        assert_eq!(Uid::from_display_hex(upper), Some(uid));
    }

    /// Lowercase hex of eight bytes, without `std` — this crate is `no_std` even in
    /// its tests.
    fn hex16(bytes: &[u8; 8]) -> [u8; 16] {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut out = [0u8; 16];
        for (i, b) in bytes.iter().enumerate() {
            out[i * 2] = DIGITS[(b >> 4) as usize];
            out[i * 2 + 1] = DIGITS[(b & 0x0f) as usize];
        }
        out
    }


    #[test]
    fn decodes_a_card_uid() {
        // BCC is the XOR of the four UID bytes.
        let uid = parse_anticollision(&[0xde, 0xad, 0xbe, 0xef, 0xde ^ 0xad ^ 0xbe ^ 0xef]);
        assert_eq!(uid, Some(CardUid([0xde, 0xad, 0xbe, 0xef])));
    }

    #[test]
    fn rejects_a_bad_bcc() {
        assert_eq!(parse_anticollision(&[0xde, 0xad, 0xbe, 0xef, 0x00]), None);
    }

    #[test]
    fn rejects_a_short_anticollision_frame() {
        assert_eq!(parse_anticollision(&[0xde, 0xad, 0xbe, 0xef]), None);
    }

    /// A cascade tag means the real UID is longer than what we have. Keying a device on
    /// the fragment would be keying it on something no card actually reports.
    #[test]
    fn rejects_the_cascade_tag() {
        let bcc = 0x88u8 ^ 0x01 ^ 0x02 ^ 0x03;
        assert_eq!(parse_anticollision(&[0x88, 0x01, 0x02, 0x03, bcc]), None);
    }

    #[test]
    fn parses_a_printed_card_uid() {
        assert_eq!(CardUid::from_hex("deadbeef"), Some(CardUid([0xde, 0xad, 0xbe, 0xef])));
        assert_eq!(CardUid::from_hex("DE:AD:BE:EF"), Some(CardUid([0xde, 0xad, 0xbe, 0xef])));
        for bad in ["", "deadbe", "deadbeef00", "deadbeeg"] {
            assert_eq!(CardUid::from_hex(bad), None, "{bad:?}");
        }
    }


    fn uid(n: u8) -> Uid {
        Uid([n, 0, 0, 0, 0, 0, 0x04, 0xe0])
    }

    #[test]
    fn a_new_tag_is_believed_immediately() {
        let mut set: TagSet<4> = TagSet::new(3);
        assert_eq!(set.update(&[uid(1)]), &[uid(1)]);
    }

    #[test]
    fn a_single_dropped_round_is_tolerated() {
        let mut set: TagSet<4> = TagSet::new(3);
        set.update(&[uid(1)]);
        assert_eq!(set.update(&[]), &[uid(1)], "one miss must not remove it");
        assert_eq!(set.update(&[uid(1)]), &[uid(1)]);
    }

    #[test]
    fn a_tag_goes_once_it_stays_missing() {
        let mut set: TagSet<4> = TagSet::new(3);
        set.update(&[uid(1)]);
        for _ in 0..3 {
            assert_eq!(set.update(&[]).len(), 1);
        }
        assert!(set.update(&[]).is_empty(), "gone after limit+1 misses");
    }

    /// The hardware bug this exists for: two tags present, one round sees only the
    /// other. A count-based check keeps the count at one and swaps which device is
    /// present, marking an untouched device as taken.
    #[test]
    fn a_round_that_sees_the_other_tag_does_not_swap_them() {
        let mut set: TagSet<4> = TagSet::new(3);
        set.update(&[uid(1), uid(2)]);
        let after = set.update(&[uid(2)]);
        assert!(after.contains(&uid(1)), "the missed tag must survive one round");
        assert!(after.contains(&uid(2)));
        assert_eq!(after.len(), 2);
    }

    #[test]
    fn a_real_removal_still_registers_with_the_other_present() {
        let mut set: TagSet<4> = TagSet::new(3);
        set.update(&[uid(1), uid(2)]);
        for _ in 0..4 {
            set.update(&[uid(2)]);
        }
        assert_eq!(set.tags(), &[uid(2)]);
    }

    #[test]
    fn removing_the_first_of_several_keeps_the_rest() {
        let mut set: TagSet<4> = TagSet::new(0);
        set.update(&[uid(1), uid(2), uid(3)]);
        assert_eq!(set.update(&[uid(2), uid(3)]).len(), 2);
        assert!(set.contains(&uid(2)) && set.contains(&uid(3)));
    }

    #[test]
    fn distinct_tags_compare_unequal() {
        let a = parse_inventory_response(&slix()).unwrap();
        let mut other = slix();
        other[2] = 0x99;
        let b = parse_inventory_response(&other).unwrap();
        assert_ne!(a, b);
    }
}
