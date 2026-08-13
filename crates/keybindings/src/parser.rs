//! Parse a textual shortcut like `"Ctrl+Shift+P"` or `"Ctrl-X Ctrl-C"` into
//! [`Shortcut`] / chord. Liberal with separators (`+` and `-`); case-insensitive
//! on modifier names.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Shortcut {
    /// Modifier mask. Bits: 0=Ctrl, 1=Alt, 2=Shift, 3=Meta/Cmd.
    pub modifiers: u8,
    /// Logical key. Lowercased for letters; named for special keys.
    pub key: KeyCode,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyCode {
    Char(char),
    Enter,
    Esc,
    Tab,
    BackTab,
    Backspace,
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Function(u8), // F1..F12
    Space,
}

pub const MOD_CTRL: u8 = 1 << 0;
pub const MOD_ALT: u8 = 1 << 1;
pub const MOD_SHIFT: u8 = 1 << 2;
pub const MOD_META: u8 = 1 << 3;

impl Shortcut {
    pub fn new(modifiers: u8, key: KeyCode) -> Self {
        Self { modifiers, key }
    }

    pub fn ctrl(c: char) -> Self {
        Self::new(MOD_CTRL, KeyCode::Char(c.to_ascii_lowercase()))
    }

    pub fn has_ctrl(&self) -> bool {
        self.modifiers & MOD_CTRL != 0
    }
    pub fn has_alt(&self) -> bool {
        self.modifiers & MOD_ALT != 0
    }
    pub fn has_shift(&self) -> bool {
        self.modifiers & MOD_SHIFT != 0
    }
    pub fn has_meta(&self) -> bool {
        self.modifiers & MOD_META != 0
    }

    pub fn render(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if self.has_ctrl() {
            parts.push("Ctrl");
        }
        if self.has_alt() {
            parts.push("Alt");
        }
        if self.has_shift() {
            parts.push("Shift");
        }
        if self.has_meta() {
            parts.push("Meta");
        }
        let key_str = match &self.key {
            KeyCode::Char(c) => c.to_string(),
            KeyCode::Enter => "Enter".into(),
            KeyCode::Esc => "Esc".into(),
            KeyCode::Tab => "Tab".into(),
            KeyCode::BackTab => "BackTab".into(),
            KeyCode::Backspace => "Backspace".into(),
            KeyCode::Delete => "Delete".into(),
            KeyCode::Up => "Up".into(),
            KeyCode::Down => "Down".into(),
            KeyCode::Left => "Left".into(),
            KeyCode::Right => "Right".into(),
            KeyCode::Home => "Home".into(),
            KeyCode::End => "End".into(),
            KeyCode::PageUp => "PageUp".into(),
            KeyCode::PageDown => "PageDown".into(),
            KeyCode::Insert => "Insert".into(),
            KeyCode::Space => "Space".into(),
            KeyCode::Function(n) => format!("F{n}"),
        };
        if parts.is_empty() {
            key_str
        } else {
            format!("{}+{}", parts.join("+"), key_str)
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("empty shortcut")]
    Empty,
    #[error("unknown key `{0}`")]
    UnknownKey(String),
    #[error("multi-char keys must be named (got `{0}`)")]
    MultiChar(String),
}

/// Parse a single shortcut like `Ctrl+P`, `Ctrl-X`, `F5`, `Esc`, `Up`.
pub fn parse_shortcut(s: &str) -> Result<Shortcut, ParseError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ParseError::Empty);
    }
    let parts: Vec<&str> = s
        .split(['+', '-'])
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();
    if parts.is_empty() {
        return Err(ParseError::Empty);
    }
    let mut modifiers: u8 = 0;
    let mut key: Option<KeyCode> = None;
    for (i, part) in parts.iter().enumerate() {
        let lower = part.to_lowercase();
        match lower.as_str() {
            "ctrl" | "control" | "c" if i < parts.len() - 1 => modifiers |= MOD_CTRL,
            "alt" | "option" | "opt" if i < parts.len() - 1 => modifiers |= MOD_ALT,
            "shift" if i < parts.len() - 1 => modifiers |= MOD_SHIFT,
            "meta" | "cmd" | "super" | "win" if i < parts.len() - 1 => modifiers |= MOD_META,
            _ => {
                key = Some(parse_key_token(&lower)?);
            }
        }
    }
    let key = key.ok_or_else(|| ParseError::UnknownKey(s.to_string()))?;
    Ok(Shortcut { modifiers, key })
}

/// Parse a chord like `"Ctrl+X Ctrl+C"` or `"Esc K D"` (whitespace separates
/// individual shortcuts within the chord).
pub fn parse_chord(s: &str) -> Result<Vec<Shortcut>, ParseError> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.is_empty() {
        return Err(ParseError::Empty);
    }
    parts.into_iter().map(parse_shortcut).collect()
}

fn parse_key_token(s: &str) -> Result<KeyCode, ParseError> {
    Ok(match s {
        "enter" | "return" | "ret" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backtab" | "shift-tab" | "shifttab" => KeyCode::BackTab,
        "backspace" | "bs" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "up" | "uparrow" => KeyCode::Up,
        "down" | "downarrow" => KeyCode::Down,
        "left" | "leftarrow" => KeyCode::Left,
        "right" | "rightarrow" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdown" | "pgdn" => KeyCode::PageDown,
        "insert" | "ins" => KeyCode::Insert,
        "space" | "spc" => KeyCode::Space,
        s if s.starts_with('f') && s.len() > 1 => {
            let n: u8 = s[1..]
                .parse()
                .map_err(|_| ParseError::UnknownKey(s.to_string()))?;
            if !(1..=12).contains(&n) {
                return Err(ParseError::UnknownKey(s.to_string()));
            }
            KeyCode::Function(n)
        }
        s if s.chars().count() == 1 => {
            KeyCode::Char(s.chars().next().unwrap().to_ascii_lowercase())
        }
        s => return Err(ParseError::MultiChar(s.to_string())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_ctrl_letter() {
        let s = parse_shortcut("Ctrl+P").unwrap();
        assert!(s.has_ctrl());
        assert_eq!(s.key, KeyCode::Char('p'));
    }

    #[test]
    fn parse_dash_separator_works() {
        let s = parse_shortcut("Ctrl-X").unwrap();
        assert!(s.has_ctrl());
        assert_eq!(s.key, KeyCode::Char('x'));
    }

    #[test]
    fn parse_multiple_modifiers() {
        let s = parse_shortcut("Ctrl+Shift+Alt+P").unwrap();
        assert!(s.has_ctrl());
        assert!(s.has_shift());
        assert!(s.has_alt());
        assert_eq!(s.key, KeyCode::Char('p'));
    }

    #[test]
    fn parse_named_keys() {
        assert_eq!(parse_shortcut("Esc").unwrap().key, KeyCode::Esc);
        assert_eq!(parse_shortcut("Up").unwrap().key, KeyCode::Up);
        assert_eq!(parse_shortcut("F5").unwrap().key, KeyCode::Function(5));
        assert_eq!(parse_shortcut("Space").unwrap().key, KeyCode::Space);
    }

    #[test]
    fn parse_chord_two_shortcuts() {
        let c = parse_chord("Ctrl+X Ctrl+C").unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0], Shortcut::ctrl('x'));
        assert_eq!(c[1], Shortcut::ctrl('c'));
    }

    #[test]
    fn parse_chord_three_keys() {
        let c = parse_chord("Esc K D").unwrap();
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].key, KeyCode::Esc);
    }

    #[test]
    fn rejects_empty() {
        assert!(matches!(parse_shortcut(""), Err(ParseError::Empty)));
        assert!(matches!(parse_chord(""), Err(ParseError::Empty)));
    }

    #[test]
    fn rejects_unknown_function_key() {
        assert!(matches!(
            parse_shortcut("F25"),
            Err(ParseError::UnknownKey(_))
        ));
    }

    #[test]
    fn render_round_trips() {
        for input in ["Ctrl+P", "Ctrl+Shift+A", "F5", "Esc"] {
            let s = parse_shortcut(input).unwrap();
            let parsed_back = parse_shortcut(&s.render()).unwrap();
            assert_eq!(s, parsed_back, "{input} did not round-trip");
        }
    }
}
