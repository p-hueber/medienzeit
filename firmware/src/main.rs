#![no_std]
#![no_main]

mod panel;

// Pulls in the panic handler and the backtrace printer; not referenced directly.
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::delay::Delay;
use esp_println::println;

use medienzeit_core::{Ledger, Policy};
use medienzeit_ui::Chrome;

esp_bootloader_esp_idf::esp_app_desc!();

const DOCKED: [bool; 2] = [true, true];
const HOME: [bool; 2] = [true, true];

#[esp_hal::main]
fn main() -> ! {
    let p = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    let delay = Delay::new();
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
    println!("medienzeit: panel up");

    // No clock yet, so drive the real ledger from a fixed instant. The point of this
    // step is to prove that the same `ui` code that renders the simulator PNGs also
    // renders on the panel — not to be correct about the time.
    let policy = Policy::default();
    let mut ledger = Ledger::<2>::new(&policy);
    // 2026-08-03 16:00 Europe/Berlin.
    let (snapshot, _) = ledger.tick(1_785_945_600, DOCKED, HOME, &policy);
    println!("medienzeit: balance {}s", snapshot.balance_secs);

    let mut fb = panel::Framebuffer::default();
    medienzeit_ui::render(&mut panel::InkTarget(&mut fb), &snapshot, &Chrome::default()).unwrap();
    panel.present(&fb);
    println!("medienzeit: frame presented");

    let mut n = 0u32;
    loop {
        println!("alive {n}");
        n = n.wrapping_add(1);
        delay.delay_millis(5_000);
    }
}
