//! `/btw` 侧问 —— 拿这次会话已有的上下文问一句题外话。
//!
//! 需求见 `docs/reqs/2026-09-02-btw-side-question.md`，行为规格抄自 Claude Code 的
//! `/btw`（<https://code.claude.com/docs/en/interactive-mode>）：
//!
//! - **只从已有上下文回答**，不进主对话历史、不占主上下文
//! - **无工具**：够不着的东西就说够不着
//! - **turn 跑着的时候也能用**，且不打断它
//! - **单次回答**，要继续就再问一次
//! - 早前问答只活在本次进程里
//!
//! # 上下文是从哪儿来的
//!
//! 侧问要看见"模型此刻看见的全部对话"。那份东西在 `runtime::Agent` 的
//! `session: SessionManager` 上，而它是 `pub(crate)` 的，`Agent` 又已经被 move 进
//! `run()`——宿主够不着。
//!
//! 但有一条正门：[`ModelInterceptor::on_request`] 在**每次模型调用发出之前**把整个
//! `ModelRequestView`（prompt blocks + messages + params）交到手上。那就是模型此刻
//! 看见的东西，一个字不差，而且每次调用都刷新。
//!
//! 这条路顺带把 CC 那句"它看得见此刻为止的一切，**除了模型正在写的那条回复**"实现
//! 得分毫不差——最后一次出站请求的内容，恰好就是"到这一刻为止、不含还没写完的回复"。
//!
//! 另外两条路都不行，记在这儿免得有人再想一遍：从 `AgentEvent` 攒影子副本，攒出来的
//! 是**渲染投影**（没有 system prompt、工具入参被摘要过），侧问会基于一份"看起来像但
//! 不是"的上下文回答；读会话 jsonl 内容是真的，但当前这一轮还没落盘，正好错过侧问最
//! 有用的那段。
//!
//! [`ModelInterceptor::on_request`]: base::interface::model_interceptor::ModelInterceptor::on_request

use base::interface::model::{
    MessageRole, Model, ModelContentBlock, ModelError, ModelEvent, ModelMessage, StreamParams,
};
use base::interface::model_interceptor::{ModelInterceptor, ModelRequestView};
use base::prompt::PromptBlock;
use futures::StreamExt;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// 每次提问要带上的早前问答条数。和 CC 一致。
const REPLAY_EXCHANGES: usize = 20;

/// 一次侧问问答。
#[derive(Debug, Clone)]
pub struct Exchange {
    pub question: String,
    pub answer: String,
}

/// 模型此刻看见的那份请求。
#[derive(Clone)]
struct Snapshot {
    prompt_blocks: Vec<PromptBlock>,
    messages: Vec<ModelMessage>,
    params: StreamParams,
}

/// 侧问的全部状态：上下文镜像 + 本次进程内的问答记录。
pub struct SideQuestions {
    /// 最后一次出站请求。`None` = 这次会话还没发过任何模型请求，问不了。
    snapshot: Mutex<Option<Snapshot>>,
    /// 本次进程内的问答，旧的在前。**不落盘**，退出即散。
    exchanges: Mutex<Vec<Exchange>>,
    model: Arc<dyn Model>,
}

impl SideQuestions {
    pub fn new(model: Arc<dyn Model>) -> Arc<Self> {
        Arc::new(Self {
            snapshot: Mutex::new(None),
            exchanges: Mutex::new(Vec::new()),
            model,
        })
    }

    /// 挂进 `Builder::model_interceptor` 的那一半。
    pub fn mirror(self: &Arc<Self>) -> Arc<dyn ModelInterceptor> {
        Arc::new(ContextMirror {
            into: Arc::clone(self),
        })
    }

    /// 现在问得了吗。会话还没发过模型请求时（刚起手、一句话都没说）问不了。
    pub fn is_ready(&self) -> bool {
        self.snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// 早前问答，旧的在前。
    pub fn exchanges(&self) -> Vec<Exchange> {
        self.exchanges
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 清空早前问答（`x` 键）。
    pub fn clear(&self) {
        self.exchanges
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    fn remember(&self, question: String, answer: String) {
        self.exchanges
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Exchange { question, answer });
    }

    /// 问一句，把答案的每一段交给 `on_delta`，最后返回完整答案。
    ///
    /// 返回 `Err` 的两种情况：还没有上下文可问，或者模型调用本身失败。两种都要让用户
    /// 看见——一个答不出来的侧问必须说自己答不出来，而不是留一个空框。
    pub async fn ask(
        self: &Arc<Self>,
        question: &str,
        cancel: CancellationToken,
        mut on_delta: impl FnMut(&str),
    ) -> Result<String, String> {
        let snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| {
                "这次会话还没有可问的上下文——先说一句话，再用 /btw 问关于它的事。".to_string()
            })?;

        let messages = self.compose(&snapshot, question);
        let mut params = snapshot.params.clone();
        // 侧问是一次性的短问答，不需要 extended thinking——它只会让用户多等。
        params.thinking_mode = base::settings::ThinkingMode::Off;

        let mut stream = self
            .model
            // **工具表是空的。** 这是 B2 那条"无工具"的落点：不给工具定义，模型就
            // 没法调工具，只能拿已有的上下文回答。
            .stream(snapshot.prompt_blocks, Vec::new(), messages, params, cancel)
            .await
            .map_err(describe)?;

        let mut answer = String::new();
        while let Some(event) = stream.next().await {
            // thinking 关了、工具没给，所以除了文本增量之外的事件对侧问没有意义。
            if let ModelEvent::TextDelta { text } = event.map_err(describe)? {
                on_delta(&text);
                answer.push_str(&text);
            }
        }

        self.remember(question.to_string(), answer.clone());
        Ok(answer)
    }

    /// 拼出侧问要发的那串消息。
    ///
    /// = 主对话（模型此刻看见的那份）+ 最近若干条侧问问答 + 这次的问题。
    ///
    /// 侧问问答也拼进去，因为 CC 的规格里"侧问看得见侧问"——问完"那个配置文件叫什么"
    /// 之后接着问"它在哪个目录"，第二句得知道第一句在说什么。
    fn compose(&self, snapshot: &Snapshot, question: &str) -> Vec<ModelMessage> {
        let mut messages = snapshot.messages.clone();

        let earlier = self.exchanges();
        for exchange in earlier.iter().rev().take(REPLAY_EXCHANGES).rev() {
            messages.push(say(MessageRole::User, &exchange.question));
            messages.push(say(MessageRole::Assistant, &exchange.answer));
        }

        // 把"这是道侧问"直接写进这一条里，而不是塞进 system prompt：prompt blocks 是
        // 主对话的，原样带着才有 cache 命中（CC 的"便宜"就是这么来的），改一个字都会
        // 让整段前缀失效。
        messages.push(say(
            MessageRole::User,
            &format!(
                "[侧问] 下面这个问题是题外话，只用上面已有的信息回答，简短一点。\
                 你现在**没有任何工具**——读不了文件、跑不了命令、搜不了东西。\
                 要是答案不在上面的上下文里，直接说你够不着，别猜。\n\n{question}"
            ),
        ));
        messages
    }
}

fn say(role: MessageRole, text: &str) -> ModelMessage {
    ModelMessage {
        role,
        content: vec![ModelContentBlock::Text { text: text.into() }],
    }
}

fn describe(e: ModelError) -> String {
    format!("侧问没能问出去：{e}")
}

/// 把每次出站请求抄一份下来。
///
/// `on_request` 明确说"everything is mutable"，但这里**一个字都不改**——侧问是旁观者，
/// 改了主对话的请求就不再是"不打扰主 turn"了。
struct ContextMirror {
    into: Arc<SideQuestions>,
}

impl ModelInterceptor for ContextMirror {
    fn on_request(&self, request: &mut ModelRequestView) {
        let snapshot = Snapshot {
            prompt_blocks: request.prompt_blocks.clone(),
            messages: request.messages.clone(),
            params: request.params.clone(),
        };
        *self.into.snapshot.lock().unwrap_or_else(|e| e.into_inner()) = Some(snapshot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::model::{ModelStream, ToolDef};
    use base::provider::ApiType;

    /// 一个把收到的请求原样记下来、回一句固定答案的模型。
    struct Spy {
        seen: Mutex<Option<(Vec<ModelMessage>, Vec<ToolDef>)>>,
        reply: String,
    }

    #[async_trait::async_trait]
    impl Model for Spy {
        fn api_type(&self) -> ApiType {
            ApiType::Anthropic
        }
        async fn stream(
            &self,
            _prompt_blocks: Vec<PromptBlock>,
            tools: Vec<ToolDef>,
            messages: Vec<ModelMessage>,
            _params: StreamParams,
            _cancel: CancellationToken,
        ) -> Result<ModelStream, ModelError> {
            *self.seen.lock().unwrap() = Some((messages, tools));
            let events: Vec<Result<ModelEvent, ModelError>> = self
                .reply
                .chars()
                .map(|c| {
                    Ok(ModelEvent::TextDelta {
                        text: c.to_string(),
                    })
                })
                .collect();
            Ok(Box::new(futures::stream::iter(events)))
        }
    }

    fn spy(reply: &str) -> Arc<Spy> {
        Arc::new(Spy {
            seen: Mutex::new(None),
            reply: reply.into(),
        })
    }

    fn params() -> StreamParams {
        StreamParams {
            model: "m".into(),
            max_tokens: 1024,
            thinking_mode: base::settings::ThinkingMode::On,
            fallback_model: None,
            cache_edits: Vec::new(),
            origin: None,
            input_map: None,
        }
    }

    fn mirrored(side: &Arc<SideQuestions>, messages: Vec<ModelMessage>) {
        let mut view = ModelRequestView {
            prompt_blocks: vec![PromptBlock::system("你是一个编程助手")],
            tool_defs: Vec::new(),
            messages,
            params: params(),
        };
        side.mirror().on_request(&mut view);
    }

    /// 会话还没发过任何模型请求时，侧问必须说自己没得问，而不是拿一份空上下文去问。
    #[tokio::test]
    async fn with_no_conversation_yet_there_is_nothing_to_ask_about() {
        let side = SideQuestions::new(spy("x"));
        assert!(!side.is_ready());
        let err = side
            .ask("那个配置文件叫什么", CancellationToken::new(), |_| {})
            .await
            .unwrap_err();
        assert!(err.contains("还没有可问的上下文"), "got: {err}");
    }

    /// 侧问带的是**模型此刻看见的那份对话**，而且工具表必须是空的。
    #[tokio::test]
    async fn a_side_question_carries_the_conversation_and_no_tools() {
        let model = spy("叫 settings.json");
        let side = SideQuestions::new(model.clone());
        mirrored(&side, vec![say(MessageRole::User, "帮我看一下配置")]);
        assert!(side.is_ready());

        let answer = side
            .ask("那个配置文件叫什么", CancellationToken::new(), |_| {})
            .await
            .unwrap();
        assert_eq!(answer, "叫 settings.json");

        let (messages, tools) = model.seen.lock().unwrap().clone().unwrap();
        assert!(tools.is_empty(), "无工具是硬要求：给了工具定义模型就会去调");
        let texts: Vec<String> = messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ModelContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            texts.iter().any(|t| t.contains("帮我看一下配置")),
            "主对话要带上"
        );
        assert!(texts.last().unwrap().contains("那个配置文件叫什么"));
        assert!(
            texts.last().unwrap().contains("没有任何工具"),
            "得明确告诉它够不着工具，否则它会假装自己读过文件"
        );
    }

    /// 答案是流式交出去的——半屏区域等一整段才显示，用户会以为卡住了。
    #[tokio::test]
    async fn the_answer_arrives_in_pieces() {
        let side = SideQuestions::new(spy("好的"));
        mirrored(&side, vec![say(MessageRole::User, "在吗")]);

        let mut pieces = Vec::new();
        side.ask("嗯", CancellationToken::new(), |d| {
            pieces.push(d.to_string())
        })
        .await
        .unwrap();
        assert_eq!(pieces, ["好", "的"]);
    }

    /// 侧问看得见侧问：第二句要知道第一句在说什么。
    #[tokio::test]
    async fn a_later_side_question_sees_the_earlier_ones() {
        let model = spy("在 .atta 下面");
        let side = SideQuestions::new(model.clone());
        mirrored(&side, vec![say(MessageRole::User, "帮我看一下配置")]);

        side.ask("那个配置文件叫什么", CancellationToken::new(), |_| {})
            .await
            .unwrap();
        side.ask("它在哪个目录", CancellationToken::new(), |_| {})
            .await
            .unwrap();

        let (messages, _) = model.seen.lock().unwrap().clone().unwrap();
        let all: String = messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ModelContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("那个配置文件叫什么"), "第一问要带上");
        assert!(all.contains("在 .atta 下面"), "第一答也要带上");
        assert_eq!(side.exchanges().len(), 2);
    }

    /// `x` 清空之后，下一次提问不该再把它们带上。
    #[tokio::test]
    async fn clearing_drops_the_earlier_exchanges() {
        let model = spy("嗯");
        let side = SideQuestions::new(model.clone());
        mirrored(&side, vec![say(MessageRole::User, "在吗")]);
        side.ask("第一问", CancellationToken::new(), |_| {})
            .await
            .unwrap();
        side.clear();
        assert!(side.exchanges().is_empty());

        side.ask("第二问", CancellationToken::new(), |_| {})
            .await
            .unwrap();
        let (messages, _) = model.seen.lock().unwrap().clone().unwrap();
        let all: String = messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ModelContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!all.contains("第一问"), "清空之后不该再带上");
    }

    /// 镜像只抄不改——改了主对话的请求，"不打扰主 turn"就不成立了。
    #[test]
    fn the_mirror_never_alters_the_request() {
        let side = SideQuestions::new(spy("x"));
        let before = vec![say(MessageRole::User, "原样")];
        let mut view = ModelRequestView {
            prompt_blocks: vec![PromptBlock::system("系统提示")],
            tool_defs: Vec::new(),
            messages: before.clone(),
            params: params(),
        };
        side.mirror().on_request(&mut view);

        assert_eq!(view.messages.len(), before.len());
        assert_eq!(view.prompt_blocks.len(), 1);
        assert!(view.tool_defs.is_empty());
    }

    /// 每次出站请求都刷新——turn 跑到一半问，看见的得是这一刻的对话。
    #[test]
    fn the_snapshot_follows_the_latest_request() {
        let side = SideQuestions::new(spy("x"));
        mirrored(&side, vec![say(MessageRole::User, "第一轮")]);
        mirrored(
            &side,
            vec![
                say(MessageRole::User, "第一轮"),
                say(MessageRole::Assistant, "答"),
                say(MessageRole::User, "第二轮"),
            ],
        );
        let held = side.snapshot.lock().unwrap().clone().unwrap();
        assert_eq!(held.messages.len(), 3);
    }
}
