//! Cross-crate "something needs a Windows present" flag.
//!
//! gpui-on-Windows draws from key handlers without presenting; the app's present
//! pump forces `WM_PAINT`. Terminal notify/key paths arm this flag so the pump
//! can skip work when nothing happened (see `muxel::present_pump`).
//!
//! **Temporary with the pump:** remove this module when zed#61469 is fixed in
//! our gpui pin and the present pump is deleted (see `present_pump.rs` header).

use std::sync::atomic::{AtomicBool, Ordering};

static PRESENT_NEEDED: AtomicBool = AtomicBool::new(true);

/// Arm a present. Cheap; safe from any thread.
pub fn mark_present_needed() {
    PRESENT_NEEDED.store(true, Ordering::Release);
}

/// Consume the flag (pump thread). Returns whether a present was requested
/// since the last take.
pub fn take_present_needed() -> bool {
    PRESENT_NEEDED.swap(false, Ordering::AcqRel)
}
