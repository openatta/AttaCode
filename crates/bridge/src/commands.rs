//! Slash 命令目录 — 直接读 `Agent` 自己那份 `CommandRegistry`。
//!
//! 这里**不**再维护第二份技能视图。Core 的 registry 现在持有 `Arc<SkillManager>`
//! 现查现算（`runtime::commands::CommandRegistry`），补全弹窗里显示的就是提交后
//! Core 真正会解析的那一套：内置 local 命令、实时技能、插件 / MCP prompts，优先级
//! 也和 `resolve()` 一致。
//!
//! 刷新时机绑在 turn 边界：Core 在每个 turn 开头 `check_for_changes()`，集合有变化
//! 就发 `AgentEvent::SkillsChanged`，reducer 收到后调 [`CommandCatalog::refresh`]。
//! 代价是用户在两个 turn 之间新建的技能，要等下一个 turn 才出现在弹窗里——换掉的是
//! 一个独立 watcher 线程和"两份技能视图各说各话"的一致性问题。

use runtime::commands::CommandRegistry;
use std::sync::Arc;
use tokio::sync::watch;
use tui::frame_state::CompletionCandidate;

pub struct CommandCatalog {
    registry: Arc<CommandRegistry>,
    tx: watch::Sender<Vec<CompletionCandidate>>,
}

impl CommandCatalog {
    /// 用 `Agent::commands()` 拿到的句柄建目录，并立刻广播一次当前快照。
    ///
    /// 必须在 `tokio::spawn(agent.run(..))` **之前**取那个 `Arc`：`run()` 会
    /// `&mut self` 借走整个 session，spawn 之后就没有 `&Agent` 可问了。
    pub fn new(
        registry: Arc<CommandRegistry>,
    ) -> (Arc<Self>, watch::Receiver<Vec<CompletionCandidate>>) {
        let (tx, rx) = watch::channel(candidates(&registry));
        (Arc::new(Self { registry, tx }), rx)
    }

    /// 重新拉一次命令表并广播。由 reducer 在 `AgentEvent::SkillsChanged` 时调用。
    pub fn refresh(&self) {
        let _ = self.tx.send(candidates(&self.registry));
    }
}

/// `list_detailed()` 给 name/description，`argument_hint()` 给参数提示——两者都走
/// `resolve()` 的同一条优先级链，所以不会出现"弹窗里是技能版、提交后解析成插件版"。
fn candidates(registry: &CommandRegistry) -> Vec<CompletionCandidate> {
    registry
        .list_detailed()
        .into_iter()
        .map(|info| {
            let description = match registry.argument_hint(&info.name) {
                Some(hint) if !hint.is_empty() => format!("{}  (args: {hint})", info.description),
                _ => info.description,
            };
            CompletionCandidate {
                name: format!("/{}", info.name),
                description,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use skills::manager::{SkillManager, SkillSource};

    /// 一份最小的、由技能目录喂出来的 registry——和 `Builder::build()` 里的装配方式
    /// 相同（`from_skill_manager` + 内置 local 命令），只是目录换成临时目录。
    /// 返回 `SkillManager` 是为了让测试能模拟 Core 在 turn 开头做的那次重载。
    fn registry_over(dir: &std::path::Path) -> (Arc<SkillManager>, Arc<CommandRegistry>) {
        let mgr = Arc::new(SkillManager::new());
        let _ = mgr.load_dir(dir, SkillSource::User);
        let registry = Arc::new(CommandRegistry::from_skill_manager(Arc::clone(&mgr)));
        (mgr, registry)
    }

    #[test]
    fn user_invocable_skill_shows_up_with_its_argument_hint() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("commit.md"),
            "---\ndescription: Create a git commit\nargument_hint: message\n---\nbody",
        )
        .unwrap();

        let (_mgr, registry) = registry_over(tmp.path());
        let (_catalog, rx) = CommandCatalog::new(registry);
        let listed = rx.borrow().clone();

        let commit = listed.iter().find(|c| c.name == "/commit").unwrap();
        assert!(commit.description.contains("Create a git commit"));
        assert!(commit.description.contains("message"));
    }

    #[test]
    fn skips_skills_with_user_invocable_false() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("internal.md"),
            "---\ndescription: not for slash invocation\nuser_invocable: false\n---\nbody",
        )
        .unwrap();

        let (_mgr, registry) = registry_over(tmp.path());
        let (_catalog, rx) = CommandCatalog::new(registry);
        assert!(rx.borrow().iter().all(|c| c.name != "/internal"));
    }

    /// 换掉独立 `SkillManager` 的全部理由：registry 是现查现算的，构造之后才落到
    /// 磁盘、被 Core 重载进来的技能，`refresh()` 一拉就在——不需要 bridge 自己盯文件。
    /// （真实链路上那次重载由 `runtime::turn` 在每个 turn 开头的
    /// `check_for_changes()` 完成，随后才发 `SkillsChanged`。）
    #[test]
    fn refresh_picks_up_a_skill_added_after_construction() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, registry) = registry_over(tmp.path());
        let (catalog, rx) = CommandCatalog::new(registry);
        assert!(rx.borrow().iter().all(|c| c.name != "/deploy"));

        std::fs::write(
            tmp.path().join("deploy.md"),
            "---\ndescription: Ship it\n---\nbody",
        )
        .unwrap();
        let _ = mgr.load_dir(tmp.path(), SkillSource::User);
        catalog.refresh();

        assert!(rx.borrow().iter().any(|c| c.name == "/deploy"));
    }
}
