//! Headless smoke test for the bootstrap → Agent → real API → AgentEvent → reducer →
//! FrameState pipeline. No terminal/keyboard involved — submits one fixed prompt and
//! prints transcript entries as they stream in, so it can run non-interactively.
//!
//! Usage: `set -a; . .env; set +a; cargo run -p bridge --example smoke`

use bridge::{BootstrapConfig, BridgeCommand};
use tui::frame_state::LineKind;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 没有 subscriber 的话 bridge/Core 里所有 `tracing::warn!` 都掉进黑洞——而这个
    // 示例存在的意义就是诊断。默认只放 warn 及以上，`RUST_LOG` 可以调。
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    // 和 `crates/app` 用同一个兜底模型；`ANTHROPIC_MODEL` / settings.json 照旧压过它。
    let config = BootstrapConfig::defaults(bridge::DEFAULT_MODEL);

    let (handle, cancel) = bridge::start(config).await?;
    let mut frame_rx = handle.subscribe();
    // 三层 settings.json 合并之后才知道最终用的是哪个模型，所以从快照里读。
    println!("model = {}", frame_rx.borrow().footer_hints.model);

    let prompt = "What is 2 + 2? Answer in one short sentence, no tools needed.";
    println!("> {prompt}");
    handle.dispatch(BridgeCommand::Submit {
        text: prompt.into(),
    })?;

    // `AssistantText` entries are mutated in place as TextDelta events accumulate
    // (same Vec index, growing string) rather than appended fresh each time, so
    // there's no clean "only the new part" diff to print incrementally here.
    // Wait for the turn to finish, then dump the whole final transcript once.
    let result = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            frame_rx.changed().await?;
            let snapshot = frame_rx.borrow().clone();

            if let Some(err) = snapshot
                .transcript
                .body
                .entries
                .iter()
                .find(|e| e.kind == LineKind::Error)
            {
                anyhow::bail!("agent reported an error: {}", err.text);
            }
            if snapshot.footer_hints.usage.turn_count >= 1 {
                for entry in &snapshot.transcript.body.entries {
                    println!("[{:?}] {}", entry.kind, entry.text);
                }
                println!(
                    "--- turn complete: {} in / {} out tokens ---",
                    snapshot.footer_hints.usage.token_in, snapshot.footer_hints.usage.token_out
                );
                return Ok::<(), anyhow::Error>(());
            }
        }
    })
    .await;

    cancel.cancel();

    match result {
        Ok(inner) => inner,
        Err(_) => anyhow::bail!("timed out waiting for turn completion (60s)"),
    }
}
