//! Reserved shortcuts — terminal- or OS-owned keys that the user can't
//! reasonably re-map without breaking the shell.

use crate::parser::{KeyCode, Shortcut, MOD_CTRL};

/// Returns true if `s` is a shortcut that should never be bound to an
/// attacode action. Caller (validate) flags these as errors.
pub fn is_reserved_shortcut(s: &Shortcut) -> bool {
    // Ctrl-Z (SIGTSTP)
    if s.modifiers == MOD_CTRL && s.key == KeyCode::Char('z') {
        return true;
    }
    // Ctrl-S / Ctrl-Q (XON/XOFF flow control)
    if s.modifiers == MOD_CTRL && matches!(s.key, KeyCode::Char('s') | KeyCode::Char('q')) {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_z_is_reserved() {
        assert!(is_reserved_shortcut(&Shortcut::ctrl('z')));
    }

    #[test]
    fn ctrl_s_and_ctrl_q_are_reserved() {
        assert!(is_reserved_shortcut(&Shortcut::ctrl('s')));
        assert!(is_reserved_shortcut(&Shortcut::ctrl('q')));
    }

    #[test]
    fn ctrl_c_is_not_reserved_we_handle_it() {
        // Ctrl-C is bound to repl.cancel — we trap SIGINT ourselves.
        assert!(!is_reserved_shortcut(&Shortcut::ctrl('c')));
    }

    #[test]
    fn arbitrary_letter_is_not_reserved() {
        assert!(!is_reserved_shortcut(&Shortcut::ctrl('p')));
    }
}
