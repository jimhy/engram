#!/usr/bin/env bash
# ensure-engram —— 把引擎二进制收敛到**公共位置** `~/.engram/bin/`（全机一份，所有 CLI 共用）。
#
# 为什么要有这个脚本：各端（claude / codex / kimi / opencode）的二进制解析都已改成
#   ENGRAM_BIN 覆盖 → 公共位置 → 插件自带的兜底
# 「公共位置优先」本身有个反向风险——**公共位置那份可能比插件自带的旧**：
# 用户先装 A 端把 1.4.0 放进公共位置，后来 B 端升到 1.5.0（自带 1.5.0），
# 按「公共优先」B 端反而会去跑 1.4.0，悄悄降级且毫无提示。
# 本脚本就是那个收敛器：**谁新用谁**，让公共位置自然收敛到「所有已安装端里最新的那份」。
#
# 与 ensure-kb.sh 的关键差别：kb sidecar 有几十 MB、不进插件包，只能下载；
# 引擎本体只有约 3.4MB、**本来就躺在插件包的 bin/ 里**，所以这里的主路径是
# **本地复制**（零网络、毫秒级），下载只是自带二进制缺失时的兜底。
#
# 用法：
#   ensure-engram.sh              确保公共位置为最新；输出 ENGRAM_BIN=<路径>
#   ensure-engram.sh --check-only 只检测不落盘，输出 ENGRAM_STATE=ok|stale|missing + 版本
#   ensure-engram.sh --force      无条件用自带的覆盖公共位置（修复损坏用）
#
# 全程 best-effort：**任何失败都不得影响会话**，恒 exit 0，诊断只走 stderr。
set -u

REPO="jimhy/engram"

# ---------- 平台资产名（与 bin/engram launcher 的判定逐条对齐）----------
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*|Windows_NT) asset="engram-windows-x86_64.exe" ;;
  Darwin)
    if [ "$(uname -m)" = "arm64" ]; then asset="engram-macos-aarch64"; else asset="engram-macos-x86_64"; fi ;;
  Linux)
    if [ "$(uname -m)" = "aarch64" ]; then asset="engram-linux-aarch64"; else asset="engram-linux-x86_64"; fi ;;
  *) asset="engram-windows-x86_64.exe" ;;
esac

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
# plugin root：四端各注各的变量名（codex=PLUGIN_ROOT / claude=CLAUDE_PLUGIN_ROOT /
# kimi=KIMI_PLUGIN_ROOT），全都认一遍，都取不到就用脚本父目录（scripts/ 就在 root 下）。
# 这样本脚本四端**逐字节同一份**，不必各维护一个变体。
root="${PLUGIN_ROOT:-${CLAUDE_PLUGIN_ROOT:-${KIMI_PLUGIN_ROOT:-$(dirname "$script_dir")}}}"
self="$root/bin/$asset"          # 插件自带的那份
dest_dir="$HOME/.engram/bin"
dest="$dest_dir/$asset"          # 公共位置那份
mode="${1:-}"

# 取二进制自报版本（`engram 1.4.0` → `1.4.0`）；取不到返回空串。
bin_version() {
  [ -f "$1" ] || return 0
  "$1" --version 2>/dev/null | awk '{print $NF}'
}

# $1 是否严格旧于 $2（语义化版本比较；相等返回 1=否）。
# 版本号归一到三段、缺失段补 0（`1.4` → `1.4.0`）。
# 必须归一：裸 `sort -V` 认为 `1.4 < 1.4.0`，而 opencode 那份 TS 实现（isOlderVersion，
# 按 `.` 分段数值比较、缺失段补 0）判它俩相等——不归一两端就分叉，同一台机器上
# bash 端与 TS 端会对「谁更新」得出相反结论。这是两版实现对拍时实测抓到的。
norm_ver() { echo "${1:-0}" | awk -F. '{printf "%d.%d.%d", $1+0, $2+0, $3+0}'; }

is_older() {
  _a="$(norm_ver "$1")"; _b="$(norm_ver "$2")"
  [ "$_a" = "$_b" ] && return 1
  [ "$(printf '%s\n%s\n' "$_a" "$_b" | sort -V | head -1)" = "$_a" ]
}

# 落盘：原子替换（先写 .part 再 mv），避免别的会话正好读到半个文件。
install_from() {
  mkdir -p "$dest_dir" 2>/dev/null || return 1
  cp "$1" "$dest.part" 2>/dev/null || return 1
  chmod +x "$dest.part" 2>/dev/null || true
  mv -f "$dest.part" "$dest" 2>/dev/null || { rm -f "$dest.part"; return 1; }
  return 0
}

# ---------- 版本比较：一律实打实地问二进制 ----------
# ⚠ 这里**曾经**有个「字节数相同即认定同版本」的短路，为的是省掉两次 `--version`
# （各约 20ms）。它是错的，而且错得很隐蔽：只要某次发版没动 Rust 源码（只改脚本 /
# 文档 / 清单），四平台二进制字节数就**完全相同**——v1.4.0 与 v1.5.0 的
# engram-windows-x86_64.exe 都是 3431936 字节，实测撞上。那不是「漏掉一次同步」，
# 而是此后**永久**判成 ok、收敛器彻底失效，公共位置一直停在旧版。
# 而「只改脚本不改引擎」恰恰是最常见的发版类型，所以这条短路必须去掉。
# 代价可以接受：本脚本只挂 SessionStart（每会话一次），而 SessionStart 本来就有
# hot-index / catchup / kb-digest 三个 hook 各起一次进程，多这 40ms 无关痛痒。
have="$(bin_version "$dest")"
want="$(bin_version "$self")"

# ---------- --force：无条件用自带的覆盖 ----------
if [ "$mode" = "--force" ]; then
  if [ -f "$self" ] && install_from "$self"; then
    echo "engram: 已用插件自带的 $want 覆盖公共位置 $dest" >&2
    echo "ENGRAM_BIN=$dest"
  else
    echo "engram: --force 失败（插件自带二进制不可用：$self）" >&2
    [ -f "$dest" ] && echo "ENGRAM_BIN=$dest"
  fi
  exit 0
fi

# ---------- 判定状态 ----------
state="ok"
if [ -z "$have" ]; then
  state="missing"
elif [ -n "$want" ] && is_older "$have" "$want"; then
  state="stale"
fi

if [ "$mode" = "--check-only" ]; then
  echo "ENGRAM_STATE=$state"
  [ -n "$have" ] && echo "ENGRAM_HAVE=$have"
  [ -n "$want" ] && echo "ENGRAM_WANT=$want"
  [ -f "$dest" ] && echo "ENGRAM_BIN=$dest"
  exit 0
fi

# ---------- 已是最新：什么都不做 ----------
# 注意 have 比 want 新也走这里——那说明别的端装了更新的版本，公共位置本就该保留它。
# 这正是「谁新用谁」：各端各自把自己那份往上抬，公共位置收敛到全机最新。
if [ "$state" = "ok" ]; then
  [ -f "$dest" ] && echo "ENGRAM_BIN=$dest"
  exit 0
fi

# ---------- 缺失 / 过旧：优先本地复制 ----------
if [ -f "$self" ]; then
  if install_from "$self"; then
    if [ "$state" = "missing" ]; then
      echo "engram: 已把引擎 $want 装到公共位置 $dest（全机共用一份）" >&2
    else
      echo "engram: 公共位置的 $have 旧于插件自带的 $want，已就地更新" >&2
    fi
    echo "ENGRAM_BIN=$dest"
    exit 0
  fi
  echo "engram: 复制到公共位置失败，继续用插件自带的（不影响使用）" >&2
  echo "ENGRAM_BIN=$self"
  exit 0
fi

# ---------- 自带的也没有：从 Release 兜底下载 ----------
# 走到这里说明是精简安装（插件包里没带二进制）。失败不报错、不挡会话。
if ! command -v curl >/dev/null 2>&1; then
  echo "engram: 无 curl，无法兜底下载；请手动放置 $dest" >&2
  exit 0
fi
base="https://github.com/$REPO/releases/latest/download"
mkdir -p "$dest_dir" 2>/dev/null || exit 0
if curl -fL --retry 2 --connect-timeout 15 -o "$dest.part" "$base/$asset" 2>/dev/null; then
  chmod +x "$dest.part" 2>/dev/null || true
  mv -f "$dest.part" "$dest" 2>/dev/null && echo "ENGRAM_BIN=$dest"
  echo "engram: 已从 latest Release 下载引擎到 $dest" >&2
else
  rm -f "$dest.part" 2>/dev/null || true
  echo "engram: 兜底下载失败（网络/代理/Release 无该资产），本次跳过" >&2
fi
exit 0
