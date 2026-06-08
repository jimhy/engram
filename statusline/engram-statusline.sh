#!/usr/bin/env bash
# Engram 状态栏脚本：输出一行「● Engram | L1:.. L2:.. L3:..」。
#
# 由 settings.json 的 statusLine.command 以 `bash <此脚本>` 调用（见 /engram:statusline）。
# Engram 段读 ~/.engram/status.txt（hot-index hook 每轮刷新），不开数据库、极快。
# 若**恰好**也装了 claude-hud 且能找到 node，则在前面拼上 claude-hud 的状态行；
# 没装 claude-hud / 找不到 node 时**静默跳过**，只输出 Engram 段——脚本对所有人可用。

input=$(cat)   # 状态栏 stdin（会话 JSON）只能读一次

CFG="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"

# ---- 可选 claude-hud 段（仅当装了 claude-hud 且 node 在 PATH 时）----
# node 用 `command -v` 动态解析，绝不写死本机绝对路径（旧版曾硬编码 node 路径，
# 换台机器即失效）。插件目录用 marketplace 感知的 glob + 版本号排序取最新版。
hud=""
node_bin=$(command -v node 2>/dev/null)
if [ -n "$node_bin" ]; then
  hud_dir=$(ls -1d "$CFG"/plugins/cache/*/claude-hud/*/ 2>/dev/null | sort -V | tail -1)
  if [ -n "$hud_dir" ] && [ -f "${hud_dir}dist/index.js" ]; then
    hud=$(printf '%s' "$input" | "$node_bin" "${hud_dir}dist/index.js" 2>/dev/null)
  fi
fi

# ---- Engram 段（快：读 hook 每轮刷新的状态文件）----
eng=$(cat "$HOME/.engram/status.txt" 2>/dev/null)
[ -z "$eng" ] && eng="● Engram (idle)"

# ---- 合并输出 ----
if [ -n "$hud" ]; then
  printf '%s   %s' "$hud" "$eng"
else
  printf '%s' "$eng"
fi
