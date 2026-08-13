# LSP Process Pool — 设计文档

**目标：** 将 AttaCore `LspTool` 从"每次调用新建进程"改为"进程池复用"，消除重复的 LSP server 启动开销（每个 2-5s）。

**范围：** 仅改 `crates/tools/src/lsp.rs`。不改 Tool trait，不改 LspInput schema。

---

## 1. 现状

```
每次 LSP 调用:
  spawn rust-analyzer → initialize (2-5s) → send 1 request → shutdown → kill
```

关键代码路径：`execute_lsp_request()`（lsp.rs:363-479）在函数内 spawn、函数结束时 drop child。

**问题：**
- 同一次 turn 中模型可能连续调多次 LSP（hover → definition → references），每次都重新初始化
- rust-analyzer 初始化要索引整个 crate，2-5 秒是常态
- LSP 协议本身就设计为长连接 —— initialize 之后可以发无限次请求

---

## 2. 设计方案

### 2.1 核心思路

新增 `LspManager` 持有活跃 server 的进程池。`LspTool` 变为有状态结构体，持有 `Arc<LspManager>`。

```
Tool call
  → LspManager.acquire(server_cmd, root_path)
    → 池中有匹配的且进程存活？→ 复用
    → 没有或已死？→ spawn + initialize + 存入池
  → 发 LSP 请求 → 收响应
  → 归还连接（不关进程）
```

### 2.2 数据结构

```rust
/// 进程池管理器。一个 Engine 实例持有一个 LspManager。
/// Clone 友好（内部用 Arc），可跨 tool invocation 共享。
#[derive(Clone)]
pub struct LspManager {
    inner: Arc<Mutex<LspManagerInner>>,
    idle_timeout: Duration,
}

struct LspManagerInner {
    /// 池：key = (server_cmd, canonical_root_path)
    servers: HashMap<PoolKey, LspHandle>,
}

type PoolKey = (String, PathBuf);

struct LspHandle {
    /// 已初始化的 LSP server 子进程
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// initialize 返回的 capabilities（预留，当前未使用）
    #[allow(dead_code)]
    capabilities: Value,
    /// 上次使用时间，用于空闲超时驱逐
    last_used: Instant,
    /// 递增的请求 ID，每次发请求 +1
    next_request_id: u64,
}
```

### 2.3 LspManager API

```rust
impl LspManager {
    /// 创建管理器。idle_timeout 为 0 时永不驱逐。
    pub fn new(idle_timeout: Duration) -> Self;

    /// 从池中获取或创建一个已初始化的 server 连接。
    /// 返回 (stdin_handle, stdout_reader, next_id_ref)。
    /// 连接在用完后通过 `release()` 归还。
    pub async fn acquire(
        &self,
        server_cmd: &str,
        root_path: &Path,
    ) -> Result<Lease<'_>, String>;

    /// 归还连接。标记 last_used = now。
    pub fn release(&self, key: &PoolKey);

    /// 驱逐所有空闲超过 idle_timeout 的连接。
    /// 由 Engine 的后台定时器每 5 分钟调用一次。
    pub fn evict_idle(&self);

    /// 当前活跃连接数（测试用）。
    pub fn active_servers(&self) -> usize;
}

/// RAII guard：drop 时自动归还连接。
struct Lease<'a> {
    manager: &'a LspManager,
    key: PoolKey,
    stdin: &'a mut ChildStdin,
    reader: &'a mut BufReader<ChildStdout>,
    next_id: &'a mut u64,
}
```

### 2.4 LspTool 改造

```rust
// 之前：
pub struct LspTool;

// 之后：
pub struct LspTool {
    manager: LspManager,
}

impl LspTool {
    /// 使用进程池构造（推荐）。
    pub fn new(manager: LspManager) -> Self { Self { manager } }

    /// 不使用池：每次调用即 spawn + shutdown。用于测试或单次调用场景。
    pub fn ephemeral() -> Self { Self { manager: LspManager::new(Duration::ZERO) } }
}
```

`call()` 改造要点：
- 当前 `execute_lsp_request()` 内的 spawn → initialize → request → shutdown 替换为：
  1. `manager.acquire(server_cmd, root_path)` 获取连接
  2. 使用连接发送请求、读取响应
  3. 归还连接（不 shutdown）
- 如果请求返回 LSP error，标记连接为 dead，下次 acquire 重新 spawn

### 2.5 错误恢复

```
send request → 成功 → release（归还连接）
send request → LSP error（如 server crash）→ remove from pool → 下次 acquire 重建
send request → 进程已死（write/read 失败）→ remove from pool → 当前调用立即 spawn 新连接
```

---

## 3. 并发模型

- `LspManager::acquire()` 持锁时间极短：只做 HashMap 查找/插入。spawn + initialize 在锁外完成。
- 同一个 `PoolKey` 同一时刻只有一个调用者使用（当前 LspTool 标记 `is_concurrency_safe = true`，但实际不会并发调同一个 server）。
- 如果未来需要同一 server 的并发请求，可改为 `tokio::sync::Semaphore` 保护每个 handle。

**当前策略：持锁 → 取出 handle → 释放锁 → 使用 handle → 持锁 → 放回 handle。** 使用期间不持锁。

---

## 4. 空闲驱逐

```rust
impl LspManager {
    pub fn evict_idle(&self) {
        let mut inner = self.inner.lock().unwrap();
        let now = Instant::now();
        inner.servers.retain(|_, h| {
            if now - h.last_used > self.idle_timeout {
                // drop handle → Child 被 kill_on_drop
                false
            } else {
                true
            }
        });
    }
}
```

Engine 侧集成：在 turn loop 的空闲点或定时器触发 `manager.evict_idle()`。

---

## 5. 实施步骤

### Step 1: 抽取 spawn + initialize 为独立函数

从 `execute_lsp_request()` 中抽取 `spawn_and_initialize()`：

```rust
async fn spawn_and_initialize(
    server_cmd: &str,
    root_path: &Path,
) -> Result<(Child, ChildStdin, BufReader<ChildStdout>, Value), String>
```

返回 `(child, stdin, reader, capabilities)`。不改现有行为，只是代码重组。

### Step 2: 实现 LspManager + LspHandle

文件内新增 `LspManager`、`LspHandle`、`Lease` 结构体及方法。实现 `acquire`、`release`、`evict_idle`。

### Step 3: 改造 LspTool

- `LspTool` 从 unit struct 改为持有 `LspManager`
- `call()` 从调用 `execute_lsp_request()` 改为 `manager.acquire() + send_request + release`
- 保留旧的 `execute_lsp_request()` 用于 `LspTool::ephemeral()` 路径

### Step 4: 适配调用方

AttaCore 的 daemon 和 CLI 中创建 `LspTool` 的地方改为 `LspTool::new(manager)`。共享同一个 `LspManager` 实例。

### Step 5: 注册空闲驱逐

在 Agent/Engine 的后台循环中调用 `manager.evict_idle()`。

---

## 6. 测试计划

```rust
// 单元测试（放在 lsp.rs 底部 #[cfg(test)] 中）

#[tokio::test]
async fn pool_reuses_server() {
    // 假设 rust-analyzer 在 PATH 上
    let manager = LspManager::new(Duration::from_secs(300));
    let tool = LspTool::new(manager.clone());

    // 第一次调用：创建新连接
    tool.call(json!({"operation":"hover","filePath":"src/main.rs","line":1,"character":1}), ...).await;
    assert_eq!(manager.active_servers(), 1);

    // 第二次调用：复用
    tool.call(json!({"operation":"definition","filePath":"src/main.rs","line":1,"character":1}), ...).await;
    assert_eq!(manager.active_servers(), 1);
}

#[tokio::test]
async fn pool_recovers_from_crashed_server() {
    // kill 进程后，下次调用自动重建
}

#[tokio::test]
async fn idle_eviction_cleans_up() {
    // 设 idle_timeout = 0，调 evict_idle，验证连接被回收
}

#[tokio::test]
async fn ephemeral_mode_no_pooling() {
    let tool = LspTool::ephemeral();
    tool.call(...).await;
    // 验证进程已退出（通过检查 PID）
}
```

---

## 7. 改动影响面

| 文件 | 改动 |
|---|---|
| `crates/tools/src/lsp.rs` | 主要改动：新增 ~200 行（LspManager 等），改 ~50 行（LspTool） |
| `daemon/src/main.rs` | LspManager 创建 & 注入 Agent builder |
| `crates/runtime/src/agent.rs` 或 `turn.rs` | 注册 evict_idle 调用 |

**不碰的文件：** Tool trait、LspInput schema、call() 的返回值格式、LSP 请求/响应逻辑。

---

## 8. 与 AttaCode 的关系

此改动在 AttaCore 侧完成。AttaCode 目前没有 LSP 集成（TUI 中无 LSP 引用），后续如果要接入，直接使用 `LspTool::new(manager)` 即可。
