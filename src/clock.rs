//! Monotonic-enough wall clock for retries and TTLs.
//!
//! Uses `Date.now()` on wasm so we never pull in the broken `Performance`
//! wasm-bindgen import that browsers sometimes cache against mismatched glue.

use std::time::Duration;

#[cfg(not(target_arch = "wasm32"))]
pub fn now() -> Duration {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
}

#[cfg(target_arch = "wasm32")]
pub fn now() -> Duration {
    Duration::from_millis(js_sys::Date::now() as u64)
}
