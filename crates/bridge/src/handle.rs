//! `EngineHandle` — 命令入口，`crates/app` 与 bridge 之间的唯一边界。

use crate::reducer::Reducer;
use runtime::agent::{InputMessage, InputSender, PermissionDecision as RuntimeDecision};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::watch;
use tui::frame_state::ApprovalOption;
use tui::FrameState;

#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("engine is no longer running (input channel closed)")]
    EngineStopped,
    #[error("no pending approval with prompt_id {0}")]
    UnknownPrompt(String),
}

/// `app` 层可下发给 bridge 的命令集合。风格上与 `runtime::InputMessage` 对齐。
/// 权限决定复用既有 `tui::frame_state::ApprovalOption`，不重复定义一套等价枚举。
#[derive(Debug, Clone)]
pub enum BridgeCommand {
    Submit {
        text: String,
    },
    RespondPermission {
        prompt_id: String,
        decision: ApprovalOption,
    },
    ToggleExpand {
        block_id: String,
    },
    CancelTurn,
}

/// bridge 对外暴露的唯一入口：下发命令、订阅渲染快照。
pub trait EngineHandle: Send + Sync {
    fn dispatch(&self, cmd: BridgeCommand) -> Result<(), BridgeError>;
    fn subscribe(&self) -> watch::Receiver<FrameState>;
}

/// `EngineHandle` 的具体实现：持有 `InputSender`（转发给 `runtime::Agent`）
/// 与 reducer 侧的 `watch::Sender`（拿它的 receiver 分发给 app）。
pub struct BridgeHandle {
    input_tx: InputSender,
    frame_rx: watch::Receiver<FrameState>,
    reducer: Arc<Reducer>,
}

impl BridgeHandle {
    pub fn new(
        input_tx: InputSender,
        frame_rx: watch::Receiver<FrameState>,
        reducer: Arc<Reducer>,
    ) -> Self {
        Self {
            input_tx,
            frame_rx,
            reducer,
        }
    }
}

impl EngineHandle for BridgeHandle {
    fn dispatch(&self, cmd: BridgeCommand) -> Result<(), BridgeError> {
        match cmd {
            BridgeCommand::Submit { text } => {
                let turn_id = self.reducer.begin_turn(text.clone());
                self.input_tx
                    .send(InputMessage::User {
                        content: text,
                        attachments: Vec::new(),
                        turn_id,
                    })
                    .map_err(|_| BridgeError::EngineStopped)
            }
            BridgeCommand::RespondPermission {
                prompt_id,
                decision,
            } => {
                let runtime_decision = match decision {
                    ApprovalOption::Deny => RuntimeDecision::Deny {
                        reason: "denied by user".into(),
                    },
                    ApprovalOption::PermitOnce
                    | ApprovalOption::PermitSession
                    | ApprovalOption::PermitProject => RuntimeDecision::Permit,
                };
                self.reducer.resolve_prompt(&prompt_id);
                self.input_tx
                    .send(InputMessage::PermissionResponse {
                        prompt_id,
                        decision: runtime_decision,
                    })
                    .map_err(|_| BridgeError::EngineStopped)
            }
            BridgeCommand::ToggleExpand { block_id } => {
                self.reducer.toggle_expand(&block_id);
                Ok(())
            }
            BridgeCommand::CancelTurn => {
                // TS parity 的 cancel 走 CancellationToken，不经过 InputMessage；
                // bridge 目前没有持有 cancel token 的写口，留给后续任务接入。
                Ok(())
            }
        }
    }

    fn subscribe(&self) -> watch::Receiver<FrameState> {
        self.frame_rx.clone()
    }
}
