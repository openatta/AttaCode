//! `AskUserQuestion` 的"人"那一端 —— 模型提问，TUI 去问，答案原路回去。
//!
//! # 为什么需要这个模块
//!
//! AttaCore 0.2.0 之前，`AskUserQuestion` 把问题原样当成结果回给模型（模型于是
//! 把自己的回声读成回答，继续往下跑）。0.2.0 把它改成走
//! [`base::interface::elicitation::Elicitation`] 契约：真去问人，问不到就明说
//! "用户没被问到"。
//!
//! 对 AttaCode 的直接后果是：这个工具**注册着但答不了**。`Builder` 的默认实现
//! `runtime::elicitation::ChannelElicitation` 只认权限提问一种，`Clarification`
//! 一律拒绝——它自己的文档写着"这个宿主只接了权限提问"。而 TUI 恰恰是那个够得着
//! 人的宿主。
//!
//! # 为什么不去实现 `Elicitation`
//!
//! 那才是这件事的正门，我们走不进去。`Builder::elicitation` 是**整体替换**：换掉
//! 之后权限提问也归我们。而权限提问那条链的另一半在引擎内部——
//! `InputMessage::PermissionResponse` 由 `Agent` 的解复用循环投递进它自己的
//! `pending_permissions`（`pub(crate)`），子代理的权限还要经
//! `AgentTool::set_parent_pending_permissions` 转发上来。我们够不着那张表，也没法
//! 只替换其中一半：结果是为了修好提问，弄坏了审批。
//!
//! 所以这里走的是**换工具**那条路：`ToolRegistry::replace` 就是为这件事存在的
//! （"替换同名条目"，而 `register` 是追加、`find` 取第一个匹配）。模型看见的那一面
//! ——名字、描述、schema、prompt、校验、权限——全部委派给 Core 那个工具本身，所以
//! 不会跟着上游漂；这里改写的只有 `call`。
//!
//! 正门什么时候能走：Core 让 `ChannelElicitation` 可以被组合（暴露它的 pending 表，
//! 或者让 `Builder` 接一个"只管非授权类"的 `Elicitation`）之后。记在 `scripts/` 的
//! patch 规格里。
//!
//! # 两种问法
//!
//! `options` 非空是多选题，走审批对话框（composer 锁住，方向键 + 回车）。`options`
//! 为空是自由文本题，Core 的 schema 明说这一档存在——这时对话框只显示问题，composer
//! 保持可编辑，用户提交的下一行就是答案（**不**当成新一轮对话发给引擎）。

use base::error::ToolError;
use base::tool::{
    InterruptBehavior, PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext,
    ToolResult, ValidationResult,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tools::ask_user::{AskUserQuestionInput, AskUserQuestionTool};

/// 一个正在等人回答的问题。
#[derive(Debug, Clone)]
pub struct PendingQuestion {
    /// 路由用。取 `tool_use_id`，和 Core 的 `ElicitRequest::id` 同源。
    pub id: String,
    /// 对话框标题。模型不给就退回工具名。
    pub header: String,
    pub question: String,
    /// `(key, label)`。空 = 自由文本题。`key` 是模型自己起的，要原样还给它。
    pub options: Vec<(String, String)>,
}

/// 归约器要听的两件事。
#[derive(Debug, Clone)]
pub enum QuestionEvent {
    Ask(PendingQuestion),
    /// 提问方不等了（turn 被取消、会话关掉），把对话框收走。
    ///
    /// 没有这条的话，一个被取消的 turn 会在屏幕上留下一个永远等不到人接的对话框，
    /// 而它一旦占着 composer，用户连输入都做不了。Core 在自己那份
    /// `ChannelElicitation` 里把注册绑在 ask 的生命周期上，正是同一个理由。
    Withdraw(String),
}

/// 提问方（引擎里的工具）与回答方（TUI）之间的会合点。
pub struct Questions {
    tx: mpsc::UnboundedSender<QuestionEvent>,
    pending: Mutex<HashMap<String, oneshot::Sender<String>>>,
}

impl Questions {
    /// 返回会合点本身，以及归约器要消费的那条流。
    pub fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<QuestionEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                tx,
                pending: Mutex::new(HashMap::new()),
            }),
            rx,
        )
    }

    /// 把问题送到屏幕上并等答案。`None` = 没人回答（对话框被撤、宿主先走了）。
    pub async fn ask(self: &Arc<Self>, q: PendingQuestion) -> Option<String> {
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(q.id.clone(), tx);
        // 先注册再发问：答案可能在 `send` 返回之前就到了。
        let guard = AskGuard {
            id: q.id.clone(),
            questions: self.clone(),
        };
        if self.tx.send(QuestionEvent::Ask(q)).is_err() {
            return None;
        }
        let answer = rx.await.ok();
        drop(guard);
        answer
    }

    /// 用户答了。`false` = 这个 id 已经不在等了（问题被撤走，或者答重了）。
    pub fn answer(&self, id: &str, text: String) -> bool {
        let sender = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
        match sender {
            Some(tx) => tx.send(text).is_ok(),
            None => false,
        }
    }

    /// 还有没有人在等这个 id。给归约器判重用。
    pub fn is_waiting(&self, id: &str) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(id)
    }
}

/// 把注册绑在 `ask` 的生命周期上：调用方不等了（future 被 drop）就同时撤掉注册和
/// 屏幕上的对话框。绑在生命周期上而不是在每个放弃分支里手写一遍，是因为放弃的
/// 理由会变多，而漏掉一处的代价是一个卡住 composer 的幽灵对话框。
struct AskGuard {
    id: String,
    questions: Arc<Questions>,
}

impl Drop for AskGuard {
    fn drop(&mut self) {
        let still_waiting = self
            .questions
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id)
            .is_some();
        // 已经答过就什么都不用做——对话框那边收到答案时自己就撤了。
        if still_waiting {
            let _ = self
                .questions
                .tx
                .send(QuestionEvent::Withdraw(self.id.clone()));
        }
    }
}

/// 会问人的那版 `AskUserQuestion`。
///
/// 除了 `call`，每一个方法都委派给 Core 的 [`AskUserQuestionTool`]——模型看见的
/// 那一面（名字/描述/schema/prompt/校验/权限）不该在这里有第二份定义，否则上游
/// 一改就悄悄漂了。
pub struct TuiAskUserQuestion {
    inner: AskUserQuestionTool,
    questions: Arc<Questions>,
}

impl TuiAskUserQuestion {
    pub fn new(questions: Arc<Questions>) -> Self {
        Self {
            inner: AskUserQuestionTool,
            questions,
        }
    }
}

#[async_trait::async_trait]
impl Tool for TuiAskUserQuestion {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn input_schema(&self) -> Value {
        self.inner.input_schema()
    }
    fn source(&self) -> std::borrow::Cow<'_, str> {
        self.inner.source()
    }
    async fn prompt(&self, ctx: &PromptContext) -> String {
        self.inner.prompt(ctx).await
    }
    fn prompt_fragment(&self) -> String {
        self.inner.prompt_fragment()
    }
    fn self_allow_overrides_mode(&self) -> bool {
        self.inner.self_allow_overrides_mode()
    }
    async fn detailed_prompt(&self, ctx: &PromptContext) -> Option<String> {
        self.inner.detailed_prompt(ctx).await
    }
    fn is_enabled(&self) -> bool {
        self.inner.is_enabled()
    }
    fn is_read_only(&self, input: &Value) -> bool {
        self.inner.is_read_only(input)
    }
    fn is_concurrency_safe(&self, input: &Value) -> bool {
        self.inner.is_concurrency_safe(input)
    }
    fn is_destructive(&self, input: &Value) -> bool {
        self.inner.is_destructive(input)
    }
    fn strict(&self) -> bool {
        self.inner.strict()
    }
    fn is_deferred(&self) -> bool {
        self.inner.is_deferred()
    }
    fn is_dynamic(&self) -> bool {
        self.inner.is_dynamic()
    }
    fn is_direct(&self) -> bool {
        self.inner.is_direct()
    }
    fn short_description(&self) -> Option<String> {
        self.inner.short_description()
    }
    fn permission_match_content(&self, input: &Value) -> Option<String> {
        self.inner.permission_match_content(input)
    }
    fn affected_paths(&self, input: &Value) -> Vec<PathBuf> {
        self.inner.affected_paths(input)
    }
    fn interrupt_behavior(&self, input: &Value) -> InterruptBehavior {
        self.inner.interrupt_behavior(input)
    }
    async fn validate_input(&self, input: &Value, ctx: &ToolContext) -> ValidationResult {
        self.inner.validate_input(input, ctx).await
    }
    async fn check_permissions(&self, input: &Value, ctx: &ToolContext) -> PermissionDecision {
        self.inner.check_permissions(input, ctx).await
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let parsed: AskUserQuestionInput = serde_json::from_value(input)?;
        // 和 Core 同形的 structured_content：自己画对话框的宿主读的是这个，答没答上
        // 都带着。我们**就是**那个宿主，但转录/回放的读者不是，所以照样带上。
        let rendered = json!({
            "question": parsed.question,
            "header": parsed.header,
            "options": parsed.options.iter()
                .map(|o| json!({"key": o.key, "label": o.label}))
                .collect::<Vec<_>>(),
        });

        let answer = self
            .questions
            .ask(PendingQuestion {
                id: ctx.tool_use_id.clone(),
                header: parsed
                    .header
                    .clone()
                    .filter(|h| !h.trim().is_empty())
                    .unwrap_or_else(|| "Question".into()),
                question: parsed.question.clone(),
                options: parsed
                    .options
                    .iter()
                    .map(|o| (o.key.clone(), o.label.clone()))
                    .collect(),
            })
            .await;

        // 没答上就必须读起来像没答上。把问题回显给模型（0.2.0 之前就是这么干的）
        // 会让它把回声当成回答——这正是上游改掉它的理由，不要在这里复活。
        let text = match answer {
            Some(a) => a,
            None => "The user was not asked and has not answered: the question was \
                     withdrawn before anyone could answer it (the turn was cancelled, \
                     or the session ended)."
                .to_string(),
        };

        Ok(ToolResult {
            content: base::tool::ToolResultContent::Text(text),
            is_error: false,
            structured_content: Some(rendered),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(id: &str, options: &[(&str, &str)]) -> PendingQuestion {
        PendingQuestion {
            id: id.into(),
            header: "H".into(),
            question: "which?".into(),
            options: options
                .iter()
                .map(|(k, l)| (k.to_string(), l.to_string()))
                .collect(),
        }
    }

    #[tokio::test]
    async fn an_answer_comes_back_to_the_asker() {
        let (questions, mut rx) = Questions::new();
        let asker = questions.clone();
        let ask = tokio::spawn(async move { asker.ask(q("t1", &[("a", "A")])).await });

        let QuestionEvent::Ask(shown) = rx.recv().await.unwrap() else {
            panic!("the question must reach the reducer first");
        };
        assert_eq!(shown.id, "t1");
        assert!(questions.answer("t1", "a".into()));

        assert_eq!(ask.await.unwrap().as_deref(), Some("a"));
    }

    /// 自由文本题走的是同一条路，只是选项为空。
    #[tokio::test]
    async fn a_free_form_question_carries_no_options() {
        let (questions, mut rx) = Questions::new();
        let asker = questions.clone();
        let ask = tokio::spawn(async move { asker.ask(q("t2", &[])).await });

        let QuestionEvent::Ask(shown) = rx.recv().await.unwrap() else {
            panic!("expected an Ask");
        };
        assert!(shown.options.is_empty());
        questions.answer("t2", "自己写的答案".into());
        assert_eq!(ask.await.unwrap().as_deref(), Some("自己写的答案"));
    }

    /// 提问方放弃时对话框必须跟着消失，否则它会一直占着 composer。
    #[tokio::test]
    async fn giving_up_withdraws_the_dialog_and_stops_waiting() {
        let (questions, mut rx) = Questions::new();
        let asker = questions.clone();
        let ask = tokio::spawn(async move { asker.ask(q("t3", &[("a", "A")])).await });
        assert!(matches!(rx.recv().await, Some(QuestionEvent::Ask(_))));

        ask.abort();
        assert!(matches!(
            rx.recv().await,
            Some(QuestionEvent::Withdraw(id)) if id == "t3"
        ));
        assert!(!questions.is_waiting("t3"));
        // 撤走之后再答就是无主的答案，不该被当成有人回答了。
        assert!(!questions.answer("t3", "too late".into()));
    }

    /// 答过之后不该再发一条 Withdraw——对话框那边已经因为答案撤掉了，再撤一次
    /// 会误伤同一个 id 的下一个问题。
    #[tokio::test]
    async fn answering_does_not_also_withdraw() {
        let (questions, mut rx) = Questions::new();
        let asker = questions.clone();
        let ask = tokio::spawn(async move { asker.ask(q("t4", &[("a", "A")])).await });
        assert!(matches!(rx.recv().await, Some(QuestionEvent::Ask(_))));
        questions.answer("t4", "a".into());
        ask.await.unwrap();
        assert!(rx.try_recv().is_err(), "answering must not emit a Withdraw");
    }
}
