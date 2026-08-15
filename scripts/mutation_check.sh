#!/bin/bash
# 变异测试：往源码里注入一个"真实可能犯的错"，跑对应 crate 的测试，看红不红。
# 红 = 用例守住了这条行为；绿 = 这条行为没人守。
#
# 还原用的是文件快照而不是 `git checkout`——后者会把**尚未提交**的改动一起还原掉。
# 第一版正是这么把刚写好的测试自己删了，复验时还以为是测试没生效。
set -u
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
SNAP=$(mktemp -d)
trap 'rm -rf "$SNAP"' EXIT

caught=0
missed=0

run() {
  local name="$1" file="$2" from="$3" to="$4" crate="$5"
  local backup="$SNAP/$(echo "$file" | tr / _)"
  cp "$file" "$backup"
  python3 - "$file" "$from" "$to" <<'PY'
import sys
p, a, b = sys.argv[1], sys.argv[2], sys.argv[3]
s = open(p).read()
if a not in s:
    sys.exit(3)
open(p, "w").write(s.replace(a, b, 1))
PY
  if [ $? -eq 3 ]; then echo "⚠️  $name —— 匹配不到（源码变了？）"; cp "$backup" "$file"; return; fi
  if cargo test -p "$crate" >/dev/null 2>&1; then
    echo "🔴 $name —— 没抓到"
    missed=$((missed + 1))
  else
    echo "🟢 $name"
    caught=$((caught + 1))
  fi
  cp "$backup" "$file"
}

echo "=== 变异测试 ==="

# ── bridge / 归约器 ──
run "工具结果配对：不比对 id" crates/bridge/src/reducer.rs \
  'if *block_id == id => Some(result)' 'if true => Some(result)' bridge
run "折叠阈值差一" crates/bridge/src/reducer.rs \
  'if lines.len() <= FOLD_LINE_THRESHOLD || *expanded {' \
  'if lines.len() < FOLD_LINE_THRESHOLD || *expanded {' bridge
run "diff 识别：不要求表头成对" crates/bridge/src/reducer.rs \
  'w[0].starts_with("--- ") && w[1].starts_with("+++ ")' \
  'w[0].starts_with("-") || w[1].starts_with("+")' bridge
run "TodoWrite：任何工具的 input 都当清单读" crates/bridge/src/reducer.rs \
  'if name == TODO_TOOL {' 'if true {' bridge
run "resume：注入的 system-reminder 不过滤" crates/bridge/src/reducer.rs \
  'let head = match text.find("<system-reminder>") {
        Some(at) => &text[..at],
        None => text,
    };' 'let head = text;' bridge

# ── bridge / 命令分派 ──
run "权限：本会话允许塌成一次性允许" crates/bridge/src/handle.rs \
  'ApprovalOption::PermitSession => RuntimeDecision::PermitAlways {
                        scope: PersistScope::Session,
                    },' 'ApprovalOption::PermitSession => RuntimeDecision::Permit,' bridge
run "取消：发 Shutdown 而不是 CancelTurn" crates/bridge/src/handle.rs \
  'kind: EngineCommand::CancelTurn,
                        content: String::new(),' \
  'kind: EngineCommand::Shutdown,
                        content: String::new(),' bridge
run "/model：不把模型名发给 Core" crates/bridge/src/handle.rs \
  'kind: EngineCommand::UpdateModel,
                        content: name,' \
  'kind: EngineCommand::UpdateModel,
                        content: String::new(),' bridge
run "展开折叠：顺手也发给引擎" crates/bridge/src/handle.rs \
  'self.reducer.toggle_expand(&block_id);
                Ok(())' \
  'self.reducer.toggle_expand(&block_id);
                let _ = self.input_tx.send(InputMessage::System {
                    kind: EngineCommand::RefreshMcp,
                    content: String::new(),
                });
                Ok(())' bridge

# ── bridge / 装配 ──
run "settings：环境变量不再压过 settings.json" crates/bridge/src/bootstrap.rs \
  'if let Some(model) = &config.model_override {
        settings.model.model_name = model.clone();
    }' '' bridge
run "--resume：不校验 id 格式" crates/bridge/src/bootstrap.rs \
  'base::session::SessionId::parse(id).map_err(|e| BootstrapError::Resume {' \
  'Ok::<_, base::session::SessionIdError>(()).map_err(|e| BootstrapError::Resume {' bridge

# ── app ──
run "sticky header：可见性判断差一" crates/app/src/main.rs \
  'None => idx + page >= entries.len(),' 'None => idx + page > entries.len(),' app
run "sticky header：忽略滚动位置" crates/app/src/main.rs \
  'Some(offset) => idx >= offset && idx < offset + page,' 'Some(_) => true,' app
run "补全：已打全的命令仍然补全而不提交" crates/app/src/main.rs \
  '"editor.submit" if completion_active && !completion_already_typed(local) => {' \
  '"editor.submit" if completion_active => {' app
run "本地命令：前缀匹配（/models 也当 /model）" crates/app/src/main.rs \
  '"/model" => Some(LocalCommand::Model({' \
  'h if h.starts_with("/model") => Some(LocalCommand::Model({' app
run "词移动：跳过空白后不跳词身" crates/app/src/main.rs \
  'let trimmed = head.trim_end_matches(char::is_whitespace);
            trimmed
                .rfind(char::is_whitespace)' \
  'let trimmed = head;
            trimmed
                .rfind(char::is_whitespace)' app
run "块选择：越过最新块后停住而不是清空" crates/app/src/main.rs \
  '} else if next as usize >= blocks.len() {
                    None' \
  '} else if next as usize >= blocks.len() {
                    blocks.last().cloned()' app
run "输入历史：不去重连续相同" crates/app/src/main.rs \
  'if self.history.last().map(String::as_str) != Some(text) {' 'if true {' app

# ── tui ──
run "光标：块永远画在行尾" crates/tui/src/regions/composer.rs \
  'let local = state
            .cursor
            .checked_sub(offset)
            .filter(|rel| !state.locked && *rel <= segment.len());' \
  'let local = Some(segment.len()).filter(|_| !state.locked);' tui
run "选中竖条：画在所有行上" crates/tui/src/regions/transcript.rs \
  '(Some(id), Some(sel)) => id == sel,' '(Some(_), Some(_)) => true,' tui
run "layout：正文高度算错一行" crates/tui/src/layout.rs 'area.height' 'area.height + 1' tui

# ── 新补的那几层 ──
run "权限 tab：Tab 切了但快照不跟着切" crates/app/src/main.rs \
  'approval.active_idx = local
                .approval_active
                .min(approval.pending.len().saturating_sub(1));' \
  'approval.active_idx = 0;' app
run "权限 tab：切换后不复位选项高亮" crates/app/src/main.rs \
  'local.approval_active = step(local.approval_active, 1, pending);
            local.approval_selected = 0;' \
  'local.approval_active = step(local.approval_active, 1, pending);' app
run "光标：允许落在字符中间" crates/app/src/main.rs \
  'self.cursor -= prev.len_utf8();' 'self.cursor -= 1;' app
run "tick：状态行不再刷新" crates/bridge/src/reducer.rs \
  'refresh_running_status(&mut state);
        self.broadcast(&state);
    }

    /// 用户对某个待确认请求做出决定' \
  'return;
    }

    /// 用户对某个待确认请求做出决定' bridge

# ── 真跑挖出来的两个致命问题（回归） ──
run "工具注册表：只注册 web_search（等于没有文件工具）" crates/bridge/src/bootstrap.rs \
  'tools::register_builtin_tools(&tools);' '' bridge
run "diff：行首标记不去掉（屏幕上出现双重减号）" crates/bridge/src/reducer.rs \
  'LineKind::DiffOld | LineKind::DiffNew | LineKind::DiffContext => {
            let mut chars = line.chars();' \
  'LineKind::DiffOld | LineKind::DiffNew | LineKind::DiffContext => {
            #[allow(unreachable_code)]
            return line.to_string();
            let mut chars = line.chars();' bridge
run "对话框挤不下时不裁剪（回到修之前，按键提示被切掉）" crates/tui/src/regions/composer.rs \
  'Paragraph::new(fit_card(head, body, tail, inner.height)),' \
  'Paragraph::new({ let mut all = head; all.extend(body); all.extend(tail); all }),' tui

# ── keybindings ──
run "键位：Alt+Up 改绑到滚动" crates/keybindings/src/defaults.rs \
  'bind(
            "Alt+Up",
            "transcript.select-prev",' \
  'bind(
            "Alt+Up",
            "repl.scroll-up",' keybindings

echo "=== 抓到 $caught / 漏掉 $missed ==="
[ "$missed" -eq 0 ]
