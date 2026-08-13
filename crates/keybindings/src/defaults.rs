//! Default keybindings shipped with attacode. User overrides loaded from
//! `~/.atta/code/keybindings.json` are merged on top via [`crate::merge_bindings`].
//!
//! Action namespaces:
//! - `editor.*` — input area: cursor, delete, history nav, submit
//! - `repl.*` — REPL/TUI controls: cancel, exit, scroll, clear
//! - `ask.*` — ask-dialog navigation: select / confirm / deny
//! - `slash.*` — fast-path slash commands

use crate::parser::{KeyCode, Shortcut};
use crate::{Keybinding, KeybindingSource};

pub fn default_bindings() -> Vec<Keybinding> {
    vec![
        // ---- editor ----
        bind("Enter", "editor.submit", "Submit input"),
        bind(
            "Shift+Enter",
            "editor.newline",
            "Insert newline (multi-line input)",
        ),
        bind("Up", "editor.history.prev", "Previous prompt in history"),
        bind("Down", "editor.history.next", "Next prompt in history"),
        bind("Ctrl+U", "editor.clear", "Clear input buffer"),
        bind("Ctrl+W", "editor.delete-word", "Delete previous word"),
        bind("Ctrl+K", "editor.kill-to-eol", "Kill to end of line"),
        bind("Ctrl+L", "editor.redraw", "Redraw screen"),
        // ---- repl / TUI ----
        bind("Ctrl+C", "repl.cancel", "Cancel current turn"),
        bind("Ctrl+D", "repl.exit", "Exit (when input is empty)"),
        bind("PageUp", "repl.scroll-up", "Scroll transcript up"),
        bind("PageDown", "repl.scroll-down", "Scroll transcript down"),
        bind("Esc", "repl.dismiss", "Dismiss dialog / cancel ask"),
        // ---- ask dialog ----
        bind("Up", "ask.prev", "Previous option in ask-dialog"),
        bind("Down", "ask.next", "Next option in ask-dialog"),
        bind("Enter", "ask.confirm", "Confirm current ask-dialog choice"),
        bind("y", "ask.yes-shortcut", "Quick-yes in ask-dialog"),
        bind("n", "ask.no-shortcut", "Quick-no in ask-dialog"),
    ]
}

fn bind(shortcut: &str, action: &str, desc: &str) -> Keybinding {
    Keybinding {
        chord: vec![crate::parser::parse_shortcut(shortcut).expect("valid default shortcut")],
        action: action.into(),
        description: Some(desc.into()),
        source: KeybindingSource::Default,
    }
}

/// Convenience: shortcuts that user *cannot* re-map because they're owned by
/// something else (terminal driver, OS). See [`crate::reserved`].
pub fn unmappable_shortcuts() -> Vec<Shortcut> {
    vec![
        // Ctrl-Z = SIGTSTP (job control); we intentionally don't intercept
        Shortcut::ctrl('z'),
        // Ctrl-Q / Ctrl-S = XON/XOFF flow control on some terminals
        Shortcut::ctrl('q'),
        Shortcut::ctrl('s'),
        // Backtab is owned by terminal usually; left alone unless you know what you're doing
        Shortcut {
            modifiers: 0,
            key: KeyCode::BackTab,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::KeyCode;

    #[test]
    fn defaults_include_ctrl_c_cancel() {
        let bs = default_bindings();
        let cancel = bs.iter().find(|b| b.action == "repl.cancel").unwrap();
        assert_eq!(cancel.chord.len(), 1);
        assert!(cancel.chord[0].has_ctrl());
        assert_eq!(cancel.chord[0].key, KeyCode::Char('c'));
        assert_eq!(cancel.source, KeybindingSource::Default);
    }

    #[test]
    fn defaults_include_kill_to_eol() {
        let bs = default_bindings();
        let kte = bs
            .iter()
            .find(|b| b.action == "editor.kill-to-eol")
            .unwrap();
        assert_eq!(kte.chord.len(), 1);
        assert!(kte.chord[0].has_ctrl());
        assert_eq!(kte.chord[0].key, KeyCode::Char('k'));
    }

    #[test]
    fn all_defaults_have_descriptions() {
        for b in default_bindings() {
            assert!(b.description.is_some(), "{} missing description", b.action);
        }
    }
}
