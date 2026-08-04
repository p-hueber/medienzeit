#![no_std]
#![no_main]

mod fritzbox;
mod net;
mod panel;
mod rtc;

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

static RESOURCES: StaticCell<StackResources<4>> = StaticCell::new();

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
    let mut clock = rtc::Rtc::new(i2c);

    // Stand-in for the NFC reader until it arrives: BOOT toggles device 1's presence
    // at the reader, so the whole spend/block path can be exercised by hand.
    let boot_button = Input::new(p.GPIO0, InputConfig::default().with_pull(Pull::Up));

    let policy = Policy::default();
    let mut ledger = Ledger::<2>::new(&policy);

    let mut now = clock.startup_time();
    if let Some(t) = now {
        let (snapshot, _) = ledger.tick(t, [true; 2], [false; 2], &policy);
        show(&mut panel, &snapshot);
    }

    // --- radio + network -------------------------------------------------
    let (controller, interfaces) =
        esp_radio::wifi::new(p.WIFI, Default::default()).expect("wifi init failed");

    let rng = Rng::new();
    let seed = ((rng.random() as u64) << 32) | rng.random() as u64;

    let (stack, runner) = embassy_net::new(
        interfaces.station,
        NetConfig::dhcpv4(Default::default()),
        RESOURCES.init(StackResources::new()),
        seed,
    );

    spawner.spawn(net::connection(controller).unwrap());
    spawner.spawn(net::net_task(runner).unwrap());

    let cfg = net::wait_for_dhcp(stack).await;
    let Some(gateway) = cfg.gateway else {
        println!("medienzeit: no gateway from DHCP; cannot reach NTP or TR-064");
        park().await
    };

    match net::sntp_once(stack, gateway).await {
        Ok(t) => {
            if let Ok(before) = clock.now() {
                println!("rtc: drift vs sntp {}s", t - before);
            }
            match clock.set(t) {
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
    let mut last_fingerprint = None;

    loop {
        // The RTC is authoritative between SNTP syncs, so a missed tick or a slow
        // network call cannot make the ledger lose time.
        let t = clock.now().unwrap_or(state.last_tick + 1);

        let docked = [boot_button.is_high(), true];
        let (snapshot, events) = ledger.tick(t, docked, state.present, &policy);
        state.last_tick = t;

        for e in &events {
            report(e);
        }

        if last_presence.elapsed() >= PRESENCE_PERIOD {
            last_presence = Instant::now();
            state.refresh_presence(&mut fb, stack).await;
        }

        state.apply_blocks(&mut fb, stack, &snapshot).await;

        // e-paper is slow and finite: only redraw when something visible changed.
        let fingerprint = fingerprint(&snapshot);
        if last_fingerprint != Some(fingerprint) {
            last_fingerprint = Some(fingerprint);
            show(&mut panel, &snapshot);
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
fn fingerprint(s: &Snapshot<2>) -> (i32, u32, u32, bool, bool, [bool; 2]) {
    (
        s.balance_secs / 60,
        s.local.hour,
        s.local.minute,
        s.night,
        matches!(s.flow, medienzeit_core::Flow::Draining),
        s.docked,
    )
}

fn report(e: &Event) {
    match e {
        Event::Exhausted => println!("  [event] EXHAUSTED"),
        Event::Restored => println!("  [event] restored"),
        Event::Warning => println!("  [event] 5 minutes left"),
        Event::NightBegan => println!("  [event] night began"),
        Event::NightEnded => println!("  [event] night ended"),
        Event::UndockedAtNight { device } => {
            println!("  [event] {} TAKEN AWAY AT NIGHT", DEV_NAMES[*device])
        }
        Event::TimeJump { gap_secs } => println!("  [event] time jump {gap_secs}s ignored"),
    }
}

fn show(panel: &mut panel::Panel<'static>, snapshot: &Snapshot<2>) {
    let mut fbuf = panel::Framebuffer::default();
    let chrome = Chrome { device_names: DEV_NAMES };
    medienzeit_ui::render(&mut panel::InkTarget(&mut fbuf), snapshot, &chrome).unwrap();
    panel.present(&fbuf);
}

async fn park() -> ! {
    loop {
        Timer::after(Duration::from_secs(30)).await;
    }
}
