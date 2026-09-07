//! Getting the room out of the way before core 0 writes flash.
//!
//! # The deadlock this exists to prevent
//!
//! `esp-storage` cannot write flash while the other core is executing, because the
//! write disables the flash cache and that core is running code out of it. Its
//! `multicore_auto_park` strategy solves that by *stalling* the other core — and
//! `park_core` is a hardware stall (`sw_stall_appcpu_c1`), not a request. It halts core
//! 1 at whatever instruction it happens to be on, with no cooperation and no warning.
//!
//! That is fine for the cache, and fatal for locks. Every `CriticalSectionRawMutex` in
//! this firmware — `shared`, the notify queue, the web signals — and `esp-println`'s own
//! mutex are shared between the cores. Stall core 1 while it holds one, then touch the
//! same lock from core 0, and core 0 spins forever on a lock whose owner cannot run.
//! Both cores are then dead: the screen stops and the admin page stops answering, which
//! is exactly the failure this device showed after seven hours.
//!
//! The print lock is the fat target. On a mains supply there is no USB host, so
//! `esp-println` runs into its timeout paths and core 1 holds that lock far longer than
//! it would on a desk — which is why this only appeared once the device was left alone.
//!
//! # The handshake
//!
//! Core 0 asks; core 1 answers at a point where it holds nothing and has interrupts off;
//! core 0 writes; core 0 releases. The stall still happens, but it now lands on a core
//! that is provably holding no lock, so there is nothing for core 0 to wait on.
//!
//! Interrupts are disabled on core 1 for the same reason the handshake exists at all: an
//! interrupt handler taking a lock in the parked window would recreate the deadlock in
//! miniature. `xtensa_lx::interrupt::free` disables them locally without acquiring the
//! global critical section, which acquiring would defeat the whole point.
//!
//! Both sides are bounded. A wedged partner must degrade to a missed flash write or a
//! late tick, never to a hang — the failure being fixed here is a hang.

use core::sync::atomic::{AtomicBool, Ordering};

/// Core 0 wants to write flash.
static REQUEST: AtomicBool = AtomicBool::new(false);
/// Core 1 is parked somewhere safe and is holding nothing.
static PARKED: AtomicBool = AtomicBool::new(false);

/// How long the room will sit parked before deciding core 0 is not coming back.
///
/// Counted in spin iterations rather than wall clock on purpose: reading a timer is a
/// call into the scheduler, and the entire contract of this spin is that it touches
/// nothing that could be locked.
const PARK_SPINS: u32 = 20_000_000;

/// Called by the room, at a point where it holds no lock.
///
/// Cheap when nothing is asked of it: one relaxed load per iteration of a 1 Hz loop.
pub fn yield_if_asked() {
    if !REQUEST.load(Ordering::Acquire) {
        return;
    }
    // Interrupts off for the duration, so no handler can take a lock while we are
    // stalled. Nothing in here allocates, prints, or touches a mutex.
    esp_hal::xtensa_lx::interrupt::free(|| {
        PARKED.store(true, Ordering::Release);
        let mut spins = 0u32;
        while REQUEST.load(Ordering::Acquire) && spins < PARK_SPINS {
            spins += 1;
            core::hint::spin_loop();
        }
        PARKED.store(false, Ordering::Release);
    });
}

/// Whether the room has parked. Core 0 must not write flash until this is true.
pub fn room_is_parked() -> bool {
    PARKED.load(Ordering::Acquire)
}

/// Ask the room to park. Paired with [`release`].
pub fn request() {
    REQUEST.store(true, Ordering::Release);
}

/// Let the room go.
pub fn release() {
    REQUEST.store(false, Ordering::Release);
}
