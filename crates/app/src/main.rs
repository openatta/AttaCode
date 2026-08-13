//! `attacode` — application entry point: terminal setup, event loop, key dispatch.
//!
//! Owns two concerns bridge intentionally doesn't: terminal I/O (ratatui/crossterm) and
//! UI-local composer state (draft/cursor). Everything Core-related goes through
//! `bridge::EngineHandle` — this file never touches an AttaCore type directly.
//!
//! Composer editing here is intentionally minimal (append/backspace at end of the
//! draft, no mid-line cursor movement) — this task establishes the event-loop wiring
//! end to end; richer text editing is follow-up work, not an architecture concern.

use bridge::{BootstrapConfig, BridgeCommand, EngineHandle};
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
use tui::frame_state::ApprovalOption;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = BootstrapConfig::defaults("claude-sonnet-4-6");
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

/// UI-local composer state — never sent to bridge; merged onto bridge's
/// `FrameState` snapshot right before each render.
#[derive(Default)]
struct LocalUi {
    draft: String,
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    handle: &dyn EngineHandle,
) -> anyhow::Result<()> {
    let mut frame_rx = handle.subscribe();
    let mut resolver = Resolver::new(default_bindings());
    let mut local = LocalUi::default();
    let mut keys = EventStream::new();

    loop {
        let snapshot = merge(frame_rx.borrow().clone(), &local);
        terminal.draw(|f| {
            tui::layout::render(f, f.area(), &snapshot, spinner_frame());
        })?;

        tokio::select! {
            changed = frame_rx.changed() => {
                if changed.is_err() {
                    // bridge 已经退出（Agent 后台 task 崩溃/关闭）。
                    break;
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
    match resolver.on_key(&key) {
        ResolveOutcome::Action(action) => dispatch_action(&action, local, handle, snapshot),
        ResolveOutcome::Partial | ResolveOutcome::ChordCancelled => true,
        ResolveOutcome::Unmatched(_) => {
            insert_char(key, local);
            true
        }
    }
}

fn dispatch_action(
    action: &str,
    local: &mut LocalUi,
    handle: &dyn EngineHandle,
    snapshot: &tui::FrameState,
) -> bool {
    match action {
        "editor.submit" => return submit(local, handle),
        "editor.clear" => local.draft.clear(),
        "repl.cancel" => {
            let _ = handle.dispatch(BridgeCommand::CancelTurn);
        }
        "repl.exit" => {
            if local.draft.is_empty() {
                return false;
            }
        }
        "ask.yes-shortcut" | "ask.confirm" => {
            respond_active_approval(snapshot, handle, ApprovalOption::PermitOnce)
        }
        "ask.no-shortcut" | "repl.dismiss" => {
            respond_active_approval(snapshot, handle, ApprovalOption::Deny)
        }
        _ => {}
    }
    true
}

/// `/` 前缀的一次性分流：目前只认识 `/quit`/`/exit`（本地处理，不联系 Core）。
/// 其余 slash 输入原样转发——具体命令表留给尚未创建的 slash 子系统（见需求文档
/// Out of scope）。返回 `false` 时调用方应退出事件循环。
fn submit(local: &mut LocalUi, handle: &dyn EngineHandle) -> bool {
    if local.draft.is_empty() {
        return true;
    }
    let text = std::mem::take(&mut local.draft);
    if matches!(text.as_str(), "/quit" | "/exit") {
        return false;
    }
    let _ = handle.dispatch(BridgeCommand::Submit { text });
    true
}

fn respond_active_approval(
    snapshot: &tui::FrameState,
    handle: &dyn EngineHandle,
    decision: ApprovalOption,
) {
    let Some(approval) = &snapshot.composer.content.approval else {
        return;
    };
    let Some(req) = approval.pending.get(approval.active_idx) else {
        return;
    };
    let _ = handle.dispatch(BridgeCommand::RespondPermission {
        prompt_id: req.prompt_id.clone(),
        decision,
    });
}

fn insert_char(key: KeyEvent, local: &mut LocalUi) {
    match key.code {
        CtKeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            local.draft.push(c);
        }
        CtKeyCode::Backspace => {
            local.draft.pop();
        }
        _ => {}
    }
}

fn merge(mut frame: tui::FrameState, local: &LocalUi) -> tui::FrameState {
    frame.composer.content.editor.draft = local.draft.clone();
    frame.composer.content.editor.cursor = local.draft.len();
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
