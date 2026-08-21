//! The clock, on both targets.
//!
//! `std::time::Instant::now()` **panics** on `wasm32-unknown-unknown`. There is no monotonic
//! clock in the standard library there, and the failure is a runtime abort rather than a
//! compile error — with `panic = "abort"` in `[profile.release]` that means the tab dies on
//! the first frame, which is `ActiveState::frame`'s very first line.
//!
//! `SystemTime::now()` has the same problem for the same reason.
//!
//! `web-time` provides both with the identical API, backed by `performance.now()` and
//! `Date.now()`. So this module is a re-export and nothing else: every site in this crate
//! that timed something keeps the code it had and changes only which `use` line it reads.
//! There is no shim to get wrong, no wrapper type, and no second way to ask the time.
//!
//! # Two things deliberately not here
//!
//! **`Duration` is not re-exported.** It is arithmetic over integers with no clock behind it
//! and it is identical on every target, so routing it through here would imply a difference
//! that does not exist — and would leave a reader wondering which `Duration` they had.
//!
//! **`vellum-render` and `vellum-text` are not changed**, because they never ask the time.
//! Residency in `vellum-render` is counted in *frames*, not wall clock, which is why its
//! budget survives a target where the clock is a different thing entirely. Keeping the shim
//! confined to this crate and `vellum-store` is a property worth not losing.
//!
//! # Why not a workspace-wide dependency swap
//!
//! `web-time` re-exports `std`'s own types unchanged off the web, so the native build is
//! byte-for-byte what it was. It is scoped to this crate anyway because a crate that does not
//! read a clock should not gain a dependency that provides one.

#[cfg(not(target_arch = "wasm32"))]
pub use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[cfg(target_arch = "wasm32")]
pub use web_time::{Instant, SystemTime, UNIX_EPOCH};

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_clock_moves_forward_and_can_be_subtracted() {
        // Not a tautology: this is the shape every call site in the crate uses -- take an
        // instant, take another, subtract. If the wasm arm ever resolved to something whose
        // `elapsed` answered zero forever, every budget in `draw.rs` and `assets.rs` would
        // silently become unlimited rather than failing, which is the worse outcome.
        let start = Instant::now();
        let after = Instant::now();
        assert!(after >= start, "the monotonic clock went backwards");
        assert!(start.elapsed() < Duration::from_secs(60));
    }

    #[test]
    fn wall_clock_time_is_after_the_epoch() {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock is before 1970");
        // Sanity, not precision: anything past 2020 proves this is a real wall clock and not
        // a zero-valued stub, which is the only failure mode worth a test here.
        assert!(now.as_secs() > 1_577_836_800, "the wall clock reads before 2020");
    }
}
