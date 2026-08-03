//! The onboard 1.54" 200x200 SSD1681 e-paper.
//!
//! Two things about this board that are not obvious and cost an evening each if missed:
//!
//! - **IO6 is `EPD3V3_EN`**, a power-rail enable for the panel. It must be driven high
//!   or the display is simply dead — no error, no signal, nothing.
//! - The panel's SPI pins (IO8–IO13) are **not** on the expansion header. They are
//!   dedicated, so the panel gets its own SPI peripheral and the header stays free.

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_hal_bus::spi::ExclusiveDevice;
use epd_waveshare::color::Color;
use epd_waveshare::epd1in54_v2::Epd1in54;
use epd_waveshare::graphics::Display as EpdDisplay;
use epd_waveshare::prelude::*;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::spi::Mode;
use esp_hal::time::Rate;
use esp_println::println;

pub const WIDTH: u32 = 200;
pub const HEIGHT: u32 = 200;

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

pub struct Panel<'d> {
    spi: PanelSpi<'d>,
    epd: Epd1in54<PanelSpi<'d>, Input<'d>, Output<'d>, Output<'d>, Delay>,
    delay: Delay,
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
        let power = Output::new(pins.power_en, Level::High, OutputConfig::default());
        delay.delay_millis(10);

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

        Self { spi, epd, delay, _power: power }
    }

    /// Push a framebuffer and refresh, then sleep the panel.
    ///
    /// Sleeping between updates matters on e-paper: leaving the controller awake with a
    /// charged panel is what produces long-term ghosting.
    pub fn present(&mut self, fb: &Framebuffer) {
        println!("panel: sending {} bytes", fb.buffer().len());
        let _ = self.epd.update_frame(&mut self.spi, fb.buffer(), &mut self.delay);
        println!("panel: frame sent, refreshing");
        let _ = self.epd.display_frame(&mut self.spi, &mut self.delay);
        println!("panel: refreshed");
        // NOTE: `epd.sleep()` hangs here — its opening `wait_until_idle` never returns
        // even though `display_frame` has already completed and the image is on the
        // panel. Left out until that is understood; skipping deep sleep costs some idle
        // current and, over time, risks ghosting, so this is not the final answer.
    }
}
