//! `/doctor` —— 这次会话到底装成了什么样。
//!
//! AttaCore 0.2.0 把"这东西还好吗"做成了契约（[`HealthCheck`]），但引擎自己一个
//! 检查都不注册：`Builder` 的那张表初始为空，`HealthChecks::report` 对空集合的回答
//! 是 `Ok`——"没有人说不好"。所以一个不注册任何检查的宿主，`/doctor` 永远只会说
//! "一切正常"，而那是一句没有信息量的话。
//!
//! 这里注册的四条，每一条对应一次**真实发生过的、静默的**降级：
//!
//! | 检查 | 它替谁说话 |
//! |---|---|
//! | `model.provider` | `settings.providers` 曾经被完全忽略；现在它生效了，那就得能看见生效成什么样 |
//! | `history.store` | 转录落盘失败时 `build_history_store` 只 warn 一声就退回纯内存——而宿主通常没装 tracing subscriber，于是"这次对话退出后就没了"没有任何人知道 |
//! | `sandbox` | 0.2.1 把沙箱后端改成默认不编译。我们打开了那个 feature，但 Linux 上还要有 `bwrap`，Windows 上根本没有后端 |
//! | `permissions` | 模式和规则来自三个文件的合并结果，人对着源文件推不出最终值 |
//!
//! # 检查只汇报，不修
//!
//! 契约的要求，也是这里照办的理由：一个会顺手把问题修掉的诊断，产出的每一份报告
//! 说的都是"我刚干了什么"，而不是"我看见了什么"。同样地，每条检查都只读**已经
//! 在手上的状态**——不发请求、不探测网络。健康检查在故障时挂住，比没有健康检查更糟。

use base::interface::health::{CheckResult, HealthCheck, HealthReport, HealthStatus};
use base::settings::Settings;
use serde_json::json;
use std::sync::Arc;

/// 这次会话要注册的全部检查。
pub fn checks(settings: &Settings, history_ok: bool) -> Vec<Arc<dyn HealthCheck>> {
    vec![
        Arc::new(ProviderCheck {
            provider: settings.default_provider.clone(),
            configured: settings.providers.len(),
            model: settings.model.model_name.clone(),
        }),
        Arc::new(HistoryCheck { ok: history_ok }),
        Arc::new(SandboxCheck),
        Arc::new(PermissionsCheck {
            mode: format!("{:?}", settings.permission_mode),
            project_rules: settings.permission_rules.len(),
            local_rules: settings.local_permission_rules.len(),
            allow_write: settings.sandbox.allow_write.len(),
        }),
    ]
}

/// 报告 → 转录里的一段文字。
pub fn render(report: &HealthReport) -> String {
    let mut out = format!("doctor: {}\n", report.status.label());
    for c in &report.checks {
        let mark = match c.result.status {
            HealthStatus::Ok => "·",
            HealthStatus::Degraded => "!",
            HealthStatus::Failing => "×",
        };
        out.push_str(&format!("  {mark} {:<20} {}\n", c.name, c.result.summary));
    }
    // 尾巴上的换行会在转录里变成一条空行。
    out.trim_end().to_string()
}

struct ProviderCheck {
    provider: Option<String>,
    configured: usize,
    model: String,
}

impl HealthCheck for ProviderCheck {
    fn name(&self) -> &str {
        "model.provider"
    }

    fn check(&self) -> CheckResult {
        let via = match (&self.provider, self.configured) {
            (Some(id), _) => format!("provider `{id}`"),
            (None, 0) => "the built-in anthropic provider (ANTHROPIC_* env)".to_string(),
            (None, 1) => "the single configured provider".to_string(),
            // 装配阶段就该拦住这种（`BootstrapError::AmbiguousProvider`），走到这里
            // 说明拦漏了——说出来，别装作正常。
            (None, n) => return CheckResult::degraded(format!("{n} providers, none named")),
        };
        CheckResult::ok(format!("{} · model `{}`", via, self.model))
            .with_details(json!({ "providers_configured": self.configured }))
    }
}

struct HistoryCheck {
    ok: bool,
}

impl HealthCheck for HistoryCheck {
    fn name(&self) -> &str {
        "history.store"
    }

    fn check(&self) -> CheckResult {
        if self.ok {
            CheckResult::ok("transcripts are being written to disk")
        } else {
            // 不是 `Failing`：agent 照常能用，丢的是"退出之后还在"。但也绝不是 `Ok`
            // ——用户会以为 `--continue` 明天还能接上。
            CheckResult::degraded(
                "in-memory only — this session will be gone when the process exits, \
                 and --continue will not find it",
            )
        }
    }
}

struct SandboxCheck;

impl HealthCheck for SandboxCheck {
    fn name(&self) -> &str {
        "sandbox"
    }

    fn check(&self) -> CheckResult {
        use base::interface::exec::sandbox::{Enforcement, Sandbox, SandboxPolicy};
        // `confine` 是纯变换，不跑任何东西——正是"从已有状态回答"该有的样子。
        let probe = base::interface::exec::ProcessSpec::new("true", std::env::temp_dir());
        let confined = base::interface::exec::local::sandbox::PlatformSandbox
            .confine(probe, &SandboxPolicy::default());
        let details = json!({
            "mode": format!("{:?}", confined.mode),
            "unmet": confined.unmet,
        });
        match confined.enforcement {
            Enforcement::Full => {
                CheckResult::ok(format!("{:?}", confined.mode)).with_details(details)
            }
            Enforcement::Partial => CheckResult::degraded(format!(
                "{:?}, partially: {}",
                confined.mode,
                confined.unmet.join("; ")
            ))
            .with_details(details),
            // 这是 0.2.1 之后 Linux 上最容易出现的一种状态：feature 编进来了，
            // `bwrap` 没装。说清楚是"没约束地在跑"，别只报一个后端名。
            Enforcement::None => CheckResult::degraded(format!(
                "no confinement ({:?}) — the model's shell commands run unrestricted",
                confined.mode
            ))
            .with_details(details),
        }
    }
}

struct PermissionsCheck {
    mode: String,
    project_rules: usize,
    local_rules: usize,
    allow_write: usize,
}

impl HealthCheck for PermissionsCheck {
    fn name(&self) -> &str {
        "permissions"
    }

    fn check(&self) -> CheckResult {
        let summary = format!(
            "mode {} · {} project + {} local rules · {} write exemptions",
            self.mode, self.project_rules, self.local_rules, self.allow_write
        );
        // `bypassPermissions` 下每一次工具调用都直接放行。它是个合法选择，但绝不该
        // 静悄悄地生效——一份说"一切正常"的报告是这里最坏的答案。
        if self.mode.to_lowercase().contains("bypass") {
            CheckResult::degraded(format!(
                "{summary} — every tool call is permitted unchecked"
            ))
        } else {
            CheckResult::ok(summary)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::health::HealthChecks;

    fn report(settings: &Settings, history_ok: bool) -> HealthReport {
        HealthChecks::from_vec(checks(settings, history_ok)).report()
    }

    /// 报告里必须四条都在。少一条的症状是"那件事没人替它说话"，而不是报错。
    #[test]
    fn every_check_is_in_the_report() {
        let r = report(&Settings::defaults_for("m"), true);
        let names: Vec<_> = r.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            ["model.provider", "history.store", "sandbox", "permissions"]
        );
    }

    /// 退回纯内存必须让整份报告变色。它以前只有一句 `tracing::warn!`，而 TUI
    /// 通常连 subscriber 都没装——等于没人知道。
    #[test]
    fn an_in_memory_session_is_reported_as_degraded() {
        let r = report(&Settings::defaults_for("m"), false);
        let store = r.find("history.store").unwrap();
        assert_eq!(store.result.status, HealthStatus::Degraded);
        assert!(store.result.summary.contains("--continue"));
        assert_ne!(r.status, HealthStatus::Ok, "整份报告要跟着变色");
    }

    /// 绕过权限是个合法选择，但不能是个安静的选择。
    #[test]
    fn bypass_permissions_never_reports_as_fine() {
        let mut s = Settings::defaults_for("m");
        s.permission_mode = base::settings::PermissionMode::BypassPermissions;
        let r = report(&s, true);
        assert_eq!(
            r.find("permissions").unwrap().result.status,
            HealthStatus::Degraded
        );
    }

    #[test]
    fn the_default_session_reports_which_provider_and_model_are_in_force() {
        let r = report(&Settings::defaults_for("claude-opus-5"), true);
        let p = &r.find("model.provider").unwrap().result;
        assert_eq!(p.status, HealthStatus::Ok);
        assert!(p.summary.contains("claude-opus-5"), "got: {}", p.summary);
        assert!(p.summary.contains("ANTHROPIC_"), "got: {}", p.summary);
    }

    /// 渲染出来的每一行都得有个人看得懂的名字和一句话。
    #[test]
    fn the_rendered_report_names_every_check() {
        let text = render(&report(&Settings::defaults_for("m"), false));
        assert!(text.starts_with("doctor: degraded"), "got: {text}");
        for name in ["model.provider", "history.store", "sandbox", "permissions"] {
            assert!(text.contains(name), "{name} 不在报告里:\n{text}");
        }
    }
}
