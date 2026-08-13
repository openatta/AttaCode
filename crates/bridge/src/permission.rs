//! `GatePermission` — 实现既有 `base::interface::permission::Permission`，包装既有
//! `permissions::gate::PermissionGate` 的规则决策，把 `Ask` 映射成交互式 `Prompt`。
//!
//! 这个适配器本身完整、可独立测试，但**目前接不上真实的拦截效果**：Core 侧
//! `runtime::turn::execute_tool_inner` 并不会调用 `Agent.permission`（全 runtime crate
//! 搜不到 `PermissionOutcome::Prompt` / `AgentEvent::PermissionPrompt` 的处理点）。
//! 因此 `bootstrap.rs` 暂不把它接入 `Builder::permission()`——接了也不会生效，
//! 反而会让人误以为交互式权限确认已经工作。见
//! docs/design/2026-08-13-tui-core-glue-layer.md 的风险记录。

use async_trait::async_trait;
use base::context::EngineConfig;
use base::interface::permission::{Permission, PermissionOutcome};
use base::permission::PermissionDecision as GateDecision;
use base::tool::{PermissionMode, SandboxSettings, ToolContext, ToolRegistry};
use permissions::gate::PermissionGate;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub struct GatePermission {
    gate: Arc<PermissionGate>,
    tools: Arc<dyn ToolRegistry>,
}

impl GatePermission {
    pub fn new(gate: Arc<PermissionGate>, tools: Arc<dyn ToolRegistry>) -> Self {
        Self { gate, tools }
    }
}

#[async_trait]
impl Permission for GatePermission {
    async fn check(
        &self,
        tool_name: &str,
        tool_input: &Value,
        cwd: &Path,
        session_id: &str,
    ) -> PermissionOutcome {
        let Some(tool) = self.tools.find(tool_name) else {
            return PermissionOutcome::Deny {
                reason: format!("unknown tool: {tool_name}"),
            };
        };
        let ctx = tool_context(cwd.to_path_buf(), session_id.to_string());
        match self.gate.check(tool.as_ref(), tool_input, &ctx).await {
            Ok(GateDecision::Allow { .. }) => PermissionOutcome::Permit,
            Ok(GateDecision::Deny { message, .. }) => PermissionOutcome::Deny { reason: message },
            Ok(GateDecision::Ask { message, .. }) => PermissionOutcome::Prompt {
                prompt_id: base::id::Id::new().to_string(),
                message,
                paths: tool.affected_paths(tool_input),
            },
            Err(e) => PermissionOutcome::Deny {
                reason: e.to_string(),
            },
        }
    }
}

fn tool_context(cwd: PathBuf, session_id: String) -> ToolContext {
    ToolContext {
        cwd: cwd.clone(),
        session_id,
        turn_no: 0,
        sandbox: SandboxSettings::default(),
        cancel: CancellationToken::new(),
        additional_writable_dirs: vec![],
        snapshot_file: None,
        effects: None,
        running_tasks: None,
        dangerously_disable_sandbox: false,
        max_file_read_bytes: 10 * 1024 * 1024,
        permission_mode: PermissionMode::default(),
        config: Arc::new(EngineConfig::defaults_for("unknown")),
        session: Arc::new(base::context::SessionState::new(cwd)),
        tool_use_id: String::new(),
        agent: None,
        parent_messages: None,
        agent_depth: 0,
        events_tx: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::tool::{ProgressSender, Tool, ToolResult};
    use permissions::ruleset::RuleSet;

    struct StubTool {
        name: &'static str,
        decision: base::tool::PermissionDecision,
    }

    #[async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &str {
            self.name
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({})
        }
        async fn check_permissions(
            &self,
            _: &Value,
            _: &ToolContext,
        ) -> base::tool::PermissionDecision {
            self.decision.clone()
        }
        async fn call(
            &self,
            _: Value,
            _: ToolContext,
            _: ProgressSender,
        ) -> Result<ToolResult, base::error::ToolError> {
            Ok(ToolResult::text("ok"))
        }
    }

    fn registry_with(tool: StubTool) -> Arc<dyn ToolRegistry> {
        let reg = base::tool::InMemoryToolRegistry::new();
        reg.register(Arc::new(tool));
        Arc::new(reg)
    }

    #[tokio::test]
    async fn tool_allow_maps_to_permit() {
        let tools = registry_with(StubTool {
            name: "Read",
            decision: base::tool::PermissionDecision::allow(),
        });
        let gate = Arc::new(PermissionGate::empty());
        let perm = GatePermission::new(gate, tools);
        let outcome = perm
            .check("Read", &Value::Null, Path::new("/tmp"), "sess-1")
            .await;
        assert!(matches!(outcome, PermissionOutcome::Permit));
    }

    #[tokio::test]
    async fn tool_deny_maps_to_deny() {
        let tools = registry_with(StubTool {
            name: "Bash",
            decision: base::tool::PermissionDecision::deny("blocked"),
        });
        let gate = Arc::new(PermissionGate::empty());
        let perm = GatePermission::new(gate, tools);
        let outcome = perm
            .check("Bash", &Value::Null, Path::new("/tmp"), "sess-1")
            .await;
        assert!(matches!(outcome, PermissionOutcome::Deny { .. }));
    }

    #[tokio::test]
    async fn tool_ask_maps_to_prompt() {
        // `PermissionGate::check` 不透传 tool.check_permissions 返回的 Ask 消息——
        // 它落到 gate 自己的 PermissionMode 分派逻辑，生成它自己的 ask 措辞（这是
        // Core 侧既有行为，不是本适配器决定的）。这里只断言 Ask 最终映射成了
        // 交互式 Prompt，并且带上了一个非空 prompt_id 供后续 RespondPermission 关联。
        let tools = registry_with(StubTool {
            name: "Write",
            decision: base::tool::PermissionDecision::ask("confirm write"),
        });
        let gate = Arc::new(PermissionGate::new(RuleSet::empty()));
        let perm = GatePermission::new(gate, tools);
        let outcome = perm
            .check("Write", &Value::Null, Path::new("/tmp"), "sess-1")
            .await;
        match outcome {
            PermissionOutcome::Prompt { prompt_id, .. } => assert!(!prompt_id.is_empty()),
            other => panic!("expected Prompt, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_tool_denied() {
        let tools: Arc<dyn ToolRegistry> = Arc::new(base::tool::InMemoryToolRegistry::new());
        let gate = Arc::new(PermissionGate::empty());
        let perm = GatePermission::new(gate, tools);
        let outcome = perm
            .check("Nonexistent", &Value::Null, Path::new("/tmp"), "sess-1")
            .await;
        assert!(matches!(outcome, PermissionOutcome::Deny { .. }));
    }
}
