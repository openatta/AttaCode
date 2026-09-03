//! TUI↔Core 粘合层 — 装配 `runtime::Agent`，把 `AgentEvent` 归约成 `tui::FrameState`。
//! 见 docs/design/2026-08-13-tui-core-glue-layer.md。
//!
//! `crates/app` 只应该看到 [`start`] + [`EngineHandle`]/[`BridgeCommand`]——`Agent`/
//! `EventReceiver`/`InputSender` 等 AttaCore 类型完全留在这个 crate 内部。

pub mod ask;
pub mod bootstrap;
pub mod btw;
pub mod commands;
pub mod doctor;
pub mod handle;
pub mod permission;
pub mod reducer;
pub mod sessions;
pub mod trace;

/// `EngineHandle` 有一个 async 方法，实现它就得用这个宏。从这里再导出，是为了让
/// `crates/app` 不必自己去依赖 `async-trait`——它用的是哪个宏 crate 是 bridge 的事。
pub use async_trait::async_trait;
pub use bootstrap::{BootstrapConfig, BootstrapError, Resume, DEFAULT_MODEL};
pub use handle::{BridgeCommand, BridgeError, BridgeHandle, BtwKey, EngineHandle};

use reducer::Reducer;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tui::FrameState;

/// 一次跑起来的会话，对外就这两件事：跟它说话，以及关掉它。
///
/// 打成一个结构体而不是返回一个二元组，是为了让 `crates/app` 不必给
/// `tokio_util::sync::CancellationToken` 起个名字——它连 `tokio-util` 都不依赖，
/// 而"关掉这次会话"本来也不需要知道底下用的是什么取消原语。
pub struct Session {
    pub handle: Arc<dyn EngineHandle>,
    cancel: CancellationToken,
}

impl Session {
    /// 关掉这次会话的引擎。**结束整个会话**，和中断一个 turn
    /// （`BridgeCommand::CancelTurn`）是两回事。
    pub fn shutdown(&self) {
        self.cancel.cancel();
    }
}

/// 装配 Core、启动 Agent 后台循环、启动归约器——一次性完成粘合层的全部初始化。
pub async fn start(config: BootstrapConfig) -> Result<Session, BootstrapError> {
    let engine = bootstrap::build_agent(&config).await?;

    // `Agent::commands()` 必须在 spawn 之前取：`run()` 会 `&mut self` 借走整个
    // session，之后就没有 `&Agent` 可问了。拿到的是 `Arc`，一直持有即可。
    let (command_catalog, commands_rx) = commands::CommandCatalog::new(engine.agent.commands());

    let cancel = CancellationToken::new();
    let run_cancel = cancel.clone();
    tokio::spawn(async move {
        let mut agent = engine.agent;
        agent.run(run_cancel).await;
    });

    let (reducer, frame_rx) = Reducer::spawn(
        engine.event_rx,
        engine.questions_rx,
        engine.model_name,
        cwd_display(&config),
        command_catalog,
        engine.restored,
    );

    let handle: Arc<dyn EngineHandle> = Arc::new(BridgeHandle::new(
        handle::EngineParts {
            input_tx: engine.input_tx,
            questions: engine.questions,
            health: Some(engine.health),
            history: engine.history,
            side_questions: Some(engine.side_questions),
        },
        frame_rx,
        commands_rx,
        reducer,
        cancel.clone(),
    ));

    Ok(Session { handle, cancel })
}

fn cwd_display(config: &BootstrapConfig) -> String {
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| config.paths.project_root().display().to_string())
}

/// 方便 `crates/app` 在渲染前订阅当前快照而不必知道 `watch` 的具体类型别名。
pub type FrameReceiver = tokio::sync::watch::Receiver<FrameState>;
