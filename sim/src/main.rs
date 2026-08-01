//! Virtual Medienzeit display.
//!
//!     cargo run -p medienzeit-sim              # interactive window
//!     cargo run -p medienzeit-sim -- --shots   # write PNGs of every screen state
//!
//! The point of this binary is that it is *not* a mock: it runs the real
//! `medienzeit-core` ledger and the real `medienzeit-ui` drawing code, with only the
//! clock and the NFC readers faked. If a screen looks wrong here, it is wrong.

mod clock;
mod scenarios;

use std::time::{Duration, Instant};

use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics_simulator::{
    sdl2::Keycode, BinaryColorTheme, OutputSettings, OutputSettingsBuilder, SimulatorDisplay,
    SimulatorEvent, Window,
};
use medienzeit_core::{Event, Ledger, Policy};
use medienzeit_ui::{Chrome, HEIGHT, WIDTH};

use crate::clock::berlin;

/// Simulated seconds per real second at startup: one minute of budget per second.
const DEFAULT_SPEED: f64 = 60.0;
/// Never hand the ledger a gap it would reject as a time jump.
const MAX_LEDGER_STEP: i64 = 60;

fn output_settings() -> OutputSettings {
    OutputSettingsBuilder::new()
        .scale(3)
        .pixel_spacing(0)
        // `Inverted` puts dark pixels on a light background, which is how ink on
        // e-paper actually looks. `BinaryColor::On` == dark, matching the UI crate.
        .theme(BinaryColorTheme::Inverted)
        .build()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }
    if let Some(i) = args.iter().position(|a| a == "--shots") {
        let dir = args
            .get(i + 1)
            .filter(|s| !s.starts_with("--"))
            .cloned()
            .unwrap_or_else(|| "shots".to_string());
        return write_shots(&dir);
    }
    interactive()
}

fn print_help() {
    println!(
        "medienzeit-sim — virtual e-ink display\n\n\
         USAGE\n  \
           cargo run -p medienzeit-sim                 interactive window\n  \
           cargo run -p medienzeit-sim -- --shots DIR  write one PNG per screen state\n\n\
         KEYS (interactive)\n  \
           1 / 2     dock or undock device 1 / 2\n  \
           space     pause the simulated clock\n  \
           up/down   double or halve the clock speed\n  \
           b         grant 10 bonus minutes\n  \
           n         jump to 04:00 tomorrow (day reset)\n  \
           a         jump to Monday 09:00 (inside the away window)\n  \
           s         save a screenshot\n  \
           q / esc   quit\n"
    );
}

fn write_shots(dir: &str) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(dir)?;
    let settings = output_settings();
    let chrome = Chrome::default();

    for scenario in scenarios::all() {
        let mut display: SimulatorDisplay<BinaryColor> =
            SimulatorDisplay::new(Size::new(WIDTH, HEIGHT));
        medienzeit_ui::render(&mut display, &scenario.snapshot, &chrome)?;
        let path = format!("{dir}/{}.png", scenario.name);
        display.to_rgb_output_image(&settings).save_png(&path)?;
        println!("{path}  —  {}", scenario.description);
    }
    Ok(())
}

fn interactive() -> Result<(), Box<dyn std::error::Error>> {
    run_window(None).map(|_| ())
}

/// The windowed loop. `max_frames` bounds it so a headless smoke test can drive the
/// real SDL path — window creation, event polling, presentation — rather than only the
/// drawing code. Returns the final snapshot so the test has something to assert on.
fn run_window(
    max_frames: Option<usize>,
) -> Result<medienzeit_core::Snapshot<2>, Box<dyn std::error::Error>> {
    if max_frames.is_none() {
        print_help();
    }

    let policy = Policy::default();
    let chrome = Chrome::default();
    let settings = output_settings();

    let mut display: SimulatorDisplay<BinaryColor> =
        SimulatorDisplay::new(Size::new(WIDTH, HEIGHT));
    let mut window = Window::new("Medienzeit", &settings);

    let mut ledger = Ledger::<2>::new();
    // Monday 16:00: just after school, full weekday budget, nothing spent.
    let mut sim_now = berlin(2026, 8, 3, 16, 0);
    let mut speed = DEFAULT_SPEED;
    let mut paused = false;
    let mut docked = [true, true];
    let mut shot_index = 0usize;

    let (mut snapshot, _) = ledger.tick(sim_now, docked, &policy);

    // Prime the window before the loop: `Window::events()` panics if called before the
    // first `update()`, because the SDL window is created lazily on that first call.
    medienzeit_ui::render(&mut display, &snapshot, &chrome)?;
    window.update(&display);

    let mut last_frame = Instant::now();
    let mut frames = 0usize;

    'running: loop {
        let elapsed = last_frame.elapsed().as_secs_f64();
        last_frame = Instant::now();

        let mut jump_to: Option<i64> = None;
        for event in window.events() {
            match event {
                SimulatorEvent::Quit => break 'running,
                SimulatorEvent::KeyDown { keycode, repeat: false, .. } => {
                    if keycode == Keycode::Escape || keycode == Keycode::Q {
                        break 'running;
                    } else if keycode == Keycode::Num1 {
                        docked[0] = !docked[0];
                        println!("device 1 {}", if docked[0] { "docked" } else { "UNDOCKED" });
                    } else if keycode == Keycode::Num2 {
                        docked[1] = !docked[1];
                        println!("device 2 {}", if docked[1] { "docked" } else { "UNDOCKED" });
                    } else if keycode == Keycode::Space {
                        paused = !paused;
                        println!("clock {}", if paused { "paused" } else { "running" });
                    } else if keycode == Keycode::Up {
                        speed = (speed * 2.0).min(1_800.0);
                        println!("speed {speed}x");
                    } else if keycode == Keycode::Down {
                        speed = (speed / 2.0).max(1.0);
                        println!("speed {speed}x");
                    } else if keycode == Keycode::B {
                        ledger.grant_bonus(10 * 60);
                        println!("granted 10 bonus minutes");
                    } else if keycode == Keycode::N {
                        // Next day at 04:00 local — exercises the reset path.
                        let l = medienzeit_core::civil::local(sim_now);
                        jump_to = Some(berlin(l.year, l.month, l.day, 4, 0) + 86_400);
                    } else if keycode == Keycode::A {
                        jump_to = Some(berlin(2026, 8, 3, 9, 0));
                    } else if keycode == Keycode::S {
                        let path = format!("medienzeit-{shot_index:02}.png");
                        display.to_rgb_output_image(&settings).save_png(&path)?;
                        println!("saved {path}");
                        shot_index += 1;
                    }
                }
                _ => {}
            }
        }

        if let Some(target) = jump_to {
            sim_now = target;
            let (s, events) = ledger.tick(sim_now, docked, &policy);
            report(&events);
            snapshot = s;
        } else {
            let target = sim_now + if paused { 0 } else { (elapsed * speed) as i64 };
            // Step in <=60 s slices so a fast simulated clock is accounted exactly the
            // same way the 1 Hz firmware loop would account it.
            while sim_now < target {
                sim_now = (sim_now + MAX_LEDGER_STEP).min(target);
                let (s, events) = ledger.tick(sim_now, docked, &policy);
                report(&events);
                snapshot = s;
            }
            if paused {
                let (s, events) = ledger.tick(sim_now, docked, &policy);
                report(&events);
                snapshot = s;
            }
        }

        medienzeit_ui::render(&mut display, &snapshot, &chrome)?;
        window.update(&display);

        frames += 1;
        if max_frames.is_some_and(|m| frames >= m) {
            break 'running;
        }
        std::thread::sleep(Duration::from_millis(40));
    }

    Ok(snapshot)
}

/// The firmware turns these into TR-064 calls, ntfy pushes and a chime. Here we just
/// print them, which is enough to see that each edge fires exactly once.
fn report(events: &medienzeit_core::Events) {
    for e in events {
        match e {
            Event::DayReset => println!("  [event] day reset — budget restored"),
            Event::Exhausted => println!("  [event] EXHAUSTED — block both devices"),
            Event::Restored => println!("  [event] restored — unblock both devices"),
            Event::Warning => println!("  [event] 5 minutes left — chime"),
            Event::TimeJump { gap_secs } => {
                println!("  [event] time jump of {gap_secs}s ignored")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Boot the real windowed loop under SDL's dummy video driver.
    ///
    /// This exists because the `--shots` path constructs no `Window` at all: it
    /// exercises every line of drawing code and none of the windowing, which is how a
    /// panic in the very first frame once shipped. `Window::events()` is documented to
    /// panic unless `update()` has been called at least once, so the ordering inside
    /// the loop is load-bearing and deserves a test that would actually notice.
    ///
    /// The whole binary has exactly this one test, so setting the process-wide SDL
    /// driver here cannot race another thread.
    #[test]
    fn windowed_loop_survives_its_first_frames_headless() {
        std::env::set_var("SDL_VIDEODRIVER", "dummy");

        let snapshot = run_window(Some(3)).expect("simulator should start headless");

        // Startup state: Monday 16:00, both devices on their cradles, full weekday
        // budget, clock deliberately still.
        assert_eq!(snapshot.docked, [true, true]);
        assert!(!snapshot.spending);
        assert!(!snapshot.exhausted);
        assert_eq!(snapshot.allowance_secs, 60 * 60);
    }
}
