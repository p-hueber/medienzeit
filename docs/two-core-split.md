# Plan: no blocking calls under `async fn`

## The problem

Every task in the firmware shares one thread-mode executor, so cooperative scheduling
means one task's blocking call is every task's blocking call. Five places block:

Measured on hardware 2026-09-06, not estimated. The first two lines came in far worse
than the guesses this plan originally carried (~2 s / "a few hundred ms" for the panel,
~200 ms for the reader), which strengthens the case rather than weakening it.

| Where | Cost per call | How often |
|---|---|---|
| `panel.present()` — `panel.rs:166`, via `Screen::draw` at `main.rs:452` | **full 1812 ms, quick ~1050 ms** | quick ~1/min, full every 30 quicks or on lockout |
| `reader::task` 16-slot inventory — `reader.rs:456` | **avg 610–620 ms, max 1208 ms** | 1 Hz, so **~62% duty** |
| `chime.warning()` — `chime.rs:200` | up to 1 s, capped by the assert at `chime.rs:53` | one event, and currently silent |
| `journal.append()` / `settings_store.save()` — `main.rs:357`, `:308` | <1 ms for a 20-byte record; tens of ms for a 4 KB sector erase | record every 30 s; erase ~hourly (`storage.rs:8`) |
| `rtc::now(&mut i2c)` — `main.rs:303` | ~1 ms | 1 Hz |

The panel is the worst, not the reader. Moving only the reader to another core would
leave a multi-second freeze every half hour.

Everything else — `net`, `web`, `notify`, and the TR-064 calls in `refresh_presence` /
`apply_blocks` — is genuinely async and awaits real sockets.

## The line

**Core 1 is the room. Core 0 is the network.**

Core 1 gets every blocking hardware driver *and the ledger*, and runs as a plain
synchronous thread with no executor at all. Core 0 keeps everything async and gains the
invariant that a blocking call there is a bug.

The ledger goes to core 1 because both of its per-second inputs — the RTC timestamp and
the docking state — are core-1 hardware, and one of its outputs is the panel. Its only
network input is `present`, a `[bool; 2]` refreshed every 30 s.

| | Core 1 (`room`) | Core 0 (`main`) |
|---|---|---|
| Peripherals | SPI2 + panel pins, SPI3 + PN5180 pins, I2C0 + GPIO47/48, I2S0 + DMA_CH0 + audio pins, GPIO0, GPIO6, GPIO42 | WIFI, FLASH |
| Owns | `Panel`, `Reader`, `Docking`, `Chime`, RTC, `Ledger`, `Screen` | `embassy-net`, `web`, `notify`, `fritzbox::Client`, `Journal`, `SettingsStore` |
| Style | synchronous `loop`, no executor | async; nothing blocks longer than the network stack already tolerates |

I²C0 is shared between the PCF85063 and the ES8311 (`chime::Chime::new` borrows it).
Both land on core 1, so the sharing stays a single-owner `&mut` exactly as today.

## The interface

All of it is `CriticalSectionRawMutex`, which on multicore esp-hal is a real spinlock —
so the pattern already used by `reader::DOCKED`, `web::STATE` and `notify::QUEUE` extends
across cores unchanged. That is the main reason this refactor is cheap.

New `src/shared.rs`:

| Cell | Direction | Type |
|---|---|---|
| `PRESENCE` | 0 → 1 | `Mutex<RefCell<[bool; 2]>>` |
| `POLICY` | 0 → 1 | `Mutex<RefCell<Policy>>` |
| `SNTP` | 0 → 1 | `Signal<i64>`, one-shot clock correction |
| `PERSIST` | 1 → 0 | `Signal<(i32, i64)>`, "journal this balance and timestamp" |

Reused unchanged: `web::STATE` (snapshot 1 → 0), `web::BONUS` (0 → 1), `notify::QUEUE` —
already `try_send`, non-blocking, safe to call from core 1.

`reader::DOCKED` **disappears**. The reader and the ledger are on the same core now, so
it becomes a local value passed straight into `ledger.tick`.

`web::PENDING` and `web::take_settings()` **disappear** — see below.

### Edges are delivered, never polled

Core 1's loop polls hardware, which is what a 1 Hz loop is for. Every *software* event
goes to whoever owns the state it mutates, at the moment it happens.

| Crossing | Kind | Mechanism |
|---|---|---|
| Settings save | edge, human-initiated | Handled entirely in the web POST on core 0: validate → persist → publish `POLICY` only on a successful write → respond |
| `POLICY` → ledger | not an edge | Core 1 reads the cell as the `&Policy` argument it already passes to `tick` |
| Bonus grant | edge, mutates the ledger | `Signal`, consumed by core 1 once per tick — it must be core 1, which owns `Ledger`. 1 s latency is invisible to a button press |
| Journal write | edge (flow change) or 30 s timer | `Signal` from core 1, awaited by a small `journal` task on core 0 |

Handling the settings save at the POST also fixes a small honesty bug: `web.rs:245` sets
`saved = Some(true)` when the change is *queued*, so the page reports success before the
flash write happens, and a failed write survives only as a `println!`.

The ordering rule at `main.rs:305` — persist before applying, so a power cut cannot leave
the running and stored rules disagreeing — survives and tightens. Today it spans a loop
iteration; afterwards it is persist-then-publish inside one handler.

### Flash is the one blocking call left on core 0

"Core 0 has no blocking calls" would be false, so it is not the rule. The rule is a
threshold: **core 0 must not block for longer than the network stack already tolerates.**

Every long blocker moved to core 1 — panel (~2 s), reader (~200 ms), chime (~1 s), and
the RTC I²C read, which goes along because I²C0 is shared with the ES8311 codec. Flash is
what remains, and it clears the bar rather than being excused from it: sub-millisecond
for a 20-byte record, tens of milliseconds for the roughly hourly sector erase, against
100 ms Wi-Fi beacons and TCP retransmit timers in the hundreds of milliseconds.

Flash stays on core 0 despite the ledger moving, because `multicore_auto_park` parks *the
other* core. The stall is the same either way; only its target differs. From core 0 it
parks core 1, costing one reader poll. From core 1 it would park core 0, freezing Wi-Fi
and the network stack mid-flight.

## Boot sequence

Core 0, in order:

1. `esp_hal::init`, heap, `esp_rtos::start(timg0.timer0, sw.software_interrupt0)`.
2. Open flash; recover balance and settings. **Must precede core 1**, which needs the
   starting balance and policy.
3. `esp_rtos::start_second_core(p.CPU_CTRL, sw.software_interrupt1, STACK, move || room(...))`,
   moving the peripherals and the recovered state into the closure.
4. Wi-Fi, DHCP, SNTP → `SNTP.signal(t)`.
5. Outage detection — needs `recovered.last_tick` and a validated clock, both here.
6. The async control loop: presence and enforcement, on their own timers. Journaling is
   a separate task awaiting `PERSIST`; settings are handled in the web POST. Neither is
   polled from here.

Core 1, in `room()`:

1. Init panel, reader (`identify`, `start_rf`), chime.
2. `rtc::startup_time` → first frame.
3. 1 Hz loop: read RTC → poll reader → `ledger.tick` → `screen.draw` →
   `web::publish` → set `PERSIST` when the flow changes or 30 s have passed.

### The clock gate gets better, not worse

Today a failed SNTP calls `park()` and the whole device does nothing. Under the split,
core 1 runs on `rtc::startup_time` (which already has a build-time plausibility floor)
and holds in "time unknown" only if that is `None`, correcting itself when `SNTP` fires.
The rule from the design plan — never `tick` on an untrusted timestamp — is preserved,
but a missing NTP server no longer costs the display and the reader.

## The shape of the room loop

A superloop, not an event loop. Everything on core 1 is periodic at 1 Hz — RTC read,
reader poll, `ledger.tick`. Exactly two things are edge-triggered and both already have
their conditional factored: the panel, via the fingerprint comparison in `Screen::draw`
(`main.rs:441`), and the chime, via the `Event::Warning` that `tick` already returns. A
dispatcher for two `if`s would be worse than the two `if`s.

**Pace it with a deadline, not a sleep.** Today the loop ends in `Timer::after(1s)`, so
the real period is one second *plus* the work — drifting ~200 ms per iteration from the
reader alone. Core 1 computes the next deadline and uses
`esp_rtos::CurrentThreadHandle::get().delay_until(..)`
(`esp-rtos-0.3.0/src/task/mod.rs:675`), a real thread sleep that yields to the scheduler.
**Not** `embassy_time::block_for`, which busy-waits.

**Serialising the long calls is safe.** `Ledger::tick` bills `gap = utc - last` from the
RTC timestamp (`state.rs:221`) with a 300 s tolerance, so a late iteration bills the
elapsed time correctly rather than losing a second.

| Case | Cost | Frequency |
|---|---|---|
| idle | ~620 ms reader + ~1 ms RTC | most seconds |
| quick refresh | ~1.7 s — **misses the deadline** | ~1/min |
| full refresh | ~2.4 s — **misses the deadline** | every ~30 min |
| chime | +1 s | per Warning event |

**Every redraw overruns, not just the full ones.** An earlier draft of this plan claimed
only the full refresh missed the deadline; that was based on estimates that measurement
disproved. Worst-case reader detection latency is therefore ~2.4 s, against a 180 s grace
window — still immaterial, and accounting is unaffected because `tick` is driven by the
RTC timestamp rather than by iteration count.

The honest summary is that core 1 will be busy roughly two thirds of the time and will
skip a beat once a minute. That is fine for what it does, and it is precisely why none of
it belongs on the core running the network stack.

**Not attempted: concurrency within core 1.** esp-rtos 0.3 exposes no thread-spawn API,
so the only mechanism is an interrupt executor — and putting a 200 ms blocking SPI round
at interrupt priority to preempt the panel is worse than the problem. If panel latency
ever bites, the honest answer is that a display is allowed to lag.

## esp-storage must be told about the second core

`FlashStorage` defaults to `MultiCoreStrategy::Error` (`common.rs:116`). The moment core
1 runs, every journal write returns `OtherCoreRunning`. `storage::Journal::open` must
call `.multicore_auto_park()` (`common.rs:252`), which parks core 1 for the duration of
the write and unparks it after.

Cost: the reader and panel freeze for <1 ms on each 30 s record, and tens of ms on the
roughly hourly sector erase. Acceptable — one missed reader poll, less than hourly.

It fails loudly rather than silently, which is worth noting given this project's three
prior silent-truncation bugs.

## Stack

`show()` at `main.rs:565` builds a `Framebuffer` as a local — 200×200/8 = 5 000 bytes on
the stack. That is a **move** from core 0's main stack, not an addition. Budget 16 KB for
core 1's `Stack<N>` and watch the guard.

## Risks, in the order I'd retire them

1. **esp-radio under SMP.** Wi-Fi initialises on core 0 and its threads are scheduled by
   esp-rtos; starting core 1 makes that scheduler SMP. It is a supported configuration —
   `start_second_core` ships alongside the `esp-radio` feature — but it is the one thing
   that would invalidate the whole plan, so it gets tested first and alone.
2. **`Send` bounds.** Every peripheral, `Spi`, `Output`, `Input` and the
   `ExclusiveDevice`/`Delay` inside `Panel` must be `Send` to cross into the closure.
   Compile-time, cheap to find, potentially annoying to fix.
3. **Interrupt affinity — believed a non-issue.** SPI2/SPI3/I2C0/I2S0 are all used in
   blocking mode (`Spi<'d, esp_hal::Blocking>`), and GPIO0 is polled with `is_high()`
   rather than an interrupt. Nothing on core 1 registers a handler.
4. **Interleaved logs — a non-issue.** `esp-println` takes an `esp_sync::RawMutex`
   (`lib.rs:540`), so concurrent `println!` from two cores will not garble.

## Step 0 result (2026-09-06): go

Flashed with core 1 started *before* Wi-Fi, running nothing but a 5 s heartbeat.

- **esp-radio survives SMP.** Associated, DHCP lease, `rtc: drift vs sntp 0s`. This was
  the risk that could have killed the design; it did not materialise.
- **Auto-park works.** No `journal: write failed` across ~5 appends. Appends are silent
  on success and loud on failure (`storage.rs:115`), and with the old default
  (`MultiCoreStrategy::Error`) every one would have failed once core 1 existed.
- **The heartbeat never missed**, including across journal writes.
- Balance survived the reflash: `journal: recovered balance 7956s from slot 575`.

Open question the numbers raise: **620 ms for a 16-slot inventory is suspicious.** The
SPI traffic is tiny even at 1 MHz, so the time is almost certainly fixed delays or
timeouts in the driver — roughly 38 ms per slot. It is the single largest CPU consumer in
the firmware and may be cheap to cut. Worth investigating on its own, independently of
this refactor.

## Sequencing

**Step 0 — measure and spike, one flash.** **Done.** Bracket `panel.present()` and the reader
round with `Instant::now()` and log the elapsed time, so the "before" numbers are
measured rather than guessed. In the same build, add `start_second_core` with an *empty*
closure plus `.multicore_auto_park()` on the flash, and confirm Wi-Fi still associates
and the journal still writes. Ten lines, and it retires risk 1 before any refactoring.

**Step 1 — pure refactor, no core change.** Add `shared.rs`, extract the control loop
into `room()` and an async remainder, still both on core 0. Behaviour identical, and the
whole thing is bench-verifiable.

**Step 2 — flip.** Move `room()` into the `start_second_core` closure.

**Step 3 — remeasure** against step 0's numbers.

## What this changes elsewhere

- **An async PN5180 driver is no longer wanted.** The "Also outstanding" item in the
  design plan can be dropped: the driver stays synchronous and that becomes correct
  rather than tolerated.
- **The reader task's own doc comment** (`reader.rs:449`) currently explains that the
  task is organisational and does not make polling concurrent. After this it does.
- **`Screen` becomes testable.** Once the loop is straight-line synchronous, its body
  extracts as a pure step function, host-testable the way `Docking` now is. The refresh
  decision — quick vs full, and the lockout-inversion case at `main.rs:449` — is exactly
  the kind of logic this project has been bitten by and currently has no test for. Not
  part of this refactor; cheap afterwards.
- **Web latency** is not directly addressed. `web`, `net` and `notify` still share core
  0's executor — but all three are genuinely async, so cooperative scheduling is right
  there. If the admin page is still slow afterwards, the cause is elsewhere (socket
  budget, or the 8 KB per-acceptor page buffers), and this refactor will have ruled out
  the obvious suspect.
