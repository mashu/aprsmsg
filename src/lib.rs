//! Interactive APRS messaging client for Direwolf (KISS) or APRS-IS.

pub mod aprs;
pub mod ax25;
pub mod client;
pub mod clock;
pub mod decode;
pub mod heard;
pub mod kiss;

#[cfg(not(target_arch = "wasm32"))]
pub mod link;
#[cfg(not(target_arch = "wasm32"))]
pub mod ui;

#[cfg(target_arch = "wasm32")]
mod wasm;

pub use client::{Action, Client, ClientConfig, UiMsg, COMMANDS};
pub use heard::{passcode, Heard, Hop};
