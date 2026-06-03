# Engram — Claude Code 插件

仿人脑的**分层长期记忆系统**：会话开始注入记忆热索引、会话结束自动复盘巩固、按当前子项目动态挂载。引擎是单个 Rust 二进制，零外部依赖、零安装。

## 安装（像 claude-hud 一样一键）
```
/plugin marketplace add <你的GitHub用户名>/engram-plugin
/plugin install engram
```
然后重启一个新会话；首次可能需要在 `/hooks` 里**批准** engram 的 hook 命令。

## 它提供什么
- **三个 hook**
  - `SessionStart` → 注入"热索引"（公共 L1-3 + 当前目录/活跃子项目的 L4）
  - `UserPromptSubmit` → 按你正在动的子项目**动态挂载**它的 L4（活跃项目变化才重注入）
  - `SessionEnd` → 起一个**独立复盘者**读本场转录、按 `skills/engram/SKILL.md` 判定并写入/升降级
- **slash 命令**：`/engram list`、`/engram recall <词>`、`/engram status`、`/engram render`
- **状态栏指示**（可选，见下）

## 记忆存哪
- **公共库**（跨项目 L1-3）：`~/.engram/general.redb`（首次自动建）
- **项目库**（L4）：`<项目目录>/.claude/engram.redb`（随项目走）

## 查看 / 检验
```
/engram status          # 概况：各层条数、项目、冷库
/engram list            # 列全部记忆
/engram recall <词>     # 检索
/engram render          # 预览会注入什么
```

## 状态栏显示 "Engram"
Claude Code 全局**只允许一个 statusLine**。若你已用 claude-hud，用 `statusline/engram-statusline.sh` 把两者**组合**（先 claude-hud、再追加 `● Engram | L1:2 …`）：
```json
"statusLine": { "type": "command", "command": "bash \"<插件目录>/statusline/engram-statusline.sh\"", "refreshInterval": 5 }
```
它读 `~/.engram/status.txt`（hook 每轮刷新），不开数据库、极快。看到 `● Engram | …` 就说明在运行。

## 复盘者调用哪个 claude
`SessionEnd` 复盘者默认调用 PATH 上的 `claude -p`。若你的 CLI 不叫 `claude`（例如本地版 `claude-haha.exe`），设环境变量 `ENGRAM_REVIEWER_CLI` 指向它。

## 已知边界
- **Windows + bash**：hook 命令经 bash 执行，路径用正斜杠（脚本已处理）。`${CLAUDE_PLUGIN_ROOT}` 在 Windows 上的斜杠形式需首次安装时实测——若 hook 报 `command not found`，多半是反斜杠问题，改正斜杠即可。
- **二进制**：目前 `bin/` 只带 Windows `engram.exe`；跨平台需补 CI 交叉编译（Linux/macOS）+ 对应二进制，hook 里按平台选。
- 各层容量 / 衰减 / 阈值为实测起点值，可调（见设计文档 §13/§14）。
