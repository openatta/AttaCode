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
//! 3. `add_persistent_allow` —— "一直允许"落成一条 `RuleSource::Session` 的真规则，
//!    这是 `PermissionDecision::PermitAlways` 的落点。（**只在内存里**：Core 那边
//!    没有任何一处把它写回 `settings.local.json`，所以"本项目一直允许"活不过这次
//!    进程。要让它活下来，得由我们自己写盘——记在 TODO 里，不在本次范围内。）
//!
//! 上面两个 bind 由 `Builder::build()` 自动调用，所以这里只负责把规则和模式装好。

use base::settings::Settings;
use permissions::rule_set_permission::RuleSetPermission;
use std::sync::Arc;

/// 按 `settings` 里的模式 + 规则 + sandbox 配置装一个权限门。
///
/// `BypassPermissions` 不做特判：gate 自己认这个模式并直接 Allow。daemon 那边为它
/// 保留了一条零开销的 allow-all 快捷路径，是因为它一个进程里跑很多 session；单个
/// TUI session 省这一次 `check` 没有意义，少一条分支反而少一处可能说谎的地方。
///
/// **走 `from_settings` 而不是 `new`**，它比手搓那版多做两件事，两件都不是可选的：
///
/// - 规则取的是 `rules_from_all_tiers`，也就是 `settings.json`（`ProjectSettings`，
///   优先级 30）**加上** `settings.local.json`（`LocalSettings`，40）。手搓那版只读
///   前者，于是用户写在 `settings.local.json` 里的规则一条都不生效——那正是"本项目
///   一直允许"该落的地方。
/// - 带上 `sandbox` 设置。这是**唯一**一条能把 `sandbox.*` 送到工具自己那份
///   `check_permissions` 的路：AttaCore 0.2.0 把写路径的控制清单接上了 `FileWrite`/
///   `FileEdit`（在那之前那份检查是死代码），`.env` / `.gitignore` / lockfile /
///   `.atta` / `.claude` 现在真的写不进去，而 `sandbox.allow_write` 是**唯一**的豁免
///   口子。不传等于给用户留一堵没有门的墙。`deny_read`/`allow_read` 同理。
///
/// 传进去的空工具表只是占位——`Builder::build()` 会用会话真正分派的那张表把它换掉
/// （见模块注释第 1 条）。
pub fn build(settings: &Settings) -> Arc<dyn base::interface::permission::Permission> {
    Arc::new(RuleSetPermission::from_settings(
        settings,
        settings.permission_mode.into(),
        Arc::new(base::tool::InMemoryToolRegistry::new()),
        [],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::permission::{Permission, PermissionOutcome};
    use base::settings::{PermissionAction, PermissionMode, PermissionRule};
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

    /// `settings.local.json` 那一层（`local_permission_rules`）必须和
    /// `settings.json` 那层一起进 gate。漏掉它的症状是静默的：用户写在
    /// `settings.local.json` 里的规则一条都不生效，而那个文件正是"本项目一直允许"
    /// 该落的地方。
    #[tokio::test]
    async fn local_settings_rules_reach_the_gate_too() {
        let mut settings = settings_with(PermissionMode::Default, vec![]);
        settings.local_permission_rules = vec![PermissionRule {
            tool: "Write".into(),
            action: PermissionAction::Deny,
        }];
        let perm = build(&settings);
        bound(&perm);
        assert!(matches!(check(&perm).await, PermissionOutcome::Deny { .. }));
    }

    /// `sandbox.allow_write` 必须抵达工具**自己那份** `check_permissions`。
    ///
    /// AttaCore 0.2.0 才把写路径的控制清单真正接上 `FileWrite`/`FileEdit`，而
    /// `RuleSetPermission` 携带的 `sandbox` 是它唯一的到达路径。这两条断言合起来
    /// 才有意义：第一条证明墙在（否则第二条测的是"本来就能写"），第二条证明门在
    /// （否则用户对着一堵没有门的墙）。
    #[tokio::test]
    async fn sandbox_allow_write_reaches_the_tools_own_check() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(".env");
        let input = serde_json::json!({
            "file_path": target.display().to_string(),
            "content": "x",
        });
        let ask = |settings: &Settings| {
            let perm = build(settings);
            let tools = Arc::new(InMemoryToolRegistry::new());
            tools.register(Arc::new(tools::file_write::FileWriteTool));
            perm.bind_tool_registry(tools);
            let input = input.clone();
            let cwd = dir.path().to_path_buf();
            async move { perm.check("Write", &input, &cwd, "sess-1").await }
        };

        let walled = settings_with(PermissionMode::Default, vec![]);
        assert!(
            matches!(ask(&walled).await, PermissionOutcome::Deny { .. }),
            "`.env` is on the built-in credential deny list and must not be writable"
        );

        let mut exempt = walled.clone();
        exempt.sandbox.allow_write = vec![target.clone()];
        assert!(
            !matches!(ask(&exempt).await, PermissionOutcome::Deny { .. }),
            "`sandbox.allow_write` is the only way to say \"that one is fine here\"; \
             if it does not reach the tool the setting is inert"
        );
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
