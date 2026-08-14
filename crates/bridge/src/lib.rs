//! TUI↔Core 粘合层 — 装配 `runtime::Agent`，把 `AgentEvent` 归约成 `tui::FrameState`。
//! 见 docs/design/2026-08-13-tui-core-glue-layer.md。
//!
//! `crates/app` 只应该看到 [`start`] + [`EngineHandle`]/[`BridgeCommand`]——`Agent`/
//! `EventReceiver`/`InputSender` 等 AttaCore 类型完全留在这个 crate 内部。

pub mod bootstrap;
pub mod commands;
pub mod handle;
pub mod permission;
pub mod reducer;

pub use bootstrap::{BootstrapConfig, BootstrapError, Resume, DEFAULT_MODEL};
pub use handle::{BridgeCommand, BridgeError, BridgeHandle, EngineHandle};

use reducer::Reducer;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tui::FrameState;

/// 装配 Core、启动 Agent 后台循环、启动归约器——一次性完成粘合层的全部初始化。
/// `crates/app` 只需要持有返回的 `EngineHandle` 和 `CancellationToken`（退出时 `.cancel()`）。
pub async fn start(
    config: BootstrapConfig,
) -> Result<(Arc<dyn EngineHandle>, CancellationToken), BootstrapError> {
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
        engine.model_name,
        cwd_display(&config),
        command_catalog,
        engine.restored,
    );

    let handle: Arc<dyn EngineHandle> = Arc::new(BridgeHandle::new(
        engine.input_tx,
        frame_rx,
        commands_rx,
        reducer,
        cancel.clone(),
    ));

    Ok((handle, cancel))
}

fn cwd_display(config: &BootstrapConfig) -> String {
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| config.paths.project_root().display().to_string())
}

/// 方便 `crates/app` 在渲染前订阅当前快照而不必知道 `watch` 的具体类型别名。
pub type FrameReceiver = tokio::sync::watch::Receiver<FrameState>;
