//! Shared visual constants, ported as literals from crates/tui_legacy (spinner frames,
//! status icons, prefixes, color table) — no dependency on that crate.

use crate::frame_state::SeparatorColor;
use ratatui::style::Color;

pub const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

pub fn spinner_frame(now_ms: u128) -> char {
    SPINNER_FRAMES[((now_ms / 100) % SPINNER_FRAMES.len() as u128) as usize]
}

pub const TOOL_HEADING_PREFIX: &str = "⏺ ";
pub const TOOL_RESULT_OK_PREFIX: &str = "  ✓ ";
pub const TOOL_RESULT_ERR_PREFIX: &str = "  ✗ ";
pub const NOTE_PREFIX: &str = "  · ";
pub const WARNING_PREFIX: &str = "  ⚠ ";
pub const ERROR_PREFIX: &str = "  ✗ ";
pub const DIFF_OLD_PREFIX: &str = "  - ";
pub const DIFF_NEW_PREFIX: &str = "  + ";
pub const USER_PROMPT_PREFIX: &str = "> ";
/// Left gutter bar marking the transcript block the fold/expand keys act on.
pub const SELECTION_GUTTER: &str = "▌";

pub const COLOR_PRIMARY: Color = Color::White;
pub const COLOR_SECONDARY: Color = Color::DarkGray;
pub const COLOR_ACCENT: Color = Color::Cyan;
pub const COLOR_SUCCESS: Color = Color::Green;
pub const COLOR_WARNING: Color = Color::Yellow;
pub const COLOR_ERROR: Color = Color::Red;
pub const COLOR_SUBAGENT: Color = Color::Magenta;
pub const COLOR_USER_PROMPT_BG: Color = Color::Rgb(65, 65, 85);
pub const COLOR_DIFF_OLD_BG: Color = Color::Rgb(60, 20, 20);
pub const COLOR_DIFF_NEW_BG: Color = Color::Rgb(20, 60, 20);

pub fn separator_color(c: SeparatorColor) -> Color {
    match c {
        SeparatorColor::DarkGray => COLOR_SECONDARY,
        SeparatorColor::Cyan => COLOR_ACCENT,
        SeparatorColor::Yellow => COLOR_WARNING,
        SeparatorColor::Red => COLOR_ERROR,
    }
}
