---
description: 预览 engram 会注入到会话上下文的"热索引"
---

# /engram render

```bash
engram hot-index --workspace-root "$PWD"
```

展示渲染出的热索引——**这就是 SessionStart hook 会注入进会话的内容**（公共 L1-3 + 当前目录及活跃子项目的 L4）。用于核对"会话开头到底看到了什么记忆"。
