//! The onboard 1.54" 200x200 SSD1681 e-paper.
//!
//! Two things about this board that are not obvious and cost an evening each if missed:
//!
//! - **IO6 is `EPD3V3_EN`, and it is ACTIVE LOW.** Drive it *low* to power the panel.
//!   Driving it high leaves the display unpowered, and nothing reports an error: SPI
//!   writes vanish, BUSY never asserts, so every `wait_until_idle` returns instantly and
//!   the driver cheerfully claims the frame was displayed. Confirmed against Waveshare's
//!   own `board_power_bsp.cpp`, where `POWEER_EPD_ON()` sets the level to 0.
//! - The panel's SPI pins (IO8–IO13) are **not** on the expansion header. They are
//!   dedicated, so the panel gets its own SPI peripheral and the header stays free.

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_hal_bus::spi::ExclusiveDevice;
use epd_waveshare::color::Color;
use epd_waveshare::epd1in54_v2::Epd1in54;
use epd_waveshare::prelude::RefreshLut;
use epd_waveshare::graphics::{Display as EpdDisplay, DisplayRotation};
use epd_waveshare::prelude::*;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::spi::Mode;
use esp_hal::time::Rate;
use esp_println::println;

pub const WIDTH: u32 = 200;
pub const HEIGHT: u32 = 200;

/// How the drawn image sits on the glass.
///
/// The panel is square, so this changes nothing about the layout — it only decides which
/// physical edge is "up". Driven by where the ribbon and connectors end up, which is a
/// property of the installation rather than of the UI.
pub const ROTATION: DisplayRotation = DisplayRotation::Rotate90;

/// A framebuffer oriented for the panel as mounted.
///
/// Always construct through this: a plain `Framebuffer::default()` is unrotated, and the
/// mistake shows up as a picture that is merely sideways rather than as an error.
pub fn framebuffer() -> Framebuffer {
    let mut fb = Framebuffer::default();
    fb.set_rotation(ROTATION);
    fb
}

/// Framebuffer matching the panel geometry.
pub type Framebuffer =
    EpdDisplay<WIDTH, HEIGHT, false, { epd_waveshare::buffer_len(WIDTH as usize, HEIGHT as usize) }, Color>;

/// Lets `medienzeit-ui` — which draws in [`BinaryColor`] so the host simulator and the
/// panel share one code path — write into a buffer that speaks e-paper `Color`.
///
/// `BinaryColor::On` means ink, which is `Color::Black`. Getting this backwards
/// produces a perfectly readable inverted screen, so it is worth stating plainly.
pub struct InkTarget<'a>(pub &'a mut Framebuffer);

impl DrawTarget for InkTarget<'_> {
    type Color = BinaryColor;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        self.0
            .draw_iter(pixels.into_iter().map(|Pixel(p, c)| {
                Pixel(p, if c.is_on() { Color::Black } else { Color::White })
            }))
            .ok();
        Ok(())
    }
}

impl OriginDimensions for InkTarget<'_> {
    fn size(&self) -> Size {
        Size::new(WIDTH, HEIGHT)
    }
}

type PanelSpi<'d> = ExclusiveDevice<Spi<'d, esp_hal::Blocking>, Output<'d>, Delay>;

/// How the panel should be driven for one update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    /// The full waveform: inverts the panel a few times, takes ~2 s, and is visibly
    /// disruptive. Clears accumulated ghosting.
    Full,
    /// The quick waveform: no inversion flashing, far faster. Ghosting builds up over
    /// many consecutive quick updates, so it needs a periodic full refresh to clear.
    Quick,
}

impl From<Refresh> for RefreshLut {
    fn from(r: Refresh) -> Self {
        match r {
            Refresh::Full => RefreshLut::Full,
            Refresh::Quick => RefreshLut::Quick,
        }
    }
}

pub struct Panel<'d> {
    spi: PanelSpi<'d>,
    epd: Epd1in54<PanelSpi<'d>, Input<'d>, Output<'d>, Output<'d>, Delay>,
    delay: Delay,
    /// Deep sleep ignores commands until the controller is woken, so this has to be
    /// tracked: updating a sleeping panel silently does nothing.
    asleep: bool,
    /// Which waveform is currently loaded, to skip redundant LUT uploads.
    lut: Option<Refresh>,
    /// Held so the panel's 3V3 rail stays enabled for the lifetime of the driver.
    _power: Output<'d>,
}

pub struct Pins {
    pub power_en: esp_hal::peripherals::GPIO6<'static>,
    pub busy: esp_hal::peripherals::GPIO8<'static>,
    pub rst: esp_hal::peripherals::GPIO9<'static>,
    pub dc: esp_hal::peripherals::GPIO10<'static>,
    pub cs: esp_hal::peripherals::GPIO11<'static>,
    pub sclk: esp_hal::peripherals::GPIO12<'static>,
    pub mosi: esp_hal::peripherals::GPIO13<'static>,
}

impl Panel<'static> {
    pub fn new(spi_periph: esp_hal::peripherals::SPI2<'static>, pins: Pins) -> Self {
        let mut delay = Delay::new();

        // Power the panel first and let its rail settle before touching SPI.
        // Active low: Level::Low is ON.
        let power = Output::new(pins.power_en, Level::Low, OutputConfig::default());
        delay.delay_millis(50);

        let cs = Output::new(pins.cs, Level::High, OutputConfig::default());
        let dc = Output::new(pins.dc, Level::Low, OutputConfig::default());
        let rst = Output::new(pins.rst, Level::High, OutputConfig::default());
        let busy = Input::new(pins.busy, InputConfig::default().with_pull(Pull::None));

        let bus = Spi::new(
            spi_periph,
            SpiConfig::default()
                .with_frequency(Rate::from_mhz(4))
                .with_mode(Mode::_0),
        )
        .unwrap()
        .with_sck(pins.sclk)
        .with_mosi(pins.mosi);

        println!("panel: BUSY before init = {}", if busy.is_high() { "HIGH" } else { "low" });

        let mut spi = ExclusiveDevice::new(bus, cs, delay).unwrap();
        let epd = Epd1in54::new(&mut spi, busy, dc, rst, &mut delay, None)
            .expect("e-paper init failed — is IO6 (EPD3V3_EN) high?");
        println!("panel: init ok");

        Self { spi, epd, delay, asleep: false, lut: None, _power: power }
    }

    /// Push a framebuffer and refresh, then put the controller back to sleep.
    ///
    /// Waking is not optional: the controller ignores commands in deep sleep, so
    /// without this the second update of the session would silently do nothing while
    /// every call still reported success.
    pub fn present(&mut self, fb: &Framebuffer, mode: Refresh) {
        if self.asleep {
            let _ = self.epd.wake_up(&mut self.spi, &mut self.delay);
            self.asleep = false;
            // Waking re-initialises the controller, so any loaded waveform is gone.
            self.lut = None;
        }

        if self.lut != Some(mode) {
            let _ = self
                .epd
                .set_lut(&mut self.spi, &mut self.delay, Some(mode.into()));
            self.lut = Some(mode);
        }

        let _ = self.epd.update_frame(&mut self.spi, fb.buffer(), &mut self.delay);
        let _ = self.epd.display_frame(&mut self.spi, &mut self.delay);
        let _ = self.epd.sleep(&mut self.spi, &mut self.delay);
        self.asleep = true;
    }
}
