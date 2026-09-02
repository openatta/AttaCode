//! `/resume` 的候选列表 —— 本项目里有哪些会话，以及它们各自是关于什么的。
//!
//! AttaCore 0.2.0 把"给我若干个会话"做成了一个独立的契约（`history.query`），和
//! "给我某一个会话"分开。这之前 AttaCode 只用得上 `list_recent_sessions`，于是
//! `--resume` 只能手打一个 BASE58 的 id——一个人得先自己去
//! `~/.atta/projects/<项目>/` 底下翻文件名。
//!
//! 这里把结果投影成 [`CompletionCandidate`]（`name` + `description`）而不是往上传
//! `history::store::SessionSummary`：`crates/app` 不许看见任何 AttaCore 类型，
//! 而补全弹窗要的本来就是这两个字段。
//!
//! # 为什么搜索和"最近几个"是同一个函数
//!
//! 因为对 Core 来说它们本来就是同一个问题的两种形状：`SessionQuery` 的 `text` 为空
//! 就是"最近的那些"。分成两个函数只会多一个两边可能说不一样话的地方。

use history::store::{JsonlHistoryStore, SessionSummary};
use tui::frame_state::CompletionCandidate;

/// 弹窗里一次最多列这么多。
///
/// 不是性能考虑——`SummaryDetail::Full` 每条要读一次转录，而弹窗本来也放不下更多。
/// 用户要找更早的东西，用 `/resume <关键词>` 而不是往下翻两百行。
pub const MAX_CANDIDATES: usize = 20;

/// 本项目的会话，`query` 为空时是最近的几个。
///
/// 出错不是致命的：`/resume` 是个查询，查不到就该是一份空列表加一句话，而不是让
/// 整个 TUI 停下来。调用方拿到空列表时说"没有找到"。
pub async fn candidates(store: &JsonlHistoryStore, query: &str) -> Vec<CompletionCandidate> {
    let query = query.trim();
    let found = if query.is_empty() {
        store.list_recent_session_summaries(MAX_CANDIDATES).await
    } else {
        store.search_session_summaries(query, MAX_CANDIDATES).await
    };
    match found {
        Ok(summaries) => summaries.iter().map(describe).collect(),
        Err(e) => {
            tracing::warn!(error = %e, query, "failed to list sessions for /resume");
            Vec::new()
        }
    }
}

/// 一条摘要 → 弹窗里的一行。
///
/// `name` 必须是**完整的 session id**，因为选中之后它就是 `--resume` 的参数。
/// 展示归 `description` 管。
fn describe(s: &SessionSummary) -> CompletionCandidate {
    let subject = s
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| first_line(&s.preview));
    CompletionCandidate {
        name: s.session_id.to_string(),
        description: format!("{}  {} msgs  {}", s.last_modified, s.message_count, subject),
    }
}

/// 预览的第一行，掐到一个弹窗放得下的长度。
///
/// 按**字符**掐而不是按字节：中文一个字三个字节，按字节切会把它切成两半，输出
/// 一个渲染不出来的字符。
fn first_line(preview: &str) -> String {
    const MAX_CHARS: usize = 60;
    let line = preview.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let line = line.trim();
    if line.chars().count() <= MAX_CHARS {
        return line.to_string();
    }
    line.chars().take(MAX_CHARS).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::session::SessionId;

    fn summary(preview: &str, title: Option<&str>) -> SessionSummary {
        SessionSummary {
            session_id: SessionId::new(),
            last_modified: "2026-09-02 10:00".into(),
            entry_count: 9,
            message_count: 4,
            preview: preview.into(),
            canonical_cwd: None,
            title: title.map(Into::into),
            total_input_tokens: None,
            total_output_tokens: None,
            compact_count: 0,
        }
    }

    /// `name` 就是选中之后要交给 `--resume` 的东西，一个字都不能加。
    #[test]
    fn the_candidate_name_is_the_whole_session_id() {
        let s = summary("hello", None);
        let c = describe(&s);
        assert_eq!(c.name, s.session_id.to_string());
        assert!(SessionId::parse(&c.name).is_ok());
    }

    #[test]
    fn a_title_wins_over_the_preview() {
        let c = describe(&summary("first message", Some("重构权限门")));
        assert!(c.description.contains("重构权限门"));
        assert!(!c.description.contains("first message"));
    }

    #[test]
    fn the_description_carries_when_and_how_big() {
        let c = describe(&summary("hi", None));
        assert!(c.description.contains("2026-09-02 10:00"));
        assert!(c.description.contains("4 msgs"));
    }

    /// 按字符掐，不按字节——中文一个字三个字节，按字节切会切出半个字符。
    #[test]
    fn a_long_cjk_preview_is_truncated_on_a_character_boundary() {
        let long = "问".repeat(200);
        let out = first_line(&long);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), 61);
    }

    /// 预览的头几行可能是空的（比如一条只有换行的消息）。
    #[test]
    fn an_empty_preview_still_produces_a_row() {
        let c = describe(&summary("\n\n", None));
        assert!(c.description.contains("4 msgs"));
    }
}
