# Medienzeit

A chess clock for screen time. A shared balance refills while the devices are put back
at the reader — or away from home — and drains while they are in use at home. A 1.54"
e-ink display shows what is left. At zero, the FRITZ!Box cuts the devices off the
internet, but only the ones that have been taken away.

Enforcement lives in the network and in the room, not on the phones, so it works the
same for Android and iOS with nothing installed on either.

The installation shape is deliberately undecided. Nothing in the code assumes the
devices go in a container, on a stand, or anywhere in particular — only that they are
within the reader's field or they are not.

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

## Configuration, and what must never be committed

**This repository is public.** Everything device-specific lives in files under
`firmware/` that are gitignored and have never been tracked:

| File | Holds |
|---|---|
| `wifi.toml` | SSID and PSK |
| `fritzbox.toml` | TR-064 user and password, device names and MACs |
| `web.toml` | admin page credentials |
| `ntfy.toml` | push endpoint and **topic** |
| `tags.toml` | the NFC sticker UIDs |

`build.rs` reads them at compile time and prints a template if one is missing, so a
fresh clone tells you what it wants rather than failing obscurely.

**The ntfy topic is a credential**, even though it does not look like one: ntfy uses it
as a bearer token for publishing *and* subscribing, so anyone holding it can post alerts
into this household or read them. It is deliberately not logged — the firmware prints
only a four-character prefix — because serial output is the thing most likely to end up
pasted into an issue.

If you add a new configuration file, add it to `firmware/.gitignore` in the same commit.

## Running

```sh
cargo test                       # 128 tests, all host-side
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
| `1` / `2` | put device 1 / 2 back, or take it away |
| `3` / `4` | toggle device 1 / 2 on the home network (simulate leaving the house) |
| `space` | pause the simulated clock |
| `up` / `down` | double or halve clock speed (starts at 60x — one budget-minute per second) |
| `b` | grant 10 bonus minutes |
| `n` | jump forward one hour |
| `a` | jump to 22:30 (inside the night window) |
| `s` | save a screenshot |
| `q` | quit |

Ledger events (`Exhausted`, `Restored`, `Warning`, `NightBegan`, `NightEnded`,
`UndockedAtNight`, `TimeJump`) print to the console as they fire. In the firmware these become TR-064 calls, ntfy pushes and a
chime; watching them here is how you check the edges fire exactly once.

## The rules, as implemented

A single **balance**, in seconds. There is no daily allowance and no reset — the bucket
is the whole model.

| Situation | Balance | Internet |
|---|---|---|
| Put back, at home | fills | on |
| In use at home | drains 1× | on |
| Brief pickup, under `grace_secs` | held | on |
| …that pickup runs long | drains, **billed from the pickup** | on |
| Out of the house (off the home network) | fills | on |
| Night, put back | fills | on |
| Night, taken away | held | **blocked** + alert |
| Balance at zero, taken away | drains toward the floor | **blocked** |
| Balance below zero, put back | fills back out of the hole | on |

- **Refill is a ratio, not a rate.** The default 1:10 means "ten minutes not using earns
  one minute of screen time", so a 21:00–07:00 night at the reader funds an hour. A ratio
  explains to a child; a rate does not. Kept rational so the accounting is integer-exact.
- **Wall clock, not device-minutes.** Any device out drains at 1×. Both out still 1×.
- **Being out earns exactly what being docked earns.** Anything else penalises her for
  leaving the house, which is the opposite of the intent.
- **A device at the reader is never blocked.** That is what keeps a reason to put it
  back once the balance is gone — and the negative floor is what keeps that reason alive past zero.
- **Grace is retroactive.** Cross the threshold and the *whole* pickup is charged, which
  is what stops short pickups being farmed into free time.
- **`prefill_secs`** seeds a fresh ledger so day one is not spent staring at an empty bank.
- **Fails closed**: a fresh ledger assumes every device is at the reader and away from
  home, so
  neither a dead NFC reader nor an unreachable FRITZ!Box can drain the balance.
- **Time jumps over 5 minutes are neither charged nor credited** — a crash or an NTP
  correction must not move the balance.

## Known limits

These are design decisions, not bugs, and are worth stating to the kid openly:

- NTAG UIDs can be cloned and stickers can be moved. The mitigation is alerting, not
  prevention. NTAG424 DNA (~€1.50/tag, AES-CMAC) is the upgrade if it becomes a problem.
- One device has mobile data, which a router block does not touch.
- Offline use — downloaded video, offline games — is unaffected by a router block.
