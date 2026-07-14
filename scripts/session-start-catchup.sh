#!/usr/bin/env bash
# engram 插件 SessionStart 补跑入口（跨平台薄壳）。
# 上个会话若异常收尾（硬杀 / 关窗 / 断电），复盘者没跑完会留下 pending 标记；
# 下次会话启动时在这里扫到最近的残留并补跑巩固（切片即未巩固增量，不丢也不重做）。
# hooks.json 统一走 bash 调本脚本：
#   - Windows（Git Bash / msys / cygwin）→ 原样转调既有 session-start-catchup.ps1，行为不变；
#   - macOS / Linux → 在此执行等价 POSIX 逻辑：catchup-scan → launch-reviewer.sh。
# 防递归：ENGRAM_REVIEWER=1 时直接退出。补跑绝不能弄坏会话启动，恒 exit 0。
[ "$ENGRAM_REVIEWER" = "1" ] && exit 0

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="${CLAUDE_PLUGIN_ROOT:-$(dirname "$script_dir")}"

case "${OSTYPE:-$(uname -s 2>/dev/null)}" in
  msys*|cygwin*|MINGW*|MSYS*|CYGWIN*|Windows_NT)
    # Windows：转调既有 PowerShell 实现，保持行为不变。
    exec powershell -NoProfile -ExecutionPolicy Bypass -File "$root/scripts/session-start-catchup.ps1"
    ;;
esac

# ---------- 以下为 macOS / Linux 的等价 POSIX 逻辑 ----------
case "$(uname -s)" in
  Darwin) case "$(uname -m)" in arm64|aarch64) bin="engram-macos-aarch64" ;; *) bin="engram-macos-x86_64" ;; esac ;;
  *)      bin="engram-linux-x86_64" ;;
esac
engram="${ENGRAM_BIN:-$root/bin/$bin}"
[ -x "$engram" ] || exit 0

work="$HOME/.engram/pending"

# 问引擎是否有需要补跑的 pending（它同时会清理更老的残留）。
# --sessions-dir/--watermark：孤儿会话补登——强杀/断电时 SessionEnd 不触发、
# 连 pending 都没落下，引擎靠 UserPromptSubmit 留下的 active-sessions 登记
# 现场补建 pending 再领取（仍只补最近一场）。
plan="$("$engram" catchup-scan --work-dir "$work" \
  --sessions-dir "$HOME/.engram/active-sessions" \
  --watermark "$HOME/.engram/watermark.json" 2>/dev/null)"
case "$plan" in
  *'"action":"review"'*)
    # 用 bash 显式调起（与 hooks.json 风格一致）：仓库 core.fileMode=false，
    # .sh 以 644 入库，Linux/macOS 检出后无执行位，直接执行会 Permission denied。
    bash "$script_dir/launch-reviewer.sh" "$plan" >/dev/null 2>&1
    ;;
esac
exit 0
