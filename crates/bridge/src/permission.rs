//! 权限门装配 —— AttaCode 版的 `daemon::session_pool::resolve_session_permission`。
//!
//! 这里**不再自己实现** `base::interface::permission::Permission`。Core 的
//! `permissions::rule_set_permission::RuleSetPermission` 就是那个实现，而且做对了
//! 三件手写适配器做不到的事：
//!
//! 1. `bind_tool_registry` —— 权限句柄是在 `Builder::build()` 填充**会话级**工具表
//!    之前构造的。不重绑的话，`Skill`/`Agent`/`Team*`/`mcp__*` 这些 build 过程中才
//!    注册进去的工具在权限侧全是"未知工具"，会整片裸奔。
//! 2. `bind_session_state` —— 读会话的**实时** `permission_mode`，所以
//!    `EnterPlanMode`/`ExitPlanMode` 真能挪动这道门，而不是停在构造时的模式。
//! 3. `add_persistent_allow` —— "本会话/本项目一直允许"落成真规则（后者还会写进
//!    `settings.local.json`），这是 `PermissionDecision::PermitAlways` 的落点。
//!
//! 上面两个 bind 由 `Builder::build()` 自动调用，所以这里只负责把规则和模式装好。

use base::interface::settings::Settings;
use base::permission::RuleSource;
use permissions::gate::PermissionGate;
use permissions::rule_set_permission::RuleSetPermission;
use permissions::ruleset::RuleSet;
use std::sync::Arc;

/// 按 `settings` 里的模式 + 规则装一个权限门。
///
/// `BypassPermissions` 不做特判：gate 自己认这个模式并直接 Allow。daemon 那边为它
/// 保留了一条零开销的 allow-all 快捷路径，是因为它一个进程里跑很多 session；单个
/// TUI session 省这一次 `check` 没有意义，少一条分支反而少一处可能说谎的地方。
///
/// 传进去的空工具表只是占位——`Builder::build()` 会用会话真正分派的那张表把它换掉
/// （见模块注释第 1 条）。
pub fn build(settings: &Settings) -> Arc<dyn base::interface::permission::Permission> {
    let rules = permissions::rule::rules_from_settings(
        &settings.permission_rules,
        RuleSource::ProjectSettings,
    );
    Arc::new(RuleSetPermission::new(
        Arc::new(PermissionGate::new(RuleSet::new(rules))),
        Arc::new(base::tool::InMemoryToolRegistry::new()),
        settings.permission_mode.into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::permission::{Permission, PermissionOutcome};
    use base::interface::settings::{PermissionAction, PermissionMode, PermissionRule};
    use base::tool::{InMemoryToolRegistry, ProgressSender, Tool, ToolContext, ToolResult};
    use serde_json::Value;
    use std::path::Path;

    /// 一个自判 `ask` 的写工具——Default 模式下必须问，才有得可验。
    struct AskingTool;

    #[async_trait::async_trait]
    impl Tool for AskingTool {
        fn name(&self) -> &str {
            "Write"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({})
        }
        fn is_read_only(&self, _: &Value) -> bool {
            false
        }
        async fn check_permissions(
            &self,
            _: &Value,
            _: &ToolContext,
        ) -> base::tool::PermissionDecision {
            base::tool::PermissionDecision::ask("confirm write")
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

    fn settings_with(mode: PermissionMode, rules: Vec<PermissionRule>) -> Settings {
        let mut s = Settings::defaults_for("test-model");
        s.permission_mode = mode;
        s.permission_rules = rules;
        s
    }

    /// `Builder::build()` 之外没有别的地方会绑工具表，所以这里手工做它那一步。
    fn bound(perm: &Arc<dyn Permission>) {
        let tools = Arc::new(InMemoryToolRegistry::new());
        tools.register(Arc::new(AskingTool));
        perm.bind_tool_registry(tools);
    }

    async fn check(perm: &Arc<dyn Permission>) -> PermissionOutcome {
        perm.check("Write", &Value::Null, Path::new("/tmp"), "sess-1")
            .await
    }

    #[tokio::test]
    async fn default_mode_asks_and_that_is_what_replaces_allow_all() {
        let perm = build(&settings_with(PermissionMode::Default, vec![]));
        bound(&perm);
        assert!(matches!(
            check(&perm).await,
            PermissionOutcome::Prompt { .. }
        ));
    }

    #[tokio::test]
    async fn settings_deny_rule_reaches_the_gate() {
        let perm = build(&settings_with(
            PermissionMode::Default,
            vec![PermissionRule {
                tool: "Write".into(),
                action: PermissionAction::Deny,
            }],
        ));
        bound(&perm);
        assert!(matches!(check(&perm).await, PermissionOutcome::Deny { .. }));
    }

    #[tokio::test]
    async fn bypass_mode_permits_without_asking() {
        let perm = build(&settings_with(PermissionMode::BypassPermissions, vec![]));
        bound(&perm);
        assert!(matches!(check(&perm).await, PermissionOutcome::Permit));
    }

    /// 未注册的工具必须**fail closed**。老的 `GatePermission` 直接 `Deny`，
    /// Core 的实现改成"带解释的 Prompt"——两者都不放行，这里钉住不放行这件事。
    #[tokio::test]
    async fn unknown_tool_does_not_sail_through() {
        let perm = build(&settings_with(PermissionMode::Default, vec![]));
        bound(&perm);
        let outcome = perm
            .check("Nonexistent", &Value::Null, Path::new("/tmp"), "sess-1")
            .await;
        assert!(
            !matches!(outcome, PermissionOutcome::Permit),
            "an unregistered tool must never be permitted outright, got {outcome:?}"
        );
    }
}
