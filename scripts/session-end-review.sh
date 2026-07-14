#!/usr/bin/env bash
# engram 插件 SessionEnd 复盘入口（跨平台薄壳）。
# hooks.json 统一走 bash 调本脚本：
#   - Windows（Git Bash / msys / cygwin）→ 原样转调既有 session-end-review.ps1，
#     stdin（hook JSON）直接继承给 powershell，行为与旧 hooks.json 逐字节一致；
#   - macOS / Linux → 在此执行等价 POSIX 逻辑：读 hook stdin → resolve 库路径 →
#     review-prepare 切增量并落 pending → launch-reviewer.sh 起一个完全脱管的复盘者。
# 防递归：复盘者自己也是一个 claude 会话，ENGRAM_REVIEWER=1 使其 SessionEnd 在此直接退出。
# 全程 best-effort：这里任何失败都不得影响会话收尾，恒 exit 0。
[ "$ENGRAM_REVIEWER" = "1" ] && exit 0

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="${CLAUDE_PLUGIN_ROOT:-$(dirname "$script_dir")}"

case "${OSTYPE:-$(uname -s 2>/dev/null)}" in
  msys*|cygwin*|MINGW*|MSYS*|CYGWIN*|Windows_NT)
    # Windows：转调既有 PowerShell 实现（stdin 原样继承），保持行为不变。
    exec powershell -NoProfile -ExecutionPolicy Bypass -File "$root/scripts/session-end-review.ps1"
    ;;
esac

# ---------- 以下为 macOS / Linux 的等价 POSIX 逻辑 ----------
case "$(uname -s)" in
  Darwin) case "$(uname -m)" in arm64|aarch64) bin="engram-macos-aarch64" ;; *) bin="engram-macos-x86_64" ;; esac ;;
  *)      bin="engram-linux-x86_64" ;;
esac
engram="${ENGRAM_BIN:-$root/bin/$bin}"
[ -x "$engram" ] || exit 0

# 读 hook stdin JSON（transcript_path / cwd / session_id）。有 timeout 命令则加 3 秒
# 兜底防挂起（macOS 原生无 timeout）；字段用 sed 抽取（无 jq 依赖，与 codex 适配器同款写法）。
if command -v timeout >/dev/null 2>&1; then
  raw="$(timeout 3 cat 2>/dev/null || true)"
else
  raw="$(cat 2>/dev/null || true)"
fi
jget() { printf '%s' "$raw" | sed -n "s/.*\"$1\"[ ]*:[ ]*\"\\([^\"]*\\)\".*/\\1/p"; }
transcript="$(jget transcript_path)"
cwd="$(jget cwd)"; [ -n "$cwd" ] || cwd="$(pwd)"
sid="$(jget session_id)"
[ -n "$transcript" ] || exit 0
[ -n "$sid" ] || sid="session"

work="$HOME/.engram/pending"
wm="$HOME/.engram/watermark.json"

# 经引擎解析当前作用域的库路径
paths="$("$engram" resolve --project-dir "$cwd" --format json 2>/dev/null)"
pget() { printf '%s' "$paths" | sed -n "s/.*\"$1\":\"\\([^\"]*\\)\".*/\\1/p"; }
gdb="$(pget general_db)"; pdb="$(pget project_db)"; pname="$(pget project_name)"

# 切增量 + 落 pending 标记；plan 为 review 时才起复盘者
plan="$("$engram" review-prepare --transcript "$transcript" --session-id "$sid" \
  --watermark "$wm" --work-dir "$work" --general-db "$gdb" --project-db "$pdb" --project-name "$pname" 2>/dev/null)"
case "$plan" in
  *'"action":"review"'*)
    # 用 bash 显式调起（与 hooks.json 风格一致）：仓库 core.fileMode=false，
    # .sh 以 644 入库，Linux/macOS 检出后无执行位，直接执行会 Permission denied。
    bash "$script_dir/launch-reviewer.sh" "$plan" >/dev/null 2>&1
    ;;
esac
exit 0
