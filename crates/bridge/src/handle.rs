//! `EngineHandle` — 命令入口，`crates/app` 与 bridge 之间的唯一边界。

use crate::reducer::Reducer;
use runtime::agent::{
    EngineCommand, InputMessage, InputSender, PermissionDecision as RuntimeDecision, PersistScope,
};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tui::frame_state::{AskOption, PickerCandidate};
use tui::FrameState;

#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("engine is no longer running (input channel closed)")]
    EngineStopped,
}

/// `app` 层可下发给 bridge 的命令集合。风格上与 `runtime::InputMessage` 对齐。
/// 权限决定复用既有 `tui::frame_state::AskOption`，不重复定义一套等价枚举。
#[derive(Debug, Clone)]
pub enum BridgeCommand {
    Submit {
        text: String,
    },
    /// 用户在审批对话框里选了一项。
    ///
    /// 对话框服务两扇门——引擎的权限门和模型的提问（[`crate::ask`]）——所以哪一扇
    /// 由**选项本身**决定，不由命令名决定：`app` 只知道用户选中了第几项，不该也
    /// 不需要知道这一条最后走 `InputMessage` 还是走问答通道。
    Respond {
        prompt_id: String,
        decision: AskOption,
    },
    /// 自由文本那一档的答复：用户提交的一整行。
    ///
    /// 和 `Respond` 分开，是因为它没有对应的 `AskOption`——那个枚举装的是
    /// "可以点的东西"，而这里的答案是打出来的。
    AnswerQuestion {
        prompt_id: String,
        text: String,
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
    /// `/doctor` —— 跑一遍健康检查，把报告写进转录。见 [`crate::doctor`]。
    Doctor,
    /// `/btw <问题>` —— 开一道侧问。空串 = 重开侧问区，停在最近一次问答上。
    /// 见 [`crate::btw`]。
    Btw {
        question: String,
    },
    /// 侧问区里的操作。
    BtwKey(BtwKey),
}

/// 侧问区开着时能按的东西。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtwKey {
    /// 滚动答案，`+1` 往下。
    Scroll(i8),
    /// 往旧翻。
    Older,
    /// 往新翻。
    Newer,
    /// 清空早前问答。
    ClearEarlier,
    /// 退出，回主 UI。
    Close,
}

/// bridge 对外暴露的唯一入口：下发命令、订阅渲染快照。
#[async_trait::async_trait]
pub trait EngineHandle: Send + Sync {
    fn dispatch(&self, cmd: BridgeCommand) -> Result<(), BridgeError>;

    /// `/resume` 的候选列表：本项目的会话，`query` 为空时是最近的几个。
    ///
    /// **这是唯一一个 async 的方法**，因为它是唯一一件要去读盘的事，而读的量随项目
    /// 里的会话数走。做成同步的话，事件循环会在一个几百个会话的项目里卡住画面。
    /// 返回投影过的候选而不是 Core 的 `SessionSummary`——`crates/app` 不许看见
    /// AttaCore 类型。见 [`crate::sessions`]。
    async fn sessions(&self, query: &str) -> Vec<PickerCandidate>;

    fn subscribe(&self) -> watch::Receiver<FrameState>;
    /// 当前可用的 slash 命令列表——直接来自 `Agent` 自己那份实时 `CommandRegistry`，
    /// 就是提交后 Core 真正会解析的那一套（见 `crate::commands`）。
    fn subscribe_commands(&self) -> watch::Receiver<Vec<PickerCandidate>>;

    /// 诊断用：把**即将渲染的那一帧**记进 `ATTACODE_TRACE`（没开就是空操作）。
    /// 默认什么都不做，测试里的假 handle 不必理会。
    fn trace_render(&self, _frame: &FrameState) {}

    /// 诊断用：记一次按键和它解析出来的 action。
    fn trace_key(&self, _key: &str, _outcome: &str) {}
}

/// `BridgeHandle` 要从引擎那边接过来的四样东西。
///
/// 打成一个包而不是四个参数：它们同出一源（`bootstrap::BuiltEngine`），同生共死，
/// 而且都是 `Option`/`Arc` 这类长得很像的类型——摊平成参数列表之后，传错顺序编译器
/// 未必拦得住。
pub struct EngineParts {
    pub input_tx: InputSender,
    /// 模型提问的会合点。见 [`crate::ask`]。
    pub questions: Arc<crate::ask::Questions>,
    /// `/doctor` 问的那份检查表。`None` = 没接引擎（测试）。
    pub health: Option<Arc<base::interface::health::HealthChecks>>,
    /// `/resume` 列会话用。`None` = 这次会话纯内存。
    pub history: Option<Arc<history::store::JsonlHistoryStore>>,
    /// `/btw` 侧问。`None` = 测试里那个不接引擎的 handle。
    pub side_questions: Option<Arc<crate::btw::SideQuestions>>,
}

/// `EngineHandle` 的具体实现：持有 `InputSender`（转发给 `runtime::Agent`）
/// 与 reducer 侧的 `watch::Sender`（拿它的 receiver 分发给 app）。
pub struct BridgeHandle {
    input_tx: InputSender,
    frame_rx: watch::Receiver<FrameState>,
    commands_rx: watch::Receiver<Vec<PickerCandidate>>,
    reducer: Arc<Reducer>,
    /// 模型提问的会合点。答案从这里回到还在 `await` 的那个工具调用上。
    questions: Arc<crate::ask::Questions>,
    /// `/doctor` 的来源。见 [`EngineParts`]。
    health: Option<Arc<base::interface::health::HealthChecks>>,
    /// `/resume` 列表的来源。见 [`EngineParts`]。
    history: Option<Arc<history::store::JsonlHistoryStore>>,
    /// `/btw` 侧问。见 [`crate::btw`]。
    side_questions: Option<Arc<crate::btw::SideQuestions>>,
    /// Same token passed to `Agent::run()` — it ends the *whole session*, and is
    /// nothing to do with the interrupt key. `BridgeCommand::CancelTurn` goes
    /// through `EngineCommand::CancelTurn` instead, which interrupts one turn and
    /// leaves the session usable.
    ///
    /// `/btw` hangs its own requests off a child of this token, so ending the
    /// session ends them too — a side question outliving the engine it borrowed
    /// its context from would be answering about a conversation nobody is in
    /// any more.
    cancel: CancellationToken,
}

impl BridgeHandle {
    pub fn new(
        engine: EngineParts,
        frame_rx: watch::Receiver<FrameState>,
        commands_rx: watch::Receiver<Vec<PickerCandidate>>,
        reducer: Arc<Reducer>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            input_tx: engine.input_tx,
            frame_rx,
            commands_rx,
            reducer,
            questions: engine.questions,
            health: engine.health,
            history: engine.history,
            side_questions: engine.side_questions,
            cancel,
        }
    }

    /// 把答案交回给还在等的那个工具调用，并收走对话框。
    ///
    /// 没人在等**不是错误**：turn 被取消时对话框和等待方是分头消失的（问题先撤走，
    /// 而 app 手上的快照可能还差一拍），用户在那一瞬按下的回车理应落空，而不该让
    /// 整个 UI 报错。`resolve_prompt` 照样跑，它是幂等的——保证屏幕上不会留下一个
    /// 已经没有对家的框。
    ///
    /// **但落空要说出来。** 自由文本那一档不锁 composer，所以那一行是用户真打出来
    /// 的字；既没送到工具、也没当成普通消息发出去，一声不吭地消失是最坏的结果
    /// ——人会以为自己发出去了。
    fn answer_question(&self, prompt_id: String, text: String) -> Result<(), BridgeError> {
        let delivered = self.questions.answer(&prompt_id, text.clone());
        self.reducer.resolve_prompt(&prompt_id);
        if !delivered {
            self.reducer.note(format!(
                "The question was withdrawn before this answer arrived, so it went nowhere: {text}"
            ));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl EngineHandle for BridgeHandle {
    async fn sessions(&self, query: &str) -> Vec<PickerCandidate> {
        match &self.history {
            Some(store) => crate::sessions::candidates(store, query).await,
            // 没有落盘后端就没有可恢复的东西。空列表，不是错误——`/doctor` 那边
            // 会说清楚这次会话是纯内存的。
            None => Vec::new(),
        }
    }

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
            BridgeCommand::Respond {
                prompt_id,
                decision,
            } => {
                // 模型的提问不经过引擎的输入通道——它在等的是 `crate::ask` 里那个
                // oneshot，`InputMessage::PermissionResponse` 那条路上没有它的位置。
                if let AskOption::Answer { key, .. } = decision {
                    return self.answer_question(prompt_id, key);
                }
                // 三档"允许"是三件不同的事，不能都塌成 `Permit`：后两档要让引擎调
                // `Permission::add_persistent_allow` 落一条真规则（见 `runtime::turn`
                // 的 `PermitAlways` 分支）。塌成 `Permit` 的话，用户选了"本会话一直
                // 允许"，下一次调用照样弹。
                let runtime_decision = match decision {
                    AskOption::Deny => RuntimeDecision::Deny {
                        reason: "denied by user".into(),
                    },
                    AskOption::PermitOnce => RuntimeDecision::Permit,
                    AskOption::PermitSession => RuntimeDecision::PermitAlways {
                        scope: PersistScope::Session,
                    },
                    AskOption::PermitProject => RuntimeDecision::PermitAlways {
                        scope: PersistScope::Local,
                    },
                    // 上面刚 return 过。
                    AskOption::Answer { .. } => unreachable!(),
                };
                self.reducer.resolve_prompt(&prompt_id);
                self.input_tx
                    .send(InputMessage::PermissionResponse {
                        prompt_id,
                        decision: runtime_decision,
                    })
                    .map_err(|_| BridgeError::EngineStopped)
            }
            BridgeCommand::AnswerQuestion { prompt_id, text } => {
                self.answer_question(prompt_id, text)
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
            // 侧问区里的键。这些都是纯本地状态，不经引擎——所以侧问区开着时主 turn
            // 照常在跑，一点没被打扰。
            BridgeCommand::BtwKey(key) => {
                match key {
                    BtwKey::Scroll(delta) => self.reducer.btw_scroll(delta as isize),
                    BtwKey::Older => self.reducer.btw_step(true),
                    BtwKey::Newer => self.reducer.btw_step(false),
                    BtwKey::ClearEarlier => {
                        if let Some(side) = &self.side_questions {
                            side.clear();
                        }
                        self.reducer.btw_clear_earlier();
                    }
                    // 关掉 = 不要了。在途那次调用照着跑完的话，用户按了 Esc 之后
                    // 还在替他烧 token，答完了还进 `exchanges` —— 而他已经不想要
                    // 这个答案了。见 `SideQuestions::abandon`。
                    BtwKey::Close => {
                        if let Some(side) = &self.side_questions {
                            side.abandon();
                        }
                        self.reducer.btw_close();
                    }
                }
                Ok(())
            }
            BridgeCommand::Btw { question } => {
                let Some(side) = self.side_questions.clone() else {
                    self.reducer
                        .note("/btw: no engine is attached to this session".into());
                    return Ok(());
                };
                let question = question.trim().to_string();
                if question.is_empty() {
                    // 不带问题 = 重开侧问区，停在最近一次问答上（照 CC）。
                    let earlier = side.exchanges();
                    match earlier.last().cloned() {
                        Some(last) => {
                            let head = earlier[..earlier.len() - 1].to_vec();
                            let generation = self.reducer.btw_open(last.question, head);
                            self.reducer.btw_done(generation, Some(last.answer));
                        }
                        None => self
                            .reducer
                            .note("/btw: nothing asked yet — try `/btw <question>`".into()),
                    }
                    return Ok(());
                }

                // 立刻上屏（带着"在想"），答案随后流式填进来。**这一步不 await**——
                // 侧问跑在自己的 task 上，主 turn 一点都不等它。
                //
                // `generation` 一路带进回调：关掉侧问区**不**取消已经发出去的那次请求，
                // 所以用户 Esc 之后马上再问一句时，上一问的 task 还在往回吐字。不带
                // 编号的话那些字会落进这一问的答案框里。见 `Reducer::btw_delta`。
                let generation = self.reducer.btw_open(question.clone(), side.exchanges());
                let reducer = self.reducer.clone();
                let cancel = self.cancel.child_token();
                tokio::spawn(async move {
                    let outcome = side
                        .ask(&question, cancel, |delta| {
                            reducer.btw_delta(generation, delta)
                        })
                        .await;
                    // 失败也要说出来。一个答不出来的侧问必须说自己答不出来，
                    // 而不是留一个永远"在想"的空框。
                    reducer.btw_done(generation, outcome.err());
                });
                Ok(())
            }
            BridgeCommand::Doctor => {
                let text = match &self.health {
                    Some(health) => crate::doctor::render(&health.report()),
                    None => "doctor: no engine is attached to this session".to_string(),
                };
                self.reducer.note(text);
                Ok(())
            }
        }
    }

    fn subscribe(&self) -> watch::Receiver<FrameState> {
        self.frame_rx.clone()
    }

    fn subscribe_commands(&self) -> watch::Receiver<Vec<PickerCandidate>> {
        self.commands_rx.clone()
    }

    fn trace_render(&self, frame: &FrameState) {
        self.reducer.trace_render(frame);
    }

    fn trace_key(&self, key: &str, outcome: &str) {
        self.reducer.trace_key(key, outcome);
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
        let (h, rx, frame, _q) = handle_with_questions();
        (h, rx, frame)
    }

    #[allow(clippy::type_complexity)]
    fn handle_with_questions() -> (
        BridgeHandle,
        InputReceiver,
        watch::Receiver<FrameState>,
        Arc<crate::ask::Questions>,
    ) {
        let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel();
        let (reducer, frame_rx) = Reducer::build_for_test();
        let (commands_tx, commands_rx) = watch::channel(Vec::new());
        // 发送端得活到测试结束，否则接收端立刻废掉。
        std::mem::forget(commands_tx);
        let (questions, questions_rx) = crate::ask::Questions::new();
        std::mem::forget(questions_rx);
        let handle = BridgeHandle::new(
            EngineParts {
                input_tx,
                questions: questions.clone(),
                health: None,
                history: None,
                side_questions: None,
            },
            frame_rx.clone(),
            commands_rx,
            reducer,
            CancellationToken::new(),
        );
        (handle, input_rx, frame_rx, questions)
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
    fn each_permission_answer_maps_to_its_own_runtime_decision() {
        let cases = [
            (AskOption::PermitOnce, "Permit"),
            (AskOption::PermitSession, "PermitAlways(Session)"),
            (AskOption::PermitProject, "PermitAlways(Local)"),
            (AskOption::Deny, "Deny"),
        ];
        for (option, expected) in cases {
            let (handle, mut rx, _frame) = handle();
            handle
                .dispatch(BridgeCommand::Respond {
                    prompt_id: "p1".into(),
                    decision: option.clone(),
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

    /// 模型的提问和权限提问共用一个对话框，但答案去的是**两个不同的地方**。
    /// 走错门的症状是静默的：工具那边永远等不到答案，而引擎那边收到一个它没在等的
    /// `PermissionResponse`。
    #[tokio::test]
    async fn an_answer_to_the_model_never_reaches_the_permission_channel() {
        let (handle, mut rx, _frame, questions) = handle_with_questions();
        let asker = questions.clone();
        let waiting = tokio::spawn(async move {
            asker
                .ask(crate::ask::PendingQuestion {
                    id: "t1".into(),
                    header: "H".into(),
                    question: "which?".into(),
                    options: vec![("a".into(), "A".into())],
                })
                .await
        });
        // 先让提问方把自己挂进等待表。
        while !questions.is_waiting("t1") {
            tokio::task::yield_now().await;
        }

        handle
            .dispatch(BridgeCommand::Respond {
                prompt_id: "t1".into(),
                decision: AskOption::Answer {
                    key: "a".into(),
                    label: "A".into(),
                },
            })
            .unwrap();

        assert_eq!(waiting.await.unwrap().as_deref(), Some("a"));
        assert!(
            drain(&mut rx).is_empty(),
            "模型的答案不该变成一条 InputMessage"
        );
    }

    /// 自由文本那一档同理，只是命令不同。
    #[tokio::test]
    async fn a_typed_answer_reaches_the_asker() {
        let (handle, mut rx, _frame, questions) = handle_with_questions();
        let asker = questions.clone();
        let waiting = tokio::spawn(async move {
            asker
                .ask(crate::ask::PendingQuestion {
                    id: "t2".into(),
                    header: "H".into(),
                    question: "叫什么？".into(),
                    options: Vec::new(),
                })
                .await
        });
        while !questions.is_waiting("t2") {
            tokio::task::yield_now().await;
        }

        handle
            .dispatch(BridgeCommand::AnswerQuestion {
                prompt_id: "t2".into(),
                text: "feat/ask".into(),
            })
            .unwrap();

        assert_eq!(waiting.await.unwrap().as_deref(), Some("feat/ask"));
        assert!(drain(&mut rx).is_empty());
    }

    /// 没人在等的答案（问题刚被撤走、用户手慢了一步）不该把整个 UI 弄出错——
    /// **但也不能一声不吭地消失**。
    ///
    /// 自由文本那一档不锁 composer，所以那一行是用户真打出来的字。既没送到工具、
    /// 也没当成普通消息发出去、转录里还什么都没有的话，人会以为自己发出去了。
    #[test]
    fn an_answer_that_arrives_too_late_is_reported_instead_of_vanishing() {
        let (handle, _rx, frame_rx) = handle();
        assert!(handle
            .dispatch(BridgeCommand::AnswerQuestion {
                prompt_id: "gone".into(),
                text: "我打了半天的这一行".into(),
            })
            .is_ok());

        let said = frame_rx.borrow().transcript.body.entries.clone();
        assert!(
            said.iter().any(|e| e.text.contains("我打了半天的这一行")),
            "落空的答案得留在转录里让人看见: {said:?}"
        );
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
            BridgeCommand::Respond {
                prompt_id: "p".into(),
                decision: AskOption::Deny,
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

    // ── /btw 侧问区 ──

    /// 一个装着侧问的 handle。`side` 那一半由 `crate::btw::stub` 提供。
    fn handle_with_side(
        side: Arc<crate::btw::SideQuestions>,
    ) -> (BridgeHandle, watch::Receiver<FrameState>) {
        let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel();
        // 引擎那头没人收，但 `/btw` 一条都不往那儿发——留着别让 sender 立刻废掉。
        std::mem::forget(input_rx);
        let (reducer, frame_rx) = Reducer::build_for_test();
        let (commands_tx, commands_rx) = watch::channel(Vec::new());
        std::mem::forget(commands_tx);
        let (questions, questions_rx) = crate::ask::Questions::new();
        std::mem::forget(questions_rx);
        let handle = BridgeHandle::new(
            EngineParts {
                input_tx,
                questions,
                health: None,
                history: None,
                side_questions: Some(side),
            },
            frame_rx.clone(),
            commands_rx,
            reducer,
            CancellationToken::new(),
        );
        (handle, frame_rx)
    }

    /// 等到某一帧满足条件为止。侧问跑在自己的 task 上，所以帧是异步到的。
    async fn until(
        rx: &mut watch::Receiver<FrameState>,
        mut ready: impl FnMut(&FrameState) -> bool,
    ) -> FrameState {
        let wait = async {
            loop {
                {
                    let frame = rx.borrow_and_update();
                    if ready(&frame) {
                        return frame.clone();
                    }
                }
                rx.changed().await.expect("归约器还活着");
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), wait)
            .await
            .expect("等超时了")
    }

    /// 问一句：立刻上屏（带着"在想"），答案随后流式填进来，最后收掉"在想"。
    #[tokio::test]
    async fn a_side_question_lands_on_screen_and_then_fills_in() {
        let side = crate::btw::stub::ready("叫 settings.json");
        let (handle, mut frame_rx) = handle_with_side(side.clone());

        handle
            .dispatch(BridgeCommand::Btw {
                question: "那个配置文件叫什么".into(),
            })
            .unwrap();

        let frame = until(&mut frame_rx, |f| {
            f.btw.as_ref().is_some_and(|b| !b.streaming)
        })
        .await;
        let btw = frame.btw.unwrap();
        assert_eq!(btw.question, "那个配置文件叫什么");
        assert_eq!(btw.answer, "叫 settings.json");
        assert_eq!(side.exchanges().len(), 1, "答完了要记进早前问答");
    }

    /// `/btw` 不带问题、而且一句都还没问过：说清楚该怎么用，别开一个空框。
    #[tokio::test]
    async fn an_empty_btw_with_nothing_asked_yet_says_how_to_ask() {
        let side = crate::btw::stub::ready("x");
        let (handle, frame_rx) = handle_with_side(side);

        handle
            .dispatch(BridgeCommand::Btw {
                question: "  ".into(),
            })
            .unwrap();

        let frame = frame_rx.borrow();
        assert!(frame.btw.is_none(), "没东西可看就别开这个区域");
        assert!(
            frame
                .transcript
                .body
                .entries
                .iter()
                .any(|e| e.text.contains("nothing asked yet")),
            "得说一句，否则用户以为命令没生效"
        );
    }

    /// `/btw` 不带问题 = 重开侧问区，停在最近一次问答上（照 CC）。
    #[tokio::test]
    async fn an_empty_btw_reopens_the_last_exchange() {
        let side = crate::btw::stub::ready("答案一");
        let (handle, mut frame_rx) = handle_with_side(side);

        handle
            .dispatch(BridgeCommand::Btw {
                question: "问题一".into(),
            })
            .unwrap();
        until(&mut frame_rx, |f| {
            f.btw.as_ref().is_some_and(|b| !b.streaming)
        })
        .await;
        handle
            .dispatch(BridgeCommand::BtwKey(BtwKey::Close))
            .unwrap();

        handle
            .dispatch(BridgeCommand::Btw {
                question: String::new(),
            })
            .unwrap();
        let btw = frame_rx.borrow().btw.clone().expect("该重新开出来");
        assert_eq!(btw.question, "问题一");
        assert_eq!(btw.answer, "答案一");
        assert!(!btw.streaming, "翻出来的旧问答不该显示成还在等模型");
        assert!(
            btw.earlier.is_empty(),
            "它自己就是最近那条，不该又列自己一遍"
        );
    }

    /// `x` 得把两处状态一起清掉：屏幕上那份，和下一问要带上去的那份。只清一处的话，
    /// 用户看着清空了，下一问照样把它们发出去。
    #[tokio::test]
    async fn clearing_earlier_exchanges_clears_both_copies() {
        let side = crate::btw::stub::ready("答案");
        let (handle, mut frame_rx) = handle_with_side(side.clone());

        for q in ["问题一", "问题二"] {
            handle
                .dispatch(BridgeCommand::Btw { question: q.into() })
                .unwrap();
            until(&mut frame_rx, |f| {
                f.btw.as_ref().is_some_and(|b| !b.streaming)
            })
            .await;
        }
        assert_eq!(side.exchanges().len(), 2);
        assert!(!frame_rx.borrow().btw.as_ref().unwrap().earlier.is_empty());

        handle
            .dispatch(BridgeCommand::BtwKey(BtwKey::ClearEarlier))
            .unwrap();
        assert!(side.exchanges().is_empty(), "下一问要带的那份没清");
        assert!(
            frame_rx.borrow().btw.as_ref().unwrap().earlier.is_empty(),
            "屏幕上那份没清"
        );
    }

    /// 关掉侧问区 = 不要在途那一问了。它照着跑完的话，用户按了 Esc 之后还在替他
    /// 烧 token，答完还进早前问答——而他已经不想要这个答案了。
    #[tokio::test]
    async fn closing_the_panel_abandons_the_question_still_in_flight() {
        let (side, dropped) = crate::btw::stub::never_ends_watched("答到一半");
        let (handle, mut frame_rx) = handle_with_side(side.clone());

        handle
            .dispatch(BridgeCommand::Btw {
                question: "问一句".into(),
            })
            .unwrap();
        until(&mut frame_rx, |f| {
            f.btw.as_ref().is_some_and(|b| !b.answer.is_empty())
        })
        .await;

        handle
            .dispatch(BridgeCommand::BtwKey(BtwKey::Close))
            .unwrap();
        assert!(frame_rx.borrow().btw.is_none());

        // 收尾是那个 task 自己做的，给它跑完的机会再看它留下了什么。
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert!(
            dropped.load(std::sync::atomic::Ordering::SeqCst),
            "那次请求得真的断掉 —— 断开的动作就是把 stream 丢掉"
        );
        assert!(
            side.exchanges().is_empty(),
            "被中断的那一问不该留下半截答案"
        );
    }
}
