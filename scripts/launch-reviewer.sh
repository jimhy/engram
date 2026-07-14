#!/usr/bin/env bash
# engram - 起一个完全脱管的 headless 复盘者（macOS / Linux 版，Windows 走 launch-reviewer.ps1）。
# 由 session-end-review.sh 与 session-start-catchup.sh 共用。参数 1 = 引擎输出的 plan JSON
# （review-prepare / catchup-scan），字段用 sed 抽取，无 jq 依赖（与 codex 适配器同款）。
plan="$1"
[ -n "$plan" ] || exit 0

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="${CLAUDE_PLUGIN_ROOT:-$(dirname "$script_dir")}"
case "$(uname -s)" in
  Darwin) case "$(uname -m)" in arm64|aarch64) bin="engram-macos-aarch64" ;; *) bin="engram-macos-x86_64" ;; esac ;;
  *)      bin="engram-linux-x86_64" ;;
esac
engram="${ENGRAM_BIN:-$root/bin/$bin}"
cli="${ENGRAM_REVIEWER_CLI:-claude}"
wm="$HOME/.engram/watermark.json"
hook_log="$HOME/.engram/hook.log"
prompt_tpl="$root/scripts/reviewer-prompt.md"
skill="$root/skills/engram/SKILL.md"
[ -f "$prompt_tpl" ] || exit 0

jget() { printf '%s' "$plan" | sed -n "s/.*\"$1\":\"\\([^\"]*\\)\".*/\\1/p"; }
slice="$(jget slice)"; general="$(jget general_db)"; project="$(jget project_db)"
pname="$(jget project_name)"; pending="$(jget pending)"

# 作用域类型：管理目录库旁边有 'workspace' 标记（<dir>/.engram/workspace）
# → 复盘者应用 workspace 巩固规则。
kind="project"
[ -f "$(dirname "$project")/workspace" ] && kind="workspace"

prompt="$(cat "$prompt_tpl")"
prompt="${prompt//"{{TRANSCRIPT}}"/$slice}"
prompt="${prompt//"{{ENGRAM}}"/$engram}"
prompt="${prompt//"{{GENERAL_DB}}"/$general}"
prompt="${prompt//"{{PROJECT_DB}}"/$project}"
prompt="${prompt//"{{PROJECT_NAME}}"/$pname}"
prompt="${prompt//"{{KIND}}"/$kind}"
prompt="${prompt//"{{PENDING}}"/$pending}"
prompt="${prompt//"{{WATERMARK}}"/$wm}"
prompt="${prompt//"{{SKILL}}"/$skill}"

# 最小显式授权：headless（claude -p）下未被 allowlist 覆盖的工具调用会被直接拒绝且无人应答，
# 导致复盘静默失败。只放行 Read（读转录切片 / SKILL.md）与 engram 引擎二进制的 Bash 调用；
# 带引号变体覆盖模型给路径加引号的写法（路径含空格时必然如此）。
allow_read="Read"
allow_bash="Bash($engram:*)"
allow_bash_q="Bash(\"$engram\":*)"

# ---- 代理派生（堵 403）：headless 的 `claude -p` 复盘子进程若缺 HTTPS_PROXY 会走直连，
# 被 Anthropic 以「403 Request not allowed」按地区限制拒绝。启动前确保 HTTPS_PROXY/HTTP_PROXY
# 有值，优先级：① ENGRAM_REVIEWER_PROXY 显式指定；② 已有 HTTPS_PROXY/https_proxy → 继承不动；
# ③ ALL_PROXY/all_proxy → 据以设两者；④ 都没有 → 不设（直连，可能失败，留给下面的 403 告警）。
# 无注册表分支（POSIX 无系统代理注册表）。值可能是 host:port（无 scheme），统一补 http://。
engram_norm_proxy() {
  case "$1" in
    *://*) printf '%s' "$1" ;;
    *)     printf 'http://%s' "$1" ;;
  esac
}
if [ -n "${ENGRAM_REVIEWER_PROXY:-}" ]; then
  _p="$(engram_norm_proxy "$ENGRAM_REVIEWER_PROXY")"
  export HTTPS_PROXY="$_p" HTTP_PROXY="$_p"
elif [ -n "${HTTPS_PROXY:-}" ] || [ -n "${https_proxy:-}" ]; then
  :  # 继承环境里已有的代理
elif [ -n "${ALL_PROXY:-}" ] || [ -n "${all_proxy:-}" ]; then
  _src="${ALL_PROXY:-$all_proxy}"
  _p="$(engram_norm_proxy "$_src")"
  export HTTPS_PROXY="$_p" HTTP_PROXY="$_p"
fi

if [ "$ENGRAM_HOOK_DRYRUN" = "1" ]; then
  echo "[dry-run] reviewer-cli = $cli"
  echo "[dry-run] allowed      = $allow_read | $allow_bash | $allow_bash_q"
  echo "[dry-run] proxy        = ${HTTPS_PROXY:-}"
  echo "[dry-run] slice        = $slice"
  echo "[dry-run] general      = $general"
  echo "[dry-run] project      = $project ($pname, kind=$kind)"
  echo "[dry-run] pending      = $pending"
  echo "[dry-run] watermark    = $wm"
  echo "[dry-run] skill        = $skill"
  echo "[dry-run] prompt ${#prompt} chars"
  exit 0
fi

tmp="${TMPDIR:-/tmp}"
# 启动前清扫：临时目录下超过 7 天的 engram-review-* 残留一律删掉，防止无限堆积。
find "$tmp" -maxdepth 1 -name 'engram-review-*' -type f -mtime +7 -delete 2>/dev/null

stamp="$$-$RANDOM"
prompt_file="$tmp/engram-review-$stamp.txt"
out_file="$tmp/engram-review-out-$stamp.txt"
err_file="$tmp/engram-review-err-$stamp.txt"
printf '%s' "$prompt" > "$prompt_file" || exit 0

# 完全脱管启动（nohup + &），hook 立即返回、绝不阻塞会话收尾/启动。
# ENGRAM_REVIEWER=1 被子进程继承 → 复盘者自己的 hook 直接退出，不递归。
# 复盘者退出后把退出码 + out/err 尾部若干行追进 ~/.engram/hook.log，让静默失败可观测。
RV_CLI="$cli" RV_PROMPT="$prompt_file" RV_OUT="$out_file" RV_ERR="$err_file" \
RV_LOG="$hook_log" RV_A1="$allow_read" RV_A2="$allow_bash" RV_A3="$allow_bash_q" \
ENGRAM_REVIEWER=1 nohup bash -c '
  "$RV_CLI" -p --allowedTools "$RV_A1" "$RV_A2" "$RV_A3" < "$RV_PROMPT" > "$RV_OUT" 2> "$RV_ERR"
  ec=$?
  mkdir -p "$(dirname "$RV_LOG")" 2>/dev/null
  {
    printf "[%s] engram reviewer done exit=%s\n" "$(date "+%Y-%m-%d %H:%M:%S")" "$ec"
    tail -n 5 "$RV_OUT" 2>/dev/null | sed "s/^/  out| /"
    tail -n 5 "$RV_ERR" 2>/dev/null | sed "s/^/  err| /"
    # 认证/地区限制诊断：非零退出，或 out/err 含 403 / not allowed / authenticate 时，
    # 补一句常见根因提示（headless 子进程缺 HTTPS_PROXY 走直连触发地区限制）。
    if [ "$ec" -ne 0 ] || grep -qiE "403|not allowed|authenticate" "$RV_OUT" "$RV_ERR" 2>/dev/null; then
      printf "  diag| 复盘者请求被拒(403)——常见原因：headless 子进程缺 HTTPS_PROXY 走了直连触发地区限制；请确认 HTTPS_PROXY 已设或用 ENGRAM_REVIEWER_PROXY 指定\n"
    fi
  } >> "$RV_LOG" 2>/dev/null
' >/dev/null 2>&1 &
exit 0
