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

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::agent::InputReceiver;

    /// 一个不接 `Agent` 的 handle——`InputSender` 的另一头攥在测试手里，
    /// 于是能直接断言"这条命令最后变成了哪条 `InputMessage`"。
    ///
    /// 这一层以前一个测试都没有，`dispatch` 里那张映射表全靠人眼。变异测试实测：
    /// 把"本会话一直允许"塌成一次性、把取消发成 Shutdown、`/model` 不带模型名——
    /// 三个都不被发现，而且三个都是"屏幕上看着生效了、实际没有"的静默错误。
    fn handle() -> (BridgeHandle, InputReceiver, watch::Receiver<FrameState>) {
        let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel();
        let (reducer, frame_rx) = Reducer::build_for_test();
        let (commands_tx, commands_rx) = watch::channel(Vec::new());
        // 发送端得活到测试结束，否则接收端立刻废掉。
        std::mem::forget(commands_tx);
        let handle = BridgeHandle::new(
            input_tx,
            frame_rx.clone(),
            commands_rx,
            reducer,
            CancellationToken::new(),
        );
        (handle, input_rx, frame_rx)
    }

    fn drain(rx: &mut InputReceiver) -> Vec<InputMessage> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            out.push(msg);
        }
        out
    }

    #[test]
    fn submit_becomes_a_user_message_and_echoes_into_the_transcript() {
        let (handle, mut rx, frame_rx) = handle();
        handle
            .dispatch(BridgeCommand::Submit {
                text: "你好".into(),
            })
            .unwrap();

        let msgs = drain(&mut rx);
        assert!(
            matches!(&msgs[..], [InputMessage::User { content, turn_id, .. }]
                     if content == "你好" && !turn_id.is_empty()),
            "got: {msgs:?}"
        );
        let entries = &frame_rx.borrow().transcript.body.entries;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "你好");
    }

    /// 三档"允许"是三件不同的事。塌成 `Permit` 的话，用户选了"本会话一直允许"，
    /// 下一次调用照样弹——设计文档专门点名的坑。
    #[test]
    fn each_approval_option_maps_to_its_own_runtime_decision() {
        let cases = [
            (ApprovalOption::PermitOnce, "Permit"),
            (ApprovalOption::PermitSession, "PermitAlways(Session)"),
            (ApprovalOption::PermitProject, "PermitAlways(Local)"),
            (ApprovalOption::Deny, "Deny"),
        ];
        for (option, expected) in cases {
            let (handle, mut rx, _frame) = handle();
            handle
                .dispatch(BridgeCommand::RespondPermission {
                    prompt_id: "p1".into(),
                    decision: option,
                })
                .unwrap();
            let msgs = drain(&mut rx);
            let InputMessage::PermissionResponse {
                prompt_id,
                decision,
            } = &msgs[0]
            else {
                panic!("expected a PermissionResponse, got: {msgs:?}");
            };
            assert_eq!(prompt_id, "p1");
            let actual = match decision {
                RuntimeDecision::Permit => "Permit".to_string(),
                RuntimeDecision::Deny { .. } => "Deny".to_string(),
                RuntimeDecision::PermitAlways { scope } => format!("PermitAlways({scope:?})"),
            };
            assert_eq!(actual, expected, "{option:?} 映射错了");
        }
    }

    /// 中断一次 turn ≠ 关掉会话。发成 `Shutdown` 的话用户按一次 Ctrl+C 引擎就没了。
    #[test]
    fn cancel_sends_cancel_turn_not_shutdown() {
        let (handle, mut rx, _frame) = handle();
        handle.dispatch(BridgeCommand::CancelTurn).unwrap();
        let msgs = drain(&mut rx);
        assert!(
            matches!(
                &msgs[..],
                [InputMessage::System {
                    kind: EngineCommand::CancelTurn,
                    ..
                }]
            ),
            "got: {msgs:?}"
        );
    }

    /// `/model` 必须把名字**发给 Core**。只更状态栏的话，屏幕说换了、引擎没换。
    #[test]
    fn set_model_sends_the_name_to_core_and_updates_the_footer() {
        let (handle, mut rx, frame_rx) = handle();
        handle
            .dispatch(BridgeCommand::SetModel {
                name: "claude-sonnet-5".into(),
            })
            .unwrap();

        let msgs = drain(&mut rx);
        assert!(
            matches!(&msgs[..], [InputMessage::System { kind: EngineCommand::UpdateModel, content }]
                     if content == "claude-sonnet-5"),
            "got: {msgs:?}"
        );
        assert_eq!(frame_rx.borrow().footer_hints.model, "claude-sonnet-5");
    }

    /// 展开/折叠和往转录写提示都是**本地**动作，一个字节都不该发给引擎。
    #[test]
    fn local_only_commands_do_not_reach_the_engine() {
        let (handle, mut rx, frame_rx) = handle();
        handle
            .dispatch(BridgeCommand::ToggleExpand {
                block_id: "t1".into(),
            })
            .unwrap();
        handle
            .dispatch(BridgeCommand::Note {
                text: "提示".into(),
            })
            .unwrap();

        assert!(drain(&mut rx).is_empty(), "这两条不该发给 Core");
        assert!(frame_rx
            .borrow()
            .transcript
            .body
            .entries
            .iter()
            .any(|e| e.text == "提示"));
    }

    /// 引擎那头没了以后，每条要过 Core 的命令都得报错而不是假装成功——
    /// app 靠这个错误决定是不是该收摊。
    #[test]
    fn dispatch_reports_a_dead_engine() {
        let (handle, rx, _frame) = handle();
        drop(rx);
        for cmd in [
            BridgeCommand::Submit { text: "x".into() },
            BridgeCommand::CancelTurn,
            BridgeCommand::SetModel { name: "m".into() },
            BridgeCommand::RespondPermission {
                prompt_id: "p".into(),
                decision: ApprovalOption::Deny,
            },
        ] {
            assert!(
                matches!(
                    handle.dispatch(cmd.clone()),
                    Err(BridgeError::EngineStopped)
                ),
                "{cmd:?} 应该报 EngineStopped"
            );
        }
        // 纯本地的两条不经过引擎，照样成功。
        assert!(handle
            .dispatch(BridgeCommand::Note { text: "x".into() })
            .is_ok());
    }
}
