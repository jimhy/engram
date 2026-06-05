---
description: 列出 engram 记忆库里的记忆（层级 / 状态 / 重要度 / cue）
---

# /engram list

用 engram 引擎列出长期记忆并整理展示给用户。引擎二进制在插件 `bin/` 下、已加入 PATH，直接用 `engram` 即可。

**先锚定当前项目作用域**（引擎从当前目录向上找 `.engram/` 锚点，自动定位本项目的库）：

```bash
engram resolve --format json
```

它输出 `general_db`（公共库）/ `project_db`（当前项目库）/ `project_name` / `kind`。据此列出（把路径填进去，含空格的路径加引号）：

```bash
engram list --general-db "<general_db>" --project-db "<project_name>=<project_db>" --status all
```

- 用户在 `$ARGUMENTS` 里给了过滤条件（如 `--level L2`、`--status active`、`--project xxx`）就一并传入。
- 若 `kind` 是 `workspace`（当前在项目管理目录本身），`project_db` 即该管理目录的管理库。

把输出按层级清晰呈现（L1/L2/L3 + 项目 L4），并说明 `eff` 是当前有效活跃度、`INF` 表示置顶。
