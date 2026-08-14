//! `attacode` — application entry point: terminal setup, event loop, key dispatch.
//!
//! Owns two concerns bridge intentionally doesn't: terminal I/O (ratatui/crossterm) and
//! UI-local composer state (draft/cursor/completion selection). Everything Core-related
//! goes through `bridge::EngineHandle` — this file never touches an AttaCore type directly.
//!
//! Composer editing lives in the `impl LocalUi` block near the bottom: insert/delete
//! at the cursor, char/word/line motion, kill-to-end. `cursor` is a byte index into
//! `draft` and is always on a char boundary; the renderer indexes the same way, so
//! neither side converts. Still missing: prompt history recall (the `editor.history.*`
//! actions are bound but currently drive completion selection and line motion),
//! selection, and undo.

use bridge::{BootstrapConfig, BridgeCommand, EngineHandle, Resume, DEFAULT_MODEL};
use crossterm::event::{
    Event, EventStream, KeyCode as CtKeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use keybindings::{default_bindings, ResolveOutcome, Resolver};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::stdout;
use tokio_stream::StreamExt;
use tui::frame_state::{
    ApprovalOption, ApprovalRequest, CompletionCandidate, CompletionKind, CompletionPopupState,
};

const USAGE: &str = "\
attacode — AttaCore 引擎的终端 UI

用法: attacode [选项]

选项:
  -m, --model <NAME>  这次运行用的模型（压过 ANTHROPIC_MODEL 和 settings.json）
  -c, --continue      接着本项目最近一次会话跑
      --resume <ID>   接着指定的会话跑（id 见 ~/.atta/sessions/<项目>/）
  -h, --help          打印这段帮助
";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let Some(args) = Args::parse(std::env::args().skip(1))? else {
        print!("{USAGE}");
        return Ok(());
    };

    let mut config = BootstrapConfig::defaults(DEFAULT_MODEL);
    // `--model` 是为这一次运行显式指定的，比 `ANTHROPIC_MODEL` 更近，压过它。
    if args.model.is_some() {
        config.model_override = args.model;
    }
    config.resume = args.resume;
    let (handle, cancel) = bridge::start(config).await?;

    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(out))?;

    let result = run(&mut terminal, handle.as_ref()).await;

    cancel.cancel();
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

/// 命令行参数。手写解析而不是拉 `clap`：目前就这几个开关，一个依赖换不来这点便利。
#[derive(Default)]
struct Args {
    model: Option<String>,
    resume: Option<Resume>,
}

impl Args {
    /// `Ok(None)` = 用户要的是 `--help`，调用方打印用法后正常退出。
    /// 不认识的参数是错误，不是静默忽略——静默忽略会让打错的 `--modle x` 看起来生效了。
    fn parse(args: impl Iterator<Item = String>) -> anyhow::Result<Option<Self>> {
        let mut out = Args::default();
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            let value = |args: &mut std::iter::Peekable<_>| -> anyhow::Result<String> {
                Iterator::next(args)
                    .filter(|v: &String| !v.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("{arg} 需要一个值"))
            };
            match arg.as_str() {
                "-h" | "--help" => return Ok(None),
                "-m" | "--model" => out.model = Some(value(&mut args)?),
                "-c" | "--continue" => out.resume = Some(Resume::Latest),
                "--resume" => out.resume = Some(Resume::Id(value(&mut args)?)),
                other => match split_long(other) {
                    Some(("--model", name)) => out.model = Some(name.to_string()),
                    Some(("--resume", id)) => out.resume = Some(Resume::Id(id.to_string())),
                    Some((flag, _)) => anyhow::bail!("{flag} 不接受 `=值` 形式\n\n{USAGE}"),
                    None => anyhow::bail!("未知参数: {other}\n\n{USAGE}"),
                },
            }
        }
        Ok(Some(out))
    }
}

/// `--flag=value` 拆成 `("--flag", "value")`；值为空视为没写（走上面的报错分支）。
fn split_long(arg: &str) -> Option<(&str, &str)> {
    let (flag, value) = arg.split_once('=')?;
    (!value.is_empty()).then_some((flag, value))
}

/// UI-local composer state — never sent to bridge; merged onto bridge's
/// `FrameState` snapshot right before each render.
struct LocalUi {
    draft: String,
    /// 光标在 `draft` 里的字节下标，恒在字符边界上。见下面的 `impl LocalUi` 编辑块。
    cursor: usize,
    /// Latest slash-command list from `bridge::commands` — Core's own live
    /// `CommandRegistry`, refreshed when the engine reports the skill catalog changed.
    commands: Vec<CompletionCandidate>,
    completion_selected: usize,
    /// Esc closes the popup without touching `draft`; re-typing anything reopens it
    /// (see `note_draft_changed`).
    completion_dismissed: bool,
    /// 权限对话框里高亮的选项下标。和补全选择一样是纯 UI-本地状态：bridge 只知道
    /// 有哪些选项，不知道光标停在哪一项。渲染前由 `merge` 覆盖进快照。
    approval_selected: usize,
    /// 转录滚动位置：`None` = 跟住底部（新内容自动滚进来），`Some(n)` = 冻在
    /// "跳过前 n 条"的位置不动。滚动位置同样是 UI-本地的——bridge 每帧都会把
    /// `auto_follow` 置回 true，由 `merge` 按这个字段覆盖。
    scroll_offset: Option<usize>,
    /// 转录里被选中的可折叠块，`transcript.toggle-expand` 的目标。`None` = 没选，
    /// 这时目标是最新的那个块（不用先导航就能展开刚跑完的工具，是最常见的动作）。
    /// 和滚动位置一样是 UI-本地状态，渲染前由 `merge` 覆盖进快照。
    selected_block: Option<String>,
    /// 这次会话里提交过的输入，旧的在前。`--continue` 起手时会用转录里恢复出来的
    /// 用户输入填上（见 [`run`]），所以接着上次跑的时候历史也是接着的。
    /// 只在内存里，不落盘。
    history: Vec<String>,
    /// 正在浏览历史的第几条；`None` = 在自己的草稿里。
    history_pos: Option<usize>,
    /// 开始翻历史之前的那份草稿。翻过头（回到最新之后）时原样还回去——
    /// 手打了一半的东西不该因为好奇按了下 `↑` 就没了。
    history_stash: String,
    /// 上一帧转录正文区的高度，`PageUp`/`PageDown` 的步长。渲染循环每帧更新。
    viewport_lines: usize,
    /// `Ctrl+L`：下一帧渲染前先清屏，用来收拾被别的进程写花的终端。
    redraw_requested: bool,
}

impl LocalUi {
    fn new(commands: Vec<CompletionCandidate>) -> Self {
        Self {
            draft: String::new(),
            cursor: 0,
            commands,
            completion_selected: 0,
            completion_dismissed: false,
            approval_selected: 0,
            scroll_offset: None,
            selected_block: None,
            history: Vec::new(),
            history_pos: None,
            history_stash: String::new(),
            viewport_lines: 1,
            redraw_requested: false,
        }
    }

    /// 在可折叠块之间移动选中位置。`delta < 0` 往更早的块走，`> 0` 往更新的走。
    ///
    /// 边界不是简单夹住：从最新那个再往后走会**清掉**选中态（回到"跟最新的"），
    /// 因为选中态本身的默认语义就是"最后一个块"，多一个"停在最后一个上面"的
    /// 状态没有意义。没选中时往前走从最新的块开始，往后走没有去处，不动。
    fn select_block(&mut self, snapshot: &tui::FrameState, delta: isize) {
        let blocks = foldable_blocks(snapshot);
        if blocks.is_empty() {
            return;
        }
        let current = self
            .selected_block
            .as_ref()
            .and_then(|sel| blocks.iter().position(|b| b == sel));
        self.selected_block = match (current, delta) {
            (None, d) if d < 0 => blocks.last().cloned(),
            (None, _) => return,
            (Some(i), d) => {
                let next = i as isize + d;
                if next < 0 {
                    Some(blocks[0].clone())
                } else if next as usize >= blocks.len() {
                    None
                } else {
                    Some(blocks[next as usize].clone())
                }
            }
        };
        self.reveal_selection(snapshot);
    }

    /// 把选中的块滚进视口。落在最后一屏里就干脆回到跟随模式——否则用户选了个
    /// 靠近底部的块，视口反而被钉在一个固定 offset 上，新内容进来看着像卡住。
    fn reveal_selection(&mut self, snapshot: &tui::FrameState) {
        let Some(sel) = self.selected_block.as_deref() else {
            self.scroll_offset = None;
            return;
        };
        let entries = &snapshot.transcript.body.entries;
        let Some(first) = entries
            .iter()
            .position(|e| e.block_id.as_deref() == Some(sel))
        else {
            return;
        };
        let page = self.viewport_lines.max(1);
        if first >= entries.len().saturating_sub(page) {
            self.scroll_offset = None;
        } else {
            self.scroll_offset = Some(first);
        }
    }

    /// 往上翻一页。已经跟在底部时，先把"底部"换算成一个具体的 offset 再往上走。
    /// 内容还没满一屏就没得翻——保持跟随，免得弹出一个 "0 lines above" 的提示条。
    fn scroll_up(&mut self, total: usize) {
        let page = self.viewport_lines.max(1);
        if total <= page {
            return;
        }
        let top = self.scroll_offset.unwrap_or(total - page);
        self.scroll_offset = Some(top.saturating_sub(page));
    }

    /// 往下翻一页。翻到底就回到跟随模式，而不是停在"恰好是最后一屏"的固定
    /// offset 上——否则新消息进来会往视口外走，看着像卡住了。
    fn scroll_down(&mut self, total: usize) {
        let page = self.viewport_lines.max(1);
        let Some(top) = self.scroll_offset else {
            return;
        };
        let bottom_top = total.saturating_sub(page);
        if top + page >= bottom_top {
            self.scroll_offset = None;
        } else {
            self.scroll_offset = Some(top + page);
        }
    }

    fn note_draft_changed(&mut self) {
        self.completion_selected = 0;
        self.completion_dismissed = false;
    }
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    handle: &dyn EngineHandle,
) -> anyhow::Result<()> {
    let mut frame_rx = handle.subscribe();
    let mut commands_rx = handle.subscribe_commands();
    let mut resolver = Resolver::new(default_bindings());
    let mut local = LocalUi::new(commands_rx.borrow().clone());
    // `--continue` / `--resume` 起手时，输入历史也接着上次：转录里恢复出来的用户
    // 输入就是上次提交过的东西。bridge 不单独暴露它们，从首帧快照里读即可。
    local.history = user_prompts(&frame_rx.borrow());
    let mut keys = EventStream::new();

    loop {
        let snapshot = merge(frame_rx.borrow().clone(), &local);
        if std::mem::take(&mut local.redraw_requested) {
            terminal.clear()?;
        }
        terminal.draw(|f| {
            // 翻页步长取自这一帧真正的正文高度，而不是猜一个常数——它随状态行、
            // 子代理条、多行 composer 一起变。
            local.viewport_lines =
                usize::from(tui::layout::transcript_body_height(f.area(), &snapshot));
            tui::layout::render(f, f.area(), &snapshot, spinner_frame());
        })?;

        tokio::select! {
            changed = frame_rx.changed() => {
                if changed.is_err() {
                    // bridge 已经退出（Agent 后台 task 崩溃/关闭）。
                    break;
                }
            }
            changed = commands_rx.changed() => {
                if changed.is_ok() {
                    local.commands = commands_rx.borrow().clone();
                }
            }
            ev = keys.next() => {
                let Some(ev) = ev else { break };
                let Event::Key(key) = ev? else { continue };
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if !dispatch_key(key, &mut resolver, &mut local, handle, &snapshot) {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Returns `false` when the loop should exit.
fn dispatch_key(
    key: KeyEvent,
    resolver: &mut Resolver,
    local: &mut LocalUi,
    handle: &dyn EngineHandle,
    snapshot: &tui::FrameState,
) -> bool {
    let outcome = resolver.on_key(&key);
    // 有待批准的权限请求时，键盘整体归对话框——`FrameState` 那边 composer 已经是
    // `locked`，路由不跟着改的话 Enter 会把草稿提交给一个正卡在权限检查上的引擎。
    if let Some(req) = active_approval(snapshot) {
        return match outcome {
            ResolveOutcome::Action(action) => dispatch_approval_action(&action, local, handle, req),
            // 对话框开着时普通字符没有去处（composer 锁着），直接丢。
            _ => true,
        };
    }
    match outcome {
        // `y`/`n` 绑的是裸字符。没有对话框时它们就是普通输入——不然打 "yes" 会丢字母。
        ResolveOutcome::Action(action)
            if matches!(action.as_str(), "ask.yes-shortcut" | "ask.no-shortcut") =>
        {
            insert_char(key, local);
            true
        }
        ResolveOutcome::Action(action) => dispatch_action(&action, local, handle, snapshot),
        ResolveOutcome::Partial | ResolveOutcome::ChordCancelled => true,
        ResolveOutcome::Unmatched(_) => {
            insert_char(key, local);
            true
        }
    }
}

/// 权限对话框开着时的键位。
///
/// 每个分支都写了两个 action 名，因为 `Resolver` 取第一条匹配的绑定，而
/// `default_bindings()` 里 `editor.*` 排在 `ask.*` 前面、占着同样的 Up/Down/Enter
/// ——`ask.prev`/`ask.next`/`ask.confirm` 在默认键位下根本轮不到（用户把它们改绑到
/// 别的键才会出现）。认两个名字，两条路都通。
fn dispatch_approval_action(
    action: &str,
    local: &mut LocalUi,
    handle: &dyn EngineHandle,
    req: &ApprovalRequest,
) -> bool {
    match action {
        "editor.history.prev" | "ask.prev" => {
            local.approval_selected = step(local.approval_selected, -1, req.options.len())
        }
        "editor.history.next" | "ask.next" => {
            local.approval_selected = step(local.approval_selected, 1, req.options.len())
        }
        "editor.submit" | "ask.confirm" => {
            let choice = req
                .options
                .get(local.approval_selected)
                .copied()
                .unwrap_or(ApprovalOption::Deny);
            respond(handle, local, req, choice);
        }
        "ask.yes-shortcut" => respond(handle, local, req, ApprovalOption::PermitOnce),
        "ask.no-shortcut" | "repl.dismiss" => respond(handle, local, req, ApprovalOption::Deny),
        // 权限检查把 turn 卡住了，中断它仍然是合理动作。
        "repl.cancel" => {
            let _ = handle.dispatch(BridgeCommand::CancelTurn);
        }
        _ => {}
    }
    true
}

fn active_approval(snapshot: &tui::FrameState) -> Option<&ApprovalRequest> {
    let approval = snapshot.composer.content.approval.as_ref()?;
    approval.pending.get(approval.active_idx)
}

/// 在 `[0, len)` 内环绕移动。`len == 0` 时原地不动（没有选项可选）。
fn step(current: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let len = len as isize;
    (((current as isize + delta) % len + len) % len) as usize
}

fn dispatch_action(
    action: &str,
    local: &mut LocalUi,
    handle: &dyn EngineHandle,
    snapshot: &tui::FrameState,
) -> bool {
    let completion_active = compute_completion(local).is_some();
    match action {
        "editor.submit" if completion_active && !completion_already_typed(local) => {
            accept_completion(local)
        }
        "editor.submit" => return submit(local, handle, snapshot),
        "editor.newline" => local.insert('\n'),
        "editor.clear" => {
            local.draft.clear();
            local.cursor = 0;
            local.note_draft_changed();
        }
        "editor.delete-word" => local.delete_word_before(),
        "editor.kill-to-eol" => local.kill_to_line_end(),
        "editor.delete-forward" => local.delete_forward(),
        "editor.cursor.left" => local.move_char(-1),
        "editor.cursor.right" => local.move_char(1),
        "editor.cursor.word-left" => local.move_word(-1),
        "editor.cursor.word-right" => local.move_word(1),
        "editor.cursor.line-start" => local.cursor = local.line_start(),
        "editor.cursor.line-end" => local.cursor = local.line_end(),
        "editor.redraw" => local.redraw_requested = true,
        // Up/Down 一键三义，按当前上下文取一个：补全弹窗开着时移动选中项；
        // 多行草稿里先在行间走；走到首/末行（单行草稿则一开始就是）才翻历史。
        "editor.history.prev" if completion_active => move_completion_selection(local, -1),
        "editor.history.next" if completion_active => move_completion_selection(local, 1),
        "editor.history.prev" => local.up(),
        "editor.history.next" => local.down(),
        "repl.scroll-up" => {
            local.scroll_up(snapshot.transcript.body.entries.len());
        }
        "repl.scroll-down" => {
            local.scroll_down(snapshot.transcript.body.entries.len());
        }
        "repl.cancel" => {
            let _ = handle.dispatch(BridgeCommand::CancelTurn);
        }
        "repl.exit" => {
            if local.draft.is_empty() {
                return false;
            }
        }
        "repl.dismiss" if completion_active => local.completion_dismissed = true,
        // Esc 没有对话框/弹窗要关时，退出块选择——和它在别处"退出当前模式"的语义一致。
        "repl.dismiss" if local.selected_block.is_some() => {
            local.selected_block = None;
        }
        "transcript.select-prev" => local.select_block(snapshot, -1),
        "transcript.select-next" => local.select_block(snapshot, 1),
        "transcript.toggle-expand" => toggle_selected_block(local, snapshot, handle),
        _ => {}
    }
    true
}

/// header 钉的那句（最后一条用户输入）此刻在视口里看得见吗？
///
/// 看得见就不用钉——sticky header 的意义是"滚远了还知道在答哪个问题"。
/// 跟随模式下视口是最后一屏，滚动模式下是 `[offset, offset+高度)`。
fn prompt_is_visible(frame: &tui::FrameState, local: &LocalUi) -> bool {
    let entries = &frame.transcript.body.entries;
    let Some(idx) = entries
        .iter()
        .rposition(|e| e.kind == tui::frame_state::LineKind::UserPrompt)
    else {
        return true; // 没有 prompt，也就没什么可钉的
    };
    let page = local.viewport_lines.max(1);
    match local.scroll_offset {
        Some(offset) => idx >= offset && idx < offset + page,
        None => idx + page >= entries.len(),
    }
}

/// 转录里的用户输入，按出现顺序——resume 之后用它续上输入历史。
fn user_prompts(frame: &tui::FrameState) -> Vec<String> {
    frame
        .transcript
        .body
        .entries
        .iter()
        .filter(|e| e.kind == tui::frame_state::LineKind::UserPrompt)
        .map(|e| e.text.clone())
        .collect()
}

/// 转录里所有可折叠块的 id，按出现顺序、去重。一个块占多行，这里要的是块的序列。
fn foldable_blocks(snapshot: &tui::FrameState) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for entry in &snapshot.transcript.body.entries {
        if let Some(id) = &entry.block_id {
            if out.last() != Some(id) {
                out.push(id.clone());
            }
        }
    }
    out
}

/// F5：展开/折叠选中的块；没选中就作用于最新的那个。
///
/// "没选中 = 最新的"是有意的默认：刚跑完一个工具想看全文是最常见的动作，
/// 不该先按几下导航键。要看更早那次调用的详情（需求场景 7），
/// 用 `transcript.select-prev` 走过去再按 F5。
fn toggle_selected_block(local: &LocalUi, snapshot: &tui::FrameState, handle: &dyn EngineHandle) {
    let block_id = local.selected_block.clone().or_else(|| {
        snapshot
            .transcript
            .body
            .entries
            .iter()
            .rev()
            .find_map(|e| e.block_id.clone())
    });
    let Some(block_id) = block_id else { return };
    let _ = handle.dispatch(BridgeCommand::ToggleExpand { block_id });
}

/// app 自己处理、不转发给 Core 的 slash 命令。
enum LocalCommand {
    Quit,
    /// `/model` 不带参数 = 报当前模型；带参数 = 切过去。
    Model(Option<String>),
}

/// `/` 前缀的一次性分流。认出来的在本地处理，其余原样转发给 Core 解析——补全
/// 列表就是 Core 那份实时 `CommandRegistry`（见 `crates/bridge/src/commands.rs`），
/// 所以弹窗里选中的命令提交后一定解析得出来。
///
/// `/model` 归本地是因为 Core 的内置 local 命令表里没有它：Core 侧的开关是
/// `EngineCommand::UpdateModel`（一条 `InputMessage`），不是一条 slash 命令。
fn local_command(text: &str) -> Option<LocalCommand> {
    let (head, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    match head {
        "/quit" | "/exit" => Some(LocalCommand::Quit),
        "/model" => Some(LocalCommand::Model({
            let name = rest.trim();
            (!name.is_empty()).then(|| name.to_string())
        })),
        _ => None,
    }
}

/// 本地命令也要出现在补全弹窗里——它们和 Core 那些一样是用户敲 `/` 想找的东西，
/// 只是解析发生在这一层。Core 的 registry 里没有同名项，不用去重。
fn local_command_candidates() -> Vec<CompletionCandidate> {
    [
        (
            "/model",
            "Switch the model for this session  (args: [name])",
        ),
        ("/quit", "Exit AttaCode"),
        ("/exit", "Exit AttaCode"),
    ]
    .into_iter()
    .map(|(name, description)| CompletionCandidate {
        name: name.into(),
        description: description.into(),
    })
    .collect()
}

/// 返回 `false` 时调用方应退出事件循环。
fn submit(local: &mut LocalUi, handle: &dyn EngineHandle, snapshot: &tui::FrameState) -> bool {
    if local.draft.is_empty() {
        return true;
    }
    let text = std::mem::take(&mut local.draft);
    local.cursor = 0;
    local.remember(&text);
    local.note_draft_changed();
    // 发了新消息就跳回底部——不然自己刚发的那句在视口外，看着像没发出去。
    local.scroll_offset = None;
    local.selected_block = None;
    let cmd = match local_command(&text) {
        Some(LocalCommand::Quit) => return false,
        Some(LocalCommand::Model(Some(name))) => BridgeCommand::SetModel { name },
        Some(LocalCommand::Model(None)) => BridgeCommand::Note {
            text: format!(
                "current model: {} · usage: /model <name>",
                snapshot.footer_hints.model
            ),
        },
        None => BridgeCommand::Submit { text },
    };
    let _ = handle.dispatch(cmd);
    true
}

/// 回一个权限决定，并把选择位复位——下一个请求从第一项（"Yes"）开始，而不是
/// 继承上一个对话框停在哪。
fn respond(
    handle: &dyn EngineHandle,
    local: &mut LocalUi,
    req: &ApprovalRequest,
    decision: ApprovalOption,
) {
    let _ = handle.dispatch(BridgeCommand::RespondPermission {
        prompt_id: req.prompt_id.clone(),
        decision,
    });
    local.approval_selected = 0;
}

fn insert_char(key: KeyEvent, local: &mut LocalUi) {
    match key.code {
        CtKeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => local.insert(c),
        CtKeyCode::Backspace => local.delete_backward(),
        _ => {}
    }
}

/// 草稿的行内编辑。
///
/// `cursor` 是**字节**下标，恒落在字符边界上（下面每个改动都按字符走）——渲染那边
/// 也按字节切分（`tui::regions::composer::editor_lines`），两边用同一套坐标就不用
/// 来回换算，也不用把宽字符拆成两半。
impl LocalUi {
    fn insert(&mut self, c: char) {
        self.draft.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.note_draft_changed();
    }

    /// Backspace：删光标**前**一个字符。
    fn delete_backward(&mut self) {
        let Some(prev) = self.draft[..self.cursor].chars().next_back() else {
            return;
        };
        self.cursor -= prev.len_utf8();
        self.draft.remove(self.cursor);
        self.note_draft_changed();
    }

    /// Delete：删光标**上**的那个字符，光标不动。
    fn delete_forward(&mut self) {
        if self.draft[self.cursor..].chars().next().is_some() {
            self.draft.remove(self.cursor);
            self.note_draft_changed();
        }
    }

    /// `Ctrl+W`：往前删一个词，连同词前的空白。光标停在删除处。
    fn delete_word_before(&mut self) {
        let target = self.word_boundary(-1);
        if target != self.cursor {
            self.draft.replace_range(target..self.cursor, "");
            self.cursor = target;
            self.note_draft_changed();
        }
    }

    /// `Ctrl+K`：从光标删到**本行**行尾（多行草稿里不越过 `\n`）。已经在行尾时
    /// 把那个换行本身删掉，也就是把下一行接上来——readline 的老习惯。
    fn kill_to_line_end(&mut self) {
        let end = self.line_end();
        let cut = if end == self.cursor {
            self.next_boundary()
        } else {
            end
        };
        if cut != self.cursor {
            self.draft.replace_range(self.cursor..cut, "");
            self.note_draft_changed();
        }
    }

    fn move_char(&mut self, delta: isize) {
        self.cursor = if delta < 0 {
            self.prev_boundary()
        } else {
            self.next_boundary()
        };
    }

    fn move_word(&mut self, delta: isize) {
        self.cursor = self.word_boundary(delta);
    }

    /// 行间移动，尽量保持列（按字符数算，不是字节）。已经在首/末行时不动。
    fn move_line(&mut self, delta: isize) {
        let start = self.line_start();
        let column = self.draft[start..self.cursor].chars().count();
        let target_start = if delta < 0 {
            if start == 0 {
                return;
            }
            self.draft[..start - 1] // -1 跳过上一行末尾那个 '\n'
                .rfind('\n')
                .map(|i| i + 1)
                .unwrap_or(0)
        } else {
            let end = self.line_end();
            if end == self.draft.len() {
                return;
            }
            end + 1
        };
        let target_line = &self.draft[target_start..];
        let target_line = &target_line[..target_line.find('\n').unwrap_or(target_line.len())];
        let offset = target_line
            .char_indices()
            .nth(column)
            .map(|(i, _)| i)
            .unwrap_or(target_line.len());
        self.cursor = target_start + offset;
    }

    /// `↑`/`↓` 的语义分派：多行草稿里先在行间走，走到头了才翻历史。
    /// 单行草稿的光标既在首行也在末行，所以直接翻历史——这才是常见情形。
    fn up(&mut self) {
        if self.line_start() > 0 {
            self.move_line(-1);
        } else {
            self.recall(-1);
        }
    }

    fn down(&mut self) {
        if self.line_end() < self.draft.len() {
            self.move_line(1);
        } else {
            self.recall(1);
        }
    }

    /// 翻历史。`-1` 往更早翻，`1` 往更新翻；翻过最新一条就把原来的草稿还回来。
    ///
    /// 编辑一条翻出来的历史**不会**退出浏览态（位置还在，可以接着往上翻），
    /// 但再翻一次会覆盖掉编辑——和 zsh 的行为一致，也是最容易预期的一种。
    fn recall(&mut self, delta: isize) {
        if self.history.is_empty() {
            return;
        }
        let next = match (self.history_pos, delta < 0) {
            // 第一次往上翻：先把手头的草稿收起来。
            (None, true) => {
                self.history_stash = std::mem::take(&mut self.draft);
                Some(self.history.len() - 1)
            }
            (None, false) => return, // 没在浏览历史，往下没有去处
            (Some(i), true) => Some(i.saturating_sub(1)),
            (Some(i), false) => (i + 1 < self.history.len()).then_some(i + 1),
        };
        match next {
            Some(i) => {
                self.draft = self.history[i].clone();
                self.history_pos = Some(i);
            }
            // 翻过最新一条 = 回到自己的草稿。
            None => {
                self.draft = std::mem::take(&mut self.history_stash);
                self.history_pos = None;
            }
        }
        self.cursor = self.draft.len();
        self.completion_selected = 0;
        self.completion_dismissed = false;
    }

    /// 记一条提交过的输入。连续重复的不重复记——连按两次同一条命令之后，
    /// 按一下 `↑` 应该看到它，而不是按两下才翻过去。
    fn remember(&mut self, text: &str) {
        if self.history.last().map(String::as_str) != Some(text) {
            self.history.push(text.to_string());
        }
        self.history_pos = None;
        self.history_stash.clear();
    }

    fn line_start(&self) -> usize {
        self.draft[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0)
    }

    fn line_end(&self) -> usize {
        self.draft[self.cursor..]
            .find('\n')
            .map(|i| self.cursor + i)
            .unwrap_or(self.draft.len())
    }

    fn prev_boundary(&self) -> usize {
        self.draft[..self.cursor]
            .chars()
            .next_back()
            .map(|c| self.cursor - c.len_utf8())
            .unwrap_or(0)
    }

    fn next_boundary(&self) -> usize {
        self.draft[self.cursor..]
            .chars()
            .next()
            .map(|c| self.cursor + c.len_utf8())
            .unwrap_or(self.cursor)
    }

    /// 往 `delta` 方向的词边界。词的定义就是"非空白的一段"：往前先跳过空白再跳过
    /// 词身，往后反过来——和 `Ctrl+W` 一直以来的语义一致。
    fn word_boundary(&self, delta: isize) -> usize {
        if delta < 0 {
            let head = &self.draft[..self.cursor];
            let trimmed = head.trim_end_matches(char::is_whitespace);
            trimmed
                .rfind(char::is_whitespace)
                .map(|i| i + trimmed[i..].chars().next().map_or(1, char::len_utf8))
                .unwrap_or(0)
        } else {
            let tail = &self.draft[self.cursor..];
            let skipped = tail.len() - tail.trim_start_matches(char::is_whitespace).len();
            let rest = &tail[skipped..];
            let word = rest.find(char::is_whitespace).unwrap_or(rest.len());
            self.cursor + skipped + word
        }
    }
}

/// Slash-command completion is UI-local, computed fresh from `local.draft` +
/// `local.commands` each time (mirrors how draft/cursor are already merged in) —
/// bridge only supplies the raw candidate list, not the filtered/open-or-closed popup.
fn compute_completion(local: &LocalUi) -> Option<CompletionPopupState> {
    if local.completion_dismissed {
        return None;
    }
    let rest = local.draft.strip_prefix('/')?;
    if rest.is_empty() || rest.contains(' ') || rest.contains('\n') {
        return None;
    }
    let candidates: Vec<CompletionCandidate> = local_command_candidates()
        .into_iter()
        .chain(local.commands.iter().cloned())
        .filter(|c| c.name.trim_start_matches('/').starts_with(rest))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let selected = local.completion_selected.min(candidates.len() - 1);
    Some(CompletionPopupState {
        kind: CompletionKind::SlashCommand,
        query: rest.to_string(),
        candidates,
        selected,
    })
}

/// 选中的候选就是已经打完的那条命令——这时 Enter 该**提交**，不是再补一次。
///
/// 不加这条判断的话，打全 `/model` 再回车只会把草稿补成 `/model `（多个空格），
/// 得按第二次才提交；更糟的是接着打的字会续在同一条草稿上，看起来像"回车没反应"。
/// 真机跑一遍才发现——这条路径所有单测都是"打一半"的前缀，从没打全过。
fn completion_already_typed(local: &LocalUi) -> bool {
    let Some(popup) = compute_completion(local) else {
        return false;
    };
    popup
        .candidates
        .get(popup.selected)
        .is_some_and(|c| local.draft == c.name)
}

fn move_completion_selection(local: &mut LocalUi, delta: i32) {
    let Some(popup) = compute_completion(local) else {
        return;
    };
    let len = popup.candidates.len() as i32;
    let next = (popup.selected as i32 + delta).rem_euclid(len);
    local.completion_selected = next as usize;
}

fn accept_completion(local: &mut LocalUi) {
    let Some(popup) = compute_completion(local) else {
        return;
    };
    let Some(candidate) = popup.candidates.get(popup.selected) else {
        return;
    };
    local.draft = format!("{} ", candidate.name);
    local.cursor = local.draft.len();
    local.note_draft_changed();
}

fn merge(mut frame: tui::FrameState, local: &LocalUi) -> tui::FrameState {
    frame.composer.content.editor.draft = local.draft.clone();
    frame.composer.content.editor.cursor = local.cursor;
    // bridge 每帧都把 `auto_follow` 置回 true（它不知道用户往上翻了），滚动位置在这里
    // 覆盖。夹一次是防转录变短之后 offset 悬空。
    if let Some(offset) = local.scroll_offset {
        let total = frame.transcript.body.entries.len();
        frame.transcript.body.auto_follow = false;
        frame.transcript.body.scroll.offset = offset.min(total.saturating_sub(1));
    }
    // 顶上那条 sticky header 只在它钉的那句**已经看不见**时才留着。bridge 无条件
    // 给出当前 prompt（它不知道视口多高、滚到哪了），可见的时候留着就是同一句话
    // 在屏幕上出现两次。这一步只有 app 做得了：视口高度和滚动位置都在这边。
    if prompt_is_visible(&frame, local) {
        frame.transcript.header.text = None;
        frame.transcript.header.source = tui::frame_state::HeaderSource::None;
    }
    // 选中的块可能已经不在了（`/clear` 之类），这时当作没选中——本地下标只是个
    // 光标，块的存在与否是 bridge 说了算的。
    frame.transcript.body.selected_block = local.selected_block.clone().filter(|id| {
        frame
            .transcript
            .body
            .entries
            .iter()
            .any(|e| e.block_id.as_deref() == Some(id.as_str()))
    });
    match &mut frame.composer.content.approval {
        Some(approval) => {
            let active = approval.active_idx;
            if let Some(req) = approval.pending.get_mut(active) {
                // 越界夹回 0：选项数是 bridge 说了算的，本地下标只是个光标。
                req.selected_option = if local.approval_selected < req.options.len() {
                    local.approval_selected
                } else {
                    0
                };
            }
        }
        None => frame.composer.content.completion = compute_completion(local),
    }
    frame
}

fn spinner_frame() -> char {
    const FRAMES: [char; 4] = ['|', '/', '-', '\\'];
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    FRAMES[((ms / 120) % FRAMES.len() as u128) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tui::frame_state::{ApprovalState, ApprovalViewMode, LineKind, TranscriptEntry};

    fn local_with(draft: &str) -> LocalUi {
        let commands = vec![
            CompletionCandidate {
                name: "/commit".into(),
                description: "Create a commit".into(),
            },
            CompletionCandidate {
                name: "/compact".into(),
                description: "Compact context".into(),
            },
            CompletionCandidate {
                name: "/help".into(),
                description: "Show help".into(),
            },
        ];
        let mut local = LocalUi::new(commands);
        local.draft = draft.to_string();
        // 光标放末尾——用户敲出这段草稿之后就是这个状态。
        local.cursor = local.draft.len();
        local
    }

    #[test]
    fn no_popup_without_slash_prefix() {
        assert!(compute_completion(&local_with("hello")).is_none());
    }

    #[test]
    fn no_popup_once_a_space_is_typed() {
        assert!(compute_completion(&local_with("/commit fix bug")).is_none());
    }

    #[test]
    fn filters_by_prefix() {
        let popup = compute_completion(&local_with("/com")).unwrap();
        let names: Vec<_> = popup.candidates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["/commit", "/compact"]);
    }

    #[test]
    fn no_popup_when_nothing_matches() {
        assert!(compute_completion(&local_with("/zzz")).is_none());
    }

    #[test]
    fn selection_wraps_around_in_both_directions() {
        let mut local = local_with("/com");
        move_completion_selection(&mut local, 1);
        assert_eq!(local.completion_selected, 1);
        move_completion_selection(&mut local, 1);
        assert_eq!(local.completion_selected, 0); // wrapped past 2 candidates
        move_completion_selection(&mut local, -1);
        assert_eq!(local.completion_selected, 1); // wrapped the other way
    }

    #[test]
    fn accept_fills_draft_with_selected_candidate_and_a_trailing_space() {
        let mut local = local_with("/com");
        move_completion_selection(&mut local, 1); // -> /compact
        accept_completion(&mut local);
        assert_eq!(local.draft, "/compact ");
        assert!(compute_completion(&local).is_none()); // space closes the popup
    }

    // ── 转录滚动 ──

    /// 视口高度平时由渲染循环从真实布局里填，测试里直接给定。
    fn with_page(page: usize) -> LocalUi {
        let mut local = local_with("");
        local.viewport_lines = page;
        local
    }

    #[test]
    fn scroll_up_from_the_bottom_lands_one_page_above_the_last_screen() {
        // 100 条、一屏 10 条：跟随时视口顶在第 90 条，往上翻一页到第 80 条。
        let mut local = with_page(10);
        local.scroll_up(100);
        assert_eq!(local.scroll_offset, Some(80));
        local.scroll_up(100);
        assert_eq!(local.scroll_offset, Some(70));
    }

    #[test]
    fn scroll_up_stops_at_the_top() {
        let mut local = with_page(10);
        for _ in 0..10 {
            local.scroll_up(25);
        }
        assert_eq!(local.scroll_offset, Some(0));
    }

    /// 内容不满一屏时全都看得见，翻页应该什么都不做——否则会冒出一个
    /// "0 lines above" 的提示条。
    #[test]
    fn scroll_up_is_a_no_op_when_everything_fits() {
        let mut local = with_page(10);
        local.scroll_up(5);
        assert_eq!(local.scroll_offset, None);
    }

    /// 翻回底部必须回到**跟随**状态，不能停在某个固定 offset 上，否则新消息
    /// 会往视口外走。
    #[test]
    fn scrolling_back_down_resumes_following() {
        let mut local = with_page(10);
        local.scroll_up(100);
        local.scroll_up(100);
        assert_eq!(local.scroll_offset, Some(70));
        local.scroll_down(100);
        assert_eq!(local.scroll_offset, Some(80));
        local.scroll_down(100);
        assert_eq!(local.scroll_offset, None, "到底了就该重新跟随");
    }

    #[test]
    fn scroll_down_while_following_does_nothing() {
        let mut local = with_page(10);
        local.scroll_down(100);
        assert_eq!(local.scroll_offset, None);
    }

    /// 这几个键在 `default_bindings()` 里都绑好了，缺的一直是 `dispatch_action`
    /// 里的分支。走真实 `Resolver` 打一遍，顺带确认它们没像 `ask.*` 那样被同键的
    /// 别的动作抢先匹配掉。
    #[test]
    fn the_newly_handled_keys_actually_reach_their_handlers() {
        let handle = FakeHandle::new();
        let mut snapshot = frame_without_approval();
        snapshot.transcript.body.entries = (0..100)
            .map(|i| TranscriptEntry {
                kind: LineKind::AssistantText,
                text: format!("line{i}"),
                block_id: None,
            })
            .collect();

        let press = |local: &mut LocalUi, code, mods| {
            let mut resolver = Resolver::new(default_bindings());
            dispatch_key(
                KeyEvent::new(code, mods),
                &mut resolver,
                local,
                &handle,
                &snapshot,
            );
        };

        let mut local = with_page(10);
        press(&mut local, CtKeyCode::PageUp, KeyModifiers::NONE);
        assert_eq!(local.scroll_offset, Some(80), "PageUp 没接上");
        press(&mut local, CtKeyCode::PageDown, KeyModifiers::NONE);
        assert_eq!(local.scroll_offset, None, "PageDown 没接上");

        let mut local = with_page(10);
        local.draft = "git commit".into();
        local.cursor = local.draft.len();
        press(&mut local, CtKeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(local.draft, "git ", "Ctrl+W 没接上");

        let mut local = with_page(10);
        press(&mut local, CtKeyCode::Char('l'), KeyModifiers::CONTROL);
        assert!(local.redraw_requested, "Ctrl+L 没接上");

        // 光标键：同样走真实 Resolver，确认没被 `editor.*` 里别的绑定抢掉。
        let mut local = with_page(10);
        local.draft = "git commit".into();
        local.cursor = local.draft.len();
        press(&mut local, CtKeyCode::Left, KeyModifiers::NONE);
        assert_eq!(local.cursor, 9, "Left 没接上");
        press(&mut local, CtKeyCode::Right, KeyModifiers::NONE);
        assert_eq!(local.cursor, 10, "Right 没接上");
        press(&mut local, CtKeyCode::Home, KeyModifiers::NONE);
        assert_eq!(local.cursor, 0, "Home 没接上");
        press(&mut local, CtKeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(local.draft, "it commit", "Delete 没接上");
        press(&mut local, CtKeyCode::End, KeyModifiers::NONE);
        assert_eq!(local.cursor, local.draft.len(), "End 没接上");
        press(&mut local, CtKeyCode::Left, KeyModifiers::ALT);
        assert_eq!(local.cursor, 3, "Alt+Left 没接上");

        // 没有补全弹窗时 Up/Down 走行间移动（有弹窗时移动的是选中项，另有测试）。
        let mut local = with_page(10);
        local.draft = "first\nsecond".into();
        local.cursor = local.draft.len();
        press(&mut local, CtKeyCode::Up, KeyModifiers::NONE);
        assert_eq!(local.cursor, 5, "Up 没走到上一行");
    }

    #[test]
    fn merge_turns_off_auto_follow_only_while_scrolled() {
        let mut local = local_with("");
        let mut frame = frame_without_approval();
        frame.transcript.body.entries = (0..50)
            .map(|i| TranscriptEntry {
                kind: LineKind::AssistantText,
                text: format!("line{i}"),
                block_id: None,
            })
            .collect();

        assert!(merge(frame.clone(), &local).transcript.body.auto_follow);

        local.scroll_offset = Some(12);
        let merged = merge(frame, &local);
        assert!(!merged.transcript.body.auto_follow);
        assert_eq!(merged.transcript.body.scroll.offset, 12);
    }

    // ── 转录块选择（需求场景 7）──

    /// 三个块、块 "b" 占两行，用来验证"按块走"而不是"按行走"。
    fn frame_with_blocks() -> tui::FrameState {
        let mut frame = frame_without_approval();
        frame.transcript.body.entries = [("a", 1), ("b", 2), ("c", 1)]
            .iter()
            .flat_map(|(id, rows)| {
                (0..*rows).map(move |r| TranscriptEntry {
                    kind: LineKind::ToolResultOk,
                    text: format!("{id}{r}"),
                    block_id: Some((*id).to_string()),
                })
            })
            .collect();
        frame
    }

    #[test]
    fn foldable_blocks_are_deduped_and_ordered() {
        assert_eq!(foldable_blocks(&frame_with_blocks()), ["a", "b", "c"]);
    }

    /// 没选中时 F5 打的是最新的块——不用先导航就能展开刚跑完的工具。
    #[test]
    fn toggle_without_a_selection_targets_the_newest_block() {
        let local = local_with("");
        let handle = FakeHandle::new();
        toggle_selected_block(&local, &frame_with_blocks(), &handle);
        assert_eq!(handle.toggled(), ["c"]);
    }

    /// 场景 7 的正题：走到更早的块上，F5 打的就是那个块，而不是最新的。
    #[test]
    fn selecting_backwards_lets_an_earlier_block_be_expanded() {
        let frame = frame_with_blocks();
        let mut local = local_with("");

        local.select_block(&frame, -1); // 没选中 → 从最新的开始
        assert_eq!(local.selected_block.as_deref(), Some("c"));
        local.select_block(&frame, -1);
        assert_eq!(local.selected_block.as_deref(), Some("b"));

        let handle = FakeHandle::new();
        toggle_selected_block(&local, &frame, &handle);
        assert_eq!(handle.toggled(), ["b"], "F5 应该打选中的块");
    }

    #[test]
    fn selection_stops_at_the_oldest_block_and_clears_past_the_newest() {
        let frame = frame_with_blocks();
        let mut local = local_with("");

        for _ in 0..5 {
            local.select_block(&frame, -1);
        }
        assert_eq!(
            local.selected_block.as_deref(),
            Some("a"),
            "最早的块上再往前走应该停住"
        );

        for _ in 0..2 {
            local.select_block(&frame, 1);
        }
        assert_eq!(local.selected_block.as_deref(), Some("c"));
        local.select_block(&frame, 1);
        assert_eq!(
            local.selected_block, None,
            "越过最新的块应该回到「跟最新的」而不是钉在最后一个上"
        );
    }

    /// 选中的块被 bridge 丢掉了（`/clear` 之类）时，快照里不该还留着一个悬空 id。
    #[test]
    fn merge_drops_a_selection_whose_block_is_gone() {
        let mut local = local_with("");
        local.selected_block = Some("gone".into());
        let merged = merge(frame_with_blocks(), &local);
        assert_eq!(merged.transcript.body.selected_block, None);

        local.selected_block = Some("b".into());
        let merged = merge(frame_with_blocks(), &local);
        assert_eq!(merged.transcript.body.selected_block.as_deref(), Some("b"));
    }

    /// 选中靠上的块要把它滚进视口；靠底部的块则回到跟随模式。
    #[test]
    fn selecting_scrolls_the_block_into_view() {
        let mut frame = frame_without_approval();
        frame.transcript.body.entries = (0..50)
            .map(|i| TranscriptEntry {
                kind: LineKind::ToolResultOk,
                text: format!("row{i}"),
                block_id: Some(format!("b{i}")),
            })
            .collect();
        let mut local = local_with("");
        local.viewport_lines = 10;

        local.selected_block = Some("b3".into());
        local.reveal_selection(&frame);
        assert_eq!(local.scroll_offset, Some(3));

        local.selected_block = Some("b48".into());
        local.reveal_selection(&frame);
        assert_eq!(local.scroll_offset, None, "最后一屏里的块应该回到跟随");
    }

    // ── 本地 slash 命令 ──

    #[test]
    fn model_command_parses_with_and_without_an_argument() {
        assert!(matches!(
            local_command("/model claude-opus-5"),
            Some(LocalCommand::Model(Some(n))) if n == "claude-opus-5"
        ));
        assert!(matches!(
            local_command("/model"),
            Some(LocalCommand::Model(None))
        ));
        assert!(matches!(
            local_command("/model   "),
            Some(LocalCommand::Model(None))
        ));
        assert!(matches!(local_command("/quit"), Some(LocalCommand::Quit)));
        // 前缀相同但不是同一条命令，别抢 Core 的。
        assert!(local_command("/models").is_none());
        assert!(local_command("/compact").is_none());
    }

    #[test]
    fn submitting_model_with_a_name_switches_the_model() {
        let mut local = local_with("/model claude-opus-5");
        let handle = FakeHandle::new();
        assert!(submit(&mut local, &handle, &frame_without_approval()));
        assert!(matches!(
            handle.commands().as_slice(),
            [BridgeCommand::SetModel { name }] if name == "claude-opus-5"
        ));
    }

    /// 不带参数时报当前模型，而不是把 `/model` 当普通文本发给引擎。
    #[test]
    fn submitting_bare_model_reports_the_current_one() {
        let mut local = local_with("/model");
        let handle = FakeHandle::new();
        let mut frame = frame_without_approval();
        frame.footer_hints.model = "claude-sonnet-5".into();
        assert!(submit(&mut local, &handle, &frame));
        assert!(matches!(
            handle.commands().as_slice(),
            [BridgeCommand::Note { text }] if text.contains("claude-sonnet-5")
        ));
    }

    /// 真机跑出来的坑：打全一条命令再回车，应该提交，而不是把弹窗里的同一条
    /// 再"补"一次（补出来是 `/model ` 带个空格，还得再按一次回车）。
    #[test]
    fn enter_submits_when_the_command_is_already_fully_typed() {
        let mut local = local_with("/model");
        assert!(compute_completion(&local).is_some(), "弹窗应该开着");
        assert!(completion_already_typed(&local));

        let handle = FakeHandle::new();
        dispatch_action(
            "editor.submit",
            &mut local,
            &handle,
            &frame_without_approval(),
        );
        assert!(
            matches!(handle.commands().as_slice(), [BridgeCommand::Note { .. }]),
            "应该提交（/model 无参 → 报当前模型），而不是补全；实际: {:?}",
            handle.commands()
        );
        assert_eq!(local.draft, "", "提交后草稿清空");
    }

    /// 只打了一半时 Enter 仍然是"补全"。
    #[test]
    fn enter_still_completes_a_partially_typed_command() {
        let mut local = local_with("/mod");
        assert!(!completion_already_typed(&local));

        let handle = FakeHandle::new();
        dispatch_action(
            "editor.submit",
            &mut local,
            &handle,
            &frame_without_approval(),
        );
        assert_eq!(local.draft, "/model ");
        assert!(handle.commands().is_empty(), "补全不该发命令给 bridge");
    }

    #[test]
    fn local_commands_show_up_in_the_completion_popup() {
        let popup = compute_completion(&local_with("/mod")).expect("应该有候选");
        assert!(popup.candidates.iter().any(|c| c.name == "/model"));
    }

    // ── 命令行参数 ──

    #[test]
    fn model_flag_is_accepted_in_both_spellings() {
        let parse = |args: &[&str]| {
            Args::parse(args.iter().map(|s| s.to_string()))
                .unwrap()
                .unwrap()
                .model
        };
        assert_eq!(parse(&["--model", "x"]).as_deref(), Some("x"));
        assert_eq!(parse(&["-m", "x"]).as_deref(), Some("x"));
        assert_eq!(parse(&["--model=x"]).as_deref(), Some("x"));
        assert_eq!(parse(&[]), None);
    }

    #[test]
    fn resume_flags_map_to_the_right_target() {
        let parse = |args: &[&str]| {
            Args::parse(args.iter().map(|s| s.to_string()))
                .unwrap()
                .unwrap()
                .resume
        };
        assert!(matches!(parse(&["-c"]), Some(Resume::Latest)));
        assert!(matches!(parse(&["--continue"]), Some(Resume::Latest)));
        assert!(matches!(parse(&["--resume", "abc"]), Some(Resume::Id(id)) if id == "abc"));
        assert!(matches!(parse(&["--resume=abc"]), Some(Resume::Id(id)) if id == "abc"));
        assert!(parse(&[]).is_none());
        // 缺值要报错，不能默默当成 `--continue`。
        assert!(Args::parse(["--resume".to_string()].into_iter()).is_err());
    }

    #[test]
    fn help_short_circuits_and_unknown_args_are_errors() {
        assert!(Args::parse(["--help".to_string()].into_iter())
            .unwrap()
            .is_none());
        // 打错的开关必须报错——静默忽略会让人以为它生效了。
        assert!(Args::parse(["--modle".to_string()].into_iter()).is_err());
        assert!(Args::parse(["--model".to_string()].into_iter()).is_err());
    }

    /// 自己刚发出去的话必须在视口里。
    #[test]
    fn submitting_snaps_back_to_the_bottom() {
        let mut local = local_with("hello");
        local.scroll_offset = Some(30);
        let handle = FakeHandle::new();
        submit(&mut local, &handle, &frame_without_approval());
        assert_eq!(local.scroll_offset, None);
    }

    // ── 编辑键 ──

    /// 草稿 + 光标。`|` 标出光标位置，测试里读写都用这一种写法。
    fn editing(draft_with_caret: &str) -> LocalUi {
        let cursor = draft_with_caret.find('|').expect("需要用 | 标出光标");
        let mut local = local_with("");
        local.draft = draft_with_caret.replace('|', "");
        local.cursor = cursor;
        local
    }

    fn caret(local: &LocalUi) -> String {
        let mut s = local.draft.clone();
        s.insert(local.cursor, '|');
        s
    }

    #[test]
    fn typing_and_deleting_happen_at_the_cursor_not_at_the_end() {
        let mut local = editing("git |commit");
        local.insert('x');
        assert_eq!(caret(&local), "git x|commit");
        local.delete_backward();
        assert_eq!(caret(&local), "git |commit");
        local.delete_forward();
        assert_eq!(caret(&local), "git |ommit");
    }

    #[test]
    fn cursor_moves_by_whole_characters_across_multibyte_text() {
        let mut local = editing("重构模块|");
        local.move_char(-1);
        assert_eq!(caret(&local), "重构模|块");
        local.move_char(-1);
        assert_eq!(caret(&local), "重构|模块");
        local.move_char(1);
        assert_eq!(caret(&local), "重构模|块");
        // 头尾夹住，不会走出草稿。
        for _ in 0..10 {
            local.move_char(-1);
        }
        assert_eq!(caret(&local), "|重构模块");
        local.delete_backward();
        assert_eq!(caret(&local), "|重构模块", "行首退格是空操作");
    }

    #[test]
    fn word_motion_and_word_delete_work_from_the_cursor() {
        let mut local = editing("git commit -m|");
        local.move_word(-1);
        assert_eq!(caret(&local), "git commit |-m");
        local.move_word(-1);
        assert_eq!(caret(&local), "git |commit -m");
        local.move_word(1);
        assert_eq!(caret(&local), "git commit| -m");

        let mut local = editing("git commit |-m");
        local.delete_word_before();
        assert_eq!(caret(&local), "git |-m", "只删光标前那个词，后面的不动");
    }

    #[test]
    fn delete_word_before_takes_trailing_whitespace_and_multibyte_words() {
        let mut local = editing("重构 这个模块   |");
        local.delete_word_before();
        assert_eq!(caret(&local), "重构 |");
        local.delete_word_before();
        assert_eq!(caret(&local), "|");
        local.delete_word_before();
        assert_eq!(caret(&local), "|", "空草稿上再删一次不该 panic");
    }

    #[test]
    fn kill_to_end_stops_at_the_line_break_then_joins_lines() {
        let mut local = editing("first |line\nsecond");
        local.kill_to_line_end();
        assert_eq!(caret(&local), "first |\nsecond", "不该越过换行");
        local.kill_to_line_end();
        assert_eq!(caret(&local), "first |second", "已经在行尾时把下一行接上来");
    }

    #[test]
    fn home_and_end_are_line_wise_not_draft_wise() {
        let mut local = editing("first\nse|cond\nthird");
        local.cursor = local.line_start();
        assert_eq!(caret(&local), "first\n|second\nthird");
        local.cursor = local.line_end();
        assert_eq!(caret(&local), "first\nsecond|\nthird");
    }

    #[test]
    fn vertical_motion_keeps_the_column_and_clamps_on_short_lines() {
        let mut local = editing("longest line\nab\nanother line|");
        local.move_line(-1);
        assert_eq!(
            caret(&local),
            "longest line\nab|\nanother line",
            "短行夹到行尾"
        );
        local.move_line(-1);
        assert_eq!(
            caret(&local),
            "lo|ngest line\nab\nanother line",
            "回到原来的列"
        );
        local.move_line(-1);
        assert_eq!(
            caret(&local),
            "lo|ngest line\nab\nanother line",
            "首行再往上不动"
        );
    }

    // ── 输入历史 ──

    fn submitted(local: &mut LocalUi, handle: &FakeHandle, text: &str) {
        local.draft = text.into();
        local.cursor = local.draft.len();
        submit(local, handle, &frame_without_approval());
    }

    #[test]
    fn up_walks_back_through_submitted_prompts_and_down_returns() {
        let mut local = local_with("");
        let handle = FakeHandle::new();
        submitted(&mut local, &handle, "第一句");
        submitted(&mut local, &handle, "第二句");

        local.up();
        assert_eq!(local.draft, "第二句", "先翻到最近一条");
        local.up();
        assert_eq!(local.draft, "第一句");
        local.up();
        assert_eq!(local.draft, "第一句", "翻到头就停住");
        local.down();
        assert_eq!(local.draft, "第二句");
        local.down();
        assert_eq!(local.draft, "", "翻过最新一条回到草稿");
        assert_eq!(local.cursor, 0);
    }

    /// 手打了一半的东西不该因为按了下 `↑` 就没了。
    #[test]
    fn a_half_typed_draft_survives_a_trip_through_history() {
        let mut local = local_with("");
        let handle = FakeHandle::new();
        submitted(&mut local, &handle, "旧的");

        local.draft = "打了一半".into();
        local.cursor = local.draft.len();
        local.up();
        assert_eq!(local.draft, "旧的");
        local.down();
        assert_eq!(local.draft, "打了一半", "草稿要原样还回来");
        assert_eq!(local.cursor, local.draft.len());
    }

    #[test]
    fn consecutive_duplicates_are_recorded_once() {
        let mut local = local_with("");
        let handle = FakeHandle::new();
        submitted(&mut local, &handle, "同一句");
        submitted(&mut local, &handle, "同一句");
        assert_eq!(local.history, vec!["同一句"], "连按两次不该占两条");
    }

    /// 多行草稿里 `↑` 先在行间走，走到首行才翻历史。
    #[test]
    fn vertical_motion_takes_priority_until_the_edge_of_a_multiline_draft() {
        let mut local = local_with("");
        let handle = FakeHandle::new();
        submitted(&mut local, &handle, "历史里的");

        local.draft = "first\nsecond".into();
        local.cursor = local.draft.len();
        local.up();
        assert_eq!(local.draft, "first\nsecond", "还在草稿里，只是光标上移");
        assert_eq!(local.cursor, 5);
        local.up();
        assert_eq!(local.draft, "历史里的", "已经在首行，这次才翻历史");
    }

    /// sticky header 只在它钉的那句滚出视口之后才出现，否则同一句话在屏幕上
    /// 出现两次（真机跑一眼就看出来了）。
    #[test]
    fn the_sticky_header_hides_while_its_prompt_is_still_on_screen() {
        let mut local = local_with("");
        local.viewport_lines = 3;
        let mut frame = frame_without_approval();
        frame.transcript.header = tui::frame_state::HeaderState {
            text: Some("问题".into()),
            source: tui::frame_state::HeaderSource::UserPrompt,
        };
        frame.transcript.body.entries = vec![
            TranscriptEntry {
                kind: LineKind::UserPrompt,
                text: "问题".into(),
                block_id: None,
            },
            TranscriptEntry {
                kind: LineKind::AssistantText,
                text: "答案".into(),
                block_id: None,
            },
        ];

        assert_eq!(
            merge(frame.clone(), &local).transcript.header.text,
            None,
            "prompt 就在屏幕上，不该再钉一份"
        );

        // 回答变长，prompt 被挤出视口 → header 该出现了。
        for i in 0..10 {
            frame.transcript.body.entries.push(TranscriptEntry {
                kind: LineKind::AssistantText,
                text: format!("续{i}"),
                block_id: None,
            });
        }
        assert_eq!(
            merge(frame, &local).transcript.header.text.as_deref(),
            Some("问题")
        );
    }

    /// resume 起手时输入历史接着上次——从转录里恢复出来的用户输入读。
    #[test]
    fn history_is_seeded_from_a_restored_transcript() {
        let mut frame = frame_without_approval();
        frame.transcript.body.entries = vec![
            TranscriptEntry {
                kind: LineKind::UserPrompt,
                text: "上次问的".into(),
                block_id: None,
            },
            TranscriptEntry {
                kind: LineKind::AssistantText,
                text: "上次答的".into(),
                block_id: None,
            },
        ];
        assert_eq!(user_prompts(&frame), vec!["上次问的"]);
    }

    /// 光标在中间时提交，整段草稿都要发出去，光标复位。
    #[test]
    fn submitting_from_mid_draft_sends_the_whole_draft() {
        let mut local = editing("hello |world");
        let handle = FakeHandle::new();
        assert!(submit(&mut local, &handle, &frame_without_approval()));
        assert!(matches!(
            handle.commands().as_slice(),
            [BridgeCommand::Submit { text }] if text == "hello world"
        ));
        assert_eq!(local.cursor, 0);
    }

    // ── 权限对话框 ──

    /// 只走 `dispatch`——订阅接口在这些测试里不会被碰到。
    struct FakeHandle(std::sync::Mutex<Vec<BridgeCommand>>);

    impl FakeHandle {
        fn new() -> Self {
            Self(std::sync::Mutex::new(Vec::new()))
        }
        fn commands(&self) -> Vec<BridgeCommand> {
            self.0.lock().unwrap().clone()
        }
        fn toggled(&self) -> Vec<String> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter_map(|c| match c {
                    BridgeCommand::ToggleExpand { block_id } => Some(block_id.clone()),
                    _ => None,
                })
                .collect()
        }
        fn decisions(&self) -> Vec<ApprovalOption> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter_map(|c| match c {
                    BridgeCommand::RespondPermission { decision, .. } => Some(*decision),
                    _ => None,
                })
                .collect()
        }
    }

    impl EngineHandle for FakeHandle {
        fn dispatch(&self, cmd: BridgeCommand) -> Result<(), bridge::BridgeError> {
            self.0.lock().unwrap().push(cmd);
            Ok(())
        }
        fn subscribe(&self) -> tokio::sync::watch::Receiver<tui::FrameState> {
            unimplemented!("these tests only exercise dispatch")
        }
        fn subscribe_commands(&self) -> tokio::sync::watch::Receiver<Vec<CompletionCandidate>> {
            unimplemented!("these tests only exercise dispatch")
        }
    }

    fn approval_request() -> ApprovalRequest {
        ApprovalRequest {
            prompt_id: "p1".into(),
            tool_name: "Bash".into(),
            message: "run rm -rf /?".into(),
            options: vec![
                ApprovalOption::PermitOnce,
                ApprovalOption::PermitSession,
                ApprovalOption::PermitProject,
                ApprovalOption::Deny,
            ],
            selected_option: 0,
        }
    }

    /// Enter 以前硬编码成 `PermitOnce`，高亮在哪一项完全没人看——"本会话一直允许"
    /// 因此永远选不中。
    #[test]
    fn confirm_sends_the_highlighted_option() {
        let mut local = local_with("");
        let handle = FakeHandle::new();
        let req = approval_request();

        dispatch_approval_action("editor.history.next", &mut local, &handle, &req);
        dispatch_approval_action("editor.history.next", &mut local, &handle, &req);
        dispatch_approval_action("editor.submit", &mut local, &handle, &req);

        assert_eq!(handle.decisions(), vec![ApprovalOption::PermitProject]);
        assert_eq!(local.approval_selected, 0, "下一个请求应该从头开始");
    }

    #[test]
    fn approval_selection_wraps_and_quick_keys_still_work() {
        let mut local = local_with("");
        let handle = FakeHandle::new();
        let req = approval_request();

        dispatch_approval_action("ask.prev", &mut local, &handle, &req);
        assert_eq!(local.approval_selected, 3, "从第一项往上应该绕到最后一项");

        dispatch_approval_action("ask.yes-shortcut", &mut local, &handle, &req);
        dispatch_approval_action("ask.no-shortcut", &mut local, &handle, &req);
        assert_eq!(
            handle.decisions(),
            vec![ApprovalOption::PermitOnce, ApprovalOption::Deny]
        );
    }

    #[test]
    fn escape_denies_rather_than_leaving_the_tool_call_hanging() {
        let mut local = local_with("");
        let handle = FakeHandle::new();
        dispatch_approval_action("repl.dismiss", &mut local, &handle, &approval_request());
        assert_eq!(handle.decisions(), vec![ApprovalOption::Deny]);
    }

    /// `y`/`n` 绑的是裸字符：没有对话框时它们必须落进草稿，否则打 "yes" 会变成 "es"。
    #[test]
    fn y_and_n_are_plain_text_when_no_dialog_is_open() {
        let mut local = local_with("");
        let handle = FakeHandle::new();
        let mut resolver = Resolver::new(default_bindings());
        let snapshot = frame_without_approval();

        for c in "yn".chars() {
            dispatch_key(
                KeyEvent::new(CtKeyCode::Char(c), KeyModifiers::NONE),
                &mut resolver,
                &mut local,
                &handle,
                &snapshot,
            );
        }
        assert_eq!(local.draft, "yn");
        assert!(handle.decisions().is_empty());
    }

    /// 对话框开着时 Enter 不能把草稿提交给引擎——引擎正卡在这次权限检查上。
    #[test]
    fn enter_does_not_submit_the_draft_while_a_dialog_is_open() {
        let mut local = local_with("hello");
        let handle = FakeHandle::new();
        let mut resolver = Resolver::new(default_bindings());
        let mut snapshot = frame_without_approval();
        snapshot.composer.content.approval = Some(ApprovalState {
            pending: vec![approval_request()],
            active_idx: 0,
            view_mode: ApprovalViewMode::TabView,
        });

        dispatch_key(
            KeyEvent::new(CtKeyCode::Enter, KeyModifiers::NONE),
            &mut resolver,
            &mut local,
            &handle,
            &snapshot,
        );

        assert_eq!(local.draft, "hello", "草稿不该被消费");
        let cmds = handle.0.lock().unwrap();
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, BridgeCommand::Submit { .. })),
            "Enter 应该确认对话框，而不是提交草稿"
        );
    }

    /// 本地高亮下标必须覆盖进快照，否则渲染出来的选中项永远是第一项。
    #[test]
    fn merge_writes_the_local_selection_into_the_active_request() {
        let mut local = local_with("");
        local.approval_selected = 2;
        let mut snapshot = frame_without_approval();
        snapshot.composer.content.approval = Some(ApprovalState {
            pending: vec![approval_request()],
            active_idx: 0,
            view_mode: ApprovalViewMode::TabView,
        });

        let merged = merge(snapshot, &local);
        let approval = merged.composer.content.approval.unwrap();
        assert_eq!(approval.pending[0].selected_option, 2);
    }

    fn frame_without_approval() -> tui::FrameState {
        use tui::frame_state::*;
        tui::FrameState {
            transcript: TranscriptState {
                header: HeaderState {
                    text: None,
                    source: HeaderSource::None,
                },
                body: TranscriptBodyState {
                    scroll: ScrollState {
                        offset: 0,
                        total_lines: 0,
                    },
                    entries: vec![],
                    auto_follow: true,
                    selected_block: None,
                },
            },
            operation_status: OperationStatusState {
                status_line: StatusLineState { content: None },
                task_list: TaskListState { items: vec![] },
            },
            composer: ComposerState {
                app_info: AppInfoLineState { text: None },
                top_rule: TopRuleState {
                    color: SeparatorColor::DarkGray,
                    right_label: None,
                },
                content: ContentState {
                    editor: EditorState {
                        mode: InputMode::Normal,
                        draft: String::new(),
                        cursor: 0,
                        paste_placeholder: None,
                        locked: false,
                    },
                    completion: None,
                    approval: None,
                },
                bottom_rule: BottomRuleState {
                    color: SeparatorColor::DarkGray,
                },
            },
            sub_agent_bar: SubAgentBarState { agents: vec![] },
            footer_hints: FooterHintsState {
                model: "test-model".into(),
                cwd: "/tmp".into(),
                mode: AppMode::Normal,
                right_hint: String::new(),
                usage: SessionUsageState::default(),
            },
        }
    }

    #[test]
    fn dismiss_hides_popup_until_draft_changes_again() {
        let mut local = local_with("/com");
        assert!(compute_completion(&local).is_some());
        local.completion_dismissed = true;
        assert!(compute_completion(&local).is_none());
        insert_char(
            KeyEvent::new(CtKeyCode::Char('m'), KeyModifiers::NONE),
            &mut local,
        );
        assert!(compute_completion(&local).is_some());
    }
}
