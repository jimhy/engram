---
description: 项目知识库（本地 RAG）——文档入库与语义检索，首次使用按需下载组件
allowed-tools: Bash, Read, Glob
argument-hint: [ingest <路径>... | search <查询> | list | status | remove <doc_path>]
---

# /engram kb

engram 知识库：把项目文档分块、向量化存入本地 LanceDB，之后用中文语义 + 全文混合检索。
用户的请求在 `$ARGUMENTS`。组件是独立 sidecar（不在插件包里），首次使用需下载。

**重要：每次 Bash 调用都是独立 shell，变量不会跨调用保持。** 下面凡是 `<KB_BIN>` 这样的
角括号占位符，都要替换成你从前一步输出里**实际拿到的路径**再执行。

## Step 1：确保 sidecar 就绪（一条命令完成定位 + 检测）

```bash
PLUGIN="${CLAUDE_PLUGIN_ROOT:-$(ls -1d "${CLAUDE_CONFIG_DIR:-$HOME/.claude}"/plugins/cache/*/engram/*/ 2>/dev/null | sort -V | tail -1)}"; if [ -z "$PLUGIN" ]; then echo "ENGRAM_KB_PLUGIN_NOT_FOUND"; else bash "$PLUGIN/scripts/ensure-kb.sh" --check-only; fi
```

- 输出 `ENGRAM_KB_PLUGIN_NOT_FOUND` → 插件目录定位失败，告知用户并停止；
- 输出 `KB_BIN=<路径>` → 已就绪，**记下这个路径**，直接进 Step 2；
- 输出 `KB_MISSING=1` → **先停下来告诉用户**：首次使用知识库需要下载 engram-kb 组件
  （`KB_SIZE_HINT` 里的体积说明原样转告），**征得同意后**再执行下载（同样一条命令）：

```bash
PLUGIN="${CLAUDE_PLUGIN_ROOT:-$(ls -1d "${CLAUDE_CONFIG_DIR:-$HOME/.claude}"/plugins/cache/*/engram/*/ 2>/dev/null | sort -V | tail -1)}"; bash "$PLUGIN/scripts/ensure-kb.sh"
```

下载成功输出 `KB_BIN=<路径>`，记下它。下载失败时把 stderr 的中文错误转告用户，
不要重试超过一次。

## Step 2：按用户请求分发子命令

把 `<KB_BIN>` 替换为 Step 1 拿到的实际路径（含空格要加引号）。知识库目录自动锚定
当前项目（向上找 `.engram/`），无需显式传 `--db`。

**入库**（`ingest <文件或目录>...`）：

```bash
"<KB_BIN>" ingest --json <路径>...
```

- 增量的：内容没变的文档自动跳过，不会重复算嵌入；
- 首次运行会自动下载嵌入模型（约 95MB，国内可先 `export HF_ENDPOINT=https://hf-mirror.com`）——
  模型未就绪时 stderr 会有提示，转告用户即可，属预期行为；
- v1 支持 markdown / txt（`--ext` 可调）。

**检索**（`search <查询>` 或直接给一句自然语言）：

```bash
"<KB_BIN>" search --json --query "<用户的查询>" --limit 8
```

展示命中时给出 doc_path、面包屑（章节路径）、正文片段与分数。**分数解读**（RRF 融合分，
不是相似度）：≈0.033 表示该片段在向量与全文两路检索中**都排第一**（强共识）；≈0.016 表示
只在单路上榜；分数出现断崖（如 0.033 → 0.016）说明断崖之前的结果可信度高。
**提醒用户（也提醒你自己）：命中片段只是线索，要顺 doc_path + 面包屑去读原文拿完整
上下文，不要只凭片段下结论。**

**清单 / 概况 / 删除**：

```bash
"<KB_BIN>" list --json
"<KB_BIN>" status --json
"<KB_BIN>" remove --json --doc "<doc_path>"   # doc_path 须与 list 输出完全一致
```

## 错误处理

- 「不在任何 engram 项目内」→ 提示用户先 `/engram root` 建锚点，或在正确的项目目录下使用；
- 「知识库为空」→ 提示先 ingest；
- 「manifest 解析失败」→ 错误信息自带恢复办法（删 manifest.json + ingest --force），转告即可；
- 其余错误信息都是中文，原样转告即可。
