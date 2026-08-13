# AttaCode

AttaCode is a terminal UI (TUI) for the [AttaCore](https://github.com/openatta/AttaCore) AI agent engine. It is **not** an agent engine itself — all reasoning, tools, permissions, and session logic come from AttaCore. AttaCode is a thin glue layer plus a ratatui frontend on top of AttaCore's `AgentEvent` stream.

## Architecture

```
┌────────────────────────────────────────────────────────────────────┐
│                         AttaCode (this repo)                        │
│                                                                       │
│  crates/app/          bin `attacode` — terminal I/O, key dispatch,   │
│                        merges UI-local composer state onto the       │
│                        FrameState snapshot before each render        │
│         │                                                            │
│         ├─ crates/tui/         pure ratatui rendering.               │
│         │                      FrameState in, terminal frame out.    │
│         │                      Zero AttaCore dependency.             │
│         │                                                            │
│         ├─ crates/bridge/      the glue layer: bootstraps            │
│         │                      runtime::Agent, reduces AgentEvent    │
│         │                      into tui::FrameState, exposes         │
│         │                      EngineHandle. The only crate that     │
│         │                      knows both AttaCore and tui types.    │
│         │                                                            │
│         └─ crates/keybindings/ shortcut/chord parser + resolver      │
│                                                                       │
└───────────────────────────────┬───────────────────────────────────┘
                                 │ cargo path dependency
                                 ▼
┌────────────────────────────────────────────────────────────────────┐
│                     AttaCore (git submodule, core/)                  │
│  core/crates/{base, runtime, model, scene, tools, permissions,       │
│               mcp, hooks, skills, session, history, compaction, ...} │
│  core/daemon/    JSON-RPC reference consumer (not used by AttaCode)  │
└────────────────────────────────────────────────────────────────────┘
```

**Core principle**: AttaCode = AttaCore + TUI. `crates/tui` never touches an AttaCore type; `crates/bridge` never touches ratatui/crossterm. `crates/app` is the only place that depends on both, and only through `EngineHandle` + `tui::FrameState` — never directly on `runtime::Agent`/`AgentEvent`.

See `docs/design/2026-08-13-tui-core-glue-layer.md` for the full design rationale and the known Core-side gaps (interactive permission confirmation is not yet wired into `runtime::turn` — see that doc before relying on it).

## Data Flow

```
crossterm KeyEvent
  │
  ▼
keybindings::Resolver::on_key  ──►  action name (or Unmatched → composer edit)
  │
  ▼
crates/app: dispatch_action()
  ├─ local action (draft edit, scroll)  ──►  mutate LocalUi, no Core involved
  └─ Core-bound action                  ──►  EngineHandle::dispatch(BridgeCommand)
                                                    │
                                                    ▼
                                    bridge: InputSender.send(InputMessage) 
                                    → runtime::Agent's own serial input loop
                                    (this is what gives "submit while a turn
                                    is running" queueing semantics — bridge
                                    doesn't need to implement a queue itself)
                                                    │
                                                    ▼
                                        AttaCore AgentEvent stream
                                    (TextDelta | ToolUse | ToolResult |
                                     PermissionPrompt | TurnComplete | ...)
                                                    │
                                                    ▼
                                bridge::reducer::Reducer::apply_event()
                                  mutates an internal domain model (per-turn
                                  text buffers, tool-call blocks keyed by id,
                                  session usage), then derives a fresh
                                  tui::FrameState and broadcasts it over a
                                  tokio::sync::watch channel
                                                    │
                                                    ▼
                        crates/app render loop: merge(bridge_snapshot, LocalUi)
                          → tui::layout::render(frame, area, &state, spinner)
```

## FrameState (`crates/tui/src/frame_state.rs`)

`FrameState` is the one serializable, AttaCore-free snapshot the renderer consumes. See `docs/TUI_DESIGN.md` for the full Z0–Z4 region tree. It mixes two kinds of state:

- **Engine-truth**, owned and derived by `bridge`: transcript entries, pending permission approvals, sub-agent bar, cumulative session usage.
- **UI-local**, owned by `crates/app` and merged in just before rendering: composer draft/cursor, scroll position.

## AgentEvent → FrameState mapping (`bridge::reducer`)

| AttaCore `AgentEvent` | Reducer behavior |
|---|---|
| `TextDelta` | Appended to the current turn's streaming assistant-text block |
| `ToolUse` | New tool block (`id`-keyed) with a `ToolHeading` entry |
| `ToolResult` | Matched to its `ToolUse` by `id`; folded to a summary line past 8 lines (toggle to expand — see `BridgeCommand::ToggleExpand`) |
| `PermissionPrompt` | Pushed into `ApprovalState.pending`; composer locks until resolved. **Not yet reachable in practice** — `runtime::turn::execute_tool_inner` doesn't call the permission gate before executing a tool, so this event is never actually emitted by Core today |
| `TurnComplete` | Accumulates session token usage (footer, persistent) |
| `AgentSpawned` / `AgentCompleted` | Updates the sub-agent bar |
| `CompactAction` | Clears the transient turn-running status line |
| `Error` | Pushed as an `Error` transcript entry, does not stop the loop |
| `SystemInit` / `SessionPersisted` | No-op today |

## Key Bindings

Defaults come from `keybindings::default_bindings()`. `crates/app` currently wires up:

| Action | Key | Behavior |
|---|---|---|
| `editor.submit` | `Enter` | Submit the draft (or run local `/quit`/`/exit`) |
| `editor.clear` | `Ctrl-U` | Clear the draft |
| `repl.cancel` | `Ctrl-C` | `BridgeCommand::CancelTurn` |
| `repl.exit` | `Ctrl-D` | Quit when the draft is empty |
| `ask.confirm` / `ask.yes-shortcut` | `Enter` / `y` (in an approval) | Respond `PermitOnce` to the active approval |
| `ask.no-shortcut` / `repl.dismiss` | `n` / `Esc` | Respond `Deny` to the active approval |

`keybindings` also ships chords for history navigation, word/line kill, multi-line insert, and scroll (`editor.history.*`, `editor.kill-to-eol`, `repl.scroll-*`, `ask.prev`/`ask.next`) — resolved by `Resolver` but not yet dispatched to a behavior in `crates/app`. Composer editing itself is intentionally minimal right now: append/backspace at the end of the draft, no mid-line cursor movement.

## Slash commands

There is no slash-command subsystem yet. `crates/app::submit()` recognizes exactly `/quit` and `/exit` locally (quits without contacting Core); everything else — including other `/`-prefixed text — is forwarded to Core as plain text.

## Project Structure

```
AttaCode/
├── core/                     AttaCore git submodule (read-only dependency)
├── crates/
│   ├── tui/                  pure ratatui rendering
│   │   ├── src/frame_state.rs    FrameState + all region sub-states
│   │   ├── src/layout.rs         Z0..Z4 composition
│   │   ├── src/regions/          one renderer module per region
│   │   └── examples/layout_demo.rs   scripted visual demo, no Core involved
│   ├── bridge/                the glue layer
│   │   ├── src/bootstrap.rs      assembles Settings/Model/Scene → runtime::agent::Builder
│   │   ├── src/handle.rs         EngineHandle / BridgeCommand
│   │   ├── src/reducer.rs        AgentEvent → FrameState
│   │   └── src/permission.rs     GatePermission (implemented, not yet wired — see above)
│   ├── app/                   bin `attacode` — terminal event loop
│   └── keybindings/           shortcut/chord parser + resolver
├── docs/
│   ├── TUI_DESIGN.md              Z0–Z4 region design
│   ├── reqs/, design/             requirements + architecture docs per feature
│   └── README_CN.md               中文文档
├── scripts/                   dev helpers / AttaCore patch specs
├── Cargo.toml                 workspace: tui, keybindings, bridge, app
└── README.md                  this file
```

## Quick Start

```sh
# 1. Clone with submodule
git clone --recurse-submodules https://github.com/openatta/AttaCode.git
cd AttaCode

# 2. Configure credentials (see .env.example)
cp .env.example .env
# edit .env — needs ANTHROPIC_AUTH_TOKEN or ANTHROPIC_API_KEY at minimum

# 3. Build
cargo build --workspace

# 4. Test
cargo test --workspace

# 5. Run
set -a; . .env; set +a
cargo run -p app
```

**Prerequisites**: Rust (see `rust-toolchain.toml`), a C compiler (for AttaCore native deps), and an Anthropic-compatible API key.

## Development

```sh
# Format + lint
cargo fmt --all
cargo clippy --workspace -- -D warnings
```

Note: `core/` is its own separate Cargo workspace (submodule); `crates/bridge` reaches into it via path dependencies. `cargo clippy --workspace -D warnings` and `cargo fmt --all --check` therefore also surface `core/`'s own pre-existing lint/format state — that code is out of scope for this repo (see below), so when triaging a failure, check whether it's under `core/` before assuming it's a regression here.

## Relationship to AttaCore

AttaCode treats `core/` as a **read-only** dependency. Changes to the engine must go through the [AttaCore](https://github.com/openatta/AttaCore) repo:

1. `cd core` → create branch → make changes → `cargo test --workspace`
2. Open PR to `openatta/AttaCore`
3. After merge: `git pull origin main` in the submodule
4. Commit the submodule pointer bump in AttaCode: `AttaCode: bump AttaCore to <sha>`

Patch proposals that haven't been upstreamed yet live in `scripts/`.

## License

Apache-2.0 (see `Cargo.toml`'s `[workspace.package]`).
