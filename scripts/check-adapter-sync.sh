#!/usr/bin/env bash
# check-adapter-sync.sh —— 三端一致性校验入口
#
# 用法（在仓库任意位置执行均可）：
#   bash plugin/scripts/check-adapter-sync.sh
#
# 校验内容：主插件的两份"内核资产"与各适配器副本是否漂移——
#   1. scripts/reviewer-prompt.md：要求与主版**逐字节一致**
#      （codex / opencode 的转录格式说明由各自 launcher 在运行时追加，不进模板文件）；
#   2. skills/engram/SKILL.md：剔除适配器端特有段后要求与主版一致。
#      合法的端特有差异只允许放在成对标记
#      `<!-- adapter:begin ... -->` / `<!-- adapter:end -->` 之间
#      （现仅一段：引擎二进制定位说明），比对前会被剔除、空行折叠后 diff。
#   3. 各适配器 SKILL.md 副本之间要求逐字节一致（防适配器间互相漂移）。
#
# 有任何差异：非零退出并列出漂移文件；全部一致：exit 0。
# 改了主版 prompt / SKILL 后，跑一次本脚本确认已同步三端（同步方式见各差异行提示）。
set -u

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# 脚本位于 <仓库根>/plugin/scripts/ → 仓库根 = 上上级（adapters/ 与 plugin/ 平级）
root="$(cd "$script_dir/../.." && pwd)"

main_prompt="$root/plugin/scripts/reviewer-prompt.md"
main_skill="$root/plugin/skills/engram/SKILL.md"

prompt_copies=(
  "$root/adapters/codex/plugin/scripts/reviewer-prompt.md"
  "$root/adapters/opencode/opencode-plugin/scripts/reviewer-prompt.md"
  "$root/adapters/opencode/engram-desktop-bundle/engram-data/scripts/reviewer-prompt.md"
)
skill_copies=(
  "$root/adapters/codex/plugin/skills/engram/SKILL.md"
  "$root/adapters/opencode/opencode-plugin/skills/engram/SKILL.md"
  "$root/adapters/opencode/engram-desktop-bundle/engram-data/skills/engram/SKILL.md"
)

fail=0
report() { fail=1; printf '漂移: %s\n  ↳ %s\n' "$1" "$2"; }

# 剔除适配器特有段（adapter:begin/end 之间）并折叠连续空行，输出规范化文本。
# 比对前先 tr -d '\r' 统一行尾：主版为 CRLF，若副本混入 LF 行，Linux 上
# sed/cat -s 不吃 \r（"\r 行"不算空行、不被折叠），会把纯行尾差异误报成漂移；
# Git Bash 此前能过只是 MSYS 文本模式读文件时吞 \r 的巧合。
strip_adapter_blocks() {
  tr -d '\r' < "$1" | sed '/<!-- adapter:begin/,/<!-- adapter:end -->/d' | cat -s
}

# 1) reviewer-prompt.md：逐字节比对
for f in "${prompt_copies[@]}"; do
  if [ ! -f "$f" ]; then
    report "$f" "文件缺失（应为主版 $main_prompt 的逐字节副本）"
  elif ! cmp -s "$main_prompt" "$f"; then
    report "$f" "与主版不一致（应逐字节等于 $main_prompt，直接 cp 主版覆盖）"
  fi
done

# 2) SKILL.md：剔除 adapter 段后与主版比对
main_norm="$(strip_adapter_blocks "$main_skill")"
for f in "${skill_copies[@]}"; do
  if [ ! -f "$f" ]; then
    report "$f" "文件缺失（应为主版 $main_skill + adapter 标记段）"
  elif [ "$main_norm" != "$(strip_adapter_blocks "$f")" ]; then
    report "$f" "剔除 adapter 标记段后与主版不一致（主版改动需同步进来，端特有内容只能放 adapter:begin/end 之间）"
  fi
done

# 3) 各适配器 SKILL.md 副本之间逐字节一致
base="${skill_copies[0]}"
for f in "${skill_copies[@]:1}"; do
  if [ -f "$base" ] && [ -f "$f" ] && ! cmp -s "$base" "$f"; then
    report "$f" "与 $base 不一致（各适配器副本之间也必须逐字节相同）"
  fi
done

if [ "$fail" -ne 0 ]; then
  echo '结论：三端存在漂移（见上），请同步后重跑。'
  exit 1
fi
echo '三端一致：reviewer-prompt.md 与 SKILL.md 均无漂移。'
exit 0
