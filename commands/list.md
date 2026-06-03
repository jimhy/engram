---
description: 列出 engram 记忆库里的记忆（层级 / 状态 / 重要度 / cue）
---

# /engram list

用 engram 引擎列出长期记忆并整理展示给用户。引擎二进制在插件 `bin/` 下、已加入 PATH，直接用 `engram` 即可。

执行（公共库默认在 `~/.engram/general.redb`）：

```bash
engram list --general-db "$HOME/.engram/general.redb" --status all
```

- 若当前目录是某项目（存在 `./.claude/engram.redb`），追加它的 L4 一并展示：
  `--project-db "$(basename "$PWD")=$PWD/.claude/engram.redb"`
- 用户在 `$ARGUMENTS` 里给了过滤条件（如 `--level L2`、`--status active`、`--project xxx`）就一并传入。

把输出按层级清晰呈现（L1/L2/L3 + 各项目 L4），并说明 `eff` 是当前有效活跃度、`INF` 表示置顶。
