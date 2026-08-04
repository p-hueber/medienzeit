//! The five-minutes-left chime, through the onboard ES8311 codec and NS4150B amp.
//!
//! The board has a real speaker rather than a buzzer, so a tone means configuring an
//! audio codec over I²C and feeding it I²S samples. Worth it: the alternative was an
//! extra part, and the amplifier is already there.
//!
//! Pin map from Waveshare's `07_Audio_out` example: MCLK 14, BCK 15, LRCK 38,
//! DOUT 45, DIN 16, PA_CTRL 46. IO42 gates the whole audio rail and is enabled
//! elsewhere at boot, because the I²C devices sit behind it too.
//!
//! **The amplifier is only enabled while a tone is playing.** Leaving it on idles
//! current through the speaker and picks up hiss, and a chime that becomes background
//! noise stops being a signal.

use es8311::{ClockConfig, Es8311, Resolution};
use esp_hal::delay::Delay;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::i2c::master::I2c;
use esp_hal::i2s::master::{Channels, Config as I2sConfig, DataFormat, I2s, I2sTx, UnitConfig};
use esp_hal::time::Rate;
use esp_hal::Blocking;
use esp_println::println;

const SAMPLE_RATE: u32 = 24_000;
/// The codec derives its internal clocks from MCLK; 256x is the usual multiple.
const MCLK_MULTIPLE: u32 = 256;
const MCLK_HZ: u32 = SAMPLE_RATE * MCLK_MULTIPLE;

/// 0-100. Loud enough to hear across a room, not loud enough to startle.
const VOLUME: u8 = 60;

/// Hard ceiling on any single tone.
///
/// Not a stylistic choice: a stuck or mis-sized buffer would otherwise leave a tone
/// running indefinitely in a child's bedroom, and the amplifier stays enabled for
/// exactly as long as we are playing.
pub const MAX_TONE_MS: u32 = 400;

/// Samples for one tone, sized at the ceiling so the buffer itself enforces the limit.
const MAX_SAMPLES: usize = (SAMPLE_RATE as usize * MAX_TONE_MS as usize) / 1000;

/// (frequency, milliseconds) for the two notes of the warning.
const WARNING_NOTES: [(u32, u32); 2] = [(880, 120), (1174, 160)];
const WARNING_GAP_MS: u32 = 60;

/// Nothing this device plays may run for a second. Asserted at compile time against
/// the *clamped* worst case, so the guarantee survives someone later editing the notes
/// without reading this comment.
const _: () = {
    let worst = if WARNING_NOTES[0].1 > MAX_TONE_MS { MAX_TONE_MS } else { WARNING_NOTES[0].1 }
        + WARNING_GAP_MS
        + if WARNING_NOTES[1].1 > MAX_TONE_MS { MAX_TONE_MS } else { WARNING_NOTES[1].1 };
    assert!(worst < 1000, "the chime must never run for a second");
};

pub struct Chime<'d> {
    tx: I2sTx<'d, Blocking>,
    pa_ctrl: Output<'d>,
    delay: Delay,
    buf: [i16; MAX_SAMPLES],
}

pub struct Pins {
    pub mclk: esp_hal::peripherals::GPIO14<'static>,
    pub bclk: esp_hal::peripherals::GPIO15<'static>,
    pub lrclk: esp_hal::peripherals::GPIO38<'static>,
    pub dout: esp_hal::peripherals::GPIO45<'static>,
    pub pa_ctrl: esp_hal::peripherals::GPIO46<'static>,
}

impl Chime<'static> {
    /// Configure the codec and the I²S transmitter.
    ///
    /// Returns `None` if the codec does not answer, which is not fatal — a silent
    /// build is better than a dead one, and the display still says everything the
    /// chime would have.
    pub fn new(
        i2s: esp_hal::peripherals::I2S0<'static>,
        dma: esp_hal::peripherals::DMA_CH0<'static>,
        pins: Pins,
        i2c: &mut I2c<'_, Blocking>,
    ) -> Option<Self> {
        let mut delay = Delay::new();

        // 0x18, the address the bus scan finds at boot.
        let codec = Es8311::new(0x18);
        let clk = ClockConfig {
            mclk_inverted: false,
            sclk_inverted: false,
            mclk_from_mclk_pin: true,
            mclk_frequency: MCLK_HZ,
            sample_frequency: SAMPLE_RATE,
        };
        if let Err(e) = codec.init(i2c, &clk, Resolution::Bits16, Resolution::Bits16, &mut delay) {
            println!("chime: ES8311 init failed ({e:?}), running silent");
            return None;
        }
        if let Err(e) = codec.sample_frequency_config(i2c, MCLK_HZ, SAMPLE_RATE) {
            println!("chime: ES8311 clock config failed ({e:?}), running silent");
            return None;
        }
        if let Err(e) = codec.volume_set(i2c, VOLUME, None) {
            println!("chime: ES8311 volume failed ({e:?}), running silent");
            return None;
        }
        // Waveshare's example does this too; the mic path shares registers with the
        // DAC path on this part.
        if let Err(e) = codec.microphone_config(i2c, false) {
            println!("chime: ES8311 mic config failed ({e:?})");
        }

        // Read the volume back: proves the codec is actually alive and holding
        // configuration, rather than merely ACKing writes into the void.
        match codec.volume_get(i2c) {
            Ok(v) => println!("chime: codec volume reads back as {v}"),
            Err(e) => println!("chime: codec readback failed ({e:?})"),
        }

        // MONO, not STEREO: the tone buffer is a single channel of samples. With a
        // stereo slot config each sample would land alternately in L and R, halving
        // the pitch and mangling the waveform.
        let unit = UnitConfig::new_tdm_philips()
            .with_sample_rate(Rate::from_hz(SAMPLE_RATE))
            .with_channels(Channels::MONO)
            .with_data_format(DataFormat::Data16Channel16);
        let cfg = I2sConfig::new_tdm_philips().with_tx_config(unit);

        let i2s = match I2s::new(i2s, dma, cfg) {
            Ok(i2s) => i2s.with_mclk(pins.mclk),
            Err(e) => {
                println!("chime: i2s config rejected ({e:?}), running silent");
                return None;
            }
        };

        // Must cover the largest tone we can produce. Sized at 4096 this silently
        // could not describe a full buffer.
        let (_, descriptors) = esp_hal::dma_descriptors!(0, MAX_SAMPLES * 2);
        let tx = i2s
            .i2s_tx
            .with_bclk(pins.bclk)
            .with_ws(pins.lrclk)
            .with_dout(pins.dout)
            .build(descriptors);

        // Amplifier off until there is something to play.
        let pa_ctrl = Output::new(pins.pa_ctrl, Level::Low, OutputConfig::default());

        println!("chime: ready");
        Some(Self { tx, pa_ctrl, delay, buf: [0; MAX_SAMPLES] })
    }

    /// Play a single tone. `ms` is clamped to [`MAX_TONE_MS`].
    pub fn beep(&mut self, hz: u32, ms: u32) {
        let ms = ms.min(MAX_TONE_MS);
        let samples = ((SAMPLE_RATE as usize * ms as usize) / 1000).min(MAX_SAMPLES);
        if samples == 0 {
            return;
        }

        fill_tone(&mut self.buf[..samples], hz);

        self.pa_ctrl.set_high();
        self.delay.delay_millis(2); // let the amplifier settle, or the attack pops

        // `write_words` is the blocking path and takes the samples directly, so no
        // transmute to bytes and no DMA lifetime juggling for something this short.
        if let Err(e) = self.tx.write_words(&self.buf[..samples]) {
            println!("chime: i2s write failed ({e:?})");
        }

        // Amplifier off immediately: idling it hisses, and a chime that becomes
        // background noise stops being a signal.
        self.pa_ctrl.set_low();
    }

    /// Diagnostic, kept for whenever the speaker question is settled.
    ///
    /// Toggle the amplifier enable and nothing else.
    ///
    /// A class-D amplifier pops when its output stage switches on and off, so if the
    /// amp is powered and the speaker is connected this is audible even with no signal
    /// at all. It splits the fault cleanly: clicks mean the amp and speaker are fine
    /// and the problem is upstream in the codec or the I²S clocks; silence means the
    /// enable line or the speaker connection, and everything done to the codec so far
    /// is beside the point.
    ///
    /// Ten pops over 300 ms, well inside the one-second ceiling.
    #[allow(dead_code)] // bring-up diagnostic, called by hand
    pub fn click_test(&mut self) {
        for _ in 0..10 {
            self.pa_ctrl.set_high();
            self.delay.delay_millis(15);
            self.pa_ctrl.set_low();
            self.delay.delay_millis(15);
        }
    }

    /// Two rising notes — distinguishable from a notification without being alarming.
    pub fn warning(&mut self) {
        self.beep(WARNING_NOTES[0].0, WARNING_NOTES[0].1);
        self.delay.delay_millis(WARNING_GAP_MS);
        self.beep(WARNING_NOTES[1].0, WARNING_NOTES[1].1);
    }
}

/// A triangle wave, with a short fade in and out.
///
/// Triangle rather than square: a square wave's harmonics through a small speaker are
/// unpleasantly harsh. The fade suppresses the click that a hard start and stop makes.
fn fill_tone(buf: &mut [i16], hz: u32) {
    let period = (SAMPLE_RATE / hz.max(1)).max(2) as usize;
    let half = period / 2;
    let peak = 9_000i32; // well below i16::MAX; the amp has plenty of gain
    let len = buf.len();
    let fade = (len / 8).max(1);

    for (i, sample) in buf.iter_mut().enumerate() {
        let phase = i % period;
        let tri = if phase < half {
            (phase as i32 * 2 * peak) / half as i32 - peak
        } else {
            peak - ((phase - half) as i32 * 2 * peak) / (period - half) as i32
        };

        let envelope = if i < fade {
            i as i32 * 256 / fade as i32
        } else if i >= len - fade {
            (len - i) as i32 * 256 / fade as i32
        } else {
            256
        };

        *sample = ((tri * envelope) / 256) as i16;
    }
}
