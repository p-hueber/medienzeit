//! The half of the firmware that talks to hardware in the room.
//!
//! Reader, panel, chime and RTC, plus the ledger they drive. Everything here is
//! synchronous and everything here blocks: a panel refresh is about a second, a reader
//! round is 190 ms. That is the point — see `docs/two-core-split.md`. Keeping it out of
//! the async half is what stops those stalls reaching the network stack.
//!
//! Exposed as [`Room::step`] rather than a loop so the caller owns the pacing. Today an
//! embassy task drives it on core 0 exactly as the old control loop did; the next step
//! is a plain thread on core 1. Nothing in here changes when that happens, which is the
//! reason it is shaped this way.

use embassy_time::Instant;
use esp_println::println;

use medienzeit_core::{Event, Ledger, Policy, Snapshot};

use crate::{chime, panel, reader, rtc, shared, DEV_NAMES};

/// How often to journal the balance. A power cut costs at most this much.
const JOURNAL_PERIOD: u64 = 30;

/// Quick refreshes before forcing a full one to clear accumulated ghosting.
///
/// At one update per minute that is a flash every half hour, which is roughly the
/// point at which ghosting becomes noticeable on this panel.
const QUICK_REFRESHES_PER_FULL: u32 = 30;

pub struct Room {
    panel: panel::Panel<'static>,
    screen: Screen,
    nfc: reader::Reader<'static>,
    scan: reader::Scan,
    docking: reader::Docking,
    boot: esp_hal::gpio::Input<'static>,
    chime: Option<chime::Chime<'static>>,
    i2c: rtc::Bus<'static>,
    ledger: Ledger<2>,
    policy: Policy,
    /// `None` until the clock is trustworthy. The ledger is never ticked before then:
    /// `tick` believes its timestamp, and a garbage post-reset RTC value would compute
    /// a nonsense day and reset the budget.
    last_tick: Option<i64>,
    last_journal: Instant,
    last_persisted_flow: Option<medienzeit_core::Flow>,
    /// Held rather than built per redraw. At 200x200 mono it is 5 KB, and on the stack
    /// it would set the floor for core 1's stack size on a device with no DRAM to spare.
    fbuf: panel::Framebuffer,
    round: crate::timing::Stats,
    last_docked: [bool; 2],
    last_recoveries: u32,
}

impl Room {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        panel: panel::Panel<'static>,
        nfc: reader::Reader<'static>,
        scan: reader::Scan,
        docking: reader::Docking,
        boot: esp_hal::gpio::Input<'static>,
        chime: Option<chime::Chime<'static>>,
        i2c: rtc::Bus<'static>,
        ledger: Ledger<2>,
        policy: Policy,
    ) -> Self {
        Self {
            panel,
            screen: Screen::default(),
            nfc,
            scan,
            docking,
            boot,
            chime,
            i2c,
            ledger,
            policy,
            last_tick: None,
            last_journal: Instant::now(),
            last_persisted_flow: None,
            fbuf: panel::framebuffer(),
            round: crate::timing::Stats::new("reader round", 60),
            // Matches Docking's own starting assumption, so the first real reading logs
            // a transition only if it actually differs.
            last_docked: [true; 2],
            last_recoveries: 0,
        }
    }

    /// Draw whatever can be known before the network is up.
    ///
    /// Worth doing: the RTC alone is often enough, and a blank screen for the seconds
    /// it takes to associate looks like a dead device.
    pub fn show_startup(&mut self) {
        let Some(t) = rtc::startup_time(&mut self.i2c) else {
            println!("room: no trustworthy clock yet, holding accounting");
            return;
        };
        self.last_tick = Some(t);
        let (snapshot, _) = self.ledger.tick(t, [true; 2], [false; 2], &self.policy);
        self.screen.draw(&mut self.panel, &mut self.fbuf, &snapshot);
        shared::publish_snapshot(snapshot);
    }

    /// One pass: read the world, fold it into the ledger, act on the result.
    pub fn step(&mut self) {
        self.take_clock_correction();
        self.take_policy_change();

        // The RTC is authoritative between SNTP syncs, so a missed tick or a slow
        // network call cannot make the ledger lose time.
        let Some(previous) = self.last_tick else {
            return;
        };
        let t = rtc::now(&mut self.i2c).unwrap_or(previous + 1);

        // A grant from the admin page arrives out of band; apply it before the tick so
        // the new balance is what gets journalled and displayed this second.
        if let Some(secs) = crate::web::BONUS.try_take() {
            self.ledger.grant_bonus(secs, &self.policy);
            println!("room: +{secs}s granted");
        }

        let docked = self.poll_reader();
        let (snapshot, events) =
            self.ledger
                .tick(t, docked, shared::presence(), &self.policy);
        self.last_tick = Some(t);

        for e in &events {
            report(e);
            // The chime exists for exactly one event. Everything else is on the display,
            // where it can be read rather than interpreted.
            if matches!(e, Event::Warning) {
                if let Some(c) = self.chime.as_mut() {
                    c.warning();
                }
            }
        }

        self.screen.draw(&mut self.panel, &mut self.fbuf, &snapshot);
        shared::publish_snapshot(snapshot.clone());
        self.maybe_persist(&snapshot, t);
    }

    /// Run the room forever. This is core 1's main thread.
    ///
    /// Paced to a deadline rather than by sleeping a second at the end, because the work
    /// takes a large and variable fraction of the period: a redraw is about a second on its
    /// own, so `delay(1s)` after the work would make the real cadence closer to two.
    ///
    /// A late iteration gives up the beat it missed instead of running twice to catch up.
    /// Nothing is owed — `Ledger::tick` bills from the RTC timestamp, so a skipped beat
    /// costs no time, and running two reader rounds back to back would only make the
    /// overrun worse.
    pub fn run(&mut self) -> ! {
        use esp_hal::time::{Duration, Instant};
        const PERIOD: Duration = Duration::from_secs(1);

        // Done here rather than by the caller so core 0 is not held up by a full refresh,
        // which is the better part of two seconds.
        self.show_startup();

        let mut next = Instant::now() + PERIOD;
        loop {
            esp_rtos::CurrentThreadHandle::get().delay_until(next);
            self.step();
            let now = Instant::now();
            next = if next > now { next + PERIOD } else { now + PERIOD };
        }
    }

    /// Apply a validated wall clock to the RTC, if the network has produced one.
    fn take_clock_correction(&mut self) {
        let Some(t) = shared::SNTP.try_take() else {
            return;
        };
        if let Ok(before) = rtc::now(&mut self.i2c) {
            println!("rtc: drift vs sntp {}s", t - before);
        }
        match rtc::set(&mut self.i2c, t) {
            Ok(()) => println!("rtc: set from sntp"),
            Err(e) => println!("rtc: set failed ({e:?})"),
        }
        // This is also what releases accounting when the RTC alone was not trustworthy
        // enough to start on.
        if self.last_tick.is_none() {
            println!("room: clock trusted, accounting starts");
        }
        self.last_tick = Some(t);
    }

    fn take_policy_change(&mut self) {
        if let Some(p) = shared::policy() {
            self.policy = p;
        }
    }

    /// Poll the reader and publish what it saw, emitting the edge-triggered alerts.
    ///
    /// Alerts belong here rather than in a published cell because they are edges: a cell
    /// the caller polled would re-fire them every second.
    fn poll_reader(&mut self) -> [bool; 2] {
        let (nfc, scan) = (&mut self.nfc, &mut self.scan);
        // `scan.poll` returns None when the reader failed, and Docking must see that as
        // a failure: `Some(&[])` would say "the reader is fine and nothing is there",
        // which is what makes a broken reader start the clock.
        let seen = self.round.time(|| scan.poll(nfc));
        let d = self.docking.update(seen, [self.boot.is_high(), true]);

        // Reads can keep succeeding while the front end is quietly having to be cycled,
        // and a rising count is the early warning.
        let r = self.nfc.recoveries();
        if r != self.last_recoveries {
            println!("reader: rf recoveries {r}");
            self.last_recoveries = r;
        }

        // Log the transition, not the state: at the balance cap, filling and held look
        // identical in the numbers, so this is what shows identity driving the ledger.
        if d.docked != self.last_docked {
            for (i, (&now, &before)) in d.docked.iter().zip(self.last_docked.iter()).enumerate() {
                if now != before {
                    println!(
                        "reader: {} {}",
                        DEV_NAMES[i],
                        if now { "zurückgelegt" } else { "genommen" }
                    );
                }
            }
            self.last_docked = d.docked;
        }

        if let Some(uid) = d.unknown {
            let hex = reader::uid_hex(&uid);
            println!("  [alert] unknown tag {hex}");
            let mut m: crate::notify::Message = heapless::String::new();
            let _ = core::fmt::Write::write_fmt(
                &mut m,
                format_args!("Unbekannter Tag {hex} am Leser"),
            );
            crate::notify::send(&m);
        }
        if d.reader_fault {
            println!("  [alert] reader not responding");
            let mut m: crate::notify::Message = heapless::String::new();
            let _ = core::fmt::Write::write_fmt(&mut m, format_args!("Leser antwortet nicht"));
            crate::notify::send(&m);
        }
        d.docked
    }

    /// Ask for a journal write on a timer, and immediately whenever the flow changes —
    /// the moment spending starts or stops is exactly when a stale record would be wrong
    /// by the largest amount.
    fn maybe_persist(&mut self, snapshot: &Snapshot<2>, t: i64) {
        let flow_changed = self.last_persisted_flow != Some(snapshot.flow);
        let due = self.last_journal.elapsed().as_secs() >= JOURNAL_PERIOD;
        if flow_changed || due {
            self.last_journal = Instant::now();
            self.last_persisted_flow = Some(snapshot.flow);
            shared::PERSIST.signal((snapshot.balance_secs, t));
        }
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
/// E-paper updates are slow and the panel has a finite number of them, so the screen is
/// only touched when something visible changed. Most of those changes are one digit of a
/// countdown, which the quick waveform handles without flashing the whole panel.
#[derive(Default)]
struct Screen {
    last: Option<Fingerprint>,
    quick_since_full: u32,
}

impl Screen {
    fn draw(
        &mut self,
        panel: &mut panel::Panel<'static>,
        fbuf: &mut panel::Framebuffer,
        snapshot: &Snapshot<2>,
    ) {
        let now = fingerprint(snapshot);
        let Some(previous) = self.last else {
            // First frame of the session: nothing is on the panel we can trust.
            self.present(panel, fbuf, snapshot, now, panel::Refresh::Full);
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
        self.present(panel, fbuf, snapshot, now, mode);
    }

    fn present(
        &mut self,
        panel: &mut panel::Panel<'static>,
        fbuf: &mut panel::Framebuffer,
        snapshot: &Snapshot<2>,
        fp: Fingerprint,
        mode: panel::Refresh,
    ) {
        // Timed because this is the longest blocking call in the firmware and its cost
        // was, until measured, a datasheet typical.
        let started = Instant::now();
        show(panel, fbuf, snapshot, mode);
        println!(
            "screen: {:?} redraw at {:02}:{:02}, balance {}s, took {}",
            mode,
            snapshot.local.hour,
            snapshot.local.minute,
            snapshot.balance_secs,
            crate::timing::Ms(started.elapsed().as_micros())
        );
        self.last = Some(fp);
        self.quick_since_full = match mode {
            panel::Refresh::Full => 0,
            panel::Refresh::Quick => self.quick_since_full + 1,
        };
    }
}

fn show(
    panel: &mut panel::Panel<'static>,
    fbuf: &mut panel::Framebuffer,
    snapshot: &Snapshot<2>,
    mode: panel::Refresh,
) {
    let chrome = medienzeit_ui::Chrome { device_names: DEV_NAMES };
    medienzeit_ui::render(&mut panel::InkTarget(fbuf), snapshot, &chrome).unwrap();
    panel.present(fbuf, mode);
}

/// Log every event; push only the ones a parent would want to know about away from the
/// house. Alerting on routine transitions would train you to ignore the channel, which
/// costs more than the missed information.
fn report(e: &Event) {
    use core::fmt::Write as _;
    let mut push: crate::notify::Message = heapless::String::new();
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
        crate::notify::send(&push);
    }
}
