//! Default keybindings shipped with attacode. User overrides loaded from
//! `~/.atta/code/keybindings.json` are merged on top via [`crate::merge_bindings`].
//!
//! Action namespaces:
//! - `editor.*` — input area: cursor, delete, history nav, submit
//! - `repl.*` — REPL/TUI controls: cancel, exit, scroll, clear
//! - `ask.*` — ask-dialog navigation: select / confirm / deny
//! - `transcript.*` — transcript block interaction (fold/expand)
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
        // ---- transcript ----
        //
        // 选中态默认是空的，这时 F5 作用于最新的那个块；用 Alt+Up/Down 走到更早的
        // 块上再按 F5，就能展开历史轮次里的工具输出。Alt 而不是裸 Up/Down：后者
        // 已经被 `editor.history.*` 占了，`Resolver` 取第一条匹配的绑定。
        bind(
            "Alt+Up",
            "transcript.select-prev",
            "Select the previous (older) foldable tool block",
        ),
        bind(
            "Alt+Down",
            "transcript.select-next",
            "Select the next (newer) foldable tool block",
        ),
        bind(
            "F5",
            "transcript.toggle-expand",
            "Expand/collapse the selected foldable tool output (most recent if none selected)",
        ),
        // ---- ask dialog ----
        //
        // 这三条在默认键位下**解析不到**：`Resolver` 取第一条匹配的绑定，Up/Down/Enter
        // 上面已经被 `editor.*` 占了。DSL 里没有"上下文/模式"的概念，所以这件事是在
        // app 那层解决的——权限对话框开着时它按对话框的语义解释 `editor.submit` /
        // `editor.history.*`（见 `dispatch_approval_action`）。这里留着这三条是给
        // 想把选项导航改绑到别的键的用户用的，改了就能走通。
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
    fn defaults_include_f5_toggle_expand() {
        let bs = default_bindings();
        let toggle = bs
            .iter()
            .find(|b| b.action == "transcript.toggle-expand")
            .unwrap();
        assert_eq!(toggle.chord.len(), 1);
        assert_eq!(toggle.chord[0].key, KeyCode::Function(5));
    }

    #[test]
    fn all_defaults_have_descriptions() {
        for b in default_bindings() {
            assert!(b.description.is_some(), "{} missing description", b.action);
        }
    }
}
