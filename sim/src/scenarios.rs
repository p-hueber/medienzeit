//! Canonical screen states, used both for `--shots` and as a quick visual regression
//! set. Each one is produced by actually running the ledger, never by hand-building a
//! `Snapshot` — otherwise the pictures could show states the real logic cannot reach.
//!
//! The list mirrors the scenario table in the plan, row for row.

use medienzeit_core::{Ledger, Policy, Snapshot};

use crate::clock::berlin;

pub struct Scenario {
    pub name: &'static str,
    pub description: &'static str,
    pub snapshot: Snapshot<2>,
}

const DOCKED: [bool; 2] = [true, true];
const ONE_OUT: [bool; 2] = [false, true];
const BOTH_OUT: [bool; 2] = [false, false];
const HOME: [bool; 2] = [true, true];
const AWAY: [bool; 2] = [false, false];

/// Advance a ledger second by second, which is what the firmware does at 1 Hz.
fn run(
    l: &mut Ledger<2>,
    p: &Policy,
    start: i64,
    secs: i64,
    docked: [bool; 2],
    present: [bool; 2],
) -> Snapshot<2> {
    let mut last = l.tick(start, docked, present, p).0;
    for t in 1..=secs {
        last = l.tick(start + t, docked, present, p).0;
    }
    last
}

fn scenario(
    name: &'static str,
    description: &'static str,
    balance: i32,
    start: i64,
    secs: i64,
    docked: [bool; 2],
    present: [bool; 2],
) -> Scenario {
    let p = Policy::default();
    let mut l = Ledger::<2>::with_balance(balance);
    Scenario { name, description, snapshot: run(&mut l, &p, start, secs, docked, present) }
}

pub fn all() -> Vec<Scenario> {
    vec![
        scenario(
            "01-docked-filling",
            "Put back at home, balance charging up",
            40 * 60,
            berlin(2026, 8, 3, 16, 0),
            600,
            DOCKED,
            HOME,
        ),
        scenario(
            "02-draining",
            "In use at home, clock running",
            75 * 60,
            berlin(2026, 8, 3, 16, 30),
            15 * 60,
            ONE_OUT,
            HOME,
        ),
        scenario(
            "03-grace",
            "Brief pickup inside grace: dashed rule, balance untouched",
            50 * 60,
            berlin(2026, 8, 3, 17, 0),
            90,
            ONE_OUT,
            HOME,
        ),
        scenario(
            "04-out-and-about",
            "Out of the house: undocked but off-network, so it still fills",
            30 * 60,
            berlin(2026, 8, 3, 14, 0),
            30 * 60,
            BOTH_OUT,
            AWAY,
        ),
        scenario(
            "05-warning",
            "Four minutes left, thick rule",
            9 * 60,
            berlin(2026, 8, 3, 18, 0),
            5 * 60,
            ONE_OUT,
            HOME,
        ),
        // Exactly zero, not negative — the run must exceed the grace period or
        // nothing is billed at all and the balance never moves.
        scenario(
            "06-exhausted",
            "Balance exactly gone, still not put back: inverted, blocked",
            5 * 60,
            berlin(2026, 8, 3, 18, 30),
            5 * 60,
            ONE_OUT,
            HOME,
        ),
        scenario(
            "07-in-debt",
            "Overspent into the red, still not put back",
            60,
            berlin(2026, 8, 3, 19, 0),
            10 * 60,
            ONE_OUT,
            HOME,
        ),
        scenario(
            "08-night-docked",
            "Night, everything put back, charging through to morning",
            45 * 60,
            berlin(2026, 8, 3, 22, 0),
            30 * 60,
            DOCKED,
            HOME,
        ),
        scenario(
            "09-night-undocked",
            "Taken out during the night: locked out and alerted",
            45 * 60,
            berlin(2026, 8, 3, 23, 0),
            5 * 60,
            ONE_OUT,
            HOME,
        ),
        scenario(
            "10-near-cap",
            "Saved up close to the cap",
            170 * 60,
            berlin(2026, 8, 1, 11, 0),
            20 * 60,
            DOCKED,
            HOME,
        ),
    ]
}
