//! Medienzeit — the pure, hardware-free half.
//!
//! Everything that is easy to get subtly wrong (date arithmetic, DST, the day
//! boundary, the spend rule) lives here so it can be tested with `cargo test` on a
//! laptop. The firmware crate is meant to stay thin glue around this.

#![no_std]

pub mod civil;
pub mod docking;
pub mod journal;
pub mod policy;
pub mod rtc;
pub mod settings;
pub mod sntp;
pub mod state;

pub use civil::LocalDateTime;
pub use policy::{Policy, Window};
pub use state::{Event, Events, Flow, Ledger, Snapshot, WARNING_SECS};

/// Number of cradles in the first build.
pub const DEVICES: usize = 2;

/// Human-readable device names, indexed the same way as [`Snapshot::docked`].
pub type DeviceNames = [&'static str; DEVICES];
