---
description: 查看 engram 记忆系统概况（各层条数 / 项目 / 冷库 / 墓碑）
---

# /engram status

```bash
engram status --general-db "$HOME/.engram/general.redb" --format full
```

把概况展示给用户：active 总数、通用层 L1/L2/L3 条数、各项目 L4、cold / superseded / tombstone 计数。用于确认记忆系统在正常工作、库里有多少东西。
