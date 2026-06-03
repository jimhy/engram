#!/usr/bin/env bash
# 组合状态栏：先渲染 claude-hud 的状态行，再追加一个 Engram 指示段。
# 用法：把你 settings.json 的 statusLine.command 指向本脚本（bash 执行）。
#   "statusLine": { "type": "command", "command": "bash <插件>/statusline/engram-statusline.sh", "refreshInterval": 5 }
# Engram 段读 ~/.engram/status.txt（hot-index hook 每轮刷新），不开数据库、极快。

input=$(cat)   # 状态栏的 stdin（会话 JSON）只能读一次

# ---- claude-hud 段（复用你现有的 claude-hud 调用逻辑）----
hud=""
hud_dir=$(ls -d "${CLAUDE_CONFIG_DIR:-$HOME/.claude}"/plugins/cache/claude-hud/claude-hud/*/ 2>/dev/null \
  | awk -F/ '{ print $(NF-1) "\t" $0 }' | sort -t. -k1,1n -k2,2n -k3,3n -k4,4n | tail -1 | cut -f2-)
if [ -n "$hud_dir" ] && [ -f "${hud_dir}dist/index.js" ]; then
  hud=$(printf '%s' "$input" | "/c/Program Files/nodejs/node" "${hud_dir}dist/index.js" 2>/dev/null)
fi

# ---- Engram 段（快：读 hook 写的状态文件）----
eng=$(cat "$HOME/.engram/status.txt" 2>/dev/null)
[ -z "$eng" ] && eng="● Engram (idle)"

# ---- 合并输出 ----
if [ -n "$hud" ]; then
  printf '%s   %s' "$hud" "$eng"
else
  printf '%s' "$eng"
fi
