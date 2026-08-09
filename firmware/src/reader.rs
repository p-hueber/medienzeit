//! The PN5180 on SPI3.
//!
//! SPI2 belongs to the e-paper and its pins are not on the header, so the reader gets
//! its own peripheral routed through the GPIO matrix to the expansion header.
//!
//! Wiring, in the silkscreen names printed on the board:
//!
//! | PN5180 | wire | header | GPIO |
//! |---|---|---|---|
//! | 5V | | `VSYS` | — (measured 5 V on USB power) |
//! | RST | | `3V3` | — (tied high; no GPIO left for a real reset) |
//!
//! During bring-up the module is instead fed 3.3 V from an external supply on its pin 2,
//! bypassing the onboard LDO, with RST tied to that same rail and grounds commoned. That
//! is enough for everything up to and including `identify`, which only reads EEPROM over
//! SPI — the RF stage is never energised. Move to `VSYS` before measuring read range,
//! because field strength is exactly what the 5 V rail buys.
//! | NSS | white | `TXD` | IO43 |
//! | MOSI | grey | `GP2` | IO2 |
//! | MISO | purple | `RXD` | IO44 |
//! | SCK | blue | `GP1` | IO1 |
//! | RST | green | `GP3` | IO3 |
//! | BUSY | | *not connected* | — |
//! | GND | yellow | `GND` | — |
//!
//! **This module has no pull-up on RESET_N** — measured 0 V with pin 3 floating — so it
//! cannot simply be left open, and it held the chip in permanent reset until the green
//! wire moved onto it from BUSY. Only five header GPIO exist for six module signals, so
//! one of RST and BUSY has to give. RST wins for now because a chip in reset cannot
//! answer at all, whereas BUSY degrades to a fixed settling delay. The proper fix is to
//! tie RST to the `3V3` header pin and give BUSY back its GPIO.
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
//! Pin 2 (3.3V) is deliberately unconnected — the module regulates 5 V down itself, and
//! feeding both rails fights the onboard LDO. Running from 5 V rather than bypassing it
//! is what gives the RF stage its full field strength, and therefore the range figure
//! M0 is meant to measure.

use embedded_hal_bus::spi::ExclusiveDevice;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Input, Level, Output, OutputConfig};
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::spi::Mode;
use esp_hal::time::Rate;
use esp_println::{print, println};
use medienzeit_pn5180::Pn5180;

type ReaderSpi<'d> = ExclusiveDevice<Spi<'d, esp_hal::Blocking>, Output<'d>, Delay>;
/// `Input` only names the (unused) BUSY type parameter; no BUSY pin is wired.
pub type Reader<'d> = Pn5180<ReaderSpi<'d>, Input<'d>, Delay>;

pub struct Pins {
    pub sck: esp_hal::peripherals::GPIO1<'static>,
    pub mosi: esp_hal::peripherals::GPIO2<'static>,
    /// RESET_N, active low. IO3 is a strapping pin (JTAG select) latched at reset before
    /// firmware runs; we use USB-Serial-JTAG, so driving it afterwards is harmless.
    pub rst: esp_hal::peripherals::GPIO3<'static>,
    pub nss: esp_hal::peripherals::GPIO43<'static>,
    pub miso: esp_hal::peripherals::GPIO44<'static>,
}

pub fn new(spi: esp_hal::peripherals::SPI3<'static>, pins: Pins) -> Reader<'static> {
    let mut delay = Delay::new();

    // The module has no pull-up on RESET_N — measured 0 V floating — so without this the
    // chip sits in reset forever, answering every command with zeros and never raising
    // BUSY. Pulse it low, then hold high for the rest of the run.
    let mut rst = Output::new(pins.rst, Level::High, OutputConfig::default());
    delay.delay_millis(1);
    rst.set_low();
    delay.delay_millis(1);
    rst.set_high();
    // The datasheet wants a few hundred microseconds before the first command; 10 ms is
    // free at boot and removes the question.
    delay.delay_millis(10);
    // Held high for the lifetime of the program. Dropping this would release the pin and
    // put the chip straight back into reset.
    core::mem::forget(rst);

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
    Pn5180::without_busy(dev, delay)
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

    // Without BUSY the only evidence that a read landed on the right beat is that it
    // lands the same way twice. Identical repeats do not prove correct framing, but a
    // mismatch proves it is wrong — and that is the failure worth catching, because a
    // response read one beat early still looks like a plausible version number.
    // The decisive framing test. Repeats agreeing only shows we are consistently wrong
    // or consistently right; a value we chose ourselves surviving a write/read round
    // trip can only happen if both directions are framed correctly. TIMER1_RELOAD is a
    // 24-bit scratch register that nothing else here uses.
    // Measure the writable width rather than assuming it: all-ones reads back as the
    // mask of bits that actually exist, and a pattern inside that mask must then survive
    // exactly. Anything else — a shifted or garbled value — is a framing fault.
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
