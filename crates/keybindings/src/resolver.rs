//! Chord state machine: feed it [`crossterm::event::KeyEvent`]s, get back
//! [`ResolveOutcome::Action`] when a binding matches. For chord prefixes,
//! we hold partial state and expect the next key within ~1.5s.

use crate::parser::{KeyCode, Shortcut, MOD_ALT, MOD_CTRL, MOD_META, MOD_SHIFT};
use crate::Keybinding;
use crossterm::event::{KeyCode as CtCode, KeyEvent, KeyModifiers};

/// Resolver state — held by the consumer between key events.
pub struct Resolver {
    bindings: Vec<Keybinding>,
    /// Current chord prefix accumulated. Empty = at-rest.
    chord_prefix: Vec<Shortcut>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveOutcome {
    /// Full chord matched; here's the action name to dispatch.
    Action(String),
    /// Partial chord match; resolver is holding `prefix.len()` keys waiting
    /// for the next. The consumer typically updates a status hint
    /// ("Chord: Ctrl-X waiting for next…").
    Partial,
    /// No binding matches this prefix; `None` reset to at-rest. The original
    /// unmatched key is returned so the consumer can handle it normally
    /// (e.g. as a regular character into the input).
    Unmatched(Shortcut),
    /// `Esc` mid-chord cancels and returns to at-rest.
    ChordCancelled,
}

impl Resolver {
    pub fn new(bindings: Vec<Keybinding>) -> Self {
        Self {
            bindings,
            chord_prefix: Vec::new(),
        }
    }

    /// Lookup any binding by action name (used by `/keybindings` slash).
    pub fn bindings(&self) -> &[Keybinding] {
        &self.bindings
    }

    /// Convert a crossterm KeyEvent into our [`Shortcut`].
    pub fn shortcut_of(event: &KeyEvent) -> Option<Shortcut> {
        let mods = event.modifiers;
        let mut modifiers: u8 = 0;
        if mods.contains(KeyModifiers::CONTROL) {
            modifiers |= MOD_CTRL;
        }
        if mods.contains(KeyModifiers::ALT) {
            modifiers |= MOD_ALT;
        }
        if mods.contains(KeyModifiers::SHIFT) {
            modifiers |= MOD_SHIFT;
        }
        if mods.contains(KeyModifiers::SUPER) || mods.contains(KeyModifiers::META) {
            modifiers |= MOD_META;
        }
        let key = match event.code {
            CtCode::Char(c) => KeyCode::Char(c.to_ascii_lowercase()),
            CtCode::Enter => KeyCode::Enter,
            CtCode::Esc => KeyCode::Esc,
            CtCode::Tab => KeyCode::Tab,
            CtCode::BackTab => KeyCode::BackTab,
            CtCode::Backspace => KeyCode::Backspace,
            CtCode::Delete => KeyCode::Delete,
            CtCode::Up => KeyCode::Up,
            CtCode::Down => KeyCode::Down,
            CtCode::Left => KeyCode::Left,
            CtCode::Right => KeyCode::Right,
            CtCode::Home => KeyCode::Home,
            CtCode::End => KeyCode::End,
            CtCode::PageUp => KeyCode::PageUp,
            CtCode::PageDown => KeyCode::PageDown,
            CtCode::Insert => KeyCode::Insert,
            CtCode::F(n) => KeyCode::Function(n),
            _ => return None,
        };
        Some(Shortcut { modifiers, key })
    }

    /// Feed a key event to the resolver.
    pub fn on_key(&mut self, event: &KeyEvent) -> ResolveOutcome {
        let Some(s) = Self::shortcut_of(event) else {
            return ResolveOutcome::Unmatched(Shortcut {
                modifiers: 0,
                key: KeyCode::Char('?'),
            });
        };

        // Mid-chord Esc cancels.
        if !self.chord_prefix.is_empty() && s.modifiers == 0 && s.key == KeyCode::Esc {
            self.chord_prefix.clear();
            return ResolveOutcome::ChordCancelled;
        }

        let mut tentative = self.chord_prefix.clone();
        tentative.push(s.clone());

        let exact_match = self
            .bindings
            .iter()
            .find(|b| b.chord == tentative)
            .map(|b| b.action.clone());

        let could_extend = self
            .bindings
            .iter()
            .any(|b| b.chord.len() > tentative.len() && b.chord.starts_with(&tentative));

        match (exact_match, could_extend) {
            (Some(action), false) => {
                self.chord_prefix.clear();
                ResolveOutcome::Action(action)
            }
            (Some(action), true) => {
                // Ambiguous: a chord starting with `tentative` also exists.
                // Convention: prefer the shorter (immediate) match. Reset.
                self.chord_prefix.clear();
                ResolveOutcome::Action(action)
            }
            (None, true) => {
                self.chord_prefix = tentative;
                ResolveOutcome::Partial
            }
            (None, false) => {
                self.chord_prefix.clear();
                ResolveOutcome::Unmatched(s)
            }
        }
    }

    /// Reset chord state (e.g. on focus loss).
    pub fn reset(&mut self) {
        self.chord_prefix.clear();
    }

    /// Are we waiting on a continuation key?
    pub fn is_in_chord(&self) -> bool {
        !self.chord_prefix.is_empty()
    }

    /// Render the current chord prefix for status-bar hints.
    pub fn chord_hint(&self) -> Option<String> {
        if self.chord_prefix.is_empty() {
            None
        } else {
            Some(
                self.chord_prefix
                    .iter()
                    .map(|s| s.render())
                    .collect::<Vec<_>>()
                    .join(" "),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_chord;
    use crate::KeybindingSource;
    use crossterm::event::KeyEventKind;

    fn ke(c: char, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code: CtCode::Char(c),
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    fn binding(chord: &str, action: &str) -> Keybinding {
        Keybinding {
            chord: parse_chord(chord).unwrap(),
            action: action.into(),
            description: None,
            source: KeybindingSource::Default,
        }
    }

    #[test]
    fn single_shortcut_resolves_immediately() {
        let mut r = Resolver::new(vec![binding("Ctrl+P", "go")]);
        let out = r.on_key(&ke('p', KeyModifiers::CONTROL));
        assert_eq!(out, ResolveOutcome::Action("go".into()));
        assert!(!r.is_in_chord());
    }

    #[test]
    fn chord_two_step_resolves() {
        let mut r = Resolver::new(vec![binding("Ctrl+X Ctrl+C", "exit")]);
        assert_eq!(
            r.on_key(&ke('x', KeyModifiers::CONTROL)),
            ResolveOutcome::Partial
        );
        assert!(r.is_in_chord());
        assert_eq!(r.chord_hint(), Some("Ctrl+x".into()));
        assert_eq!(
            r.on_key(&ke('c', KeyModifiers::CONTROL)),
            ResolveOutcome::Action("exit".into())
        );
        assert!(!r.is_in_chord());
    }

    #[test]
    fn unmatched_key_resets_state() {
        let mut r = Resolver::new(vec![binding("Ctrl+P", "go")]);
        let out = r.on_key(&ke('a', KeyModifiers::NONE));
        assert!(matches!(out, ResolveOutcome::Unmatched(_)));
        assert!(!r.is_in_chord());
    }

    #[test]
    fn esc_cancels_chord_in_progress() {
        let mut r = Resolver::new(vec![binding("Ctrl+X Ctrl+C", "exit")]);
        let _ = r.on_key(&ke('x', KeyModifiers::CONTROL));
        assert!(r.is_in_chord());
        let out = r.on_key(&KeyEvent {
            code: CtCode::Esc,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        assert_eq!(out, ResolveOutcome::ChordCancelled);
        assert!(!r.is_in_chord());
    }

    #[test]
    fn chord_prefix_unmatched_continuation_resets() {
        let mut r = Resolver::new(vec![binding("Ctrl+X Ctrl+C", "exit")]);
        let _ = r.on_key(&ke('x', KeyModifiers::CONTROL));
        assert!(r.is_in_chord());
        let out = r.on_key(&ke('z', KeyModifiers::NONE));
        match out {
            ResolveOutcome::Unmatched(_) => assert!(!r.is_in_chord()),
            other => panic!("expected Unmatched, got {other:?}"),
        }
    }

    #[test]
    fn ambiguous_short_match_wins() {
        // Both `Ctrl+P` (single) and `Ctrl+P Ctrl+Q` (chord) bound; convention
        // is the immediate one wins.
        let mut r = Resolver::new(vec![
            binding("Ctrl+P", "single"),
            binding("Ctrl+P Ctrl+Q", "chord"),
        ]);
        let out = r.on_key(&ke('p', KeyModifiers::CONTROL));
        assert_eq!(out, ResolveOutcome::Action("single".into()));
        assert!(!r.is_in_chord());
    }

    #[test]
    fn reset_clears_chord_prefix() {
        let mut r = Resolver::new(vec![binding("Ctrl+X Ctrl+C", "exit")]);
        let _ = r.on_key(&ke('x', KeyModifiers::CONTROL));
        assert!(r.is_in_chord());
        r.reset();
        assert!(!r.is_in_chord());
    }
}
