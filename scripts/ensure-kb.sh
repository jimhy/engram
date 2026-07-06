#!/usr/bin/env bash
# ensure-kb —— 检测 / 按需下载 engram-kb sidecar 二进制。
#
# 知识库组件体积大（几十 MB），不进插件包、不进 git 仓库；只在用户首次使用
# 知识库命令时从 GitHub Release 下载到 ~/.engram/bin/（版本无关稳定路径，
# 插件安装目录带版本号、升级即换，不可放下载产物）。
#
# 用法：
#   ensure-kb.sh --check-only   只检测，不下载。输出三选一：
#                                 · KB_BIN=<路径>                （就绪且最新，直接用）
#                                 · KB_OUTDATED=1 + 版本/体积 + KB_BIN（旧版，能用但建议升级）
#                                 · KB_MISSING=1 + KB_ASSET/KB_SIZE_HINT（没装）
#                               调用方（Claude）据此决定：直接用 / 提示升级 / 提示安装。
#   ensure-kb.sh                缺失则下载（含 sha256 校验），成功输出 KB_BIN=<路径>
#   ensure-kb.sh --force        无条件重新下载（升级 / 修复用）
#
# 平台选择逻辑与 bin/engram 启动器一致；Intel Mac 无 ONNX Runtime 预编译
# CPU 包（上游 ort rc.12 限制），明确报错而非给 404。
set -e

REPO="jimhy/engram"
# 本插件期望的 sidecar 版本：发 kb 新版时，把此处与 kb/Cargo.toml 的 version 一起 bump。
# check-only 时拿本地 `engram-kb --version` 与之比对，旧了就报 KB_OUTDATED 让 AI 提示升级。
KB_WANT_VERSION="0.3.0"
SIZE_HINT="约 60-80MB（另：首次入库/检索时还会自动下载嵌入模型约 95MB）"

case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*|Windows_NT)
    asset="engram-kb-windows-x86_64.exe" ;;
  Darwin)
    if [ "$(uname -m)" = "arm64" ]; then
      asset="engram-kb-macos-aarch64"
    else
      echo "engram-kb: Intel Mac 暂不支持（ONNX Runtime 无该平台预编译包）" >&2
      exit 2
    fi ;;
  Linux)
    if [ "$(uname -m)" = "aarch64" ]; then
      echo "engram-kb: linux-aarch64 暂未提供预编译二进制" >&2
      exit 2
    fi
    asset="engram-kb-linux-x86_64" ;;
  *)
    echo "engram-kb: 无法识别的平台 $(uname -s)" >&2
    exit 2 ;;
esac

dest_dir="$HOME/.engram/bin"
dest="$dest_dir/$asset"

# 已安装（且非 --force）：检测版本，按 --check-only / 普通调用决定输出。
if [ -f "$dest" ] && [ "${1:-}" != "--force" ]; then
  local_ver="$("$dest" --version 2>/dev/null | awk '{print $NF}')"
  outdated=0
  if [ -n "$local_ver" ] && [ "$local_ver" != "$KB_WANT_VERSION" ]; then
    # 语义版本排序：若期望版本排在本地版本之后，说明本地更旧 → 过时。
    older="$(printf '%s\n%s\n' "$KB_WANT_VERSION" "$local_ver" | sort -V | head -1)"
    [ "$older" = "$local_ver" ] && outdated=1
  fi
  if [ "${1:-}" = "--check-only" ]; then
    if [ "$outdated" = "1" ]; then
      echo "KB_OUTDATED=1"
      echo "KB_LOCAL_VERSION=$local_ver"
      echo "KB_WANT_VERSION=$KB_WANT_VERSION"
      echo "KB_UPDATE_HINT=约 60-80MB（仅更新程序本体；已建知识库与嵌入模型不受影响、无需重建）"
    fi
    echo "KB_BIN=$dest"
    exit 0
  fi
  # 普通调用（非 check-only、非 force）：旧版也能跑，直接给路径；升级走 check-only→--force。
  echo "KB_BIN=$dest"
  exit 0
fi

# 到这里：dest 不存在，或 --force 升级。仅检测模式报告缺失。
if [ "${1:-}" = "--check-only" ]; then
  echo "KB_MISSING=1"
  echo "KB_ASSET=$asset"
  echo "KB_SIZE_HINT=$SIZE_HINT"
  exit 0
fi

# 下载 / 升级（latest Release；sidecar 与主插件版本解耦，始终取最新）。
[ -f "$dest" ] && verb="更新" || verb="下载"
base="https://github.com/$REPO/releases/latest/download"
echo "engram-kb: 正在${verb} $asset（$SIZE_HINT）..." >&2
mkdir -p "$dest_dir"
if ! curl -fL --retry 3 --connect-timeout 15 -o "$dest.part" "$base/$asset"; then
  rm -f "$dest.part"
  echo "engram-kb: 下载失败。可能原因：网络不通 / 代理问题 / 当前 latest Release 未附带 kb 组件（404）。" >&2
  echo "engram-kb: 可稍后重试，或到 https://github.com/$REPO/releases 手动下载 $asset 放到 $dest_dir/" >&2
  exit 1
fi

# sha256 校验：能取到校验单则强校验，取不到则警告跳过（不挡使用）。
sums_file="$dest_dir/engram-kb-SHA256SUMS.tmp"
if curl -fsL --retry 2 --connect-timeout 15 -o "$sums_file" "$base/engram-kb-SHA256SUMS"; then
  expected="$(grep "  $asset\$" "$sums_file" | awk '{print $1}' || true)"
  if [ -n "$expected" ]; then
    if command -v sha256sum >/dev/null 2>&1; then
      actual="$(sha256sum "$dest.part" | awk '{print $1}')"
    else
      actual="$(shasum -a 256 "$dest.part" | awk '{print $1}')"
    fi
    if [ "$expected" != "$actual" ]; then
      rm -f "$dest.part" "$sums_file"
      echo "engram-kb: 下载校验失败（sha256 不匹配），已删除损坏文件，请重试" >&2
      exit 1
    fi
  else
    echo "engram-kb: 校验单中没有 $asset 的条目，跳过校验" >&2
  fi
  rm -f "$sums_file"
else
  echo "engram-kb: 未能获取校验单（Release 可能未附 SHA256SUMS），跳过校验" >&2
fi

mv "$dest.part" "$dest"
chmod +x "$dest" 2>/dev/null || true
echo "engram-kb: 下载完成" >&2
echo "KB_BIN=$dest"
