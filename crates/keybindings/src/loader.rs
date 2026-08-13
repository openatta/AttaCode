//! Load user keybindings from `~/.atta/code/keybindings.json` and merge with
//! defaults.

use crate::parser::parse_chord;
use crate::{Keybinding, KeybindingSource};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct KeybindingsFile {
    /// Each entry: `{ "shortcut": "Ctrl+P", "action": "...", "description": "..." }`.
    /// The `shortcut` field accepts a single shortcut or a chord (whitespace-separated).
    #[serde(default)]
    pub bindings: Vec<UserBindingEntry>,
}

#[derive(Debug, Deserialize)]
pub struct UserBindingEntry {
    pub shortcut: String,
    pub action: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// Load `~/.atta/code/keybindings.json` if present. Returns parsed entries
/// converted into [`Keybinding`]s. Parse errors of individual entries are
/// logged + skipped (don't bring down the whole file).
pub fn load_user_bindings() -> Vec<Keybinding> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let path = PathBuf::from(home)
        .join(".atta")
        .join("code")
        .join("keybindings.json");
    load_user_bindings_at(&path)
}

pub fn load_user_bindings_at(path: &Path) -> Vec<Keybinding> {
    if !path.exists() {
        return Vec::new();
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "could not read keybindings");
            return Vec::new();
        }
    };
    let file: KeybindingsFile = match serde_json::from_slice(&bytes) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "keybindings.json malformed; ignoring");
            return Vec::new();
        }
    };
    let mut out = Vec::with_capacity(file.bindings.len());
    for entry in file.bindings {
        match parse_chord(&entry.shortcut) {
            Ok(chord) => out.push(Keybinding {
                chord,
                action: entry.action,
                description: entry.description,
                source: KeybindingSource::User,
            }),
            Err(e) => {
                tracing::warn!(
                    shortcut = %entry.shortcut,
                    error = %e,
                    "ignoring keybinding entry with bad shortcut"
                );
            }
        }
    }
    out
}

/// Merge defaults + user bindings. User entries take precedence: any
/// default whose chord exactly matches a user entry is replaced. New user
/// chords are appended.
pub fn merge_bindings(defaults: Vec<Keybinding>, user: Vec<Keybinding>) -> Vec<Keybinding> {
    let mut out = defaults;
    for u in user {
        // Drop any default with same chord
        out.retain(|d| d.chord != u.chord);
        out.push(u);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn merge_user_overrides_default() {
        use crate::parser::parse_shortcut;
        let defaults = vec![Keybinding {
            chord: vec![parse_shortcut("Ctrl+C").unwrap()],
            action: "repl.cancel".into(),
            description: None,
            source: KeybindingSource::Default,
        }];
        let user = vec![Keybinding {
            chord: vec![parse_shortcut("Ctrl+C").unwrap()],
            action: "custom.action".into(),
            description: None,
            source: KeybindingSource::User,
        }];
        let merged = merge_bindings(defaults, user);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].action, "custom.action");
    }

    #[test]
    fn merge_user_appends_new_binding() {
        use crate::parser::parse_shortcut;
        let defaults = vec![Keybinding {
            chord: vec![parse_shortcut("Ctrl+C").unwrap()],
            action: "repl.cancel".into(),
            description: None,
            source: KeybindingSource::Default,
        }];
        let user = vec![Keybinding {
            chord: vec![parse_shortcut("Ctrl+P").unwrap()],
            action: "palette".into(),
            description: None,
            source: KeybindingSource::User,
        }];
        let merged = merge_bindings(defaults, user);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("does-not-exist.json");
        let r = load_user_bindings_at(&p);
        assert!(r.is_empty());
    }

    #[test]
    fn load_parses_basic_entries() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("kb.json");
        std::fs::write(
            &p,
            r#"{
                "bindings": [
                    {"shortcut": "Ctrl+P", "action": "palette", "description": "open palette"},
                    {"shortcut": "Ctrl+X Ctrl+C", "action": "force-exit"}
                ]
            }"#,
        )
        .unwrap();
        let r = load_user_bindings_at(&p);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].action, "palette");
        assert_eq!(r[1].chord.len(), 2);
    }

    #[test]
    fn load_skips_bad_entries_keeps_good_ones() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("kb.json");
        std::fs::write(
            &p,
            r#"{"bindings":[
                {"shortcut": "F25", "action": "broken"},
                {"shortcut": "Ctrl+P", "action": "ok"}
            ]}"#,
        )
        .unwrap();
        let r = load_user_bindings_at(&p);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].action, "ok");
    }
}
