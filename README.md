# Medienzeit

A chess clock for screen time. A shared daily budget drains while a device is off its
charging cradle and stops while it is docked. A 1.54" e-ink display shows what is
left. At zero the Fritz!Box cuts the devices off the internet.

Enforcement lives in the network and in the room, not on the phones, so it works the
same for Android and iOS with nothing installed on either.

Full design, hardware BOM and milestones: `~/.claude/plans/so-i-have-issues-melodic-spring.md`.

## Layout

| Crate | |
|---|---|
| `core/` | `no_std`, no hardware. Civil-time/DST maths, the day boundary, the policy, the accounting ledger. All of it unit-tested on the host — this is where the subtle bugs would otherwise live. |
| `ui/` | `no_std`. Draws the 200x200 screen against any `embedded-graphics` `DrawTarget`, so the simulator and the real SSD1681 panel run the same code. |
| `sim/` | Host binary. A virtual display driving the **real** ledger and the **real** drawing code, with only the clock and the NFC readers faked. |
| `tr064/` | `no_std`. FRITZ!Box SOAP codec: request building, HTTP digest auth, response parsing. Does **no I/O**, which is why the firmware and the CLI below share it byte for byte. |
| `tr064-cli/` | Host binary, zero dependencies beyond `std::net`. Points the codec at a real FRITZ!Box so enforcement is proven before any firmware exists. |

`pn5180/` (ISO 15693 reader driver, written from scratch) and `firmware/` (ESP32-S3,
embassy) land in M2 and M3.

## Running

```sh
cargo test                       # 61 tests, all host-side
cargo run -p medienzeit-sim      # interactive virtual display (needs SDL2)
cargo run -p medienzeit-sim -- --shots shots/   # one PNG per screen state
```

Against a real FRITZ!Box (needs *Zugriff für Anwendungen zulassen* enabled, and a user
with box-settings permission):

```sh
export FRITZBOX_USER=medienzeit FRITZBOX_PASS=...
cargo run -p medienzeit-tr064-cli -- host 3C:22:FB:11:22:33   # MAC -> IP + online
cargo run -p medienzeit-tr064-cli -- block   192.168.178.42   # cut internet
cargo run -p medienzeit-tr064-cli -- status  192.168.178.42
cargo run -p medienzeit-tr064-cli -- unblock 192.168.178.42
```

`block` and `unblock` read the state back afterwards rather than trusting the
acknowledgement, and fail loudly if the box did not actually apply the change.

Simulator keys:

| | |
|---|---|
| `1` / `2` | dock or undock device 1 / 2 |
| `space` | pause the simulated clock |
| `up` / `down` | double or halve clock speed (starts at 60x — one budget-minute per second) |
| `b` | grant 10 bonus minutes |
| `n` | jump to 04:00 tomorrow (day reset) |
| `a` | jump to Monday 09:00 (inside the away window) |
| `s` | save a screenshot |
| `q` | quit |

Ledger events (`Exhausted`, `Restored`, `Warning`, `DayReset`, `TimeJump`) print to the
console as they fire. In the firmware these become TR-064 calls, ntfy pushes and a
chime; watching them here is how you check the edges fire exactly once.

## The rules, as implemented

- **Wall-clock, not device-minutes.** The pool drains at 1x whenever *any* device is
  undocked. Both out still costs 1x. See `Ledger::any_undocked`.
- **A 3-minute grace period** on every pickup, so grabbing a phone to skip a track,
  start a podcast or answer a message costs nothing. Billing is **retroactive**: cross
  the threshold and the *whole* pickup is charged, which is what stops short pickups
  being farmed into free time. Set `Policy::grace_secs` to 0 to disable.
- **Day resets at 04:00 local**, not midnight, so a late evening does not get a fresh
  allowance at the stroke of twelve. No carryover.
- **Weekday and weekend allowances** (default 60 and 120 minutes). The day that began
  Friday 04:00 keeps the weekday allowance through Friday night.
- **Away windows** (default Mon–Fri 07:30–15:00) suppress the clock entirely, so the
  school day does not drain the budget.
- **Fails closed on the clock**: a fresh `Ledger` assumes both devices are *docked*, so
  an NFC reader that never comes up cannot silently burn the whole day.
- **Time jumps over 5 minutes are not charged** — a crash or an NTP correction must not
  bill the budget.

## Known limits

These are design decisions, not bugs, and are worth stating to the kid openly:

- NTAG UIDs can be cloned and stickers can be moved. The mitigation is alerting, not
  prevention. NTAG424 DNA (~€1.50/tag, AES-CMAC) is the upgrade if it becomes a problem.
- One device has mobile data, which a router block does not touch.
- Offline use — downloaded video, offline games — is unaffected by a router block.
