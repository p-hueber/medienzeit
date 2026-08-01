//! Canonical screen states, used both for `--shots` and as a quick visual regression
//! set. Each one is produced by actually running the ledger, never by hand-building a
//! `Snapshot` — otherwise the pictures could show states the real logic cannot reach.

use medienzeit_core::{AwayWindow, Ledger, Policy, Snapshot};

use crate::clock::berlin;

pub struct Scenario {
    pub name: &'static str,
    pub description: &'static str,
    pub snapshot: Snapshot<2>,
}

const DOCKED: [bool; 2] = [true, true];
const ONE_OUT: [bool; 2] = [false, true];
const BOTH_OUT: [bool; 2] = [false, false];

/// Advance a ledger second-by-second, which is what the firmware does at 1 Hz.
fn run(l: &mut Ledger<2>, p: &Policy, start: i64, secs: i64, docked: [bool; 2]) -> Snapshot<2> {
    let mut last = l.tick(start, docked, p).0;
    for t in 1..=secs {
        last = l.tick(start + t, docked, p).0;
    }
    last
}

pub fn all() -> Vec<Scenario> {
    let p = Policy::default();
    let mut out = Vec::new();

    // Monday 16:00, nothing used yet, both devices on their cradles.
    {
        let mut l = Ledger::<2>::new();
        let snap = run(&mut l, &p, berlin(2026, 8, 3, 16, 0), 2, DOCKED);
        out.push(Scenario {
            name: "01-fresh-docked",
            description: "Monday 16:00, full weekday budget, both docked",
            snapshot: snap,
        });
    }

    // 25 minutes in, one device in her hands.
    {
        let mut l = Ledger::<2>::new();
        let snap = run(&mut l, &p, berlin(2026, 8, 3, 16, 0), 25 * 60, ONE_OUT);
        out.push(Scenario {
            name: "02-spending",
            description: "25 min used, phone undocked, clock running",
            snapshot: snap,
        });
    }

    // Over an hour of weekend budget used.
    {
        let mut l = Ledger::<2>::new();
        let snap = run(&mut l, &p, berlin(2026, 8, 1, 14, 0), 35 * 60, BOTH_OUT);
        out.push(Scenario {
            name: "03-weekend-hours",
            description: "Saturday, 2 h allowance, both devices out, H:MM layout",
            snapshot: snap,
        });
    }

    // Inside the school away-window with both devices out — must not drain.
    {
        let mut l = Ledger::<2>::new();
        let snap = run(&mut l, &p, berlin(2026, 8, 3, 9, 0), 30 * 60, BOTH_OUT);
        out.push(Scenario {
            name: "04-away-window",
            description: "Monday 09:30 at school, undocked but paused",
            snapshot: snap,
        });
    }

    // Four minutes left: warning styling.
    {
        let mut p_short = p.clone();
        p_short.weekday_secs = 30 * 60;
        let mut l = Ledger::<2>::new();
        let snap = run(&mut l, &p_short, berlin(2026, 8, 3, 17, 0), 26 * 60, ONE_OUT);
        out.push(Scenario {
            name: "05-warning",
            description: "4 min left, thick rule, last-minutes warning",
            snapshot: snap,
        });
    }

    // Budget gone.
    {
        let mut p_short = p.clone();
        p_short.weekday_secs = 10 * 60;
        let mut l = Ledger::<2>::new();
        let snap = run(&mut l, &p_short, berlin(2026, 8, 3, 17, 0), 11 * 60, ONE_OUT);
        out.push(Scenario {
            name: "06-exhausted",
            description: "Budget spent, screen inverted, devices blocked",
            snapshot: snap,
        });
    }

    // Exactly one minute left — checks the round-up so it never shows 0 too early.
    {
        let mut p_short = p.clone();
        p_short.weekday_secs = 10 * 60;
        p_short.away.clear();
        let _ = p_short.away.push(AwayWindow::hm(AwayWindow::WEEKDAYS, 7, 30, 15, 0));
        let mut l = Ledger::<2>::new();
        let snap = run(&mut l, &p_short, berlin(2026, 8, 3, 17, 0), 9 * 60 + 30, ONE_OUT);
        out.push(Scenario {
            name: "07-last-minute",
            description: "30 s left, still reads 1 (round up)",
            snapshot: snap,
        });
    }

    out
}
