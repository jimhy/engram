---
description: 在 engram 记忆库里按关键词检索（冷库、热库都搜）
---

# /engram recall

用 engram 检索记忆。用户的查询在 `$ARGUMENTS`。

```bash
engram recall --general-db "$HOME/.engram/general.redb" --query "$ARGUMENTS"
```

- 若在某项目目录，追加 `--project-db "$(basename "$PWD")=$PWD/.claude/engram.redb"` 一起搜。
- 默认连冷库一起搜（recall 本就是"我们以前是否处理过 X"）。

展示候选（命中分 score、层级、状态、cue、指针）。提醒用户：拿到候选后**顺指针去查 ground truth**，不要凭印象。
