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
use medienzeit_pn5180::{CardUid, Pn5180, TagSet, Uid};

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
    // The ESP32 reaches this point faster than the PN5180 settles after power-up, and
    // the first command then reads back all-ff. Observed on a cold boot: the product
    // version came back 0xFFFF where every warm boot gave a real value. Harmless here
    // because the next read succeeds, but a first read that silently returns nonsense
    // is worth 50 ms to avoid.
    delay.delay_millis(50);
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
                if buf.iter().all(|&b| b == 0xff) {
                    print!("   <-- all ff: no answer, not a version");
                } else if len == 2 {
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
        Ok(0) => println!(
            "reader: register writable mask 0x00000000 — the chip is not answering, \
             not a narrow register"
        ),
        Ok(mask) => {
            println!("reader: register writable mask {mask:#010x} ({} bits)", mask.count_ones());
            let pattern = 0xa5c3_5a5a & mask;
            match (probe(reader, 0), probe(reader, pattern)) {
                // `pattern` is masked, so a dead chip would compare zero against zero
                // and call it proven. Guarded above by rejecting an empty mask, and
                // again here, because this line is the one that gets trusted.
                (Ok(0), Ok(v)) if v == pattern && pattern != 0 => {
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
pub fn start_rf(reader: &mut Reader<'static>, cards: bool) {
    if cards {
        match reader.begin_iso14443a() {
            Ok(()) => println!("reader: ISO 14443A field on"),
            Err(e) => println!("reader: could not start ISO 14443A ({e:?})"),
        }
        return;
    }
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
///
/// Kept though unreferenced: the only check that works below the protocol, for when a
/// tag is present and nothing reads it.
#[allow(dead_code)]
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
pub struct Scan {
    set: TagSet<MAX_TAGS>,
    last: usize,
    known: [Uid; MAX_TAGS],
}

/// Rounds a tag may be missing before it counts as gone.
///
/// A tag is passive and sits at the edge of the field, so single dropped reads are
/// routine; treating one as a departure would start the clock while the device has not
/// moved. Tolerance is per tag, not per count — see [`TagSet`].
const MISS_LIMIT: u32 = 3;

impl Default for Scan {
    fn default() -> Self {
        Self { set: TagSet::new(MISS_LIMIT), last: 0, known: [Uid::default(); MAX_TAGS] }
    }
}

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

        let believed = self.set.update(&found[..n]);
        let changed = believed.len() != self.last
            || !believed.iter().all(|u| self.known[..self.last].contains(u));
        if changed {
            self.last = believed.len();
            self.known[..believed.len()].copy_from_slice(believed);
            print!("reader: {} tag(s)", believed.len());
            for uid in believed {
                print!(" ");
                for b in uid.display_order() {
                    print!("{b:02x}");
                }
            }
            println!();
        }
        Some(self.set.tags())
    }
}

/// Poll the field for a while, reporting changes, before the network comes up.
///
/// Bring-up only, and deliberately ahead of Wi-Fi: the control loop that normally polls
/// the reader sits behind DHCP and SNTP, so a flaky association would otherwise mean the
/// antenna is never even asked. Reader faults should not be diagnosed through the
/// network's availability.
pub async fn bringup_scan(
    reader: &mut Reader<'static>,
    scan: &mut Scan,
    secs: u32,
    cards: bool,
) {
    println!("reader: scanning for {secs}s — present a tag");
    let mut last_card = None;
    let mut tracker = CardTracker::default();
    for _ in 0..secs {
        if cards {
            // Report card UIDs so they can be copied into tags.toml, debounced the same
            // way the control loop does it — an unfiltered log would show flapping that
            // the running system never sees.
            let card = tracker.update(poll_card(reader).unwrap_or(None));
            if card != last_card {
                match card {
                    Some(c) => println!("reader: card {}", card_hex(&c)),
                    None => println!("reader: card gone"),
                }
                last_card = card;
            }
        } else {
            scan.poll(reader);
        }
        embassy_time::Timer::after(embassy_time::Duration::from_secs(1)).await;
    }
    println!("reader: scan window over");
}

/// Maps the tags in the field onto each device's `docked` flag.
///
/// # What happens when the reader fails
///
/// A reader that stops answering holds the **last known** docking state rather than
/// guessing, and reports the fault. Both alternatives are worse: declaring everything
/// undocked would drain the balance for devices sitting untouched at the reader, which
/// is the unfairness this whole design exists to avoid; declaring everything docked
/// would hand out unmetered screen time for as long as the fault lasts. Holding is
/// wrong in neither direction for the length of a glitch, and the alert is what makes a
/// long fault visible — detection over prevention, as everywhere else here.
pub struct Docking {
    /// Configured UID per device; `None` means fall back to the BOOT button.
    known: [Option<Uid>; 2],
    /// Configured ISO 14443A card per device, for the interim before stickers arrive.
    known_cards: [Option<CardUid>; 2],
    last: [bool; 2],
    /// The unknown tag most recently reported, so one strange tag left lying on the
    /// reader does not produce an alert every second.
    reported_unknown: Option<Uid>,
    consecutive_failures: u32,
}

/// Reader failures tolerated before saying so. At one poll per second this is a few
/// seconds of silence, which distinguishes a wedged chip from a single bad transfer.
const FAILURES_BEFORE_ALERT: u32 = 5;

/// What a poll concluded.
pub struct Docked {
    pub docked: [bool; 2],
    /// A tag in the field belonging to no configured device, worth telling the parent
    /// about — a device carrying someone else's sticker looks exactly like this.
    pub unknown: Option<Uid>,
    /// The reader has been unresponsive long enough to be a fault, not a glitch.
    pub reader_fault: bool,
}

impl Docking {
    pub fn new(known: [Option<Uid>; 2], known_cards: [Option<CardUid>; 2]) -> Self {
        // Start docked, matching the ledger's own fresh-state assumption: until
        // something is known, do not spend.
        Self {
            known,
            known_cards,
            last: [true; 2],
            reported_unknown: None,
            consecutive_failures: 0,
        }
    }

    /// True when no device has any identity configured, so the reader cannot drive
    /// anything and the BOOT button is still the only input.
    pub fn unconfigured(&self) -> bool {
        self.known.iter().all(|k| k.is_none()) && self.known_cards.iter().all(|k| k.is_none())
    }

    /// Whether a device is identified by a card rather than a sticker.
    fn card_present(&self, i: usize, card: Option<CardUid>) -> Option<bool> {
        self.known_cards[i].map(|known| card == Some(known))
    }

    pub fn update(
        &mut self,
        seen: Option<&[Uid]>,
        card: Option<CardUid>,
        fallback: [bool; 2],
    ) -> Docked {
        let Some(seen) = seen else {
            self.consecutive_failures += 1;
            return Docked {
                docked: self.last,
                unknown: None,
                reader_fault: self.consecutive_failures == FAILURES_BEFORE_ALERT,
            };
        };
        self.consecutive_failures = 0;

        let mut docked = [false; 2];
        for i in 0..2 {
            // A sticker wins if one is configured; otherwise a card; otherwise the
            // button. Both can be configured during the changeover, and either being
            // present counts as put back — so swapping identities needs no flag day.
            let by_tag = self.known[i].map(|uid| seen.contains(&uid));
            let by_card = self.card_present(i, card);
            docked[i] = match (by_tag, by_card) {
                (Some(a), Some(b)) => a || b,
                (Some(a), None) => a,
                (None, Some(b)) => b,
                (None, None) => fallback[i],
            };
        }
        self.last = docked;

        // Anything in the field that is not a configured device.
        let unknown = seen
            .iter()
            .find(|u| !self.known.iter().any(|k| k.as_ref() == Some(*u)))
            .copied();
        let report = match (unknown, self.reported_unknown) {
            (Some(u), Some(prev)) if u == prev => None,
            (Some(u), _) => Some(u),
            (None, _) => None,
        };
        self.reported_unknown = unknown;

        Docked { docked, unknown: report, reader_fault: false }
    }
}

/// Format a UID for a message, most significant byte first.
pub fn uid_hex(uid: &Uid) -> heapless::String<16> {
    let mut s = heapless::String::new();
    for b in uid.display_order() {
        let _ = core::fmt::Write::write_fmt(&mut s, format_args!("{b:02x}"));
    }
    s
}

/// Poll for a single ISO 14443A card. The front end must already be on that protocol.
///
/// Interim support while the ISO 15693 stickers are in the post: shorter range, and one
/// card at a time, since there is no anticollision here. Two cards in the field collide
/// and read as nothing, which is the safe way to fail — no identity rather than a wrong
/// one.
/// `None` means the reader failed; `Some(None)` means it worked and no card is there.
///
/// The distinction is the whole safety property. Collapsing the two lets a broken reader
/// masquerade as "the card has been taken", which starts the clock and eventually cuts
/// the internet on a device that never moved.
pub fn poll_card(reader: &mut Reader<'static>) -> Option<Option<CardUid>> {
    match reader.iso14443a_uid() {
        Ok(u) => Some(u),
        Err(e) => {
            println!("reader: card poll failed ({e:?})");
            None
        }
    }
}

/// Format a card UID for a message, most significant byte first.
pub fn card_hex(uid: &CardUid) -> heapless::String<8> {
    let mut s = heapless::String::new();
    for b in uid.0 {
        let _ = core::fmt::Write::write_fmt(&mut s, format_args!("{b:02x}"));
    }
    s
}

/// Debounces card reads.
///
/// Two failure modes seen on the bench, both of which would reach the ledger unfiltered:
/// a card at the edge of the field drops out for single polls, which would toggle
/// `docked` every second and flip the ledger between filling and draining; and a garbled
/// frame occasionally passes the BCC, since that checksum is only eight bits and roughly
/// one corrupt frame in 256 survives it. So a new identity has to appear twice in a row
/// to be believed, and an absence has to persist before it counts.
#[derive(Default)]
pub struct CardTracker {
    current: Option<CardUid>,
    candidate: Option<CardUid>,
    candidate_hits: u32,
    misses: u32,
}

/// Consecutive identical reads before a *new* card is accepted.
const CARD_CONFIRM: u32 = 2;
/// Consecutive empty polls before a card counts as gone.
///
/// Six rather than three: a card is re-powered from cold on every poll, so short dropouts
/// are normal even when it has not moved. The cost of being generous is that a real
/// pickup takes a few seconds to register, which is nothing against a budget measured in
/// hours — where a false "taken" is visible immediately as the internet being cut.
const CARD_MISS_LIMIT: u32 = 6;

impl CardTracker {
    pub fn update(&mut self, raw: Option<CardUid>) -> Option<CardUid> {
        match raw {
            Some(u) => {
                self.misses = 0;
                if self.current == Some(u) {
                    self.candidate = None;
                    self.candidate_hits = 0;
                } else if self.candidate == Some(u) {
                    self.candidate_hits += 1;
                    if self.candidate_hits >= CARD_CONFIRM {
                        self.current = Some(u);
                        self.candidate = None;
                        self.candidate_hits = 0;
                    }
                } else {
                    self.candidate = Some(u);
                    self.candidate_hits = 1;
                }
            }
            None => {
                self.candidate = None;
                self.candidate_hits = 0;
                self.misses += 1;
                if self.misses >= CARD_MISS_LIMIT {
                    self.current = None;
                }
            }
        }
        self.current
    }
}

/// Run both inventory methods side by side and report what each sees.
///
/// Kept though unreferenced: this is what proved the anticollision round, and it is the
/// tool to reach for if tag reads ever go strange — the single-slot path is a control
/// that fails differently.
///
/// The single-slot path is known good, so it is the control: if it finds one tag while
/// the 16-slot round finds none, the anticollision stepping is still wrong rather than
/// the tags being absent. With two tags in the field the expected result is the whole
/// point — single-slot should collide and find nothing, 16-slot should find both.
#[allow(dead_code)]
pub async fn compare_inventory(reader: &mut Reader<'static>, secs: u32) {
    println!("reader: comparing inventory methods for {secs}s — put BOTH stickers in the field");
    for _ in 0..secs {
        let mut one = [Uid::default(); MAX_TAGS];
        let mut many = [Uid::default(); MAX_TAGS];
        let a = reader.inventory(&mut one);
        let b = reader.inventory_16slot(&mut many);
        print!("reader: single=");
        match a {
            Ok(n) => print_uids(&one[..n]),
            Err(e) => print!("{e:?}"),
        }
        print!("  16slot=");
        match b {
            Ok(n) => print_uids(&many[..n]),
            Err(e) => print!("{e:?}"),
        }
        println!();
        embassy_time::Timer::after(embassy_time::Duration::from_secs(1)).await;
    }
    println!("reader: comparison over");
}

#[allow(dead_code)]
fn print_uids(uids: &[Uid]) {
    if uids.is_empty() {
        print!("none");
        return;
    }
    for (i, u) in uids.iter().enumerate() {
        if i > 0 {
            print!(",");
        }
        for b in u.display_order() {
            print!("{b:02x}");
        }
    }
}
