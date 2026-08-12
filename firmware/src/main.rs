#![no_std]
#![no_main]

mod chime;
mod fritzbox;
mod net;
mod notify;
mod panel;
mod reader;
mod rtc;
mod storage;
mod web;

// Pulls in the panic handler and the backtrace printer; not referenced directly.
use esp_backtrace as _;

use embassy_executor::Spawner;
use embassy_net::{Config as NetConfig, StackResources, Stack};
use embassy_time::{Duration, Instant, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::rng::Rng;
use esp_hal::timer::timg::TimerGroup;
use core::fmt::Write as _;

use esp_println::println;
use heapless::String;
use static_cell::StaticCell;

use medienzeit_core::{Event, Ledger, Policy, Snapshot};
use medienzeit_ui::Chrome;

esp_bootloader_esp_idf::esp_app_desc!();

const DEV_NAMES: [&str; 2] = [
    env!("MEDIENZEIT_FB_DEVICE1_NAME"),
    env!("MEDIENZEIT_FB_DEVICE2_NAME"),
];
const DEV_MACS: [&str; 2] = [
    env!("MEDIENZEIT_FB_DEVICE1_MAC"),
    env!("MEDIENZEIT_FB_DEVICE2_MAC"),
];

/// How often to ask the FRITZ!Box where the devices are.
const PRESENCE_PERIOD: Duration = Duration::from_secs(30);

/// How often to journal the balance. A power cut costs at most this much.
const JOURNAL_PERIOD: Duration = Duration::from_secs(30);

/// Gaps shorter than this are a reboot, not someone pulling the plug.
const OUTAGE_MIN_SECS: i64 = 5 * 60;

/// Quick refreshes before forcing a full one to clear accumulated ghosting.
///
/// At one update per minute that is a flash every half hour, which is roughly the
/// point at which ghosting becomes noticeable on this panel.
const QUICK_REFRESHES_PER_FULL: u32 = 30;

/// Socket budget. DHCP, DNS, the admin server's listener, one transient TR-064
/// connection and one transient alert connection — with headroom, because running out
/// does not fail loudly, it just makes `connect` hang forever.
static RESOURCES: StaticCell<StackResources<8>> = StaticCell::new();

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    let p = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    esp_alloc::heap_allocator!(size: 72 * 1024);

    let timg0 = TimerGroup::new(p.TIMG0);
    let sw = SoftwareInterruptControl::new(p.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw.software_interrupt0);
    println!("medienzeit: booted");

    let mut panel = panel::Panel::new(
        p.SPI2,
        panel::Pins {
            power_en: p.GPIO6,
            busy: p.GPIO8,
            rst: p.GPIO9,
            dc: p.GPIO10,
            cs: p.GPIO11,
            sclk: p.GPIO12,
            mosi: p.GPIO13,
        },
    );

    // Waveshare's own examples call POWEER_Audio_ON() before touching I²C, even in a
    // demo that only reads the RTC — IO42 evidently gates more than the amplifier.
    // Active low, like the panel's enable. Held for the lifetime of the program.
    let _audio_power = Output::new(p.GPIO42, Level::Low, OutputConfig::default());
    embassy_time::block_for(Duration::from_millis(20));

    let mut i2c = rtc::bus(p.I2C0, p.GPIO47, p.GPIO48);
    rtc::scan(&mut i2c);

    let mut chime = chime::Chime::new(
        p.I2S0,
        p.DMA_CH0,
        chime::Pins {
            mclk: p.GPIO14,
            bclk: p.GPIO15,
            lrclk: p.GPIO38,
            dout: p.GPIO45,
            pa_ctrl: p.GPIO46,
        },
        &mut i2c,
    );


    // Fallback for any device with no tag configured yet: BOOT toggles device 1's
    // presence at the reader, so the spend/block path stays exercisable by hand.
    let boot_button = Input::new(p.GPIO0, InputConfig::default().with_pull(Pull::Up));

    let mut nfc = reader::new(
        p.SPI3,
        reader::Pins {
            sck: p.GPIO1,
            mosi: p.GPIO2,
            busy: p.GPIO3,
            nss: p.GPIO43,
            miso: p.GPIO44,
        },
    );
    reader::identify(&mut nfc);
    let mut scan = reader::Scan::default();
    let cards = env!("MEDIENZEIT_PROTOCOL") == "iso14443a";
    println!("reader: protocol {}", env!("MEDIENZEIT_PROTOCOL"));
    reader::start_rf(&mut nfc, cards);

    // Tags are configured by UID in tags.toml. An unset value leaves that device on the
    // BOOT-button fallback, so the firmware is useful before the tags physically arrive.
    let mut docking = reader::Docking::new(
        [
            medienzeit_pn5180::Uid::from_display_hex(env!("MEDIENZEIT_TAG_DEVICE1")),
            medienzeit_pn5180::Uid::from_display_hex(env!("MEDIENZEIT_TAG_DEVICE2")),
        ],
        [
            medienzeit_pn5180::CardUid::from_hex(env!("MEDIENZEIT_CARD_DEVICE1")),
            medienzeit_pn5180::CardUid::from_hex(env!("MEDIENZEIT_CARD_DEVICE2")),
        ],
    );
    for (i, raw) in [
        env!("MEDIENZEIT_CARD_DEVICE1"),
        env!("MEDIENZEIT_CARD_DEVICE2"),
    ]
    .iter()
    .enumerate()
    {
        if !raw.is_empty() && medienzeit_pn5180::CardUid::from_hex(raw).is_none() {
            println!("reader: tags.toml device{} card UID {raw:?} is not valid", i + 1);
        }
    }
    for (i, raw) in [
        env!("MEDIENZEIT_TAG_DEVICE1"),
        env!("MEDIENZEIT_TAG_DEVICE2"),
    ]
    .iter()
    .enumerate()
    {
        // A malformed UID is not the same as an unset one, and silently falling back to
        // the button would hide the typo behind a device whose clock never stops.
        if !raw.is_empty() && medienzeit_pn5180::Uid::from_display_hex(raw).is_none() {
            println!("reader: tags.toml device{} UID {raw:?} is not a valid UID", i + 1);
        }
    }
    let mut card_tracker = reader::CardTracker::default();
    // Matches Docking's own starting assumption, so the first real reading logs a
    // transition only if it actually differs.
    let mut last_docked = [true; 2];
    let mut last_recoveries = 0;
    if docking.unconfigured() {
        println!("reader: no tag UIDs configured — using the BOOT button");
        // Only worth a scan window when there is nothing to match against: it is how the
        // UIDs get read off in the first place.
        reader::bringup_scan(&mut nfc, &mut scan, 20, cards).await;
    }

    // Recover the balance before anything else can spend it.
    let (mut journal, recovered) = storage::Journal::open(p.FLASH);

    // Rules come from flash when they have ever been saved, and from the compiled-in
    // defaults otherwise. Stored settings win because they are the more recent decision;
    // a firmware update should not quietly revert a parent's choices.
    let (mut settings_store, stored) = storage::SettingsStore::open(journal.flash());
    let mut settings = stored
        .unwrap_or_else(|| medienzeit_core::settings::Settings::from_policy(&Policy::default(), 0));
    let mut policy = settings.to_policy();
    web::publish_settings(settings);
    let mut ledger = match recovered {
        Some(rec) => Ledger::<2>::with_balance(rec.balance_secs),
        None => Ledger::<2>::new(&policy),
    };

    let mut screen = Screen::default();
    let mut now = rtc::startup_time(&mut i2c);
    if let Some(t) = now {
        let (snapshot, _) = ledger.tick(t, [true; 2], [false; 2], &policy);
        screen.draw(&mut panel, &snapshot);
        web::publish(snapshot.clone());
    }

    // --- radio + network -------------------------------------------------
    let (controller, interfaces) =
        esp_radio::wifi::new(p.WIFI, Default::default()).expect("wifi init failed");

    let rng = Rng::new();
    let seed = ((rng.random() as u64) << 32) | rng.random() as u64;

    let (stack, runner) = embassy_net::new(
        interfaces.station,
        NetConfig::dhcpv4({
            let mut dhcp = embassy_net::DhcpConfig::default();
            // Ask the FRITZ!Box to register a name, so the admin page can be reached at
            // medienzeit.fritz.box rather than at whatever address the lease happens to
            // hand out. Falls back to the IP if the router ignores it.
            dhcp.hostname = Some(heapless::String::try_from("medienzeit").unwrap());
            dhcp
        }),
        RESOURCES.init(StackResources::new()),
        seed,
    );

    spawner.spawn(net::connection(controller).unwrap());
    spawner.spawn(net::net_task(runner).unwrap());
    spawner.spawn(web::serve(stack).unwrap());
    spawner.spawn(
        notify::sender(
            stack,
            notify::Config {
                // An empty host means "resolve the header name", which is how the
                // public service is reached; a literal IP is how a self-hosted one is.
                host: parse_ipv4(env!("MEDIENZEIT_NTFY_HOST")),
                port: env!("MEDIENZEIT_NTFY_PORT").parse().unwrap_or(80),
                host_header: env!("MEDIENZEIT_NTFY_HEADER"),
                topic: env!("MEDIENZEIT_NTFY_TOPIC"),
                tls: env!("MEDIENZEIT_NTFY_TLS") == "true",
            },
        )
        .unwrap(),
    );

    let cfg = net::wait_for_dhcp(stack).await;
    let Some(gateway) = cfg.gateway else {
        println!("medienzeit: no gateway from DHCP; cannot reach NTP or TR-064");
        park().await
    };

    match net::sntp_once(stack, gateway).await {
        Ok(t) => {
            if let Ok(before) = rtc::now(&mut i2c) {
                println!("rtc: drift vs sntp {}s", t - before);
            }
            match rtc::set(&mut i2c, t) {
                Ok(()) => println!("rtc: set from sntp"),
                Err(e) => println!("rtc: set failed ({e:?})"),
            }
            now = Some(t);
        }
        Err(e) => println!("medienzeit: SNTP failed: {e}"),
    }

    let Some(start) = now else {
        println!("medienzeit: no trustworthy clock, not accounting");
        park().await
    };

    // Now that the clock is trustworthy, work out how long the unit was off. The gap
    // is never billed — a real power cut must not cost her the evening — but "unplug
    // it" is otherwise the obvious way to stop the clock, so it gets reported.
    if let Some(rec) = recovered {
        if let Some(outage) = medienzeit_core::journal::detect_outage(
            rec.last_tick,
            start,
            OUTAGE_MIN_SECS,
        ) {
            // Report seconds under two minutes: integer minutes would round a real
            // outage down to "0 min", which reads as nothing having happened.
            let secs = outage.secs();
            if secs < 120 {
                println!("  [alert] unit was off for {secs}s");
                let mut m: notify::Message = heapless::String::new();
                let _ = write!(m, "Gerät war {secs}s aus");
                notify::send(&m);
            } else {
                println!("  [alert] unit was off for {} min", secs / 60);
                let mut m: notify::Message = heapless::String::new();
                let _ = write!(m, "Gerät war {} min aus", secs / 60);
                notify::send(&m);
            }
        }
    }

    // --- control loop ----------------------------------------------------
    let mut fb = fritzbox::Client::new(gateway);
    let mut state = Control::new(start);

    // Reconcile with whatever the box already believes, rather than assuming.
    state.refresh_presence(&mut fb, stack).await;
    #[allow(clippy::needless_range_loop)] // walks several parallel per-device arrays
    for i in 0..2 {
        if let Some(ip) = state.ips[i].as_deref() {
            match fb.is_blocked(stack, ip).await {
                Ok(b) => {
                    println!("enforce: {} starts {}", DEV_NAMES[i], label(b));
                    state.applied[i] = Some(b);
                }
                Err(e) => println!("enforce: {} readback failed ({e:?})", DEV_NAMES[i]),
            }
        }
    }

    let mut last_presence = Instant::now();
    let mut last_journal = Instant::now();
    let mut last_persisted_flow = None;

    loop {
        // The RTC is authoritative between SNTP syncs, so a missed tick or a slow
        // network call cannot make the ledger lose time.
        let t = rtc::now(&mut i2c).unwrap_or(state.last_tick + 1);

        // A rules change from the admin page arrives out of band. Persist first, so a
        // power cut immediately after cannot leave the running rules and the stored
        // ones disagreeing — the stored ones are what the next boot believes.
        if let Some(new) = web::take_settings() {
            if settings_store.save(journal.flash(), new) {
                settings = new;
                policy = settings.to_policy();
                web::publish_settings(settings);
                println!("medienzeit: rules updated");
            }
        }

        // A grant from the admin page arrives out of band; apply it before the tick
        // so the new balance is what gets journalled and displayed this second.
        if let Some(secs) = web::BONUS.try_take() {
            ledger.grant_bonus(secs, &policy);
            println!("medienzeit: +{}s granted", secs);
        }

        // One protocol per build; see tags.toml. A reader failure has to reach Docking
        // as a failure in either protocol — `Some(&[])` would say "the reader is fine
        // and nothing is there", which is what makes a broken reader start the clock.
        let (seen, card) = if cards {
            match reader::poll_card(&mut nfc) {
                Some(raw) => (Some(&[][..]), card_tracker.update(raw)),
                None => (None, None),
            }
        } else {
            (scan.poll(&mut nfc), None)
        };
        let d = docking.update(seen, card, [boot_button.is_high(), true]);
        let docked = d.docked;
        // Log the transition, not the state: at the balance cap, filling and held look
        // identical in the numbers, so this is what shows identity actually driving the
        // ledger.
        // Surface RF recoveries: reads can keep succeeding while the front end is
        // quietly having to be cycled, and a rising count is the early warning.
        let r = nfc.recoveries();
        if r != last_recoveries {
            println!("reader: rf recoveries {r}");
            last_recoveries = r;
        }
        if docked != last_docked {
            for i in 0..2 {
                if docked[i] != last_docked[i] {
                    println!(
                        "reader: {} {}",
                        DEV_NAMES[i],
                        if docked[i] { "zurückgelegt" } else { "genommen" }
                    );
                }
            }
            last_docked = docked;
        }
        if let Some(uid) = d.unknown {
            let hex = reader::uid_hex(&uid);
            println!("  [alert] unknown tag {hex}");
            let mut m: notify::Message = heapless::String::new();
            let _ = write!(m, "Unbekannter Tag {hex} am Leser");
            notify::send(&m);
        }
        if d.reader_fault {
            println!("  [alert] reader not responding");
            let mut m: notify::Message = heapless::String::new();
            let _ = write!(m, "Leser antwortet nicht");
            notify::send(&m);
        }
        let (snapshot, events) = ledger.tick(t, docked, state.present, &policy);
        state.last_tick = t;

        for e in &events {
            report(e);
            // The chime exists for exactly one event. Everything else is on the
            // display, where it can be read rather than interpreted.
            if matches!(e, Event::Warning) {
                if let Some(c) = chime.as_mut() {
                    c.warning();
                }
            }
        }

        if last_presence.elapsed() >= PRESENCE_PERIOD {
            last_presence = Instant::now();
            state.refresh_presence(&mut fb, stack).await;
        }

        state.apply_blocks(&mut fb, stack, &snapshot).await;

        screen.draw(&mut panel, &snapshot);
        web::publish(snapshot.clone());

        // Journal on a timer, and immediately whenever the flow changes — the moment
        // spending starts or stops is exactly when a stale record would be wrong by
        // the largest amount.
        let flow_changed = last_persisted_flow != Some(snapshot.flow);
        if flow_changed || last_journal.elapsed() >= JOURNAL_PERIOD {
            last_journal = Instant::now();
            last_persisted_flow = Some(snapshot.flow);
            journal.append(snapshot.balance_secs, t);
        }

        Timer::after(Duration::from_secs(1)).await;
    }
}

struct Control {
    present: [bool; 2],
    ips: [Option<String<46>>; 2],
    /// What we have actually told the box, so we only issue changes.
    applied: [Option<bool>; 2],
    last_tick: i64,
}

impl Control {
    fn new(start: i64) -> Self {
        Self {
            present: [false; 2],
            ips: [None, None],
            applied: [None, None],
            last_tick: start,
        }
    }

    /// Ask the box where each device is. One call yields both the IP that
    /// `HostFilter` needs and the "at home" signal the ledger needs.
    async fn refresh_presence(&mut self, fb: &mut fritzbox::Client, stack: Stack<'_>) {
        #[allow(clippy::needless_range_loop)] // walks several parallel per-device arrays
        for i in 0..2 {
            match fb.host_entry(stack, DEV_MACS[i]).await {
                Ok(entry) => {
                    if self.present[i] != entry.active {
                        println!(
                            "presence: {} {}",
                            DEV_NAMES[i],
                            if entry.active { "at home" } else { "away" }
                        );
                    }
                    self.present[i] = entry.active;
                    let mut ip: String<46> = String::new();
                    let _ = ip.push_str(&entry.ip);
                    // A changed IP means the lease moved and any existing rule is now
                    // pointing at the wrong device.
                    if self.ips[i].as_deref() != Some(ip.as_str()) {
                        if self.ips[i].is_some() {
                            println!("presence: {} ip changed to {}", DEV_NAMES[i], ip);
                            self.applied[i] = None;
                        }
                        self.ips[i] = Some(ip);
                    }
                }
                Err(e) => println!("presence: {} lookup failed ({e:?})", DEV_NAMES[i]),
            }
        }
    }

    async fn apply_blocks(
        &mut self,
        fb: &mut fritzbox::Client,
        stack: Stack<'_>,
        snapshot: &Snapshot<2>,
    ) {
        #[allow(clippy::needless_range_loop)] // walks several parallel per-device arrays
        for i in 0..2 {
            let want = snapshot.blocked[i];
            if self.applied[i] == Some(want) {
                continue;
            }
            let Some(ip) = self.ips[i].as_deref() else {
                continue;
            };
            match fb.set_blocked(stack, ip, want).await {
                Ok(()) => {
                    println!("enforce: {} -> {}", DEV_NAMES[i], label(want));
                    self.applied[i] = Some(want);
                }
                // Leave `applied` unset so the next pass retries. A silently dropped
                // block is the worst failure this system can have.
                Err(e) => println!("enforce: {} FAILED ({e:?})", DEV_NAMES[i]),
            }
        }
    }
}

fn label(blocked: bool) -> &'static str {
    if blocked {
        "blocked"
    } else {
        "allowed"
    }
}

/// Everything the screen actually shows. Redrawing on anything else wastes a refresh.
type Fingerprint = (i32, u32, u32, bool, medienzeit_core::Flow, [bool; 2]);

fn fingerprint(s: &Snapshot<2>) -> Fingerprint {
    (
        s.balance_secs / 60,
        s.local.hour,
        s.local.minute,
        s.night,
        s.flow,
        s.docked,
    )
}

/// Decides *whether* to redraw and *how*.
///
/// E-paper updates are slow and the panel has a finite number of them, so the screen
/// is only touched when something visible changed. Most of those changes are one digit
/// of a countdown, which the quick waveform handles without flashing the whole panel.
#[derive(Default)]
struct Screen {
    last: Option<Fingerprint>,
    quick_since_full: u32,
}

impl Screen {
    fn draw(&mut self, panel: &mut panel::Panel<'static>, snapshot: &Snapshot<2>) {
        let now = fingerprint(snapshot);
        let Some(previous) = self.last else {
            // First frame of the session: nothing is on the panel we can trust.
            self.present(panel, snapshot, now, panel::Refresh::Full);
            return;
        };
        if previous == now {
            return;
        }

        // A lockout is the one transition that inverts the entire screen. Doing that
        // with the quick waveform leaves the old image ghosted through the new one,
        // which is exactly when legibility matters most.
        let lockout_changed = previous.3 != now.3 || (previous.0 <= 0) != (now.0 <= 0);
        let mode = if lockout_changed || self.quick_since_full >= QUICK_REFRESHES_PER_FULL {
            panel::Refresh::Full
        } else {
            panel::Refresh::Quick
        };
        self.present(panel, snapshot, now, mode);
    }

    fn present(
        &mut self,
        panel: &mut panel::Panel<'static>,
        snapshot: &Snapshot<2>,
        fp: Fingerprint,
        mode: panel::Refresh,
    ) {
        println!(
            "screen: {:?} redraw at {:02}:{:02}, balance {}s",
            mode, snapshot.local.hour, snapshot.local.minute, snapshot.balance_secs
        );
        show(panel, snapshot, mode);
        self.last = Some(fp);
        self.quick_since_full = match mode {
            panel::Refresh::Full => 0,
            panel::Refresh::Quick => self.quick_since_full + 1,
        };
    }
}

/// Log every event; push only the ones a parent would want to know about away from
/// the house. Alerting on routine transitions would train you to ignore the channel,
/// which costs more than the missed information.
fn report(e: &Event) {
    let mut push: notify::Message = heapless::String::new();
    match e {
        Event::Exhausted => {
            println!("  [event] EXHAUSTED");
            let _ = push.push_str("Zeit ist aufgebraucht");
        }
        Event::UndockedAtNight { device } => {
            println!("  [event] {} TAKEN AWAY AT NIGHT", DEV_NAMES[*device]);
            let _ = write!(push, "{} nachts weggenommen", DEV_NAMES[*device]);
        }
        Event::Restored => println!("  [event] restored"),
        Event::Warning => println!("  [event] 5 minutes left"),
        Event::NightBegan => println!("  [event] night began"),
        Event::NightEnded => println!("  [event] night ended"),
        Event::TimeJump { gap_secs } => println!("  [event] time jump {gap_secs}s ignored"),
    }
    if !push.is_empty() {
        notify::send(&push);
    }
}

/// Dotted-quad to an address, at runtime because `env!` yields a string.
///
/// An empty or unparseable value yields `None`, meaning "resolve by name instead".
fn parse_ipv4(s: &str) -> Option<embassy_net::IpAddress> {
    let mut octets = [0u8; 4];
    let mut n = 0;
    for (i, part) in s.split('.').enumerate().take(4) {
        octets[i] = part.parse().ok()?;
        n += 1;
    }
    (n == 4).then(|| embassy_net::IpAddress::v4(octets[0], octets[1], octets[2], octets[3]))
}

fn show(panel: &mut panel::Panel<'static>, snapshot: &Snapshot<2>, mode: panel::Refresh) {
    let mut fbuf = panel::framebuffer();
    let chrome = Chrome { device_names: DEV_NAMES };
    medienzeit_ui::render(&mut panel::InkTarget(&mut fbuf), snapshot, &chrome).unwrap();
    panel.present(&fbuf, mode);
}

async fn park() -> ! {
    loop {
        Timer::after(Duration::from_secs(30)).await;
    }
}
