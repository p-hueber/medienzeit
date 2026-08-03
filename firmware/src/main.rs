#![no_std]
#![no_main]

mod net;
mod panel;
mod rtc;

// Pulls in the panic handler and the backtrace printer; not referenced directly.
use esp_backtrace as _;

use embassy_executor::Spawner;
use embassy_net::{Config as NetConfig, StackResources};
use embassy_time::{Duration, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::rng::Rng;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use static_cell::StaticCell;

use medienzeit_core::{Ledger, Policy, Snapshot};
use medienzeit_ui::Chrome;

esp_bootloader_esp_idf::esp_app_desc!();

const DOCKED: [bool; 2] = [true, true];
const HOME: [bool; 2] = [true, true];

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
    let _audio_power = esp_hal::gpio::Output::new(
        p.GPIO42,
        esp_hal::gpio::Level::Low,
        esp_hal::gpio::OutputConfig::default(),
    );
    embassy_time::block_for(Duration::from_millis(20));

    let mut i2c = rtc::bus(p.I2C0, p.GPIO47, p.GPIO48);
    rtc::scan(&mut i2c);
    let mut clock = rtc::Rtc::new(i2c);
    let policy = Policy::default();
    let mut ledger = Ledger::<2>::new(&policy);

    // Fast path: if the RTC is trustworthy we can show a correct screen within a second
    // of power-on, instead of a blank panel until the network comes up.
    let mut have_time = false;
    if let Some(t) = clock.startup_time() {
        have_time = true;
        let (snapshot, _) = ledger.tick(t, DOCKED, HOME, &policy);
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

    match cfg.gateway {
        Some(gw) => match net::sntp_once(stack, gw).await {
            Ok(t) => {
                // Report the drift before correcting it: this is the only place we ever
                // learn whether the RTC is keeping decent time.
                if let Ok(before) = clock.now() {
                    println!("rtc: drift vs sntp {}s", t - before);
                }
                match clock.set(t) {
                    Ok(()) => println!("rtc: set from sntp"),
                    Err(e) => println!("rtc: set failed ({e:?})"),
                }
                have_time = true;
                let (snapshot, _) = ledger.tick(t, DOCKED, HOME, &policy);
                println!(
                    "medienzeit: {:02}:{:02} local, balance {}s",
                    snapshot.local.hour, snapshot.local.minute, snapshot.balance_secs
                );
                show(&mut panel, &snapshot);
            }
            Err(e) => println!("medienzeit: SNTP failed: {e}"),
        },
        None => println!("medienzeit: no gateway from DHCP, cannot reach NTP"),
    }

    // Only a board with neither a valid RTC nor a successful sync has nothing to show.
    // Rendering a plausible-looking wrong screen would be worse than rendering none.
    if !have_time {
        println!("medienzeit: no trustworthy clock, not accounting");
    }

    loop {
        Timer::after(Duration::from_secs(30)).await;
    }
}

fn show(panel: &mut panel::Panel<'static>, snapshot: &Snapshot<2>) {
    let mut fb = panel::Framebuffer::default();
    medienzeit_ui::render(&mut panel::InkTarget(&mut fb), snapshot, &Chrome::default()).unwrap();
    panel.present(&fb);
}
