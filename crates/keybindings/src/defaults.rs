//! Default keybindings shipped with attacode. User overrides loaded from
//! `~/.atta/code/keybindings.json` are merged on top via [`crate::merge_bindings`].
//!
//! Action namespaces:
//! - `editor.*` — input area: cursor, delete, history nav, submit
//! - `repl.*` — REPL/TUI controls: cancel, exit, scroll, clear
//! - `ask.*` — ask-dialog navigation: select / confirm / deny
//! - `transcript.*` — transcript block interaction (select / fold / expand)
//!
//! 曾经列过一个 `slash.*` 命名空间，从来没有过绑定——slash 命令是打出来的，
//! 不是按键触发的，已经删掉。

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
        // ---- 光标移动 ----
        //
        // Up/Down 不在这里：它们已经被上面的 `editor.history.*` 占了（`Resolver`
        // 取第一条匹配的绑定），行间移动挂在那两个 action 上，由 app 按上下文分派
        // ——补全弹窗开着时移动选中项，否则移动光标。
        bind("Left", "editor.cursor.left", "Move cursor left"),
        bind("Right", "editor.cursor.right", "Move cursor right"),
        bind("Alt+Left", "editor.cursor.word-left", "Move back one word"),
        bind(
            "Alt+Right",
            "editor.cursor.word-right",
            "Move forward one word",
        ),
        bind("Home", "editor.cursor.line-start", "Move to start of line"),
        bind("End", "editor.cursor.line-end", "Move to end of line"),
        bind(
            "Delete",
            "editor.delete-forward",
            "Delete the character under the cursor",
        ),
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
        bind(
            "Tab",
            "ask.next-request",
            "Switch to the next pending approval",
        ),
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

    #[test]
    fn all_defaults_have_descriptions() {
        for b in default_bindings() {
            assert!(b.description.is_some(), "{} missing description", b.action);
        }
    }

    /// 整张默认键位表钉死。
    ///
    /// 以前只挑了三条具体绑定断言，于是"把 `Alt+Up` 从块选择改绑到滚动"这种改动
    /// 测试一声不吭（变异测试实测：全绿）。整表比对之后，任何一条绑定被改键/改名/
    /// 删掉都会红，改的人必须顺手改这里，也就顺手确认了自己是故意的。
    ///
    /// 注意大小写：`Shortcut::render()` 把字符键渲染成小写（`Ctrl+c`），而 DSL
    /// 解析时接受 `Ctrl+U`。这里跟渲染走。
    #[test]
    fn the_default_binding_table_is_pinned() {
        let mut actual: Vec<String> = default_bindings()
            .iter()
            .map(|b| {
                let keys: Vec<String> = b.chord.iter().map(|s| s.render()).collect();
                format!("{} = {}", keys.join(" "), b.action)
            })
            .collect();
        actual.sort();

        let mut expected: Vec<String> = [
            "Alt+Down = transcript.select-next",
            "Alt+Left = editor.cursor.word-left",
            "Alt+Right = editor.cursor.word-right",
            "Alt+Up = transcript.select-prev",
            "Ctrl+c = repl.cancel",
            "Ctrl+d = repl.exit",
            "Ctrl+k = editor.kill-to-eol",
            "Ctrl+l = editor.redraw",
            "Ctrl+u = editor.clear",
            "Ctrl+w = editor.delete-word",
            "Delete = editor.delete-forward",
            "Down = ask.next",
            "Down = editor.history.next",
            "End = editor.cursor.line-end",
            "Enter = ask.confirm",
            "Enter = editor.submit",
            "Esc = repl.dismiss",
            "F5 = transcript.toggle-expand",
            "Home = editor.cursor.line-start",
            "Left = editor.cursor.left",
            "PageDown = repl.scroll-down",
            "PageUp = repl.scroll-up",
            "Right = editor.cursor.right",
            "Shift+Enter = editor.newline",
            "Tab = ask.next-request",
            "Up = ask.prev",
            "Up = editor.history.prev",
            "n = ask.no-shortcut",
            "y = ask.yes-shortcut",
        ]
        .map(str::to_string)
        .to_vec();
        expected.sort();

        assert_eq!(actual, expected);
    }

    /// 同一个键绑了多条时 `Resolver` 只认第一条，排在后面的在默认键位下**够不到**。
    /// 这不是 bug（`ask.*` 由 app 在对话框上下文里另行分派），但必须是有意为之：
    /// 把"够不到的绑定"整张列出来钉住，谁不小心把新绑定排到了同键老绑定后面，
    /// 这条会告诉他。
    #[test]
    fn shadowed_bindings_are_exactly_the_ones_we_know_about() {
        let mut seen: Vec<String> = Vec::new();
        let mut shadowed: Vec<String> = Vec::new();
        for b in &default_bindings() {
            let key = b
                .chord
                .iter()
                .map(|s| s.render())
                .collect::<Vec<_>>()
                .join(" ");
            if seen.contains(&key) {
                shadowed.push(b.action.clone());
            } else {
                seen.push(key);
            }
        }
        shadowed.sort();
        assert_eq!(shadowed, vec!["ask.confirm", "ask.next", "ask.prev"]);
    }
}
