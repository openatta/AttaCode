//! `EngineHandle` — 命令入口，`crates/app` 与 bridge 之间的唯一边界。

use crate::reducer::Reducer;
use runtime::agent::{
    EngineCommand, InputMessage, InputSender, PermissionDecision as RuntimeDecision, PersistScope,
};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tui::frame_state::{ApprovalOption, CompletionCandidate};
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
    /// `/model <name>` — 换掉这个会话用的模型，下一个 turn 起效。
    SetModel {
        name: String,
    },
    /// app 侧想往转录里写一句提示（用法说明、未知的本地命令……）。
    /// 不碰 Core，纯粹是 app 借 bridge 的转录说话。
    Note {
        text: String,
    },
}

/// bridge 对外暴露的唯一入口：下发命令、订阅渲染快照。
pub trait EngineHandle: Send + Sync {
    fn dispatch(&self, cmd: BridgeCommand) -> Result<(), BridgeError>;
    fn subscribe(&self) -> watch::Receiver<FrameState>;
    /// 当前可用的 slash 命令列表——直接来自 `Agent` 自己那份实时 `CommandRegistry`，
    /// 就是提交后 Core 真正会解析的那一套（见 `crate::commands`）。
    fn subscribe_commands(&self) -> watch::Receiver<Vec<CompletionCandidate>>;
}

/// `EngineHandle` 的具体实现：持有 `InputSender`（转发给 `runtime::Agent`）
/// 与 reducer 侧的 `watch::Sender`（拿它的 receiver 分发给 app）。
pub struct BridgeHandle {
    input_tx: InputSender,
    frame_rx: watch::Receiver<FrameState>,
    commands_rx: watch::Receiver<Vec<CompletionCandidate>>,
    reducer: Arc<Reducer>,
    /// Same token passed to `Agent::run()` — it ends the *whole session*, and is
    /// nothing to do with the interrupt key. Held here so a future "switch
    /// session"/"shut the engine down" command has it; `BridgeCommand::CancelTurn`
    /// goes through `EngineCommand::CancelTurn` instead, which interrupts one turn
    /// and leaves the session usable.
    #[allow(dead_code)]
    cancel: CancellationToken,
}

impl BridgeHandle {
    pub fn new(
        input_tx: InputSender,
        frame_rx: watch::Receiver<FrameState>,
        commands_rx: watch::Receiver<Vec<CompletionCandidate>>,
        reducer: Arc<Reducer>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            input_tx,
            frame_rx,
            commands_rx,
            reducer,
            cancel,
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
                // 三档"允许"是三件不同的事，不能都塌成 `Permit`：后两档要让引擎调
                // `Permission::add_persistent_allow` 落一条真规则，`Local` 还会把它写进
                // 项目的 `settings.local.json`（见 `runtime::turn` 的 `PermitAlways` 分支）。
                // 塌成 `Permit` 的话，用户选了"本会话一直允许"，下一次调用照样弹。
                let runtime_decision = match decision {
                    ApprovalOption::Deny => RuntimeDecision::Deny {
                        reason: "denied by user".into(),
                    },
                    ApprovalOption::PermitOnce => RuntimeDecision::Permit,
                    ApprovalOption::PermitSession => RuntimeDecision::PermitAlways {
                        scope: PersistScope::Session,
                    },
                    ApprovalOption::PermitProject => RuntimeDecision::PermitAlways {
                        scope: PersistScope::Local,
                    },
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
                // 精确取消：打断当前 turn，session 继续活着，下一条消息照常能跑。
                // Core 侧在独占 `input_rx` 的解复用器里处理这条消息——turn 正在跑时
                // 主循环不在 `recv()` 上，放在那里才不会等到 turn 自己结束才生效。
                // 收尾（停 spinner、留一条 note）由随后到达的
                // `TurnComplete { stop_reason: "cancelled" }` 完成。
                self.reducer.request_cancel();
                self.input_tx
                    .send(InputMessage::System {
                        kind: EngineCommand::CancelTurn,
                        content: String::new(),
                    })
                    .map_err(|_| BridgeError::EngineStopped)
            }
            BridgeCommand::SetModel { name } => {
                // 和 `CancelTurn` 不同，这条**不**走解复用器：换模型没道理打断正在跑的
                // turn，排在它后面、下一轮生效才是对的语义。Core 在 `process_turn` 的
                // `InputMessage::System` 分支里调 `Agent::set_model`。
                self.reducer.set_model(name.clone());
                self.input_tx
                    .send(InputMessage::System {
                        kind: EngineCommand::UpdateModel,
                        content: name,
                    })
                    .map_err(|_| BridgeError::EngineStopped)
            }
            BridgeCommand::Note { text } => {
                self.reducer.note(text);
                Ok(())
            }
        }
    }

    fn subscribe(&self) -> watch::Receiver<FrameState> {
        self.frame_rx.clone()
    }

    fn subscribe_commands(&self) -> watch::Receiver<Vec<CompletionCandidate>> {
        self.commands_rx.clone()
    }
}
