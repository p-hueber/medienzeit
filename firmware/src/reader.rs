//! The PN5180 on SPI3.
//!
//! SPI2 belongs to the e-paper and its pins are not on the header, so the reader gets
//! its own peripheral routed through the GPIO matrix to the expansion header.
//!
//! Wiring, in the silkscreen names printed on the board:
//!
//! | PN5180 | header | GPIO |
//! |---|---|---|
//! | 3.3V | `3V3` | — |
//! | RST | `VSYS` | — (tied high; no GPIO left for a real reset) |
//! | NSS | `GP2` | IO2 |
//! | MOSI | `GP3` | IO3 |
//! | SCK | `GP1` | IO1 |
//! | MISO | `RXD` | IO44 |
//! | BUSY | *not connected* | — |
//! | GND | `GND` | — |
//!
//! **Nothing may be connected to `TXD`.** IO43 is UART0 TX and the ROM bootloader
//! drives it at reset, so *any* load there stops the chip booting: the board fails USB
//! enumeration outright and looks dead. Established by bisection — one wire from NSS
//! to `TXD` reproduced it, the same wire on `GP1` was fine, and it holds regardless of
//! signal direction or whether the module is powered. `RXD` is fine, because the ROM
//! only listens on it.
//!
//! That leaves four usable header pins for five signals, so **BUSY is sacrificed** and
//! the driver falls back to a fixed settling delay. It is the only line the protocol
//! can manage without, and it is the first suspect if reads turn out flaky.
//!
//! Pin 1 (5V) is deliberately unconnected: this board has no 5 V rail on the header,
//! so the module runs from 3.3 V with the onboard LDO bypassed. That costs RF field
//! strength and therefore range, which is the thing to measure first.

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
    pub mosi: esp_hal::peripherals::GPIO3<'static>,
    pub nss: esp_hal::peripherals::GPIO2<'static>,
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
    Pn5180::without_busy(dev, delay)
}

/// Ask the chip who it is. Proves wiring, SPI framing and the BUSY handshake before
/// anything touches RF.
pub fn identify(reader: &mut Reader<'static>) {
    match reader.product_version() {
        Ok((maj, min)) => println!("reader: product version {maj}.{min}"),
        Err(e) => {
            println!("reader: no product version ({e:?})");
            return;
        }
    }
    match reader.firmware_version() {
        Ok((maj, min)) => println!("reader: firmware version {maj}.{min}"),
        Err(e) => println!("reader: no firmware version ({e:?})"),
    }
    match reader.eeprom_version() {
        Ok((maj, min)) => println!("reader: eeprom version {maj}.{min}"),
        Err(e) => println!("reader: no eeprom version ({e:?})"),
    }

    let mut die = [0u8; 16];
    match reader.die_id(&mut die) {
        Ok(()) => {
            print!("reader: die id ");
            for b in die {
                print!("{b:02x}");
            }
            println!();
        }
        Err(e) => println!("reader: no die id ({e:?})"),
    }
}
