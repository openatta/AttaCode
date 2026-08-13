//! Bridge 启动装配 — 组装 `runtime::Agent` 所需依赖，调用 `Builder::build()`。
//!
//! 装配顺序沿用 `core/daemon/src/main.rs` 的参考实现：Settings → AnthropicClient/Model
//! → CodingScene → `runtime::agent::Builder`。`Permission` 字段留空——`Builder::build()`
//! 会默认注入一个 always-allow 的占位实现（见 `runtime::agent::Builder::build`），真正的
//! `GatePermission`（`crate::permission`）尚不能接入：Core 侧 `execute_tool_inner` 目前
//! 并不会调用 `Agent.permission`，接入了也不会生效（见 docs/design/2026-08-13-tui-core-glue-layer.md）。

use base::interface::settings::{
    CompactionConfig, ExecutionSettings, ModelSettings, PathSettings, SandboxConfig, Settings,
    ThinkingMode,
};
use model::adapter::AnthropicModel;
use model::client::{AuthMode, HttpAnthropicClient};
use runtime::agent::{Agent, Builder, EventReceiver, InputSender};
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;

/// 装配失败原因。
#[derive(Debug, Error)]
pub enum BootstrapError {
    #[error("missing API credentials: set ANTHROPIC_AUTH_TOKEN or ANTHROPIC_API_KEY")]
    MissingCredentials,
    #[error("invalid ANTHROPIC_BASE_URL: {0}")]
    InvalidBaseUrl(#[from] url::ParseError),
    #[error("failed to construct Anthropic client: {0}")]
    Client(#[from] model::error::AnthropicError),
    #[error("failed to build agent: {0}")]
    Engine(#[from] runtime::agent::EngineError),
}

/// 最小可用装配所需的运行参数。
pub struct BootstrapConfig {
    pub model_name: String,
    pub max_tokens: u32,
    pub user_data_dir: PathBuf,
    pub local_data_dir: PathBuf,
}

impl BootstrapConfig {
    /// 用 `$HOME/.atta/code`（用户级）+ 当前目录下 `.atta/code`（项目级）的既有约定构造默认值。
    /// `ANTHROPIC_MODEL` 环境变量存在且非空时覆盖 `fallback_model`。
    pub fn defaults(fallback_model: impl Into<String>) -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let model_name = std::env::var("ANTHROPIC_MODEL")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| fallback_model.into());
        Self {
            model_name,
            max_tokens: 8192,
            user_data_dir: PathBuf::from(home).join(".atta").join("code"),
            local_data_dir: PathBuf::from(".").join(".atta").join("code"),
        }
    }
}

/// 组装并启动一个 `runtime::Agent`。调用方负责 `tokio::spawn(agent.run(cancel))`。
pub fn build_agent(
    config: &BootstrapConfig,
) -> Result<(Agent, EventReceiver, InputSender), BootstrapError> {
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

    let settings = Arc::new(Settings {
        model: ModelSettings {
            api_type: base::provider::ApiType::Anthropic,
            base_url: String::new(),
            auth_token: String::new(),
            model_name: config.model_name.clone(),
            max_tokens: config.max_tokens,
            thinking_mode: ThinkingMode::Auto,
            fallback_model: None,
        },
        paths: PathSettings {
            user_data_dir: config.user_data_dir.clone(),
            local_data_dir: config.local_data_dir.clone(),
        },
        execution: ExecutionSettings::default(),
        compaction: CompactionConfig::default(),
        sandbox: SandboxConfig::default(),
        instruction_file: None,
        prompt_append: None,
        prompt_override: None,
        vcr: None,
        telemetry_url: None,
        session_dir: Some(config.local_data_dir.clone()),
        memory_enabled: true,
        permission_mode: base::interface::settings::PermissionMode::default(),
        permission_rules: Vec::new(),
        hooks_config: None,
        mcp_servers: Vec::new(),
        language: None,
        feature_flags: Default::default(),
    });

    let scene: Arc<dyn base::interface::scene::AgentScene> =
        Arc::new(scene::scene::coding::CodingScene);

    let (agent, event_rx, input_tx) = Builder::new()
        .scene(scene)
        .model(model)
        .settings(settings)
        .build()?;

    Ok((agent, event_rx, input_tx))
}
