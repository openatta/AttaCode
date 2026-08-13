//! TUI↔Core 粘合层 — 装配 `runtime::Agent`，把 `AgentEvent` 归约成 `tui::FrameState`。
//! 见 docs/design/2026-08-13-tui-core-glue-layer.md。
//!
//! `crates/app` 只应该看到 [`start`] + [`EngineHandle`]/[`BridgeCommand`]——`Agent`/
//! `EventReceiver`/`InputSender` 等 AttaCore 类型完全留在这个 crate 内部。

pub mod bootstrap;
pub mod handle;
pub mod permission;
pub mod reducer;

pub use bootstrap::{BootstrapConfig, BootstrapError};
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
    let (agent, event_rx, input_tx) = bootstrap::build_agent(&config)?;

    let cancel = CancellationToken::new();
    let run_cancel = cancel.clone();
    tokio::spawn(async move {
        let mut agent = agent;
        agent.run(run_cancel).await;
    });

    let (reducer, frame_rx) =
        Reducer::spawn(event_rx, config.model_name.clone(), cwd_display(&config));
    let handle: Arc<dyn EngineHandle> = Arc::new(BridgeHandle::new(input_tx, frame_rx, reducer));

    Ok((handle, cancel))
}

fn cwd_display(config: &BootstrapConfig) -> String {
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| config.local_data_dir.display().to_string())
}

/// 方便 `crates/app` 在渲染前订阅当前快照而不必知道 `watch` 的具体类型别名。
pub type FrameReceiver = tokio::sync::watch::Receiver<FrameState>;
