#![no_std]
#![no_main]

mod chime;
mod flashlock;
mod fritzbox;
mod net;
mod notify;
mod panel;
mod reader;
mod room;
mod rtc;
mod shared;
mod storage;
mod timing;
mod web;

// Pulls in the panic handler and the backtrace printer; not referenced directly.
use esp_backtrace as _;

use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
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

use medienzeit_core::{Ledger, Policy, Snapshot};

esp_bootloader_esp_idf::esp_app_desc!();

pub const DEV_NAMES: [&str; 2] = [
    env!("MEDIENZEIT_FB_DEVICE1_NAME"),
    env!("MEDIENZEIT_FB_DEVICE2_NAME"),
];
const DEV_MACS: [&str; 2] = [
    env!("MEDIENZEIT_FB_DEVICE1_MAC"),
    env!("MEDIENZEIT_FB_DEVICE2_MAC"),
];

/// How often to ask the FRITZ!Box where the devices are.
const PRESENCE_PERIOD: Duration = Duration::from_secs(30);

/// Gaps shorter than this are a reboot, not someone pulling the plug.
const OUTAGE_MIN_SECS: i64 = 5 * 60;

/// Socket budget. DHCP, DNS, three admin-server acceptors, one transient TR-064
/// connection and one transient alert connection — with headroom, because running out
/// does not fail loudly, it just makes `connect` hang forever.
static RESOURCES: StaticCell<StackResources<12>> = StaticCell::new();

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    let p = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    esp_alloc::heap_allocator!(size: 72 * 1024);

    let timg0 = TimerGroup::new(p.TIMG0);
    let sw = SoftwareInterruptControl::new(p.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw.software_interrupt0);
    println!("medienzeit: booted");


    let panel = panel::Panel::new(
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

    let chime = chime::Chime::new(
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
    reader::start_rf(&mut nfc);
    reader::bench(&mut nfc);

    // Tags are configured by UID in tags.toml. An unset value leaves that device on the
    // BOOT-button fallback, so the firmware is useful before the tags physically arrive.
    let docking = reader::Docking::new([
            medienzeit_pn5180::Uid::from_display_hex(env!("MEDIENZEIT_TAG_DEVICE1")),
            medienzeit_pn5180::Uid::from_display_hex(env!("MEDIENZEIT_TAG_DEVICE2")),
    ]);
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
    // Matches Docking's own starting assumption, so the first real reading logs a
    // transition only if it actually differs.
    if docking.unconfigured() {
        println!("reader: no tag UIDs configured — using the BOOT button");
        // Only worth a scan window when there is nothing to match against: it is how the
        // UIDs get read off in the first place.
        reader::bringup_scan(&mut nfc, &mut scan, 20).await;
    }

    // Recover the balance before anything else can spend it.
    let (mut journal, recovered) = storage::Journal::open(p.FLASH);

    // Rules come from flash when they have ever been saved, and from the compiled-in
    // defaults otherwise. Stored settings win because they are the more recent decision;
    // a firmware update should not quietly revert a parent's choices.
    let (settings_store, stored) = storage::SettingsStore::open(journal.flash());
    let settings = stored
        .unwrap_or_else(|| medienzeit_core::settings::Settings::from_policy(&Policy::default(), 0));
    let policy = settings.to_policy();
    web::publish_settings(settings);
    let ledger = match recovered {
        Some(rec) => Ledger::<2>::with_balance(rec.balance_secs),
        None => Ledger::<2>::new(&policy),
    };

    // Everything that blocks now lives behind this one object, and everything it
    // exchanges with the async half goes through `shared`. Moving it to core 1 is
    // therefore a change of where `step` is called from, and nothing else.
    let room = room::Room::new(
        panel, nfc, scan, docking, boot_button, chime, i2c, ledger, policy,
    );
    // Core 1 is the room. It gets every blocking driver and the ledger, and runs a
    // plain synchronous loop with no executor; core 0 keeps the network stack and stays
    // async. The two meet only through `shared`.
    //
    // In a StaticCell rather than moved into the closure so the object lives in .bss
    // and core 1's stack only has to cover call frames.
    static ROOM: StaticCell<room::Room> = StaticCell::new();
    static ROOM_STACK: StaticCell<esp_hal::system::Stack<12288>> = StaticCell::new();
    let room = ROOM.init(room);
    esp_rtos::start_second_core(
        p.CPU_CTRL,
        sw.software_interrupt1,
        ROOM_STACK.init(esp_hal::system::Stack::new()),
        move || room.run(),
    );

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
    for slot in 0..web::ACCEPTORS {
        spawner.spawn(web::serve(stack, slot).unwrap());
    }
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
    spawner.spawn(storage_task(journal, settings_store, recovered).unwrap());

    let cfg = net::wait_for_dhcp(stack).await;
    let Some(gateway) = cfg.gateway else {
        println!("medienzeit: no gateway from DHCP; cannot reach NTP or TR-064");
        park().await
    };

    // The room owns the RTC, so the corrected time is handed over rather than applied
    // here. It is also what releases accounting if the RTC alone was not trustworthy.
    match net::sntp_once(stack, gateway).await {
        Ok(t) => shared::SNTP.signal(t),
        Err(e) => println!("medienzeit: SNTP failed: {e}"),
    }

    // --- enforcement loop ------------------------------------------------
    let mut fb = fritzbox::Client::new(gateway);
    let mut state = Control::new();

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
    loop {
        if last_presence.elapsed() >= PRESENCE_PERIOD {
            last_presence = Instant::now();
            state.refresh_presence(&mut fb, stack).await;
        }

        // Enforcement follows what the room last computed. Nothing to do until the
        // ledger has produced a snapshot.
        if let Some(snapshot) = shared::snapshot() {
            state.apply_blocks(&mut fb, stack, &snapshot).await;
        }

        Timer::after(Duration::from_secs(1)).await;
    }
}

/// Owns the flash, and is the only thing that writes to it.
///
/// Journalling is edge-driven — the room signals when the flow changes or the timer is
/// due — so this sleeps rather than polls. Flash writes block, but briefly: under a
/// millisecond for a record, tens of milliseconds for the roughly hourly sector erase.
#[embassy_executor::task]
async fn storage_task(
    mut journal: storage::Journal<'static>,
    mut settings_store: storage::SettingsStore,
    recovered: Option<medienzeit_core::journal::Record>,
) {
    // Outage detection needs a trustworthy wall clock, and the first journal request is
    // the earliest proof there is one — the room does not tick until then. Held as
    // pending rather than waited on, because a device whose clock never becomes
    // trustworthy must still be able to save its settings.
    let mut outage_pending = recovered;

    loop {
        match select(shared::PERSIST.wait(), shared::SETTINGS_REQ.wait()).await {
            Either::First((balance, t)) => {
                if with_room_parked(|| journal.append(balance, t)).await.is_some() {
                    if let Some(rec) = outage_pending.take() {
                        report_outage(rec.last_tick, t);
                    }
                } else {
                    // Not fatal: the room asks again within JOURNAL_PERIOD.
                    println!("journal: skipped, the room did not park");
                }
            }
            Either::Second(new) => {
                let ok = with_room_parked(|| settings_store.save(journal.flash(), new))
                    .await
                    .unwrap_or(false);
                if ok {
                    // Published only on a successful write, so the running rules can
                    // never be ones that failed to persist.
                    shared::publish_policy(new.to_policy());
                    web::publish_settings(new);
                    println!("medienzeit: rules updated");
                }
                // The page is waiting on this to say whether it saved.
                shared::SETTINGS_DONE.signal(ok);
            }
        }
    }
}

/// Run a flash write with the room parked somewhere safe.
///
/// `esp-storage` will hardware-stall core 1 for the duration of the write. That is only
/// survivable if core 1 is holding no lock when it happens, which is what
/// [`flashlock`] arranges — see that module for why this is not optional.
///
/// `None` means the room never parked and **the write did not happen**. Writing anyway
/// would be choosing a deadlock over a missed record, and the record comes round again.
async fn with_room_parked<R>(f: impl FnOnce() -> R) -> Option<R> {
    flashlock::request();
    // The room checks once per iteration, so this can take a full period — longer if it
    // is mid-refresh, which is nearly two seconds. Bounded comfortably past that.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !flashlock::room_is_parked() {
        if Instant::now() > deadline {
            flashlock::release();
            println!("flash: the room did not park in time");
            return None;
        }
        Timer::after(Duration::from_millis(2)).await;
    }
    let out = f();
    flashlock::release();
    Some(out)
}

/// Report how long the unit was off.
///
/// The gap is never billed — a real power cut must not cost her the evening — but
/// "unplug it" is otherwise the obvious way to stop the clock, so it gets said out loud.
fn report_outage(last_tick: i64, now: i64) {
    let Some(outage) = medienzeit_core::journal::detect_outage(last_tick, now, OUTAGE_MIN_SECS)
    else {
        return;
    };
    // Report seconds under two minutes: integer minutes would round a real outage down
    // to "0 min", which reads as nothing having happened.
    let secs = outage.secs();
    let mut m: notify::Message = heapless::String::new();
    if secs < 120 {
        println!("  [alert] unit was off for {secs}s");
        let _ = write!(m, "Gerät war {secs}s aus");
    } else {
        println!("  [alert] unit was off for {} min", secs / 60);
        let _ = write!(m, "Gerät war {} min aus", secs / 60);
    }
    notify::send(&m);
}

struct Control {
    present: [bool; 2],
    ips: [Option<String<46>>; 2],
    /// What we have actually told the box, so we only issue changes.
    applied: [Option<bool>; 2],
}

impl Control {
    fn new() -> Self {
        Self {
            present: [false; 2],
            ips: [None, None],
            applied: [None, None],
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
        shared::publish_presence(self.present);
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

/// Nothing more to do on this half, but the room keeps running: the reader and the
/// display do not need the network.
async fn park() -> ! {
    loop {
        Timer::after(Duration::from_secs(30)).await;
    }
}
