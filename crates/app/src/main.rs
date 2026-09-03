//! `attacode` — application entry point: terminal setup, event loop, key dispatch.
//!
//! Owns two concerns bridge intentionally doesn't: terminal I/O (ratatui/crossterm) and
//! UI-local composer state (draft/cursor/picker selection). Everything Core-related
//! goes through `bridge::EngineHandle` — this file never touches an AttaCore type directly.
//!
//! Composer editing lives in the `impl LocalUi` block near the bottom: insert/delete
//! at the cursor, char/word/line motion, kill-to-end. `cursor` is a byte index into
//! `draft` and is always on a char boundary; the renderer indexes the same way, so
//! neither side converts. `editor.history.*`（Up/Down）一键三义，按上下文分派：
//! 补全弹窗 → 移动选中项；多行草稿 → 行间移动；到边界 → 翻输入历史。
//! 还缺的是选区和 undo。

use bridge::{
    BootstrapConfig, BridgeCommand, BtwKey, EngineHandle, Resume, Session, DEFAULT_MODEL,
};
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
    AnswerWith, AskOption, AskRequest, PickerCandidate, PickerKind, PickerState,
};

const USAGE: &str = "\
attacode — AttaCore 引擎的终端 UI

用法: attacode [选项]

选项:
  -m, --model <NAME>  这次运行用的模型（压过 ANTHROPIC_MODEL 和 settings.json）
  -c, --continue      接着本项目最近一次会话跑
      --resume <ID>   接着指定的会话跑（不知道 id 就先进去，再用 /resume 挑）
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

    // **第一个引擎在进 raw mode 之前建。** 凭据不对、`--resume` 点了个不存在的 id、
    // settings 坏了——这些都该在普通终端上报出来，而不是先闪一屏 alternate screen
    // 再把话说完。
    let session = bridge::start(config.clone()).await?;

    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(out))?;

    let result = sessions(&mut terminal, config, session).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

/// 一个接一个地跑会话，直到用户要退出。
///
/// `/resume` 换会话就是**换一个引擎**：模型的上下文、工具表、权限门、转录，整条链
/// 都绑在一个 `runtime::Agent` 上，而 `Agent::run` 已经 `&mut self` 借走了它。所以
/// 换会话在这里表现为"关掉这个，起一个新的"，而不是往运行中的引擎里塞一个新 id。
///
/// 终端的进出留在 `main`：换会话时屏幕不该闪一下。`LocalUi`（草稿、滚动、选中块）
/// 跟着 `run` 一起重来，这是对的——那些状态说的是上一个会话的事。
async fn sessions(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    mut config: BootstrapConfig,
    mut session: Session,
) -> anyhow::Result<()> {
    loop {
        let outcome = run(terminal, session.handle.as_ref()).await;
        let Flow::Resume(id) = outcome? else {
            // `run` 只会以 `Quit` / `Resume` 之一结束；其余的它自己就地处理了。
            session.shutdown();
            return Ok(());
        };

        let mut next_config = config.clone();
        next_config.resume = Some(Resume::Id(id));
        // **先把新的建起来，再关掉旧的。** 顺序反过来的话，新会话建不起来（文件被删了、
        // 盘满了）就意味着用户按了一下 `/resume`，然后手上那个会话没了。这里换不过去
        // 就留在原地，把原因写进转录。
        match bridge::start(next_config.clone()).await {
            Ok(next) => {
                session.shutdown();
                config = next_config;
                session = next;
            }
            Err(e) => {
                let _ = session.handle.dispatch(BridgeCommand::Note {
                    text: format!("/resume failed, staying in this session: {e}"),
                });
            }
        }
    }
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
    commands: Vec<PickerCandidate>,
    picker_selected: usize,
    /// `/resume` 选择器的候选，`None` = 没开。
    ///
    /// 和补全弹窗不同，这一份**不是**从 `draft` 推出来的：它是一次读盘的结果，
    /// 推不出来，只能存着。高亮位复用 `picker_selected`——同一时刻两个列表不会
    /// 都开着（选择器一开就霸占键盘，`draft` 也被清空了，补全的前提没了）。
    session_picker: Option<Vec<PickerCandidate>>,
    /// Esc closes the popup without touching `draft`; re-typing anything reopens it
    /// (see `note_draft_changed`).
    picker_dismissed: bool,
    /// 权限对话框里高亮的选项下标。和补全选择一样是纯 UI-本地状态：bridge 只知道
    /// 有哪些选项，不知道光标停在哪一项。渲染前由 `merge` 覆盖进快照。
    ask_selected: usize,
    /// 同时有多个待确认请求时，正在看第几个（Tab 切换）。同样是 UI-本地的：
    /// bridge 只维护待确认队列本身。渲染前由 `merge` 夹进合法范围。
    ask_active: usize,
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
    fn new(commands: Vec<PickerCandidate>) -> Self {
        Self {
            draft: String::new(),
            cursor: 0,
            commands,
            picker_selected: 0,
            session_picker: None,
            picker_dismissed: false,
            ask_selected: 0,
            ask_active: 0,
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
        self.picker_selected = 0;
        self.picker_dismissed = false;
    }

    /// 把本地状态和刚到的这一帧对齐。**在 `merge` 之前调**。
    ///
    /// 今天只做一件事：有待确认请求时收起 `/resume` 选择器。`merge` 只在没有请求时
    /// 才画选择器，而键盘路由是按"当前这条是不是选择题"分的——两者不一致时，一道
    /// 自由文本题会让选择器**从屏幕上消失却继续吃着键盘**，用户打字没反应，回车
    /// 静默换了会话。收起来是唯一诚实的选择：屏幕上没有的东西不该能被操作。
    fn reconcile(&mut self, frame: &tui::FrameState) {
        if frame.composer.content.ask.is_some() {
            self.close_picker();
        }
        // 队列缩短时本地光标要跟着回来。`merge` 只是在**渲染用的那份拷贝**上夹了一次
        // （`active_idx = min(...)`），从没写回本地，于是本地光标能停在队列末尾之外：
        // 3 个请求、Tab 到第 3 个，前面一个自己超时消失之后，再按一次 Tab
        // 算出来还是同一个——一次按键什么都不发生。
        let Some(ask) = &frame.composer.content.ask else {
            self.reset_ask_cursor();
            return;
        };
        self.ask_active = self.ask_active.min(ask.pending.len().saturating_sub(1));
        // **选项下标也得跟着夹。** 两个下标是一对（`reset_ask_cursor` 的文档
        // 就是这么说的），只夹一个的后果是回车变成死键：队列 `[A(4 个选项),
        // B(2 个)]`，用户在 A 上选到第 4 项，A 自己超时消失，active 落到 B——
        // 屏幕上高亮的是 B 的第一项（`merge` 把越界的渲染值归了 0），而
        // `ask.confirm` 读的是没夹过的本地值，`options.get(3)` 是 None，于是
        // 什么都不发生，得先按一下方向键把它绕回范围内。
        let options = ask.active().map_or(0, |r| r.options.len());
        if self.ask_selected >= options {
            self.ask_selected = 0;
        }
    }

    /// 答完一个之后，光标回到队列头。
    ///
    /// 两个下标都要复位。只复位 `ask_selected` 的话，`ask_active` 会留在
    /// 原地——而 `merge` 现在**从 active 那一条推算输入框锁不锁**，于是队列
    /// `[问答题, 权限B]`、用户 Tab 到 1 答掉 B 之后光标还停在 1；下一个权限请求一到，
    /// 它凭空成了 active，输入框在用户打了一半的草稿底下自己锁上。
    fn reset_ask_cursor(&mut self) {
        self.ask_selected = 0;
        self.ask_active = 0;
    }

    /// 打开 `/resume` 选择器。高亮位归零——上一次停在第几项和这一次的列表无关。
    fn open_picker(&mut self, candidates: Vec<PickerCandidate>) {
        self.session_picker = Some(candidates);
        self.picker_selected = 0;
    }

    fn close_picker(&mut self) {
        self.session_picker = None;
        self.picker_selected = 0;
    }
}

/// 事件循环。返回它是**怎么**结束的——调用方靠这个区分"用户要退出"和"用户要换到
/// 另一个会话"，后者要把整个引擎重建一遍。
async fn run(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    handle: &dyn EngineHandle,
) -> anyhow::Result<Flow> {
    let mut frame_rx = handle.subscribe();
    let mut commands_rx = handle.subscribe_commands();
    let mut resolver = Resolver::new(default_bindings());
    let mut local = LocalUi::new(commands_rx.borrow().clone());
    // `--continue` / `--resume` 起手时，输入历史也接着上次：转录里恢复出来的用户
    // 输入就是上次提交过的东西。bridge 不单独暴露它们，从首帧快照里读即可。
    local.history = user_prompts(&frame_rx.borrow());
    let mut keys = EventStream::new();

    loop {
        let frame = frame_rx.borrow().clone();
        local.reconcile(&frame);
        let snapshot = merge(frame, &local);
        // 打点看到的必须是**合并之后**这一帧：选中块/滚动/草稿都是这一步才加上的。
        handle.trace_render(&snapshot);
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
                match dispatch_key(key, &mut resolver, &mut local, handle, &snapshot) {
                    Flow::Continue => {}
                    Flow::Quit => break,
                    // 读盘。放在这里而不是 `submit` 里，是因为只有这一层是 async 的
                    // ——一个几百个会话的项目要读一会儿，同步做就是画面卡住。
                    Flow::ListSessions(query) => {
                        // 屏幕太矮时弹窗根本画不出来（`render_picker` 在
                        // 上方放不下时直接不画），而键盘路由不知道这件事——列表会
                        // 一个字都不显示却吃掉方向键，回车直接换到一个用户从没看见
                        // 过的会话。宁可不开，并说清楚为什么。
                        if local.viewport_lines < 3 {
                            let _ = handle.dispatch(BridgeCommand::Note {
                                text: "/resume needs a taller terminal to show the list".into(),
                            });
                            continue;
                        }
                        let found = handle.sessions(&query).await;
                        if found.is_empty() {
                            let _ = handle.dispatch(BridgeCommand::Note {
                                text: if query.is_empty() {
                                    "/resume: no earlier sessions in this project".into()
                                } else {
                                    format!("/resume: nothing matches `{query}`")
                                },
                            });
                        } else {
                            local.open_picker(found);
                        }
                    }
                    // 换会话要把引擎整个重建，这一层做不到——交给 `main`。
                    Flow::Resume(id) => return Ok(Flow::Resume(id)),
                }
            }
        }
    }
    Ok(Flow::Quit)
}

/// 一次按键 → 事件循环该干什么。
fn dispatch_key(
    key: KeyEvent,
    resolver: &mut Resolver,
    local: &mut LocalUi,
    handle: &dyn EngineHandle,
    snapshot: &tui::FrameState,
) -> Flow {
    let outcome = resolver.on_key(&key);
    handle.trace_key(
        &format!("{:?}+{:?}", key.modifiers, key.code),
        &format!("{outcome:?}"),
    );
    // **侧问区最优先。** 它是一个"进去了要出来"的模式：屏幕下半整个是它的，主 UI
    // 的任何操作都做不了。放在审批对话框前面，是因为侧问区把对话框也盖住了——盖住
    // 的东西不该还能被操作。
    if snapshot.btw.is_some() {
        return match outcome {
            ResolveOutcome::Action(action) => dispatch_btw_action(&action, handle),
            // 侧问区不收字符：它没有输入框（单次回答，要继续就再 /btw 一次）。
            _ => Flow::Continue,
        };
    }
    // 有待批准的请求时，键盘整体归对话框——`FrameState` 那边 composer 已经是
    // `locked`，路由不跟着改的话 Enter 会把草稿提交给一个正卡在权限检查上的引擎。
    //
    // **自由文本那一档除外**：那种问题要的答案就是用户在 composer 里打出来的，
    // 键盘必须留在编辑器上。它走下面的普通路径，由 `submit` 认出来送去答题。
    if let Some(req) = active_choice(snapshot) {
        let pending = snapshot
            .composer
            .content
            .ask
            .as_ref()
            .map(|a| a.pending.len())
            .unwrap_or(1);
        return match outcome {
            ResolveOutcome::Action(action) => {
                dispatch_ask_action(&action, local, handle, req, pending)
            }
            // 对话框开着时普通字符没有去处（composer 锁着），直接丢。
            _ => Flow::Continue,
        };
    }
    // 会话选择器开着时，键盘整体归它——它是个覆盖在 composer 上的列表，
    // 打字没有去处。
    if local.session_picker.is_some() {
        return match outcome {
            ResolveOutcome::Action(action) => dispatch_picker_action(&action, local, handle),
            _ => Flow::Continue,
        };
    }
    match outcome {
        // `y`/`n`/`x` 绑的是裸字符。它们各自所属的那个区域没开着时就是普通输入
        // ——不然打 "yes" 会丢字母，打 "box" 会丢 x。
        ResolveOutcome::Action(action)
            if matches!(
                action.as_str(),
                "ask.yes-shortcut" | "ask.no-shortcut" | "btw.clear"
            ) =>
        {
            insert_char(key, local);
            Flow::Continue
        }
        ResolveOutcome::Action(action) => dispatch_action(&action, local, handle, snapshot),
        ResolveOutcome::Partial | ResolveOutcome::ChordCancelled => Flow::Continue,
        ResolveOutcome::Unmatched(_) => {
            insert_char(key, local);
            Flow::Continue
        }
    }
}

/// 侧问区开着时的键位。照 CC 的那张表，去掉两条依赖外部能力的（`c` 复制、`f` fork）。
///
/// 认的 action 名都是既有的：这个区域没有自己的键位命名空间，它借编辑器和对话框那两套
/// ——用户改绑了方向键，侧问区跟着一起变，不用改第二处配置。
fn dispatch_btw_action(action: &str, handle: &dyn EngineHandle) -> Flow {
    let key = match action {
        "editor.history.prev" | "ask.prev" => BtwKey::Scroll(-1),
        "editor.history.next" | "ask.next" => BtwKey::Scroll(1),
        "editor.cursor.left" => BtwKey::Older,
        "editor.cursor.right" => BtwKey::Newer,
        // `x` 清空早前问答。绑的是裸字符，所以只在侧问区里才有这个意思。
        "btw.clear" => BtwKey::ClearEarlier,
        "repl.dismiss" | "repl.exit" | "editor.submit" | "ask.confirm" => BtwKey::Close,
        // 侧问区盖住了状态区，主 turn 还在跑——中断它仍然是合理动作。
        "repl.cancel" => {
            let _ = handle.dispatch(BridgeCommand::CancelTurn);
            return Flow::Continue;
        }
        _ => return Flow::Continue,
    };
    let _ = handle.dispatch(BridgeCommand::BtwKey(key));
    Flow::Continue
}

/// `/resume` 选择器开着时的键位。
///
/// 和审批对话框认同样的两组 action 名，理由一样：`Resolver` 取第一条匹配的绑定，
/// 而默认键位里 `editor.*` 占着 Up/Down/Enter。
fn dispatch_picker_action(action: &str, local: &mut LocalUi, handle: &dyn EngineHandle) -> Flow {
    let len = local.session_picker.as_ref().map_or(0, Vec::len);
    match action {
        "editor.history.prev" | "ask.prev" => {
            local.picker_selected = step(local.picker_selected, -1, len)
        }
        "editor.history.next" | "ask.next" => {
            local.picker_selected = step(local.picker_selected, 1, len)
        }
        "editor.submit" | "ask.confirm" => {
            let picked = local
                .session_picker
                .as_ref()
                .and_then(|c| c.get(local.picker_selected))
                .map(|c| c.name.clone());
            local.close_picker();
            // 选不出东西（空列表）时只是关掉，不是"恢复到一个叫空串的会话"。
            if let Some(id) = picked {
                return Flow::Resume(id);
            }
        }
        "repl.dismiss" | "repl.exit" => local.close_picker(),
        // 列表开着的时候 turn 可能正在跑。中断它是合理动作，而且是**唯一**能停下
        // 一个跑飞了的 turn 的办法——以前它落进下面那个 `_`，用户得先关掉列表
        // 才按得动 Ctrl+C。
        "repl.cancel" => {
            let _ = handle.dispatch(BridgeCommand::CancelTurn);
        }
        _ => {}
    }
    Flow::Continue
}

/// 权限对话框开着时的键位。
///
/// 每个分支都写了两个 action 名，因为 `Resolver` 取第一条匹配的绑定，而
/// `default_bindings()` 里 `editor.*` 排在 `ask.*` 前面、占着同样的 Up/Down/Enter
/// ——`ask.prev`/`ask.next`/`ask.confirm` 在默认键位下根本轮不到（用户把它们改绑到
/// 别的键才会出现）。认两个名字，两条路都通。
fn dispatch_ask_action(
    action: &str,
    local: &mut LocalUi,
    handle: &dyn EngineHandle,
    req: &AskRequest,
    pending: usize,
) -> Flow {
    match action {
        // 多个请求排队时切下一个。渲染那边早就画了 tab 条，但一直没有键能切——
        // `active_idx` 恒为 0，后面的请求只能等前面的答完才看得见。
        "ask.next-request" => {
            local.ask_active = step(local.ask_active, 1, pending);
            local.ask_selected = 0;
        }
        "editor.history.prev" | "ask.prev" => {
            local.ask_selected = step(local.ask_selected, -1, req.options.len())
        }
        "editor.history.next" | "ask.next" => {
            local.ask_selected = step(local.ask_selected, 1, req.options.len())
        }
        "editor.submit" | "ask.confirm" => {
            // 选不出东西时什么都不做，而不是替用户拒绝：这个分支现在也服务模型的
            // 提问，那里 `Deny` 根本不在选项里，凭空发一个等于替用户答了道没答过
            // 的题。空选项列表在 `AnswerWith::Choose` 下本就不该出现（见
            // `bridge::reducer::pending_from_question`），真出现了也该卡住而不是乱答。
            if let Some(choice) = req.options.get(local.ask_selected).cloned() {
                respond(handle, local, req, choice);
            }
        }
        // `y`/`n` 是权限门那道题的快捷键。模型的提问里没有这两个选项，按下去
        // 应该什么都不发生——`shortcut` 只在选项表里真有它的时候才作数。
        "ask.yes-shortcut" => shortcut(handle, local, req, AskOption::PermitOnce),
        "ask.no-shortcut" | "repl.dismiss" => shortcut(handle, local, req, AskOption::Deny),
        // 权限检查把 turn 卡住了，中断它仍然是合理动作。
        "repl.cancel" => {
            let _ = handle.dispatch(BridgeCommand::CancelTurn);
        }
        _ => {}
    }
    Flow::Continue
}

/// 只在 `option` 真的是这道题的选项之一时才回它。
fn shortcut(handle: &dyn EngineHandle, local: &mut LocalUi, req: &AskRequest, option: AskOption) {
    if req.options.contains(&option) {
        respond(handle, local, req, option);
    }
}

fn active_ask(snapshot: &tui::FrameState) -> Option<&AskRequest> {
    snapshot.composer.content.ask.as_ref()?.active()
}

/// 当前这条待办里**要用选的**那种。`None` 包括"没有待办"和"待办是道问答题"。
///
/// 判据来自 [`tui::frame_state::AskState::locks_composer`]——和渲染那边锁不锁
/// 输入框读的是同一个方法。这里自己写一遍 `== Choose` 就是在重新制造那个 bug。
fn active_choice(snapshot: &tui::FrameState) -> Option<&AskRequest> {
    let ask = snapshot.composer.content.ask.as_ref()?;
    ask.locks_composer().then(|| ask.active())?
}

/// 当前**正在答的**那条，如果它是道自由文本题。
///
/// 取的是 active 那一条，不是队列里随便找一条 `Type`：队列 `[Type, Choose]` 而
/// 用户 Tab 到了后面那个权限请求时，回车该确认权限，不该去答那道已经不在眼前的题。
fn pending_typed_question(snapshot: &tui::FrameState) -> Option<&AskRequest> {
    active_ask(snapshot).filter(|r| r.answer_with == AnswerWith::Type)
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
) -> Flow {
    let picker_active = compute_command_picker(local).is_some();
    match action {
        "editor.submit" if picker_active && !picker_already_typed(local) => {
            accept_picker_candidate(local)
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
        "editor.history.prev" if picker_active => move_picker_selection(local, -1),
        "editor.history.next" if picker_active => move_picker_selection(local, 1),
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
                return Flow::Quit;
            }
        }
        "repl.dismiss" if picker_active => local.picker_dismissed = true,
        // Esc 没有对话框/弹窗要关时，退出块选择——和它在别处"退出当前模式"的语义一致。
        "repl.dismiss" if local.selected_block.is_some() => {
            local.selected_block = None;
        }
        // 队列里排到下一个。以前这个 action 只在 `dispatch_ask_action` 里，而那个
        // 函数只有 active 是选择题时才到得了——于是排在一道自由文本题后面的权限请求
        // 永远 Tab 不到、永远答不了，一直挂到 300 秒超时被自动拒绝。
        "ask.next-request" => {
            if let Some(ask) = &snapshot.composer.content.ask {
                local.ask_active = step(local.ask_active, 1, ask.pending.len());
                local.ask_selected = 0;
            }
        }
        "transcript.select-prev" => local.select_block(snapshot, -1),
        "transcript.select-next" => local.select_block(snapshot, 1),
        "transcript.toggle-expand" => toggle_selected_block(local, snapshot, handle),
        _ => {}
    }
    Flow::Continue
}

/// header 钉的那句（最后一条用户输入）此刻在视口里看得见吗？
///
/// 看得见就不用钉——sticky header 的意义是"滚远了还知道在答哪个问题"。
/// 跟随模式下视口是最后一屏，滚动模式下是 `[offset, offset+高度)`。
fn prompt_is_visible(frame: &tui::FrameState, local: &LocalUi) -> bool {
    let entries = &frame.transcript.body.entries;
    let is_prompt =
        |e: &tui::frame_state::TranscriptEntry| e.kind == tui::frame_state::LineKind::UserPrompt;
    let Some(last) = entries.iter().rposition(is_prompt) else {
        return true; // 没有 prompt，也就没什么可钉的
    };
    // header 钉的是**第一行**（`reducer::current_prompt` 取 `first_line`），所以要问的
    // 也是第一行在不在视口里。按最后一行判的话，一段多行提交只露出尾巴时会被判成
    // "看得见"、于是把 header 收起来——而 header 要显示的那一行恰好在屏幕外面，
    // 正好和它存在的理由相反。
    //
    // 往回走的依据同样是 `starts_segment` 而不是相邻：相邻的上一条可能是**上一次**
    // 提交（中间那一轮被取消了），那时该钉的是这一次的第一行，不是上一次的。
    let mut idx = last;
    while idx > 0 && entries[idx].starts_segment && is_prompt(&entries[idx - 1]) {
        idx -= 1;
    }
    let page = local.viewport_lines.max(1);
    match local.scroll_offset {
        Some(offset) => idx >= offset && idx < offset + page,
        None => idx + page >= entries.len(),
    }
}

/// 转录里的用户输入，按出现顺序——resume 之后用它续上输入历史。
/// 转录里恢复出来的用户输入，一次提交算**一条**。
///
/// 一条 entry 是屏幕上的一行（见 `bridge::reducer::push_lines`），所以一次多行提交
/// 在转录里是好几条。按条收的话，`↑` 只召回最后一行，再按一次是同一次提交的上一行
/// ——而**当场**提交时 `remember()` 存的是整段。同一份历史，两种形状。
///
/// 拼的依据是 `starts_segment`，**不是相邻**：恢复出来的转录里每条用户消息各成
/// 一个 turn，两次相邻的提交（发一句、Ctrl+C、再发一句）之间什么都没有，按相邻拼
/// 会把两次提交粘成一条。
fn user_prompts(frame: &tui::FrameState) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for entry in &frame.transcript.body.entries {
        if entry.kind != tui::frame_state::LineKind::UserPrompt {
            continue;
        }
        match out.last_mut() {
            Some(last) if entry.starts_segment => {
                last.push('\n');
                last.push_str(&entry.text);
            }
            _ => out.push(entry.text.clone()),
        }
    }
    out
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

/// 一次按键之后事件循环该干什么。
///
/// 取代原来那个 `bool`。`false` 只能说"停下来"，说不了"停下来，然后换到那个会话
/// 去"——而 `/resume` 要的正是后者。两件要跑出这层的事（列会话、换会话）都得由
/// `run` 的 async 上下文来做：列会话要读盘，换会话要重建整个引擎。
#[derive(Debug, PartialEq, Eq)]
enum Flow {
    /// 接着跑。
    Continue,
    /// 收摊。
    Quit,
    /// 把这个查询的会话列表取回来，塞进选择器。空串 = 最近的几个。
    ListSessions(String),
    /// 换到这个会话去。
    Resume(String),
}

/// app 自己处理、不转发给 Core 的 slash 命令。
enum LocalCommand {
    Quit,
    /// `/model` 不带参数 = 报当前模型；带参数 = 切过去。
    Model(Option<String>),
    /// `/doctor` —— 这次会话到底装成了什么样。见 `bridge::doctor`。
    Doctor,
    /// `/resume [关键词]` —— 打开会话选择器。空串 = 最近的几个。
    Resume(String),
    /// `/btw [问题]` —— 开一道侧问。空串 = 重开侧问区，停在最近一次问答上。
    Btw(String),
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
        "/doctor" => Some(LocalCommand::Doctor),
        "/resume" => Some(LocalCommand::Resume(rest.trim().to_string())),
        "/btw" => Some(LocalCommand::Btw(rest.trim().to_string())),
        _ => None,
    }
}

/// 本地命令也要出现在补全弹窗里——它们和 Core 那些一样是用户敲 `/` 想找的东西，
/// 只是解析发生在这一层。Core 的 registry 里没有同名项，不用去重。
fn local_command_candidates() -> Vec<PickerCandidate> {
    [
        (
            "/model",
            "Switch the model for this session  (args: [name])",
        ),
        (
            "/doctor",
            "Report how this session is wired: provider, transcript, sandbox, permissions",
        ),
        (
            "/resume",
            "Switch to an earlier session in this project  (args: [search text])",
        ),
        (
            "/btw",
            "Ask a side question about this session, off the record  (args: [question])",
        ),
        ("/quit", "Exit AttaCode"),
        ("/exit", "Exit AttaCode"),
    ]
    .into_iter()
    .map(|(name, description)| PickerCandidate {
        name: name.into(),
        description: description.into(),
    })
    .collect()
}

/// 返回 `false` 时调用方应退出事件循环。
fn submit(local: &mut LocalUi, handle: &dyn EngineHandle, snapshot: &tui::FrameState) -> Flow {
    if local.draft.is_empty() {
        return Flow::Continue;
    }
    let text = std::mem::take(&mut local.draft);
    local.cursor = 0;
    local.remember(&text);
    local.note_draft_changed();
    // 发了新消息就跳回底部——不然自己刚发的那句在视口外，看着像没发出去。
    local.scroll_offset = None;
    local.selected_block = None;
    // 模型正等着一行文字时，这一行就是答案，**不是**新一轮对话。
    //
    // 放在 slash 分流之前：一道"给这个分支起个名字"的问题，答案完全可能以 `/`
    // 开头，把它当命令解析等于把用户的答案吃掉。用户想改主意就中断 turn
    // （`repl.cancel`），那条路会把问题一起撤走。
    if let Some(req) = pending_typed_question(snapshot) {
        let _ = handle.dispatch(BridgeCommand::AnswerQuestion {
            prompt_id: req.prompt_id.clone(),
            text,
        });
        local.reset_ask_cursor();
        return Flow::Continue;
    }
    let cmd = match local_command(&text) {
        Some(LocalCommand::Quit) => return Flow::Quit,
        // 列表要读盘，这里做不了——交给 `run` 的 async 上下文。
        Some(LocalCommand::Resume(query)) => return Flow::ListSessions(query),
        Some(LocalCommand::Model(Some(name))) => BridgeCommand::SetModel { name },
        Some(LocalCommand::Model(None)) => BridgeCommand::Note {
            text: format!(
                "current model: {} · usage: /model <name>",
                snapshot.footer_hints.model
            ),
        },
        Some(LocalCommand::Doctor) => BridgeCommand::Doctor,
        Some(LocalCommand::Btw(question)) => BridgeCommand::Btw { question },
        None => BridgeCommand::Submit { text },
    };
    let _ = handle.dispatch(cmd);
    Flow::Continue
}

/// 回一个权限决定，并把选择位复位——下一个请求从第一项（"Yes"）开始，而不是
/// 继承上一个对话框停在哪。
fn respond(handle: &dyn EngineHandle, local: &mut LocalUi, req: &AskRequest, decision: AskOption) {
    let _ = handle.dispatch(BridgeCommand::Respond {
        prompt_id: req.prompt_id.clone(),
        decision,
    });
    local.reset_ask_cursor();
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
        self.picker_selected = 0;
        self.picker_dismissed = false;
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

/// The slash-command picker is UI-local, computed fresh from `local.draft` +
/// `local.commands` each time (mirrors how draft/cursor are already merged in) —
/// bridge only supplies the raw candidate list, not the filtered/open-or-closed popup.
fn compute_command_picker(local: &LocalUi) -> Option<PickerState> {
    // 选择器开着时补全不该同时冒出来。它俩共用 `picker_selected`，两个都开
    // 就是两个列表争同一个高亮位。
    if local.session_picker.is_some() {
        return None;
    }
    if local.picker_dismissed {
        return None;
    }
    let rest = local.draft.strip_prefix('/')?;
    if rest.is_empty() || rest.contains(' ') || rest.contains('\n') {
        return None;
    }
    let candidates: Vec<PickerCandidate> = local_command_candidates()
        .into_iter()
        .chain(local.commands.iter().cloned())
        .filter(|c| c.name.trim_start_matches('/').starts_with(rest))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let selected = local.picker_selected.min(candidates.len() - 1);
    Some(PickerState {
        kind: PickerKind::SlashCommand,
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
fn picker_already_typed(local: &LocalUi) -> bool {
    let Some(popup) = compute_command_picker(local) else {
        return false;
    };
    popup
        .candidates
        .get(popup.selected)
        .is_some_and(|c| local.draft == c.name)
}

fn move_picker_selection(local: &mut LocalUi, delta: i32) {
    let Some(popup) = compute_command_picker(local) else {
        return;
    };
    let len = popup.candidates.len() as i32;
    let next = (popup.selected as i32 + delta).rem_euclid(len);
    local.picker_selected = next as usize;
}

fn accept_picker_candidate(local: &mut LocalUi) {
    let Some(popup) = compute_command_picker(local) else {
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
    match &mut frame.composer.content.ask {
        Some(ask) => {
            // 请求答掉一个就少一个，本地下标可能悬空——夹回去。
            ask.active_idx = local.ask_active.min(ask.pending.len().saturating_sub(1));
            let active = ask.active_idx;
            if let Some(req) = ask.pending.get_mut(active) {
                // 越界夹回 0：选项数是 bridge 说了算的，本地下标只是个光标。
                req.selected_option = if local.ask_selected < req.options.len() {
                    local.ask_selected
                } else {
                    0
                };
            }
            // bridge 那边按 `active_idx = 0` 算了一个默认值，真正的 active 是上面这行
            // 夹出来的——用同一个方法重算一次。Tab 到一道自由文本题时输入框要解锁，
            // Tab 回权限请求时要锁上。
            let locks = ask.locks_composer();
            frame.composer.content.editor.locked = locks;
        }
        None => {
            frame.composer.content.picker =
                session_picker_state(local).or(compute_command_picker(local))
        }
    }
    frame
}

/// `/resume` 选择器，借补全弹窗的壳子渲染。
///
/// 借而不是新画一个：它要的东西补全弹窗已经全有了——一列 `名字 + 说明`、一个高亮
/// 位、上下键和回车。区别只在选中之后做什么，而那是 `dispatch_picker_action` 的事，
/// 不是渲染的事。
fn session_picker_state(local: &LocalUi) -> Option<PickerState> {
    let candidates = local.session_picker.clone()?;
    let selected = local
        .picker_selected
        .min(candidates.len().saturating_sub(1));
    Some(PickerState {
        kind: PickerKind::Session,
        query: String::new(),
        candidates,
        selected,
    })
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
    use tui::frame_state::{AskState, AskViewMode, LineKind, TranscriptEntry};

    fn local_with(draft: &str) -> LocalUi {
        let commands = vec![
            PickerCandidate {
                name: "/commit".into(),
                description: "Create a commit".into(),
            },
            PickerCandidate {
                name: "/compact".into(),
                description: "Compact context".into(),
            },
            PickerCandidate {
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
        assert!(compute_command_picker(&local_with("hello")).is_none());
    }

    #[test]
    fn no_popup_once_a_space_is_typed() {
        assert!(compute_command_picker(&local_with("/commit fix bug")).is_none());
    }

    #[test]
    fn filters_by_prefix() {
        let popup = compute_command_picker(&local_with("/com")).unwrap();
        let names: Vec<_> = popup.candidates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["/commit", "/compact"]);
    }

    #[test]
    fn no_popup_when_nothing_matches() {
        assert!(compute_command_picker(&local_with("/zzz")).is_none());
    }

    #[test]
    fn selection_wraps_around_in_both_directions() {
        let mut local = local_with("/com");
        move_picker_selection(&mut local, 1);
        assert_eq!(local.picker_selected, 1);
        move_picker_selection(&mut local, 1);
        assert_eq!(local.picker_selected, 0); // wrapped past 2 candidates
        move_picker_selection(&mut local, -1);
        assert_eq!(local.picker_selected, 1); // wrapped the other way
    }

    #[test]
    fn accept_fills_draft_with_selected_candidate_and_a_trailing_space() {
        let mut local = local_with("/com");
        move_picker_selection(&mut local, 1); // -> /compact
        accept_picker_candidate(&mut local);
        assert_eq!(local.draft, "/compact ");
        assert!(compute_command_picker(&local).is_none()); // space closes the popup
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

    /// 走真实 `Resolver` 把键打一遍——绑定表和 `dispatch_action` 的分支是两处，
    /// 单独看都对、接不上的情况真发生过。顺带确认它们没像 `ask.*` 那样被同键的
    /// 别的动作抢先匹配掉。
    #[test]
    fn keys_reach_their_handlers_through_the_real_resolver() {
        let handle = FakeHandle::new();
        let mut snapshot = frame_without_ask();
        snapshot.transcript.body.entries = (0..100)
            .map(|i| TranscriptEntry {
                starts_segment: false,
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

        // Ctrl+C：真跑时发现取消从来没生效过，先在进程内钉死"键 → 命令"这一段，
        // 好把问题定位到底是分派没接上还是键根本没送到。
        let mut local = with_page(10);
        press(&mut local, CtKeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(
            handle
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|c| matches!(c, BridgeCommand::CancelTurn)),
            "Ctrl+C 没变成 CancelTurn"
        );

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
        let mut frame = frame_without_ask();
        frame.transcript.body.entries = (0..50)
            .map(|i| TranscriptEntry {
                starts_segment: false,
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
        let mut frame = frame_without_ask();
        frame.transcript.body.entries = [("a", 1), ("b", 2), ("c", 1)]
            .iter()
            .flat_map(|(id, rows)| {
                (0..*rows).map(move |r| TranscriptEntry {
                    starts_segment: false,
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
        let mut frame = frame_without_ask();
        frame.transcript.body.entries = (0..50)
            .map(|i| TranscriptEntry {
                starts_segment: false,
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
        assert_eq!(
            submit(&mut local, &handle, &frame_without_ask()),
            Flow::Continue
        );
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
        let mut frame = frame_without_ask();
        frame.footer_hints.model = "claude-sonnet-5".into();
        assert_eq!(submit(&mut local, &handle, &frame), Flow::Continue);
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
        assert!(compute_command_picker(&local).is_some(), "弹窗应该开着");
        assert!(picker_already_typed(&local));

        let handle = FakeHandle::new();
        dispatch_action("editor.submit", &mut local, &handle, &frame_without_ask());
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
        assert!(!picker_already_typed(&local));

        let handle = FakeHandle::new();
        dispatch_action("editor.submit", &mut local, &handle, &frame_without_ask());
        assert_eq!(local.draft, "/model ");
        assert!(handle.commands().is_empty(), "补全不该发命令给 bridge");
    }

    #[test]
    fn local_commands_show_up_in_the_picker() {
        let popup = compute_command_picker(&local_with("/mod")).expect("应该有候选");
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
        submit(&mut local, &handle, &frame_without_ask());
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
        submit(local, handle, &frame_without_ask());
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
        let mut frame = frame_without_ask();
        frame.transcript.header = tui::frame_state::HeaderState {
            text: Some("问题".into()),
            source: tui::frame_state::HeaderSource::UserPrompt,
        };
        frame.transcript.body.entries = vec![
            TranscriptEntry {
                starts_segment: false,
                kind: LineKind::UserPrompt,
                text: "问题".into(),
                block_id: None,
            },
            TranscriptEntry {
                starts_segment: false,
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
                starts_segment: false,
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

    /// 边界：prompt 正好在视口最上面一行时**还看得见**，再多一行才该钉 header。
    /// 变异测试把 `>=` 改成 `>` 时全绿，说明原来只测了"明显在里面/明显在外面"。
    #[test]
    fn header_visibility_is_exact_at_the_viewport_edge() {
        let mut local = local_with("");
        local.viewport_lines = 3;
        let entry = |kind, text: &str| TranscriptEntry {
            starts_segment: false,
            kind,
            text: text.into(),
            block_id: None,
        };
        let mut frame = frame_without_ask();
        // 3 条 = 正好一屏，prompt 在最上面一行 → 还看得见。
        frame.transcript.body.entries = vec![
            entry(LineKind::UserPrompt, "问题"),
            entry(LineKind::AssistantText, "答1"),
            entry(LineKind::AssistantText, "答2"),
        ];
        assert!(prompt_is_visible(&frame, &local), "视口最上面一行仍算可见");

        frame
            .transcript
            .body
            .entries
            .push(entry(LineKind::AssistantText, "答3"));
        assert!(!prompt_is_visible(&frame, &local), "被顶出去了就该钉");
    }

    /// 滚动模式下按 `[offset, offset+高度)` 判断，别退化成只看跟随模式。
    #[test]
    fn header_visibility_follows_the_scroll_offset() {
        let mut local = local_with("");
        local.viewport_lines = 2;
        let mut frame = frame_without_ask();
        frame.transcript.body.entries = (0..10)
            .map(|i| TranscriptEntry {
                starts_segment: false,
                kind: if i == 4 {
                    LineKind::UserPrompt
                } else {
                    LineKind::AssistantText
                },
                text: format!("行{i}"),
                block_id: None,
            })
            .collect();

        local.scroll_offset = Some(4); // 视口 = 行4..行6，prompt 就在第一行
        assert!(prompt_is_visible(&frame, &local));
        local.scroll_offset = Some(6); // prompt 在视口上方
        assert!(!prompt_is_visible(&frame, &local));
        local.scroll_offset = Some(0); // prompt 在视口下方
        assert!(!prompt_is_visible(&frame, &local));
    }

    /// resume 起手时输入历史接着上次——从转录里恢复出来的用户输入读。
    #[test]
    fn history_is_seeded_from_a_restored_transcript() {
        let mut frame = frame_without_ask();
        frame.transcript.body.entries = vec![
            TranscriptEntry {
                starts_segment: false,
                kind: LineKind::UserPrompt,
                text: "上次问的".into(),
                block_id: None,
            },
            TranscriptEntry {
                starts_segment: false,
                kind: LineKind::AssistantText,
                text: "上次答的".into(),
                block_id: None,
            },
        ];
        assert_eq!(user_prompts(&frame), vec!["上次问的"]);
    }

    /// 多个待确认请求排队时，Tab 切到下一个。
    ///
    /// 渲染那边早就画了 tab 条，但一直没有键能切，`active_idx` 恒为 0——后面的
    /// 请求只能等前面的答完才看得见。这是审计用例覆盖时发现的**功能**缺口，
    /// 不是测试缺口。
    #[test]
    fn tab_switches_between_pending_asks() {
        let mut local = local_with("");
        let handle = FakeHandle::new();
        let two = |a: usize| AskState {
            pending: vec![
                AskRequest {
                    answer_with: AnswerWith::Choose,
                    prompt_id: "p1".into(),
                    tool_name: "Bash".into(),
                    message: "第一个".into(),
                    options: vec![AskOption::PermitOnce, AskOption::Deny],
                    selected_option: 0,
                },
                AskRequest {
                    answer_with: AnswerWith::Choose,
                    prompt_id: "p2".into(),
                    tool_name: "Write".into(),
                    message: "第二个".into(),
                    options: vec![AskOption::PermitOnce, AskOption::Deny],
                    selected_option: 0,
                },
            ],
            active_idx: a,
            view_mode: AskViewMode::TabView,
        };

        // 先把第一个的选项挪一格，切 tab 之后要复位（不然会带着上一个的高亮）。
        local.ask_selected = 1;
        dispatch_ask_action(
            "ask.next-request",
            &mut local,
            &handle,
            &two(0).pending[0],
            2,
        );
        assert_eq!(local.ask_active, 1);
        assert_eq!(local.ask_selected, 0, "切 tab 之后选项高亮要复位");

        let mut frame = frame_without_ask();
        frame.composer.content.ask = Some(two(0));
        let merged = merge(frame, &local);
        let ask = merged.composer.content.ask.unwrap();
        assert_eq!(ask.active_idx, 1, "快照要跟着切");
        assert_eq!(ask.pending[1].prompt_id, "p2");

        // 环回去。
        dispatch_ask_action(
            "ask.next-request",
            &mut local,
            &handle,
            &two(1).pending[1],
            2,
        );
        assert_eq!(local.ask_active, 0);
    }

    /// 答掉一个之后队列变短，本地下标可能悬空——夹回去，别越界。
    #[test]
    fn merge_clamps_the_active_ask_after_one_is_answered() {
        let mut local = local_with("");
        local.ask_active = 1;
        let mut frame = frame_without_ask();
        frame.composer.content.ask = Some(AskState {
            pending: vec![AskRequest {
                answer_with: AnswerWith::Choose,
                prompt_id: "p2".into(),
                tool_name: "Write".into(),
                message: "只剩一个了".into(),
                options: vec![AskOption::Deny],
                selected_option: 0,
            }],
            active_idx: 1,
            view_mode: AskViewMode::TabView,
        });
        let merged = merge(frame, &local);
        assert_eq!(merged.composer.content.ask.unwrap().active_idx, 0);
    }

    // ── 编辑器不变量（随机操作序列）──

    /// 一个确定性的小 PRNG。不拉 `rand`/`proptest` 依赖：这里只需要"每次跑都一样、
    /// 但看起来乱"的一串数，随机数质量无所谓；确定性反而是优点——挂了能原样复现。
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, n: usize) -> usize {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
            (self.0 >> 33) as usize % n
        }
    }

    /// 随便怎么敲，编辑器都不能崩，`cursor` 也不能落到字符中间。
    ///
    /// 这两条是所有行内编辑操作共同的前提（`draft.insert/remove` 落在非边界上直接
    /// panic），单独给每个操作写用例覆盖不到组合出来的状态。用固定种子跑几千步。
    #[test]
    fn any_sequence_of_edits_keeps_the_cursor_on_a_char_boundary() {
        let charset = ['a', '重', '\n', ' ', '构', 'Z', '，'];
        for seed in [1u64, 42, 9999] {
            let mut rng = Lcg(seed);
            let mut local = local_with("");
            for step in 0..2000 {
                match rng.next(11) {
                    0 => local.insert(charset[rng.next(charset.len())]),
                    1 => local.delete_backward(),
                    2 => local.delete_forward(),
                    3 => local.move_char(-1),
                    4 => local.move_char(1),
                    5 => local.move_word(-1),
                    6 => local.move_word(1),
                    7 => local.cursor = local.line_start(),
                    8 => local.cursor = local.line_end(),
                    9 => local.delete_word_before(),
                    _ => local.kill_to_line_end(),
                }
                assert!(
                    local.draft.is_char_boundary(local.cursor),
                    "seed {seed} 第 {step} 步之后 cursor={} 落在了字符中间: {:?}",
                    local.cursor,
                    local.draft
                );
                assert!(
                    local.cursor <= local.draft.len(),
                    "seed {seed} 第 {step} 步之后 cursor 越界"
                );
            }
        }
    }

    /// 行间移动同样不能把光标搁在字符中间——保列时是按字符数算的，容易在
    /// 多字节行之间算歪。
    #[test]
    fn vertical_motion_also_keeps_the_cursor_on_a_char_boundary() {
        let mut rng = Lcg(7);
        let mut local = local_with("");
        local.draft = "abc\n重构模块\nx\n，，，\nlonger line here".into();
        local.cursor = 0;
        for step in 0..500 {
            match rng.next(4) {
                0 => local.move_line(-1),
                1 => local.move_line(1),
                2 => local.move_char(1),
                _ => local.move_char(-1),
            }
            assert!(
                local.draft.is_char_boundary(local.cursor),
                "第 {step} 步之后 cursor={} 落在了字符中间",
                local.cursor
            );
        }
    }

    /// 光标在中间时提交，整段草稿都要发出去，光标复位。
    #[test]
    fn submitting_from_mid_draft_sends_the_whole_draft() {
        let mut local = editing("hello |world");
        let handle = FakeHandle::new();
        assert_eq!(
            submit(&mut local, &handle, &frame_without_ask()),
            Flow::Continue
        );
        assert!(matches!(
            handle.commands().as_slice(),
            [BridgeCommand::Submit { text }] if text == "hello world"
        ));
        assert_eq!(local.cursor, 0);
    }

    // ── 权限对话框 ──

    /// 只走 `dispatch` + `sessions`——订阅接口在这些测试里不会被碰到。
    struct FakeHandle(
        std::sync::Mutex<Vec<BridgeCommand>>,
        std::sync::Mutex<Vec<PickerCandidate>>,
    );

    impl FakeHandle {
        fn new() -> Self {
            Self(
                std::sync::Mutex::new(Vec::new()),
                std::sync::Mutex::new(Vec::new()),
            )
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
        fn decisions(&self) -> Vec<AskOption> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter_map(|c| match c {
                    BridgeCommand::Respond { decision, .. } => Some(decision.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    #[bridge::async_trait]
    impl EngineHandle for FakeHandle {
        async fn sessions(&self, _query: &str) -> Vec<PickerCandidate> {
            self.1.lock().unwrap().clone()
        }
        fn dispatch(&self, cmd: BridgeCommand) -> Result<(), bridge::BridgeError> {
            self.0.lock().unwrap().push(cmd);
            Ok(())
        }
        fn subscribe(&self) -> tokio::sync::watch::Receiver<tui::FrameState> {
            unimplemented!("these tests only exercise dispatch")
        }
        fn subscribe_commands(&self) -> tokio::sync::watch::Receiver<Vec<PickerCandidate>> {
            unimplemented!("these tests only exercise dispatch")
        }
    }

    fn ask_request() -> AskRequest {
        AskRequest {
            answer_with: AnswerWith::Choose,
            prompt_id: "p1".into(),
            tool_name: "Bash".into(),
            message: "run rm -rf /?".into(),
            options: vec![
                AskOption::PermitOnce,
                AskOption::PermitSession,
                AskOption::PermitProject,
                AskOption::Deny,
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
        let req = ask_request();

        dispatch_ask_action("editor.history.next", &mut local, &handle, &req, 1);
        dispatch_ask_action("editor.history.next", &mut local, &handle, &req, 1);
        dispatch_ask_action("editor.submit", &mut local, &handle, &req, 1);

        assert_eq!(handle.decisions(), vec![AskOption::PermitProject]);
        assert_eq!(local.ask_selected, 0, "下一个请求应该从头开始");
    }

    #[test]
    fn ask_selection_wraps_and_quick_keys_still_work() {
        let mut local = local_with("");
        let handle = FakeHandle::new();
        let req = ask_request();

        dispatch_ask_action("ask.prev", &mut local, &handle, &req, 1);
        assert_eq!(local.ask_selected, 3, "从第一项往上应该绕到最后一项");

        dispatch_ask_action("ask.yes-shortcut", &mut local, &handle, &req, 1);
        dispatch_ask_action("ask.no-shortcut", &mut local, &handle, &req, 1);
        assert_eq!(
            handle.decisions(),
            vec![AskOption::PermitOnce, AskOption::Deny]
        );
    }

    #[test]
    fn escape_denies_rather_than_leaving_the_tool_call_hanging() {
        let mut local = local_with("");
        let handle = FakeHandle::new();
        dispatch_ask_action("repl.dismiss", &mut local, &handle, &ask_request(), 1);
        assert_eq!(handle.decisions(), vec![AskOption::Deny]);
    }

    /// `y`/`n` 绑的是裸字符：没有对话框时它们必须落进草稿，否则打 "yes" 会变成 "es"。
    #[test]
    fn y_and_n_are_plain_text_when_no_dialog_is_open() {
        let mut local = local_with("");
        let handle = FakeHandle::new();
        let mut resolver = Resolver::new(default_bindings());
        let snapshot = frame_without_ask();

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
        let mut snapshot = frame_without_ask();
        snapshot.composer.content.ask = Some(AskState {
            pending: vec![ask_request()],
            active_idx: 0,
            view_mode: AskViewMode::TabView,
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

    fn question_request(answer_with: AnswerWith, options: Vec<AskOption>) -> AskRequest {
        AskRequest {
            answer_with,
            prompt_id: "t1".into(),
            tool_name: "Branch name".into(),
            message: "叫什么好？".into(),
            options,
            selected_option: 0,
        }
    }

    fn frame_with(req: AskRequest) -> tui::FrameState {
        let mut snapshot = frame_without_ask();
        snapshot.composer.content.ask = Some(AskState {
            pending: vec![req],
            active_idx: 0,
            view_mode: AskViewMode::TabView,
        });
        snapshot
    }

    /// 模型的多选题回的是它自己的 key，不是权限决定。
    #[test]
    fn confirming_a_model_question_sends_the_models_own_key() {
        let mut local = local_with("");
        let handle = FakeHandle::new();
        let req = question_request(
            AnswerWith::Choose,
            vec![
                AskOption::Answer {
                    key: "a".into(),
                    label: "feat/x".into(),
                },
                AskOption::Answer {
                    key: "b".into(),
                    label: "fix/y".into(),
                },
            ],
        );

        dispatch_ask_action("editor.history.next", &mut local, &handle, &req, 1);
        dispatch_ask_action("editor.submit", &mut local, &handle, &req, 1);

        assert_eq!(
            handle.decisions(),
            vec![AskOption::Answer {
                key: "b".into(),
                label: "fix/y".into()
            }]
        );
    }

    /// `y`/`n`/Esc 是权限门那道题的快捷键。模型的提问里没有 `PermitOnce`/`Deny`
    /// 这两个选项，按下去替用户答一道没答过的题是最糟的一种"方便"。
    #[test]
    fn permission_shortcuts_do_nothing_on_a_model_question() {
        let mut local = local_with("");
        let handle = FakeHandle::new();
        let req = question_request(
            AnswerWith::Choose,
            vec![AskOption::Answer {
                key: "a".into(),
                label: "A".into(),
            }],
        );

        for action in ["ask.yes-shortcut", "ask.no-shortcut", "repl.dismiss"] {
            dispatch_ask_action(action, &mut local, &handle, &req, 1);
        }
        assert!(handle.decisions().is_empty());
    }

    /// 自由文本题期间键盘留在 composer 上：打字进草稿，回车把草稿当答案送走
    /// ——**不是**当成新一轮对话发给引擎。
    #[test]
    fn a_typed_question_takes_the_next_submitted_line_as_its_answer() {
        let mut local = local_with("");
        let handle = FakeHandle::new();
        let mut resolver = Resolver::new(default_bindings());
        let snapshot = frame_with(question_request(AnswerWith::Type, Vec::new()));

        for c in "feat/ask".chars() {
            dispatch_key(
                KeyEvent::new(CtKeyCode::Char(c), KeyModifiers::NONE),
                &mut resolver,
                &mut local,
                &handle,
                &snapshot,
            );
        }
        assert_eq!(local.draft, "feat/ask", "自由文本题期间打字要进草稿");

        dispatch_key(
            KeyEvent::new(CtKeyCode::Enter, KeyModifiers::NONE),
            &mut resolver,
            &mut local,
            &handle,
            &snapshot,
        );

        let cmds = handle.commands();
        assert!(
            matches!(&cmds[..], [BridgeCommand::AnswerQuestion { prompt_id, text }]
                     if prompt_id == "t1" && text == "feat/ask"),
            "got: {cmds:?}"
        );
    }

    /// 答案完全可能以 `/` 开头。把它当 slash 命令解析等于把用户的答案吃掉。
    #[test]
    fn an_answer_that_looks_like_a_command_is_still_an_answer() {
        let mut local = local_with("/model 这个名字");
        let handle = FakeHandle::new();
        let snapshot = frame_with(question_request(AnswerWith::Type, Vec::new()));

        submit(&mut local, &handle, &snapshot);

        let cmds = handle.commands();
        assert!(
            matches!(&cmds[..], [BridgeCommand::AnswerQuestion { text, .. }]
                     if text == "/model 这个名字"),
            "got: {cmds:?}"
        );
    }

    // ── 多行提交（`push_lines` 之后一次提交在转录里是好几条 entry）──

    /// `(kind, text, 是不是上一条的续行)`。
    fn frame_with_entries(entries: Vec<(LineKind, &str, bool)>) -> tui::FrameState {
        let mut f = frame_without_ask();
        f.transcript.body.entries = entries
            .into_iter()
            .map(|(kind, text, starts_segment)| TranscriptEntry {
                starts_segment,
                kind,
                text: text.into(),
                block_id: None,
            })
            .collect();
        f.transcript.body.scroll.total_lines = f.transcript.body.entries.len();
        f
    }

    /// `--continue` 之后 `↑` 召回的必须是**整段**，和当场提交时 `remember()` 存的
    /// 形状一致。按条收的话，一次三行的提交在历史里变成三条，`↑` 只召回最后一行。
    #[test]
    fn a_multi_line_prompt_comes_back_from_the_transcript_as_one_history_item() {
        let frame = frame_with_entries(vec![
            (LineKind::UserPrompt, "第一次提问", false),
            (LineKind::AssistantText, "答", false),
            (LineKind::UserPrompt, "git commit -m 修一下", false),
            (LineKind::UserPrompt, "", true),
            (LineKind::UserPrompt, "顺便把注释也改了", true),
            (LineKind::AssistantText, "好", false),
        ]);

        assert_eq!(
            user_prompts(&frame),
            vec![
                "第一次提问".to_string(),
                "git commit -m 修一下\n\n顺便把注释也改了".to_string(),
            ]
        );
    }

    /// **相邻不等于同一次提交。** 恢复出来的转录里每条用户消息各成一个 turn，
    /// 所以"发一句 → Ctrl+C → 再发一句"会留下两条紧挨着的 prompt，中间什么都没有。
    /// 按相邻拼会把两次提交粘成一条，`↑` 一次召回两句话。
    #[test]
    fn two_adjacent_submissions_are_not_glued_into_one() {
        let frame = frame_with_entries(vec![
            (LineKind::UserPrompt, "先问的那句", false),
            (LineKind::UserPrompt, "打断之后重新问的", false),
        ]);

        assert_eq!(
            user_prompts(&frame),
            vec!["先问的那句".to_string(), "打断之后重新问的".to_string()]
        );
    }

    /// sticky header 钉的是**第一行**（`reducer::current_prompt` 取 `first_line`），
    /// 所以"看得见吗"问的也得是第一行。按最后一行判的话，一段多行提交只露出尾巴时
    /// 会被判成看得见、header 被收起来——而它要显示的那一行正好在屏幕外面。
    #[test]
    fn a_partly_scrolled_multi_line_prompt_still_needs_its_header() {
        let mut entries = vec![(LineKind::AssistantText, "早先的内容", false); 10];
        entries.extend([
            (LineKind::UserPrompt, "第一行——header 钉的就是这句", false),
            (LineKind::UserPrompt, "第二行", true),
            (LineKind::UserPrompt, "第三行", true),
        ]);
        entries.extend([(LineKind::AssistantText, "回答", false); 4]);
        let frame = frame_with_entries(entries);

        let mut local = local_with("");
        local.viewport_lines = 5;
        // 视口只够显示最后 5 条：prompt 的第一行（下标 10）已经在上面看不见了。
        assert!(
            !prompt_is_visible(&frame, &local),
            "第一行在视口外，header 必须留着"
        );

        // 视口够大到把第一行也装进来了，才该收起来。
        local.viewport_lines = 20;
        assert!(prompt_is_visible(&frame, &local));
    }

    // ── 待确认队列的本地光标 ──

    /// 队列缩短时本地光标要跟着回来，否则一次 Tab 会什么都不发生。
    #[test]
    fn the_local_cursor_follows_a_shrinking_queue() {
        let mut local = local_with("");
        local.ask_active = 2;

        let mut frame = frame_without_ask();
        frame.composer.content.ask = Some(AskState {
            pending: vec![ask_request(), ask_request()],
            active_idx: 0,
            view_mode: AskViewMode::TabView,
        });
        local.reconcile(&frame);
        assert_eq!(local.ask_active, 1, "越界的光标要夹回来");

        local.reconcile(&frame_without_ask());
        assert_eq!(local.ask_active, 0, "队列空了就归零");
    }

    /// **两个下标必须一起夹。** 只夹 `ask_active` 的话回车会变成死键：
    /// 队列 `[A(4 个选项), B(2 个)]`，用户在 A 上选到第 4 项，A 自己超时消失，
    /// active 落到 B——屏幕上高亮的是 B 的第一项（`merge` 把越界的渲染值归了 0），
    /// 而 `ask.confirm` 读的是**没夹过的本地值**，`options.get(3)` 是 None，于是
    /// 什么都不发生，得先按一下方向键才活过来。
    #[test]
    fn a_shrinking_queue_never_leaves_confirm_pointing_at_nothing() {
        let two_options = question_request(
            AnswerWith::Choose,
            vec![
                AskOption::Answer {
                    key: "a".into(),
                    label: "A".into(),
                },
                AskOption::Answer {
                    key: "b".into(),
                    label: "B".into(),
                },
            ],
        );
        let mut frame = frame_without_ask();
        frame.composer.content.ask = Some(AskState {
            pending: vec![two_options.clone()],
            active_idx: 0,
            view_mode: AskViewMode::TabView,
        });

        let mut local = local_with("");
        local.ask_active = 1; // 前面那条刚消失
        local.ask_selected = 3; // 停在前面那条的第 4 个选项上
        local.reconcile(&frame);
        assert_eq!(local.ask_selected, 0, "越界的选项下标要归零");

        // 回车必须真的答出一个东西来，而不是什么都不发生。
        let handle = FakeHandle::new();
        dispatch_ask_action("editor.submit", &mut local, &handle, &two_options, 1);
        assert_eq!(
            handle.decisions(),
            vec![AskOption::Answer {
                key: "a".into(),
                label: "A".into()
            }]
        );
    }

    /// 还在范围内的选择不该被无端复位——用户挑到第 2 项、队列没动，就该停在那儿。
    #[test]
    fn an_in_range_selection_survives_reconcile() {
        let mut frame = frame_without_ask();
        frame.composer.content.ask = Some(AskState {
            pending: vec![ask_request()],
            active_idx: 0,
            view_mode: AskViewMode::TabView,
        });
        let mut local = local_with("");
        local.ask_selected = 2;
        local.reconcile(&frame);
        assert_eq!(local.ask_selected, 2);
    }

    /// 答完一个之后光标回队列头。留在原地的话，下一个请求一到会凭空成为 active
    /// ——而 `merge` 现在从 active 那条推算输入框锁不锁，于是输入框会在用户打了
    /// 一半的草稿底下自己锁上。
    #[test]
    fn answering_puts_the_cursor_back_at_the_head_of_the_queue() {
        let mut local = local_with("");
        local.ask_active = 1;
        local.ask_selected = 2;
        let handle = FakeHandle::new();

        respond(&handle, &mut local, &ask_request(), AskOption::PermitOnce);

        assert_eq!(local.ask_active, 0);
        assert_eq!(local.ask_selected, 0);
    }

    // ── /btw 侧问区 ──

    fn frame_with_btw() -> tui::FrameState {
        let mut f = frame_without_ask();
        f.btw = Some(tui::frame_state::BtwState {
            question: "那个配置文件叫什么".into(),
            answer: "叫 settings.json".into(),
            streaming: false,
            scroll: 0,
            earlier: vec!["第一问".into()],
            older: 0,
            viewing: 0,
        });
        f
    }

    #[test]
    fn btw_is_a_local_command_carrying_the_question() {
        let handle = FakeHandle::new();
        let mut local = local_with("/btw 那个配置文件叫什么");
        submit(&mut local, &handle, &frame_without_ask());
        assert!(
            matches!(&handle.commands()[..], [BridgeCommand::Btw { question }]
                     if question == "那个配置文件叫什么"),
            "got: {:?}",
            handle.commands()
        );
    }

    /// **侧问区独占键盘。** 它盖住了输入区、状态区、底栏和子代理条——盖住的东西
    /// 不该还能被操作。打字尤其不能落进草稿：屏幕上根本没有输入框。
    #[test]
    fn the_side_question_panel_takes_the_whole_keyboard() {
        let mut local = local_with("原有草稿");
        let handle = FakeHandle::new();
        let mut resolver = Resolver::new(default_bindings());
        let snapshot = frame_with_btw();

        for c in "hello".chars() {
            dispatch_key(
                KeyEvent::new(CtKeyCode::Char(c), KeyModifiers::NONE),
                &mut resolver,
                &mut local,
                &handle,
                &snapshot,
            );
        }
        assert_eq!(local.draft, "原有草稿", "草稿一个字都不该动");

        // 回车不是提交，是退出。
        dispatch_key(
            KeyEvent::new(CtKeyCode::Enter, KeyModifiers::NONE),
            &mut resolver,
            &mut local,
            &handle,
            &snapshot,
        );
        assert!(
            handle
                .commands()
                .iter()
                .any(|c| matches!(c, BridgeCommand::BtwKey(BtwKey::Close))),
            "got: {:?}",
            handle.commands()
        );
        assert!(
            !handle
                .commands()
                .iter()
                .any(|c| matches!(c, BridgeCommand::Submit { .. })),
            "回车在侧问区里绝不能把草稿提交出去"
        );
    }

    /// 侧问区的方向键：↑↓ 滚动，←→ 翻看早前。
    #[test]
    fn arrows_scroll_and_step_through_earlier_answers() {
        let handle = FakeHandle::new();
        for (key, expected) in [
            (CtKeyCode::Up, BtwKey::Scroll(-1)),
            (CtKeyCode::Down, BtwKey::Scroll(1)),
            (CtKeyCode::Left, BtwKey::Older),
            (CtKeyCode::Right, BtwKey::Newer),
        ] {
            let mut local = local_with("");
            let mut resolver = Resolver::new(default_bindings());
            dispatch_key(
                KeyEvent::new(key, KeyModifiers::NONE),
                &mut resolver,
                &mut local,
                &handle,
                &frame_with_btw(),
            );
            assert!(
                handle
                    .commands()
                    .iter()
                    .any(|c| matches!(c, BridgeCommand::BtwKey(k) if *k == expected)),
                "{key:?} 该发 {expected:?}，实际: {:?}",
                handle.commands()
            );
        }
    }

    /// `x` 绑的是裸字符。侧问区**没**开着时它必须是普通输入——不然打 "box" 会丢字母。
    #[test]
    fn x_is_plain_text_when_the_side_question_panel_is_closed() {
        let mut local = local_with("");
        let handle = FakeHandle::new();
        let mut resolver = Resolver::new(default_bindings());

        for c in "box".chars() {
            dispatch_key(
                KeyEvent::new(CtKeyCode::Char(c), KeyModifiers::NONE),
                &mut resolver,
                &mut local,
                &handle,
                &frame_without_ask(),
            );
        }
        assert_eq!(local.draft, "box");
        assert!(handle.commands().is_empty());
    }

    /// 侧问区开着时 `x` 才是"清空早前问答"。
    #[test]
    fn x_clears_the_earlier_exchanges_inside_the_panel() {
        let mut local = local_with("");
        let handle = FakeHandle::new();
        let mut resolver = Resolver::new(default_bindings());
        dispatch_key(
            KeyEvent::new(CtKeyCode::Char('x'), KeyModifiers::NONE),
            &mut resolver,
            &mut local,
            &handle,
            &frame_with_btw(),
        );
        assert!(handle
            .commands()
            .iter()
            .any(|c| matches!(c, BridgeCommand::BtwKey(BtwKey::ClearEarlier))));
        assert_eq!(local.draft, "", "在侧问区里它是快捷键，不是输入");
    }

    /// 侧问区盖住了状态区，而主 turn 可能还在跑——Ctrl+C 仍要能中断它。
    #[test]
    fn ctrl_c_still_interrupts_the_turn_from_inside_the_panel() {
        let mut local = local_with("");
        let handle = FakeHandle::new();
        let mut resolver = Resolver::new(default_bindings());
        dispatch_key(
            KeyEvent::new(CtKeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut resolver,
            &mut local,
            &handle,
            &frame_with_btw(),
        );
        assert!(handle
            .commands()
            .iter()
            .any(|c| matches!(c, BridgeCommand::CancelTurn)));
    }

    // ── /resume 会话选择器 ──

    /// `/resume` 不能就地处理：列表要读盘，而 `submit` 是同步的。它必须把这件事
    /// 交出去。
    #[test]
    fn resume_asks_the_event_loop_to_fetch_the_list() {
        let handle = FakeHandle::new();
        for (draft, expected) in [
            ("/resume", ""),
            ("/resume 权限门", "权限门"),
            ("/resume   ", ""),
        ] {
            let mut local = local_with(draft);
            let flow = submit(&mut local, &handle, &frame_without_ask());
            assert_eq!(flow, Flow::ListSessions(expected.into()), "draft: {draft}");
        }
        assert!(
            handle.commands().is_empty(),
            "/resume 不该变成一条发给引擎的命令"
        );
    }

    /// 选中一项要跑出这一层——换会话是把整个引擎重建，`run` 做不了。
    #[test]
    fn picking_a_session_asks_for_a_restart() {
        let handle = FakeHandle::new();
        let mut local = local_with("");
        local.open_picker(candidates(&["aaa", "bbb"]));

        assert_eq!(
            dispatch_picker_action("ask.next", &mut local, &handle),
            Flow::Continue
        );
        assert_eq!(local.picker_selected, 1);
        assert_eq!(
            dispatch_picker_action("ask.confirm", &mut local, &handle),
            Flow::Resume("bbb".into())
        );
        assert!(local.session_picker.is_none(), "选完要关掉");
    }

    /// Esc 关掉选择器，而不是恢复到某个会话。
    #[test]
    fn escape_closes_the_picker_without_switching() {
        let handle = FakeHandle::new();
        let mut local = local_with("");
        local.open_picker(candidates(&["aaa"]));
        assert_eq!(
            dispatch_picker_action("repl.dismiss", &mut local, &handle),
            Flow::Continue
        );
        assert!(local.session_picker.is_none());
    }

    /// 选择器开着时键盘整体归它：打字不该落进草稿，否则用户以为自己在输入。
    #[test]
    fn the_picker_takes_the_keyboard() {
        let mut local = local_with("");
        local.open_picker(candidates(&["aaa", "bbb"]));
        let handle = FakeHandle::new();
        let mut resolver = Resolver::new(default_bindings());

        for c in "hello".chars() {
            dispatch_key(
                KeyEvent::new(CtKeyCode::Char(c), KeyModifiers::NONE),
                &mut resolver,
                &mut local,
                &handle,
                &frame_without_ask(),
            );
        }
        assert_eq!(local.draft, "");
        assert!(local.session_picker.is_some());
    }

    /// 选择器和补全弹窗共用 `picker_selected`，两个同时开就是两个列表争一个
    /// 高亮位。选择器开着时补全必须让路。
    #[test]
    fn the_session_picker_and_the_command_picker_are_never_both_open() {
        let mut local = local_with("/mo");
        assert!(compute_command_picker(&local).is_some());
        local.open_picker(candidates(&["aaa"]));
        assert!(compute_command_picker(&local).is_none());

        let popup = session_picker_state(&local).expect("选择器要能上屏");
        assert_eq!(popup.kind, PickerKind::Session);
        assert_eq!(popup.candidates[0].name, "aaa");
    }

    /// 空列表按回车只是关掉，不是"恢复到一个叫空串的会话"。
    #[test]
    fn confirming_an_empty_picker_switches_to_nothing() {
        let handle = FakeHandle::new();
        let mut local = local_with("");
        local.open_picker(Vec::new());
        assert_eq!(
            dispatch_picker_action("ask.confirm", &mut local, &handle),
            Flow::Continue
        );
        assert!(local.session_picker.is_none());
    }

    /// 列表开着的时候 turn 可能正在跑，中断它是唯一能停下一个跑飞了的 turn 的办法。
    #[test]
    fn ctrl_c_still_interrupts_the_turn_while_the_picker_is_open() {
        let mut local = local_with("");
        let handle = FakeHandle::new();
        local.open_picker(candidates(&["aaa"]));

        dispatch_picker_action("repl.cancel", &mut local, &handle);

        assert!(
            handle
                .commands()
                .iter()
                .any(|c| matches!(c, BridgeCommand::CancelTurn)),
            "got: {:?}",
            handle.commands()
        );
        assert!(local.session_picker.is_some(), "中断 turn 不该顺手关掉列表");
    }

    /// 待确认请求一到，选择器就得收起来。
    ///
    /// 不收的话：`merge` 只在没有请求时才画它，而键盘路由是按"当前这条是不是
    /// 选择题"分的——一道自由文本题会让选择器**从屏幕上消失却继续吃着键盘**，
    /// 用户打字没反应，回车静默换了会话。
    #[test]
    fn an_arriving_request_takes_the_picker_off_the_screen_and_off_the_keyboard() {
        let mut local = local_with("");
        local.open_picker(candidates(&["aaa", "bbb"]));

        local.reconcile(&frame_with(question_request(AnswerWith::Type, Vec::new())));

        assert!(local.session_picker.is_none());
        assert!(session_picker_state(&local).is_none());
    }

    /// 队列 `[自由文本题, 权限请求]`：锁不锁输入框，看的是**当前正在答的那一条**。
    ///
    /// 之前三处各算各的：锁看"队列里有没有选择题"、路由看"当前这条是不是"。
    /// 于是输入框画成灰的（说"不能打字"），而打字确实能打——UI 和行为互相打脸。
    #[test]
    fn locking_follows_the_active_request_not_the_whole_queue() {
        let mut frame = frame_without_ask();
        frame.composer.content.ask = Some(AskState {
            pending: vec![
                question_request(AnswerWith::Type, Vec::new()),
                ask_request(),
            ],
            active_idx: 0,
            view_mode: AskViewMode::TabView,
        });

        let mut local = local_with("");
        let merged = merge(frame.clone(), &local);
        assert!(
            !merged.composer.content.editor.locked,
            "当前这条是问答题，输入框该能用"
        );
        assert!(active_choice(&merged).is_none(), "键盘不该归对话框");

        // Tab 到后面那个权限请求。
        local.ask_active = 1;
        let merged = merge(frame, &local);
        assert!(
            merged.composer.content.editor.locked,
            "当前这条是权限请求，输入框该锁上"
        );
        assert!(active_choice(&merged).is_some(), "键盘该归对话框了");
    }

    /// 上一条测的是"处理函数到得了"，这一条测的是"按键真能路由过去"。
    ///
    /// 两件事是分开的：`ask.next-request` 绑在 `Tab` 上，而默认键位里 `editor.*`
    /// 排在 `ask.*` 前面、遮住了 `ask.prev`/`ask.next`/`ask.confirm`。要是 `Tab` 也
    /// 被谁占了，处理函数写得再对也永远轮不到。
    #[test]
    fn tab_reaches_the_queue_switch_from_the_editor_context() {
        let mut frame = frame_without_ask();
        frame.composer.content.ask = Some(AskState {
            pending: vec![
                question_request(AnswerWith::Type, Vec::new()),
                ask_request(),
            ],
            active_idx: 0,
            view_mode: AskViewMode::TabView,
        });
        let mut local = local_with("");
        let handle = FakeHandle::new();
        let mut resolver = Resolver::new(default_bindings());
        let snapshot = merge(frame, &local);

        dispatch_key(
            KeyEvent::new(CtKeyCode::Tab, KeyModifiers::NONE),
            &mut resolver,
            &mut local,
            &handle,
            &snapshot,
        );

        assert_eq!(local.ask_active, 1, "Tab 没有路由到队列切换上");
    }

    /// 排在自由文本题后面的权限请求必须 Tab 得到。
    ///
    /// 以前 `ask.next-request` 只在 `dispatch_ask_action` 里，而那个函数只有
    /// 当前这条是选择题时才到得了——于是那个权限请求永远答不了，一直挂到 300 秒
    /// 超时被自动拒绝。
    #[test]
    fn a_permission_prompt_queued_behind_a_question_can_still_be_reached() {
        let mut frame = frame_without_ask();
        frame.composer.content.ask = Some(AskState {
            pending: vec![
                question_request(AnswerWith::Type, Vec::new()),
                ask_request(),
            ],
            active_idx: 0,
            view_mode: AskViewMode::TabView,
        });
        let mut local = local_with("");
        let handle = FakeHandle::new();
        let snapshot = merge(frame, &local);

        dispatch_action("ask.next-request", &mut local, &handle, &snapshot);

        assert_eq!(local.ask_active, 1, "Tab 该切到那个权限请求上");
    }

    fn candidates(ids: &[&str]) -> Vec<PickerCandidate> {
        ids.iter()
            .map(|id| PickerCandidate {
                name: (*id).into(),
                description: "yesterday  3 msgs  x".into(),
            })
            .collect()
    }

    /// 本地高亮下标必须覆盖进快照，否则渲染出来的选中项永远是第一项。
    #[test]
    fn merge_writes_the_local_selection_into_the_active_request() {
        let mut local = local_with("");
        local.ask_selected = 2;
        let mut snapshot = frame_without_ask();
        snapshot.composer.content.ask = Some(AskState {
            pending: vec![ask_request()],
            active_idx: 0,
            view_mode: AskViewMode::TabView,
        });

        let merged = merge(snapshot, &local);
        let ask = merged.composer.content.ask.unwrap();
        assert_eq!(ask.pending[0].selected_option, 2);
    }

    fn frame_without_ask() -> tui::FrameState {
        use tui::frame_state::*;
        tui::FrameState {
            btw: None,
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
                    picker: None,
                    ask: None,
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
        assert!(compute_command_picker(&local).is_some());
        local.picker_dismissed = true;
        assert!(compute_command_picker(&local).is_none());
        insert_char(
            KeyEvent::new(CtKeyCode::Char('m'), KeyModifiers::NONE),
            &mut local,
        );
        assert!(compute_command_picker(&local).is_some());
    }
}
