#!/usr/bin/env bash
# SessionStart 注入：本项目已建 engram-kb 知识库时，注入一行概览，让 AI 一上来就
# 知道知识库的存在与内容（含哪些文档），相关问题会主动去检索。
#
# 无感知保证（三道静默闸，任一不满足都无输出、不注入、零干扰）：
#   1. 插件目录定位失败 → 退出；
#   2. sidecar 没装（ensure-kb.sh --check-only 不返回 KB_BIN）→ 退出，**不启动重二进制**
#      （没装 sidecar 即没有知识库数据，本就无可注入）；
#   3. 装了但当前项目没建知识库 / 已清空 → digest 只读 manifest，自身静默返回空。
# 故「没用知识库的人」全程只跑两个轻量 bash，零额外开销。
PLUGIN="${CLAUDE_PLUGIN_ROOT:-$(ls -1d "${CLAUDE_CONFIG_DIR:-$HOME/.claude}"/plugins/cache/*/engram/*/ 2>/dev/null | sort -V | tail -1)}"
[ -n "$PLUGIN" ] || exit 0

line=$(bash "$PLUGIN/scripts/ensure-kb.sh" --check-only 2>/dev/null | grep '^KB_BIN=' || true)
[ -n "$line" ] || exit 0
KB="${line#KB_BIN=}"

# digest 只读 manifest.json（不启动 lancedb/嵌入），自己处理「不在项目内/空库」→ 静默。
"$KB" digest --emit json 2>/dev/null || true
