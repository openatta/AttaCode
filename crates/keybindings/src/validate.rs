//! Run validation over a merged binding list. Surfaces:
//! - duplicate chords (two bindings claiming the same shortcut/chord)
//! - reserved shortcuts (terminal-owned keys like Ctrl-Z)
//! - chord shadowing (chord A is a prefix of chord B → A always wins, B is unreachable
//!   *if* both have actions; standalone shadowing is fine)
//!
//! All issues are non-fatal — caller logs them and continues with whichever
//! defaults still apply.

use crate::reserved::is_reserved_shortcut;
use crate::Keybinding;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationIssue {
    /// Same chord appears twice (after merge). Second occurrence wins; first
    /// is reported. Includes both action names.
    DuplicateChord {
        shortcut_render: String,
        first_action: String,
        second_action: String,
    },
    /// A reserved (terminal/OS-owned) shortcut was bound. Listed for warning.
    ReservedShortcut {
        shortcut_render: String,
        action: String,
    },
    /// `prefix` is a complete chord that is also the prefix of `extender`.
    /// The extender is unreachable because the prefix matches first.
    ChordShadowed {
        prefix_render: String,
        prefix_action: String,
        shadowed_render: String,
        shadowed_action: String,
    },
}

pub fn validate_bindings(bindings: &[Keybinding]) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();

    // 1. Duplicate chord detection (a chord vs same chord)
    let mut seen: HashMap<Vec<crate::parser::Shortcut>, &Keybinding> = HashMap::new();
    for b in bindings {
        if let Some(prev) = seen.get(&b.chord) {
            issues.push(ValidationIssue::DuplicateChord {
                shortcut_render: render_chord(&b.chord),
                first_action: prev.action.clone(),
                second_action: b.action.clone(),
            });
        } else {
            seen.insert(b.chord.clone(), b);
        }
    }

    // 2. Reserved shortcut detection (only meaningful for single-shortcut
    //    bindings; if a chord *starts* with a reserved key the terminal
    //    eats it before the chord completes)
    for b in bindings {
        for s in &b.chord {
            if is_reserved_shortcut(s) {
                issues.push(ValidationIssue::ReservedShortcut {
                    shortcut_render: s.render(),
                    action: b.action.clone(),
                });
                break;
            }
        }
    }

    // 3. Chord shadowing: bindings A (length n) vs B (length > n) where
    //    B starts with A. A always matches first → B unreachable.
    for a in bindings {
        for b in bindings {
            if a.chord.len() < b.chord.len() && b.chord.starts_with(&a.chord) {
                issues.push(ValidationIssue::ChordShadowed {
                    prefix_render: render_chord(&a.chord),
                    prefix_action: a.action.clone(),
                    shadowed_render: render_chord(&b.chord),
                    shadowed_action: b.action.clone(),
                });
            }
        }
    }

    issues
}

fn render_chord(c: &[crate::parser::Shortcut]) -> String {
    c.iter().map(|s| s.render()).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{parse_chord, parse_shortcut};
    use crate::KeybindingSource;

    fn b(chord: &str, action: &str) -> Keybinding {
        Keybinding {
            chord: parse_chord(chord).unwrap(),
            action: action.into(),
            description: None,
            source: KeybindingSource::User,
        }
    }

    #[test]
    fn no_issues_for_clean_bindings() {
        let bs = vec![b("Ctrl+P", "a"), b("Ctrl+Q", "b"), b("F5", "c")];
        let issues = validate_bindings(&bs);
        // Ctrl+Q is reserved (XOFF) so we expect ONE issue
        assert_eq!(issues.len(), 1);
        match &issues[0] {
            ValidationIssue::ReservedShortcut { action, .. } => assert_eq!(action, "b"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn duplicate_chord_reported() {
        let bs = vec![b("Ctrl+P", "a"), b("Ctrl+P", "b")];
        let issues = validate_bindings(&bs);
        assert!(issues
            .iter()
            .any(|i| matches!(i, ValidationIssue::DuplicateChord { .. })));
    }

    #[test]
    fn reserved_shortcut_flagged() {
        let bs = vec![Keybinding {
            chord: vec![parse_shortcut("Ctrl+Z").unwrap()],
            action: "x".into(),
            description: None,
            source: KeybindingSource::User,
        }];
        let issues = validate_bindings(&bs);
        assert!(issues
            .iter()
            .any(|i| matches!(i, ValidationIssue::ReservedShortcut { .. })));
    }

    #[test]
    fn chord_shadowing_flagged() {
        // Ctrl+P is bound; Ctrl+P Ctrl+Q chord can never fire because the
        // single Ctrl+P matches first.
        let bs = vec![b("Ctrl+P", "a"), b("Ctrl+P Ctrl+R", "b")];
        let issues = validate_bindings(&bs);
        assert!(issues
            .iter()
            .any(|i| matches!(i, ValidationIssue::ChordShadowed { .. })));
    }

    #[test]
    fn no_shadowing_when_no_prefix_relation() {
        let bs = vec![b("Ctrl+P", "a"), b("Ctrl+Q Ctrl+R", "b")];
        let issues = validate_bindings(&bs);
        assert!(issues
            .iter()
            .all(|i| !matches!(i, ValidationIssue::ChordShadowed { .. })));
    }
}
