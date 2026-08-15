//! Bridge 启动装配 — 组装 `runtime::Agent` 所需依赖，调用 `Builder::build()`。
//!
//! 装配顺序沿用 `core/daemon/src/main.rs` 的参考实现：Settings → AnthropicClient/Model
//! → CodingScene → `runtime::agent::Builder`。`Permission` 由 `crate::permission::build`
//! 装配后显式传入：`Builder::build()` 的默认值是 always-allow 占位实现，留着它等于
//! 工具调用一律不过门、TUI 的审批对话框永远不弹。
//!
//! `Settings` 走 `Settings::load()`（Core 唯一的 settings.json 加载器，三层合并：
//! 全局 → 场景 → 项目），而不是手搓字面量。手搓那版把 `instruction_file` /
//! `hooks_config` / `permission_rules` 一律钉成空，效果是用户写在 settings.json 里的
//! 东西一件都不生效——而这些字段 `Builder::build()` 全都会自己消费。

use base::interface::settings::{PathSettings, Settings};
use history::store::HistoryStore;
use model::adapter::AnthropicModel;
use model::client::{AuthMode, HttpAnthropicClient};
use runtime::agent::{Agent, Builder, EventReceiver, InputSender};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

/// `scope` for [`base::paths::ConfigPaths`] — must equal the scene's `id()`
/// (`CodingScene::id() == "coding"`), because that is what picks the
/// user-level override tree `~/.atta/scenes/<scope>/` that Core itself reads
/// skills/settings/plugins from.
const SCENE_SCOPE: &str = "coding";

/// `Settings::defaults_for` 给 `max_tokens` 的值。见 [`resolve_settings`] 里对它的用法
/// ——和 `daemon::config::load_daemon_config` 用的是同一个哨兵值。
const CORE_DEFAULT_MAX_TOKENS: u32 = 2000;

/// 三层 settings.json 和 `ANTHROPIC_MODEL` 都没说话时用的模型。**只是兜底**——
/// 优先级见 [`resolve_settings`]：调用方的显式覆盖 > `ANTHROPIC_MODEL` >
/// 项目/场景/全局 settings.json > 这里。
///
/// 放在 bridge 而不是 `crates/app`：`examples/smoke.rs` 也要装配同一个引擎，
/// 两边各写一个字面量的结果就是它们悄悄跑在不同模型上（之前正是如此）。
pub const DEFAULT_MODEL: &str = "claude-opus-5";

/// 要恢复哪个会话。
#[derive(Debug, Clone)]
pub enum Resume {
    /// 当前项目里最近改动过的那次会话（`--continue`）。没有历史时静默开新会话
    /// ——"没有可继续的"不是错误。
    Latest,
    /// 指定 session id（`--resume <id>`）。找不到就是错误：用户点名要的东西不在，
    /// 静默开一个新会话等于把他的上下文吞了。
    Id(String),
}

/// 装配失败原因。
#[derive(Debug, Error)]
pub enum BootstrapError {
    #[error("missing API credentials: set ANTHROPIC_AUTH_TOKEN or ANTHROPIC_API_KEY")]
    MissingCredentials,
    #[error("cannot resume session {id}: {source}")]
    Resume {
        id: String,
        source: session::session::SessionError,
    },
    #[error("invalid ANTHROPIC_BASE_URL: {0}")]
    InvalidBaseUrl(#[from] url::ParseError),
    #[error("failed to construct Anthropic client: {0}")]
    Client(#[from] model::error::AnthropicError),
    #[error("failed to build agent: {0}")]
    Engine(#[from] runtime::agent::EngineError),
}

/// 最小可用装配所需的运行参数。三个 model 相关字段是**兜底**，不是最终值——真正的
/// 取值在 [`resolve_settings`] 里和 settings.json 合并之后才定下来。
pub struct BootstrapConfig {
    /// settings.json 没写 `model.model_name` 时用的兜底模型。
    pub fallback_model: String,
    /// `ANTHROPIC_MODEL` 环境变量。和 `fallback_model` 的优先级相反：这是**硬覆盖**，
    /// 压过 settings.json——用户为这一次运行显式指定的东西，不该被配置文件推翻。
    pub model_override: Option<String>,
    /// settings.json 没写 `model.max_tokens` 时用的兜底值。
    pub fallback_max_tokens: u32,
    /// 数据目录三元组。**必须**由 [`base::paths::ConfigPaths`] 派生而不是手写：
    /// Core 自己的技能/设置/插件加载器（`runtime::agent::build_default_skill_manager`
    /// 等）就是从这几个字段推目录的，手写一套等于给引擎换了个它不认识的家。
    pub paths: PathSettings,
    /// 要不要接着某次旧会话跑。`None` = 全新会话。
    pub resume: Option<Resume>,
}

impl BootstrapConfig {
    /// 走 Core 的路径约定（`ConfigPaths::from_env`）：全局 `~/.atta/`、场景覆盖
    /// `~/.atta/scenes/coding/`、项目 `<cwd>/.atta/`，并尊重 `ATTA_DATA_DIR` /
    /// `ATTA_LOCAL_DATA_DIR` 覆盖。
    pub fn defaults(fallback_model: impl Into<String>) -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let dirs = base::paths::ConfigPaths::from_env(&cwd, SCENE_SCOPE);
        Self {
            fallback_model: fallback_model.into(),
            model_override: std::env::var("ANTHROPIC_MODEL")
                .ok()
                .filter(|v| !v.is_empty()),
            fallback_max_tokens: 8192,
            paths: PathSettings {
                user_data_dir: dirs.user_data_dir,
                global_data_dir: dirs.global_data_dir,
                local_data_dir: dirs.local_data_dir,
                scope: SCENE_SCOPE.to_string(),
            },
            resume: None,
        }
    }
}

/// 三层 settings.json 合并 + AttaCode 自己拥有的那几项覆盖。
///
/// 优先级（高→低）：`ANTHROPIC_MODEL` → 项目 settings.json → 场景 → 全局 →
/// `fallback_*`。`Settings::load` 自己不失败：读不到的层跳过，解析不了的层 warn 后跳过。
fn resolve_settings(config: &BootstrapConfig) -> Settings {
    let mut settings = Settings::load(
        config.paths.global_data_dir.clone(),
        config.paths.user_data_dir.clone(),
        config.paths.local_data_dir.clone(),
        &config.paths.scope,
        &config.fallback_model,
    );

    if let Some(model) = &config.model_override {
        settings.model.model_name = model.clone();
    }
    // `max_tokens` 没法像 `model_name` 那样作为参数塞进 `Settings::load`，只能靠这个
    // 哨兵判断"没有任何一层设过它"。和 `daemon::config::load_daemon_config` 同款，
    // 连注释里的顾虑都一样：万一哪天真有人把它显式写成 2000，这里会误判成没设过，
    // 后果是拿到 8192——一个更宽松的上限，不是错误行为。
    if settings.model.max_tokens == CORE_DEFAULT_MAX_TOKENS {
        settings.model.max_tokens = config.fallback_max_tokens;
    }
    // 跟着 daemon 设这一项。**注意它在 Core 里没有任何消费方**——全仓只有 daemon 在写、
    // 测试里一律 `None`；真正的转录落盘走的是 `Builder::history_store`（见
    // [`build_history_store`]），跟这个字段无关。留着只是为了和 daemon 的 Settings
    // 形状一致，别指望改它能改变落盘位置。
    settings.session_dir = Some(config.paths.local_data_dir.clone());
    // TUI 就是这个引擎的宿主本身，没有第三方 RPC 客户端可以来放宽权限模式。
    settings.allow_client_permission_override = false;

    if settings.instruction_file.is_none() {
        settings.instruction_file = discover_instruction_file(&config.paths.project_root());
    }

    settings
}

/// settings.json 没指定 `instruction_file` 时，在项目根找一个。
///
/// **只找 `CLAUDE.md`，故意不找 `AGENTS.md`。** `AGENTS.md` 已经由
/// `base::frozen::FrozenContext` 从 cwd 往上爬着收进 system prompt（`memory_blocks`，
/// 见 `frozen/memory.rs`——那边写着 `AGENTS.md` 是"唯一权威"的指令文件）。再把它塞进
/// `instruction_file` 只会让同一份内容进两次 prompt。`CLAUDE.md` 不在那条链路上，
/// 这里是它唯一的入口。
fn discover_instruction_file(project_root: &Path) -> Option<PathBuf> {
    let candidate = project_root.join("CLAUDE.md");
    candidate.is_file().then_some(candidate)
}

/// 会话转录的落盘后端。没有它 `SessionManager` 纯内存，进程一退整段对话就没了。
///
/// 根目录用 `global_data_dir/sessions/`（**不是**场景目录）：`sessions` 和
/// `memory`/`vcr`/`mcp` 一样属于"全局 + 项目、没有场景层"那一类，而
/// `JsonlHistoryStore` 自己会按 cwd 在这个根下再分项目——见 `base::paths` 的模块文档。
/// 和 `daemon/src/main.rs` 取的是同一个根。
///
/// 失败不致命：warn 一声，这次运行退回纯内存会话——落不了盘不该让人连 agent 都用不上。
/// 返回具体类型而不是 `Arc<dyn HistoryStore>`：`list_recent_sessions`（`--continue`
/// 要的"最近一次"）是 `JsonlHistoryStore` 的固有方法，不在 trait 上。给 `Builder`
/// 时再退化成 trait object。
async fn build_history_store(
    paths: &PathSettings,
) -> Option<Arc<history::store::JsonlHistoryStore>> {
    let root = paths.global_data_dir.join("sessions");
    match history::store::JsonlHistoryStore::with_root(&paths.project_root(), root).await {
        Ok(store) => Some(Arc::new(store)),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to initialize the session history store; this session will be in-memory only"
            );
            None
        }
    }
}

/// 装配好的引擎。调用方负责 `tokio::spawn(agent.run(cancel))`。
pub struct BuiltEngine {
    pub agent: Agent,
    pub event_rx: EventReceiver,
    pub input_tx: InputSender,
    /// 三层 settings.json 合并完之后**真正生效**的模型名——状态栏要显示的是这个，
    /// 不是调用方传进来的兜底值。
    pub model_name: String,
    /// 这次会话的 id（BASE58）。转录落在
    /// `<global_data_dir>/sessions/<项目>/<session_id>.jsonl`；`--resume` 要的就是它。
    pub session_id: String,
    /// resume 时从 jsonl 读回来的历史消息，用来把转录区填回去。全新会话是空的。
    ///
    /// Core 那边 `Agent::resume_session` 已经把同一份历史灌进 `SessionManager`
    /// （模型看得见），这里是给**人**看的那一份——两条路读的是同一个文件、同一个
    /// 投影函数（`history::transcript::project_messages`），不会各说各话。
    pub restored: Vec<base::message::Message>,
}

/// 把 [`Resume`] 解析成一个具体的 session id。
///
/// 两种目标的失败语义**故意不同**：`--resume <id>` 点名要某个会话，找不到就报错
/// 退出（静默开新会话等于把用户的上下文吞了）；`--continue` 是"接着上次跑"，
/// 没有上次就正常开新的。
async fn resolve_resume_target(
    target: &Resume,
    store: &history::store::JsonlHistoryStore,
) -> Result<Option<String>, BootstrapError> {
    match target {
        Resume::Id(id) => {
            // 先自己校验一次 id 格式：让 `--resume 手滑打错的东西` 报"不是合法 id"，
            // 而不是等 `resume_session` 回一个 NotFound。
            base::session::SessionId::parse(id).map_err(|e| BootstrapError::Resume {
                id: id.clone(),
                source: session::session::SessionError::Id(e.to_string()),
            })?;
            Ok(Some(id.clone()))
        }
        Resume::Latest => match store.list_recent_sessions(1).await {
            Ok(recent) => Ok(recent.first().map(|(id, _)| id.to_string())),
            Err(e) => {
                tracing::warn!(error = %e, "failed to list recent sessions; starting fresh");
                Ok(None)
            }
        },
    }
}

/// 组装一个 `runtime::Agent`。
pub async fn build_agent(config: &BootstrapConfig) -> Result<BuiltEngine, BootstrapError> {
    let api_key = std::env::var("ANTHROPIC_AUTH_TOKEN")
        .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
        .map_err(|_| BootstrapError::MissingCredentials)?;
    let auth = AuthMode::ApiKey(api_key);
    // `ANTHROPIC_BASE_URL` targets an Anthropic-compatible endpoint other than the
    // default (e.g. DeepSeek's `/anthropic` shim) — mirrors core/daemon/src/main.rs.
    let client = match std::env::var("ANTHROPIC_BASE_URL")
        .ok()
        .filter(|v| !v.is_empty())
    {
        Some(mut raw) => {
            if !raw.ends_with('/') {
                raw.push('/');
            }
            let base = url::Url::parse(&raw)?;
            Arc::new(HttpAnthropicClient::with_base(auth, base)?)
        }
        None => Arc::new(HttpAnthropicClient::new(auth)?),
    };
    let model = Arc::new(AnthropicModel::new(client));

    let settings = Arc::new(resolve_settings(config));
    let model_name = settings.model.model_name.clone();

    let scene: Arc<dyn base::interface::scene::AgentScene> =
        Arc::new(scene::scene::coding::CodingScene);

    // 工具注册表。**不注册的后果是静默的**：`Builder::build()` 自己只挂上
    // `Agent`/`Skill`/`Cron*`/`Task*` 那几个，Read/Write/Edit/Bash/Grep/Glob/
    // TodoWrite 全都不在。而场景的 system prompt 照旧告诉模型"你有这些工具"，
    // 于是模型开开心心调 `Read`，拿回一句 `Tool not found: Read`，然后自己想办法
    // 绕（派子代理去读文件）。没有报错、没有警告，只有"这个 agent 怎么这么笨"。
    // 真跑第一条读文件的提示词就撞上了。
    //
    // `register_web_search` 单独一步：选哪个 search provider 要看解析完的
    // settings（端点 + 凭据），所以 `register_builtin_tools` 里放不下。场景的
    // 工具白名单和注册表取交集，不注册就是"模型根本看不到"。
    // 和 `core/daemon/src/session_pool.rs` 的装配顺序一致。
    let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools::register_builtin_tools(&tools);
    tools::register_web_search(&tools, &settings);

    // 权限门。默认模式是"没有规则命中就问"——工具自判允许的调用（只读工具、项目内的
    // Write/Edit）照旧静默通过，其余会走 `PermissionOutcome::Prompt` →
    // `AgentEvent::PermissionPrompt` → TUI 对话框 → `InputMessage::PermissionResponse`。
    // 没人应答时引擎等 `execution.permission_prompt_timeout_secs`（默认 300s）后
    // **拒绝**——未作答不是同意。模式和规则现在都来自 settings.json。
    let permission = crate::permission::build(&settings);

    // **必须**显式给 session id，不能用 `SessionManager::new` 的默认值。
    //
    // 那个默认值是 `uuid::Uuid::new_v4().to_string()`（带连字符的十六进制），而
    // `SessionManager::persist()` 走 `SessionId::parse()`，要的是 BASE58 的 16 字节 id。
    // UUID 永远 parse 不过，于是每个 turn 都 warn 一句 "failed to persist session" 然后
    // 一个字节都不落盘——静默失败，因为宿主通常没装 tracing subscriber。同一个不匹配
    // 还会让 `Builder::build` 里的会话记忆边车（`session_memory.md`）被静默跳过，它那边
    // 是 `if let Ok(sid) = SessionId::parse(..)`。
    //
    // daemon 没踩到是因为它的 id 来自 `session.create` RPC，本来就是合法的。
    // 这是 Core 的缺陷，记在 scripts/ 的 patch 规格里；在它修好之前，自己生成一个合法
    // id 传进去是干净的解法——`Builder::session_id` 本来就是公开 API，daemon 走的也是它。
    let store = build_history_store(&config.paths).await;
    // 要恢复的会话 id（如果有）。先定下来，因为它同时是 `Builder::session_id`——
    // 会话记忆边车和落盘文件名都跟着它走，恢复之后还用一个新 id 的话，续写会落到
    // 另一个文件里，下次 `--continue` 就只看得到后半截。
    let resuming = match (&config.resume, &store) {
        (Some(target), Some(store)) => resolve_resume_target(target, store).await?,
        // 没有落盘后端就没有可恢复的东西；显式 `--resume <id>` 时这是个错误。
        (Some(Resume::Id(id)), None) => {
            return Err(BootstrapError::Resume {
                id: id.clone(),
                source: session::session::SessionError::NotFound(id.clone()),
            })
        }
        (Some(Resume::Latest), None) | (None, _) => None,
    };

    let session_id = resuming
        .clone()
        .unwrap_or_else(|| base::session::SessionId::new().to_string());
    let mut builder = Builder::new()
        .scene(scene)
        .model(model)
        .permission(permission)
        .session_id(session_id.clone())
        .tools(tools)
        .settings(settings);
    // 有落盘后端时，`SessionManager` 每个 turn 结束增量追加一次 jsonl；没有就纯内存
    // （`Builder` 的默认行为），进程一退全丢。顺带一提，会话记忆边车
    // （`session_memory.md`）也只在有这个 store 的时候才建——见 `Builder::build`。
    if let Some(store) = &store {
        builder = builder.history_store(store.clone() as Arc<dyn history::store::HistoryStore>);
    }
    let (mut agent, event_rx, input_tx) = builder.build()?;

    // 恢复分两半：`resume_session` 把消息灌回 `SessionManager`（模型的上下文），
    // `restored` 是同一份历史给 TUI 转录区用的。必须在 `agent.run()` 之前——
    // `run()` 会 `&mut self` 借走整个 session。
    let mut restored = Vec::new();
    if let Some(id) = &resuming {
        agent
            .resume_session(id)
            .await
            .map_err(|source| BootstrapError::Resume {
                id: id.clone(),
                source,
            })?;
        if let Some(store) = &store {
            match base::session::SessionId::parse(id) {
                Ok(sid) => match store.load_messages(sid).await {
                    Ok(messages) => restored = messages,
                    // 转录读不回来不该让人连 agent 都用不上：模型侧的上下文
                    // （`resume_session`）已经成功了，这里丢的只是回显。
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to read the transcript for display")
                    }
                },
                Err(e) => tracing::warn!(error = %e, "resumed id is not a valid SessionId"),
            }
        }
        tracing::info!(session_id = %id, messages = restored.len(), "session resumed");
    } else {
        tracing::info!(session_id = %session_id, "session started");
    }

    Ok(BuiltEngine {
        agent,
        event_rx,
        input_tx,
        model_name,
        session_id,
        restored,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 三层目录 + 一个项目根。`local_data_dir` 是 `<project>/.atta`，所以
    /// `PathSettings::project_root()` 会推回 `<project>`。
    struct Layout {
        _tmp: tempfile::TempDir,
        global: PathBuf,
        scene: PathBuf,
        project: PathBuf,
        config: BootstrapConfig,
    }

    fn layout() -> Layout {
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("atta");
        let scene = global.join("scenes").join(SCENE_SCOPE);
        let project = tmp.path().join("proj");
        let local = project.join(".atta");
        for d in [&global, &scene, &project, &local] {
            std::fs::create_dir_all(d).unwrap();
        }
        let config = BootstrapConfig {
            fallback_model: "fallback-model".into(),
            model_override: None,
            fallback_max_tokens: 8192,
            paths: PathSettings {
                user_data_dir: scene.clone(),
                global_data_dir: global.clone(),
                local_data_dir: local,
                scope: SCENE_SCOPE.to_string(),
            },
            resume: None,
        };
        Layout {
            _tmp: tmp,
            global,
            scene,
            project,
            config,
        }
    }

    fn write_settings(dir: &Path, json: serde_json::Value) {
        std::fs::write(dir.join("settings.json"), json.to_string()).unwrap();
    }

    #[test]
    fn nothing_configured_falls_back_to_the_caller_supplied_defaults() {
        let l = layout();
        let s = resolve_settings(&l.config);
        assert_eq!(s.model.model_name, "fallback-model");
        assert_eq!(s.model.max_tokens, 8192);
    }

    #[test]
    fn project_settings_win_over_scene_which_win_over_global() {
        let l = layout();
        write_settings(&l.global, serde_json::json!({"model": {"model_name": "g"}}));
        write_settings(&l.scene, serde_json::json!({"model": {"model_name": "s"}}));
        assert_eq!(resolve_settings(&l.config).model.model_name, "s");

        write_settings(
            &l.config.paths.local_data_dir,
            serde_json::json!({"model": {"model_name": "p"}}),
        );
        assert_eq!(resolve_settings(&l.config).model.model_name, "p");
    }

    /// `ANTHROPIC_MODEL` 是为这一次运行显式指定的，配置文件不该推翻它。
    #[test]
    fn env_model_override_beats_every_settings_layer() {
        let mut l = layout();
        write_settings(
            &l.config.paths.local_data_dir,
            serde_json::json!({"model": {"model_name": "from-settings"}}),
        );
        l.config.model_override = Some("from-env".into());
        assert_eq!(resolve_settings(&l.config).model.model_name, "from-env");
    }

    #[test]
    fn explicit_max_tokens_is_not_clobbered_by_the_fallback() {
        let l = layout();
        write_settings(
            &l.config.paths.local_data_dir,
            serde_json::json!({"model": {"max_tokens": 4096}}),
        );
        assert_eq!(resolve_settings(&l.config).model.max_tokens, 4096);
    }

    /// 这条线以前是死的：手搓的 `Settings` 把 `permission_rules` 钉成空，于是
    /// settings.json 里写的规则永远到不了权限门。
    #[test]
    fn permission_rules_and_mode_reach_the_gate() {
        let l = layout();
        write_settings(
            &l.config.paths.local_data_dir,
            serde_json::json!({
                "permission_mode": "acceptEdits",
                "permission_rules": [{"tool": "Bash(rm:*)", "action": "deny"}],
            }),
        );
        let s = resolve_settings(&l.config);
        assert_eq!(
            s.permission_mode,
            base::interface::settings::PermissionMode::AcceptEdits
        );
        assert_eq!(s.permission_rules.len(), 1);
        assert_eq!(s.permission_rules[0].tool, "Bash(rm:*)");
    }

    /// 同上：`hooks_config` 是 `Builder::build()` 自己会消费的字段
    /// （`build_hook_runner`），钉成 `None` 等于钩子系统整个不存在。
    #[test]
    fn hooks_config_survives_the_merge() {
        let l = layout();
        write_settings(
            &l.config.paths.local_data_dir,
            serde_json::json!({"hooks_config": {"PreToolUse": []}}),
        );
        assert!(resolve_settings(&l.config).hooks_config.is_some());
    }

    #[test]
    fn claude_md_in_the_project_root_becomes_the_instruction_file() {
        let l = layout();
        std::fs::write(l.project.join("CLAUDE.md"), "# rules").unwrap();
        assert_eq!(
            resolve_settings(&l.config).instruction_file,
            Some(l.project.join("CLAUDE.md"))
        );
    }

    /// `AGENTS.md` 走的是 `FrozenContext` 的 `memory_blocks`，这里再认一次就会
    /// 让同一份内容进两次 prompt。
    #[test]
    fn agents_md_is_left_to_the_frozen_context() {
        let l = layout();
        std::fs::write(l.project.join("AGENTS.md"), "# rules").unwrap();
        assert_eq!(resolve_settings(&l.config).instruction_file, None);
    }

    #[test]
    fn an_explicit_instruction_file_is_not_overridden_by_discovery() {
        let l = layout();
        std::fs::write(l.project.join("CLAUDE.md"), "# rules").unwrap();
        write_settings(
            &l.config.paths.local_data_dir,
            serde_json::json!({"instruction_file": "/somewhere/else.md"}),
        );
        assert_eq!(
            resolve_settings(&l.config).instruction_file,
            Some(PathBuf::from("/somewhere/else.md"))
        );
    }

    /// 转录落在 `global_data_dir/sessions/` 下，**不是**场景目录——`sessions` 属于
    /// "全局 + 项目、没有场景层"那一类。这是这里唯一一个可能选错的东西，所以真写一条
    /// 进去看看文件落在哪。
    #[tokio::test]
    async fn transcripts_land_under_the_global_sessions_root() {
        let l = layout();
        let store = build_history_store(&l.config.paths).await.unwrap();
        let session = base::session::SessionId::new();

        store
            .append(
                session,
                history::entry::LogEntry::System {
                    subkind: history::entry::SystemSubkind::Notice,
                    text: "hello".into(),
                },
            )
            .await
            .unwrap();

        let written: Vec<_> = walk_files(&l.global.join("sessions")).collect();
        assert_eq!(
            written.len(),
            1,
            "expected exactly one transcript under {}, got {written:?}",
            l.global.join("sessions").display()
        );
        assert!(written[0].to_string_lossy().ends_with(".jsonl"));
        // 场景目录不该有任何东西。
        assert_eq!(walk_files(&l.scene.join("sessions")).count(), 0);
    }

    /// 往 store 里塞一个会话，返回它的 id。
    async fn seed_session(
        store: &history::store::JsonlHistoryStore,
        text: &str,
    ) -> base::session::SessionId {
        let sid = base::session::SessionId::new();
        store
            .append(
                sid,
                history::entry::LogEntry::User {
                    content: vec![base::message::ContentBlock::Text {
                        text: text.into(),
                        cache_control: None,
                    }],
                },
            )
            .await
            .unwrap();
        sid
    }

    /// `--continue` 要的是**最近改动过**的那个会话，不是随便一个。
    #[tokio::test]
    async fn latest_resolves_to_the_most_recently_touched_session() {
        let l = layout();
        let store = build_history_store(&l.config.paths).await.unwrap();

        let _older = seed_session(&store, "第一次").await;
        // mtime 的分辨率没那么细，隔一下再写第二个，否则"最近"是不确定的。
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        let newer = seed_session(&store, "第二次").await;

        let resolved = resolve_resume_target(&Resume::Latest, &store)
            .await
            .unwrap();
        assert_eq!(resolved.as_deref(), Some(newer.to_string().as_str()));

        // 上面那条断言**证明力有限**：`list_recent_sessions(1)` 只返回一条，取
        // first 还是 last 都一样（变异测试实测：把 first 换成 last，测试照样绿）。
        // `--continue` 的正确性其实整个押在 store 的排序上，那就直接钉排序。
        let ordered = store.list_recent_sessions(5).await.unwrap();
        assert_eq!(ordered.len(), 2);
        assert_eq!(
            ordered.first().map(|(id, _)| id.to_string()).as_deref(),
            Some(newer.to_string().as_str()),
            "store 必须按 mtime 倒序"
        );
    }

    /// 没有任何历史时 `--continue` 不是错误——正常开新会话。
    #[tokio::test]
    async fn latest_with_no_history_starts_fresh() {
        let l = layout();
        let store = build_history_store(&l.config.paths).await.unwrap();
        assert!(resolve_resume_target(&Resume::Latest, &store)
            .await
            .unwrap()
            .is_none());
    }

    /// `--resume` 打错 id 时立刻报"不是合法 id"，而不是等引擎回 NotFound。
    #[tokio::test]
    async fn an_unparseable_resume_id_fails_early() {
        let l = layout();
        let store = build_history_store(&l.config.paths).await.unwrap();
        let err = resolve_resume_target(&Resume::Id("手滑打的东西".into()), &store)
            .await
            .unwrap_err();
        assert!(matches!(err, BootstrapError::Resume { .. }), "got: {err}");
    }

    fn walk_files(root: &Path) -> impl Iterator<Item = PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    out.push(p);
                }
            }
        }
        out.into_iter()
    }

    /// settings.json 不能决定自己住在哪 —— `Settings::load` 会把每层的 `paths`
    /// 摘掉。钉住这一点，否则一个手滑写进项目配置的 `paths` 能把技能/权限的
    /// 目录整体挪走。
    #[test]
    fn settings_json_cannot_relocate_the_data_dirs() {
        let l = layout();
        write_settings(
            &l.config.paths.local_data_dir,
            serde_json::json!({"paths": {
                "user_data_dir": "/evil",
                "global_data_dir": "/evil",
                "local_data_dir": "/evil",
                "scope": "evil",
            }}),
        );
        let s = resolve_settings(&l.config);
        assert_eq!(s.paths.global_data_dir, l.global);
        assert_eq!(s.paths.user_data_dir, l.scene);
        assert_eq!(s.paths.scope, SCENE_SCOPE);
    }
}
