//! 打点 —— 记录 TUI 接口上**每一帧到底带了什么**。
//!
//! 装在 bridge 派生完 `FrameState`、广播给 app 之前那一刻，也就是"TUI 能看到的
//! 全部信息"这道口子上。每来一个 `AgentEvent` 写一行 JSON：这次是什么事件，
//! 事件之后各个区域（转录区/状态行/任务清单/子代理条/提问框/底栏）各有多少
//! 内容。跑完拿 `scripts/trace_report.py` 一汇总，就知道哪些区块**从来没收到过
//! 东西**——那正是"接了但其实是死的"最容易藏身的地方（子代理条曾经就是）。
//!
//! 默认完全关闭：只有设了 `ATTACODE_TRACE=<文件路径>` 才会写。关掉时开销是一次
//! `Option` 判断，不做任何格式化。

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::Mutex;
use tui::frame_state::*;
use tui::FrameState;

/// 打点输出。`None` = 没开。
pub struct Trace {
    out: Mutex<BufWriter<File>>,
    /// 上一行。摘要一模一样就不重复记——状态行每 500ms 一帧，不去重的话真信号
    /// 会被 tick 淹掉。
    last: Mutex<String>,
}

impl Trace {
    /// 按 `ATTACODE_TRACE` 决定要不要开。路径打不开就当没开——诊断工具不该让
    /// 程序起不来。
    pub fn from_env() -> Option<Self> {
        let path = std::env::var("ATTACODE_TRACE")
            .ok()
            .filter(|p| !p.is_empty())?;
        match File::create(&path) {
            Ok(f) => {
                tracing::info!(path = %path, "frame trace enabled");
                Some(Self {
                    out: Mutex::new(BufWriter::new(f)),
                    last: Mutex::new(String::new()),
                })
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %path, "could not open the trace file");
                None
            }
        }
    }

    /// 记一次按键：收到了什么键、解析成了哪个 action。
    ///
    /// 帧记录只说明"状态变成了什么"，说不了"这个键到底有没有进来"。真跑时
    /// Ctrl+C 一直没反应，光看帧记录只能看出"取消没发生"，看不出是键没送到、
    /// 解析没匹配、还是分派没接上——这条就是补那一段的。
    pub fn record_key(&self, key: &str, outcome: &str) {
        let line = format!(
            "{{\"event\":\"key\",\"key\":{},\"outcome\":{}}}",
            json_str(key),
            json_str(outcome)
        );
        if let Ok(mut out) = self.out.lock() {
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        }
    }

    /// 记一帧。`event` 是触发它的事件名（本地动作用 `local:xxx`）。
    pub fn record(&self, event: &str, frame: &FrameState) {
        let mut kinds: Vec<(String, usize)> = Vec::new();
        for entry in &frame.transcript.body.entries {
            let name = format!("{:?}", entry.kind);
            match kinds.iter_mut().find(|(k, _)| *k == name) {
                Some((_, n)) => *n += 1,
                None => kinds.push((name, 1)),
            }
        }
        let kinds = kinds
            .iter()
            .map(|(k, n)| format!("\"{k}\":{n}"))
            .collect::<Vec<_>>()
            .join(",");

        let status = match &frame.operation_status.status_line.content {
            Some(StatusContent::TurnRunning { activity, .. }) => format!("\"{activity}\""),
            Some(StatusContent::Compacting { .. }) => "\"compacting\"".into(),
            None => "null".into(),
        };
        let header = match &frame.transcript.header.text {
            Some(t) => json_str(t),
            None => "null".into(),
        };
        let line = format!(
            "{{\"event\":{},\"entries\":{},\"kinds\":{{{}}},\"header\":{},\"status\":{},\
             \"tasks\":{},\"sub_agents\":{},\"asks\":{},\"selected_block\":{},\
             \"model\":{},\"tok_in\":{},\"tok_out\":{},\"turns\":{}}}",
            json_str(event),
            frame.transcript.body.entries.len(),
            kinds,
            header,
            status,
            frame.operation_status.task_list.items.len(),
            frame.sub_agent_bar.agents.len(),
            frame
                .composer
                .content
                .ask
                .as_ref()
                .map(|a| a.pending.len())
                .unwrap_or(0),
            frame
                .transcript
                .body
                .selected_block
                .as_ref()
                .map(|s| json_str(s))
                .unwrap_or_else(|| "null".into()),
            json_str(&frame.footer_hints.model),
            frame.footer_hints.usage.token_in,
            frame.footer_hints.usage.token_out,
            frame.footer_hints.usage.turn_count,
        );
        if let Ok(mut last) = self.last.lock() {
            if *last == line {
                return;
            }
            last.clone_from(&line);
        }
        if let Ok(mut out) = self.out.lock() {
            let _ = writeln!(out, "{line}");
            // 每行都 flush：跑挂了/被 Ctrl+C 掉了也要留下现场，这东西就是干这个的。
            let _ = out.flush();
        }
    }
}

/// 最小的 JSON 字符串转义。不值得为打点拉 `serde_json` 进来——
/// 何况这里要转义的只有引号、反斜杠和控制字符。
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_escaping_covers_what_a_transcript_can_contain() {
        assert_eq!(json_str("plain"), "\"plain\"");
        assert_eq!(json_str("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(json_str("a\nb"), "\"a\\nb\"");
        assert_eq!(json_str("c:\\tmp"), "\"c:\\\\tmp\"");
        assert_eq!(json_str("\u{1}"), "\"\\u0001\"");
        assert_eq!(json_str("中文"), "\"中文\"");
    }

    /// 没设环境变量时必须是彻底关掉的——诊断开关不该有默认开销。
    #[test]
    fn tracing_is_off_unless_asked_for() {
        std::env::remove_var("ATTACODE_TRACE");
        assert!(Trace::from_env().is_none());
    }
}
