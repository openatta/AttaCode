//! attacode-keybindings
//!
//! Parser + matcher + chord resolver for terminal keyboard shortcuts.
//!
//! Pipeline:
//! 1. **Parser** ([`parse_shortcut`]) — turns `"Ctrl+Shift+P"` into a
//!    [`Shortcut`] (modifiers + key)
//! 2. **Default + user bindings** — defaults shipped in [`defaults`],
//!    user overrides loaded from `~/.atta/code/keybindings.json`
//! 3. **Validation** ([`validate`]) — checks for duplicate shortcuts, reserved
//!    shortcut conflicts (e.g. terminal-owned Ctrl-C), unknown actions
//! 4. **Resolver** ([`Resolver`]) — handles chord state machine: Esc-K-D
//!    becomes a 3-step path; partial chord state held until next key
//! 5. **Matcher** ([`Resolver::on_key`]) — receives crossterm `KeyEvent`,
//!    returns the matched action name (or progress through a chord)
//!
//! User config schema: see [`schema::USER_BINDINGS_JSON_SCHEMA`].

#![forbid(unsafe_code)]

pub mod defaults;
pub mod loader;
pub mod parser;
pub mod reserved;
pub mod resolver;
pub mod schema;
pub mod validate;

pub use defaults::default_bindings;
pub use loader::{load_user_bindings, merge_bindings, KeybindingsFile};
pub use parser::{parse_chord, parse_shortcut, Shortcut};
pub use reserved::is_reserved_shortcut;
pub use resolver::{ResolveOutcome, Resolver};
pub use schema::USER_BINDINGS_JSON_SCHEMA;
pub use validate::{validate_bindings, ValidationIssue};

use serde::{Deserialize, Serialize};

/// One keybinding: a chord (sequence of one or more shortcuts) + an action
/// name. Action names are domain-specific strings (e.g. `"cancel.turn"`,
/// `"submit"`, `"history.prev"`) that the consumer's command dispatcher
/// understands.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Keybinding {
    /// Sequence of shortcuts. Single-element vector = simple shortcut;
    /// multi-element = chord (e.g. Ctrl-X then Ctrl-C).
    pub chord: Vec<Shortcut>,
    /// Action name to dispatch when matched.
    pub action: String,
    /// Optional human-readable description (used by `/keybindings` slash).
    #[serde(default)]
    pub description: Option<String>,
    /// Source — `"default"` or `"user"`. Shown in `/keybindings` and helps
    /// validate (don't let user override reserved system shortcuts).
    #[serde(default = "default_source")]
    pub source: KeybindingSource,
}

fn default_source() -> KeybindingSource {
    KeybindingSource::User
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum KeybindingSource {
    Default,
    User,
}
