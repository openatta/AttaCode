//! AttaCode TUI (v3 layout) — pure ratatui rendering driven by `FrameState`.
//! See docs/TUI_DESIGN.md for the Z/R/S region tree this implements.

pub mod frame_state;
pub mod layout;
pub mod regions;

pub use frame_state::FrameState;
