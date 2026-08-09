//! The PN5180 on SPI3.
//!
//! SPI2 belongs to the e-paper and its pins are not on the header, so the reader gets
//! its own peripheral routed through the GPIO matrix to the expansion header.
//!
//! Wiring, in the silkscreen names printed on the board:
//!
//! | PN5180 | wire | header | GPIO |
//! |---|---|---|---|
//! | 5V (pin 1) | | `VSYS` | — (measured 5 V on USB power) |
//! | 3.3V (pin 2) | | `3V3` | — |
//! | RST (pin 3) | | bridged to pin 2 | — (tied high; no GPIO left for a real reset) |
//! | NSS | white | `TXD` | IO43 |
//! | MOSI | grey | `GP2` | IO2 |
//! | MISO | purple | `RXD` | IO44 |
//! | SCK | blue | `GP1` | IO1 |
//! | BUSY | green | `GP3` | IO3 |
//! | GND | yellow | `GND` | — |
//!
//! **This module has no pull-up on RESET_N** — measured 0 V with pin 3 floating — so RST
//! cannot be left open; it held the chip in permanent reset, answering every command
//! with zeros, until it was tied high. Bridging it to the module's own 3.3 V input
//! (pin 2 → header `3V3`) keeps all five header GPIO available for signals, which is
//! what lets BUSY stay wired. Do not "free" that bridge to save a jumper: the fallback
//! is a fixed settling delay, and RF transactions take variable time.
//!
//! **Read the header off the PCB, never off the sticker on the case.** That sticker's
//! block diagram is printed 180° out, and every pin identified from it during bring-up
//! was wrong — which put the module's supply and ground onto `GP20`/`GP19`, the native
//! USB D+/D−, so the board would not enumerate whenever the module was attached. The
//! reading that made the mislabelling look confirmed was 3.27 V between those two pins:
//! that is not VSYS, it is an enumerated full-speed device holding D+ above D− through
//! its 1.5 kΩ pull-up. Corrected by continuity: real `GND` buzzes to the USB-C shell.
//!
//! Two beliefs from that period are therefore **false** and should not be reinstated:
//! that `TXD` cannot be loaded (it can; NSS lives there), and that BUSY has to be
//! sacrificed for want of a pin (it does not; there are exactly five usable GPIO for
//! five signals). A third symptom explains itself the same way: the phantom I²C devices
//! that appeared around 0x5b–0x60 while the RTC vanished were SPI traffic landing on
//! `SDA`/`SCL`.
//!
//! **This breakout has no onboard regulator**, so both rails are required: 5 V feeds the
//! RF transmitter, 3.3 V the digital core and the SPI level shifters. Established by
//! measurement — 5 V present on pin 1 left pin 2 at nothing, where an LDO would have put
//! 3.3 V there. With only 3.3 V applied the chip answers but the shifters stay dark; with
//! only 5 V nothing works at all.

use embedded_hal_bus::spi::ExclusiveDevice;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig};
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::spi::Mode;
use esp_hal::time::Rate;
use esp_println::{print, println};
use medienzeit_pn5180::{Pn5180, Uid};

/// Most tags we expect in the field at once. Two devices, plus slack so a stray tag
/// shows up in the log rather than being silently dropped.
pub const MAX_TAGS: usize = 4;

type ReaderSpi<'d> = ExclusiveDevice<Spi<'d, esp_hal::Blocking>, Output<'d>, Delay>;
pub type Reader<'d> = Pn5180<ReaderSpi<'d>, Input<'d>, Delay>;

pub struct Pins {
    pub sck: esp_hal::peripherals::GPIO1<'static>,
    pub mosi: esp_hal::peripherals::GPIO2<'static>,
    /// IO3 is a strapping pin (JTAG select), latched at reset before firmware runs. We
    /// use USB-Serial-JTAG, so BUSY's level here is cosmetic at worst.
    pub busy: esp_hal::peripherals::GPIO3<'static>,
    pub nss: esp_hal::peripherals::GPIO43<'static>,
    pub miso: esp_hal::peripherals::GPIO44<'static>,
}

pub fn new(spi: esp_hal::peripherals::SPI3<'static>, pins: Pins) -> Reader<'static> {
    let delay = Delay::new();

    let nss = Output::new(pins.nss, Level::High, OutputConfig::default());

    let bus = Spi::new(
        spi,
        // The PN5180 tops out around 7 MHz; 2 MHz is plenty for inventory and is kind
        // to dupont wiring, which is what this is running on during bring-up.
        SpiConfig::default()
            .with_frequency(Rate::from_mhz(2))
            .with_mode(Mode::_0),
    )
    .expect("spi3 config")
    .with_sck(pins.sck)
    .with_mosi(pins.mosi)
    .with_miso(pins.miso);

    let dev = ExclusiveDevice::new(bus, nss, delay).expect("spi device");
    // No pull configured: the PN5180 drives BUSY actively in both directions, and a
    // pull-up here would make an unpowered module look permanently busy rather than
    // permanently idle — a timeout is a clearer failure than a hang.
    let busy = Input::new(pins.busy, InputConfig::default());
    Pn5180::new(dev, busy, delay)
}

/// Ask the chip who it is. Proves wiring, SPI framing and the BUSY handshake before
/// anything touches RF.
///
/// Prints the raw bytes as well as the decoded values, because during bring-up the
/// interesting cases are the ones decoding hides: all-`ff` is an absent or unpowered
/// chip, all-`00` is MISO stuck low, and a plausible-looking value shifted by one byte
/// is a BUSY handshake that returned too early.
pub fn identify(reader: &mut Reader<'static>) {
    println!("reader: BUSY idle level = {:?}", reader.busy_level());
    for (what, addr, len) in [
        ("product ", medienzeit_pn5180::eeprom::PRODUCT_VERSION, 2),
        ("firmware", medienzeit_pn5180::eeprom::FIRMWARE_VERSION, 2),
        ("eeprom  ", medienzeit_pn5180::eeprom::EEPROM_VERSION, 2),
        ("die id  ", medienzeit_pn5180::eeprom::DIE_IDENTIFIER, 16),
    ] {
        let mut buf = [0u8; 16];
        let buf = &mut buf[..len];
        print!("reader: {what} <- 07 {addr:02x} {len:02x}  ->");
        match reader.read_eeprom(addr, buf) {
            Ok(()) => {
                for b in buf.iter() {
                    print!(" {b:02x}");
                }
                // Versions are stored minor-first; the die id is just 16 opaque bytes.
                if len == 2 {
                    print!("   = v{}.{}", buf[1], buf[0]);
                }
                println!();
            }
            Err(e) => println!(" {e:?}"),
        }
    }
    match (reader.busy_level().is_some(), reader.saw_busy_high()) {
        (false, _) => println!("reader: BUSY not wired — timing is a fixed delay, not a handshake"),
        (true, true) => println!("reader: BUSY asserted — the chip is acknowledging"),
        (true, false) => println!("reader: BUSY never went high — no acknowledgement"),
    }

    // The decisive framing test: a value we chose ourselves surviving a write/read round
    // trip can only happen if both directions are framed correctly, where agreeing
    // repeats would only show we are consistently wrong or consistently right. Measure
    // the writable width rather than assume it — all-ones reads back as the mask of bits
    // that actually exist (20 here, not the 24 the name suggests), and a pattern inside
    // that mask must then survive exactly. TIMER1_RELOAD is scratch; nothing else uses it.
    const TIMER1_RELOAD: u8 = 0x0c;
    let probe = |reader: &mut Reader<'static>, v: u32| {
        reader
            .write_register(TIMER1_RELOAD, v)
            .and_then(|()| reader.read_register(TIMER1_RELOAD))
    };
    match probe(reader, 0xffff_ffff) {
        Ok(mask) => {
            println!("reader: register writable mask {mask:#010x} ({} bits)", mask.count_ones());
            let pattern = 0xa5c3_5a5a & mask;
            match (probe(reader, 0), probe(reader, pattern)) {
                (Ok(0), Ok(v)) if v == pattern => {
                    println!("reader: register round trip ok ({pattern:#010x}) — framing proven")
                }
                (z, p) => println!("reader: round trip suspect, zero={z:?} pattern={p:?}"),
            }
        }
        Err(e) => println!("reader: register probe failed ({e:?})"),
    }

    let mut a = [0u8; 16];
    let mut b = [0u8; 16];
    let ok = reader.die_id(&mut a).is_ok() && reader.die_id(&mut b).is_ok();
    if !ok {
        println!("reader: repeat die id read failed");
    } else if a == b {
        println!("reader: die id stable across repeats");
    } else {
        print!("reader: die id UNSTABLE, second read");
        for x in b {
            print!(" {x:02x}");
        }
        println!();
    }
}

/// Configure the RF front end and switch the field on.
///
/// Reports `RF_STATUS` either side of each step. Which bit means "transmitter on" is
/// then a measurement rather than a guess at the datasheet's bit map — and if no bit
/// changes across `RF_ON`, the field never came up and no tag can possibly answer.
pub fn start_rf(reader: &mut Reader<'static>) {
    // Drop the field first, so TX_RFON_IRQ afterwards means something. RF_ON on a field
    // that is already up raises no interrupt, which reads as a dead transmitter.
    let _ = reader.field_off();
    reader.delay_ms(20);
    let before = reader.read_register(medienzeit_pn5180::reg::RF_STATUS);
    if let Err(e) = reader.load_rf_config(0x0d, 0x8d) {
        println!("reader: load_rf_config failed ({e:?})");
        return;
    }
    let configured = reader.read_register(medienzeit_pn5180::reg::RF_STATUS);
    // Ask the chip directly instead of interpreting RF_STATUS: TX_RFON_IRQ is raised by
    // the transmitter actually coming up, so clearing first makes the reading after
    // unambiguous.
    let _ = reader.clear_all_irqs();
    if let Err(e) = reader.field_on() {
        println!("reader: field_on failed ({e:?})");
        return;
    }
    // The field takes time to ramp, and TX_RFON_IRQ is raised when it has. Reading
    // IRQ_STATUS immediately reports zero and looks exactly like a dead transmitter —
    // which is precisely the wrong conclusion this delay exists to prevent.
    reader.delay_ms(20);
    let after = reader.read_register(medienzeit_pn5180::reg::RF_STATUS);
    match reader.read_register(medienzeit_pn5180::reg::IRQ_STATUS) {
        // Bit 9 is TX_RFON_IRQ on this part — established by probing, not from the
        // datasheet's table.
        Ok(irq) => println!(
            "reader: irq after RF_ON = {irq:#010x} -> field {}",
            if irq & (1 << 9) != 0 { "CAME UP" } else { "did NOT come up" }
        ),
        Err(e) => println!("reader: irq read failed ({e:?})"),
    }
    match (before, configured, after) {
        (Ok(b), Ok(c), Ok(a)) => {
            println!("reader: rf_status idle={b:#010x} configured={c:#010x} field_on={a:#010x}");
            let changed = c ^ a;
            if changed == 0 {
                println!("reader: RF_ON changed nothing — the transmitter did not come up");
            } else {
                println!("reader: RF_ON toggled {changed:#010x}");
            }
        }
        _ => println!("reader: rf_status read failed"),
    }
}

/// Print the antenna's AGC reading so field loading can be watched by hand.
///
/// A tag entering the field detunes and loads the antenna, which moves this number. It
/// is the one check that works below the protocol: if AGC does not budge when a tag
/// touches the coil, the two are not coupling and no amount of protocol work will help.
pub async fn agc_watch(reader: &mut Reader<'static>, _secs: u32) {
    // Alternating narrated windows, so the log says what the coil was supposed to be
    // doing at each sample. Comparing a baseline against a known-loaded period is the
    // whole point; an unlabelled stream of numbers cannot be read after the fact.
    const WINDOW_S: u32 = 8;
    let mut bare = (u32::MAX, 0u32);
    let mut loaded = (u32::MAX, 0u32);
    for round in 0..4 {
        let tag_on = round % 2 == 1;
        println!(
            "reader: >>> {} for {WINDOW_S}s",
            if tag_on { "TAG ON the coil" } else { "TAG OFF, nothing near" }
        );
        for _ in 0..WINDOW_S * 2 {
            if let Ok(rf) = reader.read_register(medienzeit_pn5180::reg::RF_STATUS) {
                let agc = rf & 0x3ff;
                let acc = if tag_on { &mut loaded } else { &mut bare };
                acc.0 = acc.0.min(agc);
                acc.1 = acc.1.max(agc);
            }
            embassy_time::Timer::after(embassy_time::Duration::from_millis(500)).await;
        }
    }
    println!("reader: agc bare {}..{}  loaded {}..{}", bare.0, bare.1, loaded.0, loaded.1);
    if loaded.0 == u32::MAX || bare.0 == u32::MAX {
        println!("reader: agc never read");
    } else if loaded.1 > bare.1 + 5 || loaded.0 + 5 < bare.0 {
        println!("reader: the tag loads the antenna — they are coupling");
    } else {
        println!("reader: no measurable loading — field and tag are not coupling");
    }
}

/// Tracks which tags are present, and reports only when that changes.
///
/// Printing every poll would bury the transitions that matter in a wall of identical
/// lines, and the transitions are the whole signal during bring-up.
#[derive(Default)]
pub struct Scan {
    tags: [Uid; MAX_TAGS],
    n: usize,
    /// Consecutive polls in which a previously-seen tag did not answer.
    misses: u32,
}

/// Misses tolerated before believing a tag has really gone.
///
/// A single dropped read is routine — the tag is passive and at the edge of the field —
/// and treating one as a departure would start the clock every few seconds while the
/// device sits untouched.
const MISS_LIMIT: u32 = 3;

impl Scan {
    /// Poll once. Returns the current tag set, or `None` if the reader errored.
    pub fn poll(&mut self, reader: &mut Reader<'static>) -> Option<&[Uid]> {
        let mut found = [Uid::default(); MAX_TAGS];
        let n = match reader.inventory(&mut found) {
            Ok(n) => n,
            Err(e) => {
                println!("reader: inventory failed ({e:?})");
                return None;
            }
        };

        let same = n == self.n && found[..n].iter().all(|u| self.tags[..self.n].contains(u));
        if same {
            self.misses = 0;
        } else if n < self.n && self.misses < MISS_LIMIT {
            // Fewer tags than last time: hold the old set until it repeats, so one
            // glitchy read cannot look like a device being picked up.
            self.misses += 1;
            return Some(&self.tags[..self.n]);
        } else {
            self.misses = 0;
            self.tags = found;
            self.n = n;
            print!("reader: {n} tag(s)");
            for uid in &found[..n] {
                print!(" ");
                for b in uid.display_order() {
                    print!("{b:02x}");
                }
            }
            println!();
        }
        Some(&self.tags[..self.n])
    }
}

/// Poll the field for a while, reporting changes, before the network comes up.
///
/// Bring-up only, and deliberately ahead of Wi-Fi: the control loop that normally polls
/// the reader sits behind DHCP and SNTP, so a flaky association would otherwise mean the
/// antenna is never even asked. Reader faults should not be diagnosed through the
/// network's availability.
pub async fn bringup_scan(reader: &mut Reader<'static>, scan: &mut Scan, secs: u32) {
    println!("reader: scanning for {secs}s — present a tag");
    for i in 0..secs {
        scan.poll(reader);
        // Periodic register dump: with no tag answering, these three say whether the
        // transmitter is even reaching the right state, and whether anything was heard.
        // Alternate the data rate: some ICODE tags answer far more reliably at the low
        // rate, and trying only one would leave that untested.
        let high = i % 2 == 0;
        let mut raw = [0u8; 32];
        match reader.inventory_single_raw(&mut raw, high) {
            Ok(0) => {}
            Ok(n) => {
                print!("reader: single-slot ({}) rx {n}:", if high { "high" } else { "low" });
                for b in &raw[..n] {
                    print!(" {b:02x}");
                }
                println!();
            }
            Err(e) => println!("reader: single-slot failed ({e:?})"),
        }
        if i % 10 == 0 {
            match reader.rf_debug() {
                Ok((rf, irq, rx)) => println!(
                    "reader: rf={rf:#010x} (state {}) irq={irq:#010x} rx={rx:#010x}",
                    medienzeit_pn5180::transceive_state(rf)
                ),
                Err(e) => println!("reader: rf_debug failed ({e:?})"),
            }
        }
        embassy_time::Timer::after(embassy_time::Duration::from_secs(1)).await;
    }
    println!("reader: scan window over");
}

/// Try to bring the transmitter up several different ways, reporting what each does.
///
/// The question this answers is narrow: does `RF_ON` ever raise an interrupt? A bare
/// `RF_ON` with no configuration loaded separates "our RF config index is wrong" from
/// "the transmitter is dead", and sweeping a few indices covers the possibility that
/// this module's EEPROM lays them out differently from the datasheet's table.
pub fn rf_probe(reader: &mut Reader<'static>) {
    // `None` means skip LOAD_RF_CONFIG entirely.
    let candidates: [(Option<(u8, u8)>, &str); 6] = [
        (None, "no config at all"),
        (Some((0x0d, 0x8d)), "ISO 15693 ASK100 26k"),
        (Some((0x0c, 0x8c)), "neighbouring index"),
        (Some((0x0e, 0x8e)), "neighbouring index"),
        (Some((0x00, 0x80)), "ISO 14443A 106k"),
        (Some((0x0d, 0x8c)), "mismatched tx/rx"),
    ];

    for (cfg, what) in candidates {
        let _ = reader.field_off();
        reader.delay_ms(20);
        let loaded = match cfg {
            None => Ok(()),
            Some((tx, rx)) => reader.load_rf_config(tx, rx),
        };
        let _ = reader.clear_all_irqs();
        let on = reader.field_on();
        reader.delay_ms(20);
        let irq = reader.read_register(medienzeit_pn5180::reg::IRQ_STATUS);
        let rf = reader.read_register(medienzeit_pn5180::reg::RF_STATUS);
        match (loaded, on, irq, rf) {
            (Ok(()), Ok(()), Ok(i), Ok(r)) => println!(
                "reader: probe {what:22} irq={i:#010x} rf={r:#010x} {}",
                if i != 0 { "<-- SOMETHING HAPPENED" } else { "" }
            ),
            (l, o, i, r) => {
                println!("reader: probe {what:22} load={l:?} on={o:?} irq={i:?} rf={r:?}")
            }
        }
    }
    let _ = reader.field_off();
    println!("reader: probe done");
}

/// Control experiment: does *any* card answer, in any protocol?
///
/// Runs before the ISO 15693 work so a silent inventory can be attributed correctly.
/// A card that answers here is a 14443A part and will never appear in an ISO 15693
/// inventory however well the reader works.
pub fn probe_other_protocol(reader: &mut Reader<'static>) {
    match reader.probe_iso14443a() {
        Ok(Some(atqa)) => println!(
            "reader: ISO 14443A card answered, ATQA {:02x}{:02x} — antenna and receive path work",
            atqa[0], atqa[1]
        ),
        Ok(None) => println!("reader: no ISO 14443A answer either"),
        Err(e) => println!("reader: 14443A probe failed ({e:?})"),
    }
}
