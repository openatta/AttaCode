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
//! | `settings.unused` | `Settings` 是全量的，daemon 消费一部分、我们消费另一部分，交集之外用户写了也白写——而且没有任何一处会说 |
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
pub fn checks(
    settings: &Settings,
    selected_provider: Option<(&str, &base::provider::ProviderConfig)>,
    history_ok: bool,
) -> Vec<Arc<dyn HealthCheck>> {
    vec![
        Arc::new(ProviderCheck {
            selected: selected_provider.map(|(id, _)| id.to_string()),
            configured: settings.providers.len(),
            base_url: selected_provider
                .and_then(|(_, cfg)| cfg.base_url.clone())
                .filter(|u| !u.is_empty()),
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
        Arc::new(UnusedSettingsCheck::of(settings)),
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
    /// 这次真正选中的 provider id。`None` 只可能是装配阶段拦漏了。
    selected: Option<String>,
    /// `settings.providers` 里配了几个。0 = 用的是合成的那个内置 anthropic。
    configured: usize,
    /// 端点被指到别处了没有。默认端点是 `None`——**这件事必须报出来**：一个人
    /// 排查"为什么模型答得不对"时，最先要确认的就是请求到底发去了哪里。
    base_url: Option<String>,
    model: String,
}

impl ProviderCheck {
    fn endpoint_note(&self) -> String {
        match &self.base_url {
            Some(url) => format!(" → {url}"),
            None => String::new(),
        }
    }
}

impl HealthCheck for ProviderCheck {
    fn name(&self) -> &str {
        "model.provider"
    }

    fn check(&self) -> CheckResult {
        // 报的是**这次真正选中的那个 id**，不是"配了几个"。这条检查存在的理由就是
        // 让人看见哪个 provider 生效了；在只配了一个、`resolve_provider` 明明是按
        // 名字选的那种情况下说一句"the single configured provider"，恰好是在最该
        // 说出名字的时候不说。
        let via = match (&self.selected, self.configured) {
            (Some(id), 0) => format!(
                "the built-in `{id}` provider (ANTHROPIC_* env{})",
                self.endpoint_note()
            ),
            (Some(id), _) => format!("provider `{id}`{}", self.endpoint_note()),
            // 装配阶段就该拦住这种（`BootstrapError::AmbiguousProvider`），走到这里
            // 说明拦漏了——说出来，别装作正常。
            (None, n) => return CheckResult::degraded(format!("{n} providers, none selected")),
        };
        CheckResult::ok(format!("{via} · model `{}`", self.model)).with_details(json!({
            "providers_configured": self.configured,
            "selected": self.selected,
            "base_url": self.base_url,
        }))
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
        //
        // **形状必须是 `bash -c <命令>`。** `PlatformSandbox` 只包这一种 spec，别的
        // 一律原样放行并报 `Unavailable`（"this backend only confines shell commands"）。
        // 拿 `ProcessSpec::new("true", …)` 去探，得到的是探针形状不对的回答，而这份
        // 报告会把它说成"你的沙箱没在工作"——一句在每台机器上都成立的假话，还会
        // 把整份报告拖成 degraded。见 `sandbox_probe_is_shaped_like_a_real_bash_call`。
        let probe = base::interface::exec::ProcessSpec::new("bash", std::env::temp_dir())
            .arg("-c")
            .arg("true");
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

/// `settings.json` 里 AttaCode **读了但不消费**的那些段。
///
/// 这一条不像另外四条那样报告"装成了什么"，它报告的是"你写的东西不会生效"。存在
/// 的理由是：`base::settings::Settings` 是全量的，daemon 消费一部分、AttaCode 消费
/// 另一部分，交集之外的字段用户写进去也白写——**而且今天没有任何一处会说话**。
/// daemon 至少在没有脚本引擎时会 `error!` 一句；我们连那句都没有。
///
/// 报出来不等于接上了。接不接是单独的排期，但"静默"是可以现在就取消的：一个人
/// 写了 3 条 `scripts` 然后发现脚本从来没跑过，他会先去查自己的 JavaScript，
/// 而那是最贵的一条排查路径。
struct UnusedSettingsCheck {
    ignored: Vec<(&'static str, usize, &'static str)>,
}

impl UnusedSettingsCheck {
    fn of(settings: &Settings) -> Self {
        let mut ignored = Vec::new();
        if !settings.scripts.is_empty() {
            ignored.push((
                "scripts",
                settings.scripts.len(),
                "this build carries no script engine (`script-host`), so no binding runs",
            ));
        }
        if !settings.task_models.is_empty() {
            ignored.push((
                "task_models",
                settings.task_models.len(),
                "no task router is built, so sub-agents all use the session model",
            ));
        }
        if settings.recorder.is_some() {
            ignored.push((
                "recorder",
                1,
                "record/replay is not wired, so nothing is recorded or replayed",
            ));
        }
        Self { ignored }
    }
}

impl HealthCheck for UnusedSettingsCheck {
    fn name(&self) -> &str {
        "settings.unused"
    }

    fn check(&self) -> CheckResult {
        if self.ignored.is_empty() {
            return CheckResult::ok("every configured section is consumed");
        }
        let summary = self
            .ignored
            .iter()
            .map(|(name, n, why)| format!("`{name}` ({n}): {why}"))
            .collect::<Vec<_>>()
            .join(" · ");
        // `Degraded` 而不是 `Ok`：用户写了东西、以为它在生效，而它没有。这正是
        // "有事情该让人知道"那一档。
        CheckResult::degraded(format!("configured but ignored — {summary}")).with_details(json!({
            "ignored": self.ignored.iter().map(|(n, _, _)| *n).collect::<Vec<_>>(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::health::HealthChecks;

    /// 默认那种装配：什么 provider 都没配，用合成的内置 anthropic。
    fn report(settings: &Settings, history_ok: bool) -> HealthReport {
        let builtin = base::provider::ProviderConfig {
            api_type: Some("anthropic".into()),
            ..Default::default()
        };
        HealthChecks::from_vec(checks(settings, Some(("anthropic", &builtin)), history_ok)).report()
    }

    /// 报告里必须五条都在。少一条的症状是"那件事没人替它说话"，而不是报错。
    #[test]
    fn every_check_is_in_the_report() {
        let r = report(&Settings::defaults_for("m"), true);
        let names: Vec<_> = r.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "model.provider",
                "history.store",
                "sandbox",
                "permissions",
                "settings.unused"
            ]
        );
    }

    /// 什么都没多写的时候，这条不该无端制造噪音。
    #[test]
    fn a_plain_settings_file_has_nothing_ignored() {
        let r = report(&Settings::defaults_for("m"), true);
        assert_eq!(
            r.find("settings.unused").unwrap().result.status,
            HealthStatus::Ok
        );
    }

    /// 写了却不生效的段必须点名。今天这三个是**完全静默**的——用户写了
    /// `scripts` 然后发现脚本从没跑过，会先去查自己的 JavaScript。
    #[test]
    fn sections_we_do_not_consume_are_named_rather_than_ignored() {
        let mut s = Settings::defaults_for("m");
        s.scripts = vec![base::settings::ScriptBinding {
            path: ".atta/scripts/x.js".into(),
            point: "prompt.assemble".into(),
            entry: "onAssemble".into(),
            timeout_ms: None,
            calls_per_turn: None,
        }];
        s.recorder = Some(base::settings::RecorderConfig {
            mode: base::settings::RecorderMode::Record,
            name: None,
            root: "/tmp/rec".into(),
            on_divergence: Default::default(),
        });

        let r = report(&s, true);
        let c = &r.find("settings.unused").unwrap().result;
        assert_eq!(c.status, HealthStatus::Degraded);
        assert!(c.summary.contains("scripts"), "got: {}", c.summary);
        assert!(c.summary.contains("recorder"), "got: {}", c.summary);
        assert!(
            c.summary.contains("script engine"),
            "得说清楚为什么不生效: {}",
            c.summary
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
        assert!(
            p.summary.contains("anthropic"),
            "得说出选中的是哪个: {}",
            p.summary
        );
    }

    /// 选中的 id 和被改过的端点都必须报出来。排查"模型答得不对"时，最先要确认的
    /// 就是请求发去了哪里——而这两件事人对着三层 settings 是推不出来的。
    #[test]
    fn a_configured_provider_is_named_along_with_its_endpoint() {
        let mut settings = Settings::defaults_for("deepseek-chat");
        settings
            .providers
            .insert("deepseek".into(), base::provider::ProviderConfig::default());
        let cfg = base::provider::ProviderConfig {
            api_type: Some("openai_compatible".into()),
            base_url: Some("https://api.deepseek.com/v1".into()),
            ..Default::default()
        };
        let r = HealthChecks::from_vec(checks(&settings, Some(("deepseek", &cfg)), true)).report();

        let p = &r.find("model.provider").unwrap().result;
        assert!(p.summary.contains("deepseek"), "got: {}", p.summary);
        assert!(
            p.summary.contains("https://api.deepseek.com/v1"),
            "端点被指到别处必须说出来: {}",
            p.summary
        );
    }

    /// **探针的形状是这条检查的全部前提。** `PlatformSandbox` 只包
    /// `bash -c <命令>`，别的一律原样放行并报 `Unavailable`——拿别的形状去探，得到的
    /// 是"探针不对"的回答，而报告会把它说成"你的沙箱没在工作"：一句在每台机器上都
    /// 成立的假话。这条断言和平台无关（Linux 上没装 bwrap 时后端确实不可用，那是
    /// 真话），它钉的只是"我们问对了问题"。
    #[test]
    fn the_sandbox_probe_is_shaped_like_a_real_bash_call() {
        let r = report(&Settings::defaults_for("m"), true);
        let sandbox = &r.find("sandbox").unwrap().result;
        let unmet = sandbox.details["unmet"].to_string();
        assert!(
            !unmet.contains("only confines shell commands"),
            "探针形状不对，这条检查报的是它自己的毛病: {sandbox:?}"
        );
    }

    /// macOS 上后端就在那儿，`base/sandbox` feature 也开着——所以它必须真的生效。
    /// 这条挂了就说明 `crates/bridge/Cargo.toml` 里那个 feature 掉了。
    #[cfg(target_os = "macos")]
    #[test]
    fn on_macos_the_sandbox_backend_is_actually_in_force() {
        let r = report(&Settings::defaults_for("m"), true);
        let sandbox = &r.find("sandbox").unwrap().result;
        assert_eq!(
            sandbox.status,
            HealthStatus::Ok,
            "macOS 上应该是 MacOSSandboxExec / Full: {sandbox:?}"
        );
    }

    /// 渲染出来的每一行都得有个人看得懂的名字和一句话。
    #[test]
    fn the_rendered_report_names_every_check() {
        let text = render(&report(&Settings::defaults_for("m"), false));
        assert!(text.starts_with("doctor: degraded"), "got: {text}");
        for name in [
            "model.provider",
            "history.store",
            "sandbox",
            "permissions",
            "settings.unused",
        ] {
            assert!(text.contains(name), "{name} 不在报告里:\n{text}");
        }
    }
}
