//! What crosses between the room and the network.
//!
//! The room half owns the blocking hardware and the ledger; the network half owns
//! Wi-Fi, the admin page, alerts and flash. They are still on one core today — see
//! `docs/two-core-split.md` — but everything they exchange already goes through here,
//! so moving the room to core 1 changes where the code runs and not how it talks.
//!
//! All of it is `CriticalSectionRawMutex`, which on multicore esp-hal is a real
//! spinlock. That is the reason this is cheap: the cells work unchanged across cores.

use core::cell::RefCell;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::signal::Signal;
use medienzeit_core::{Policy, Snapshot};

/// Where the FRITZ!Box says each device is. Network → room.
///
/// A value cell rather than a signal: the room reads it every tick, and a signal would
/// hand it to whoever asked first and leave the next read with nothing.
static PRESENCE: Mutex<CriticalSectionRawMutex, RefCell<[bool; 2]>> =
    Mutex::new(RefCell::new([false; 2]));

pub fn publish_presence(present: [bool; 2]) {
    PRESENCE.lock(|c| *c.borrow_mut() = present);
}

pub fn presence() -> [bool; 2] {
    PRESENCE.lock(|c| *c.borrow())
}

/// The rules in force. Network → room, because the admin page and flash live there.
static POLICY: Mutex<CriticalSectionRawMutex, RefCell<Option<Policy>>> =
    Mutex::new(RefCell::new(None));

pub fn publish_policy(p: Policy) {
    POLICY.lock(|c| *c.borrow_mut() = Some(p));
}

pub fn policy() -> Option<Policy> {
    POLICY.lock(|c| c.borrow().clone())
}

/// What the ledger last computed. Room → network, read by the admin page and by
/// enforcement.
static SNAPSHOT: Mutex<CriticalSectionRawMutex, RefCell<Option<Snapshot<2>>>> =
    Mutex::new(RefCell::new(None));

pub fn publish_snapshot(s: Snapshot<2>) {
    SNAPSHOT.lock(|c| *c.borrow_mut() = Some(s));
}

pub fn snapshot() -> Option<Snapshot<2>> {
    SNAPSHOT.lock(|c| c.borrow().clone())
}

/// A validated wall clock, once. Network → room.
///
/// One-shot because it is an event, not a state: the room applies it to the RTC and
/// from then on the RTC is authoritative again.
pub static SNTP: Signal<CriticalSectionRawMutex, i64> = Signal::new();

/// "Persist this balance and timestamp." Room → network, where the flash lives.
///
/// A signal because journalling is edge-driven — on a flow change, or when the timer
/// says so — and the storage task should sleep rather than poll for it.
pub static PERSIST: Signal<CriticalSectionRawMutex, (i32, i64)> = Signal::new();
