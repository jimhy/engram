# Engram

> 为 Claude Code 打造的**类人长期记忆系统**——分层、会遗忘、自动巩固。会话开始注入相关记忆、会话结束自动复盘巩固。单 Rust 二进制、零依赖、不用向量数据库。

[English](./README.md) | **中文**

---

## 为什么

大多数 AI agent 的"记忆"是把一切塞进向量库、再把切片硬塞回上下文——费 token、噪声大、用起来别扭。Engram 反其道而行，照人类记忆的方式来：

- **记总结，不记细节。** 每条记忆 = 一句话**线索（cue）** + 一个指向 ground truth 的**指针**（`文件:行`、文档、URL）。先回忆线索，需要细节时顺指针去查——**是验证，不是脑补重建**。
- **只存"产物的补集"。** 代码本身就是最完美的细节存储（`grep` 就能找到）。Engram 只存代码里**没有**的：意图、决策、走过的死路、"为什么"。
- **遗忘噪声。** 记忆随时间衰减（ACT-R 式），除非被真实使用加固；低价值的会降级出热集。**遗忘 = 降级，不是删除。**

结果：上下文里始终是一小撮**高相关的"热索引"**，外加一个大得多、可检索的**冷库**——**而且不需要向量数据库**。

## 安装

```
/plugin marketplace add jimhy/engram
/plugin install engram
```

重启一个会话；若提示就在 `/hooks` 里批准。引擎是随插件携带的单个自包含 Rust 二进制——别的什么都不用装。

## 你会得到什么

**全自动记忆，读写双向：**

| Hook | 作用 |
|------|------|
| `SessionStart` | 把**热索引**（相关记忆）注入上下文 |
| `UserPromptSubmit` | **动态挂载**你当前正在做的子项目的 L4 记忆——只在活跃项目变化时才重注入 |
| `SessionEnd` | 起一个**独立复盘者**读本场转录并巩固：写入新记忆、升降级、标记取代、合并 |

**slash 命令：** `/engram list` · `/engram recall <词>` · `/engram status` · `/engram render`

**状态栏指示**（可选）：`● Engram | L1:2 L2:0 L3:1`

## 工作原理

记忆**分层**，仿人脑：

| 层级 | 角色 | 衰减 |
|------|------|------|
| **L1** | "潜意识"——核心身份 / 偏好 | 几乎不忘（高 floor） |
| **L2** | 重要 | 慢 |
| **L3** | 普通 | 中等 |
| **L4** | 项目级，存于 `<项目>/.claude/engram.redb` | 作用域内、按需挂载 |

- **activation = 重要度 + 近因 + 频率**（ACT-R base-level），每层带 floor，让 L1 站得住。
- **爬升要靠挣来的活跃度；下跌有 floor 和宽限期兜底**——新记忆、重要记忆不会被过早杀掉。
- **巩固**在会话结束由一个**独立**的 `claude -p` 复盘者读转录完成，所以"哪些真被用到、值得留"的判断不会自卖自夸。

## 记忆存哪

- **公共库**（跨项目 L1-3）：`~/.engram/general.redb`（首次自动建）
- **项目库**（L4）：`<项目>/.claude/engram.redb`（随项目走）

存储用 **[redb](https://github.com/cberner/redb)**（嵌入式、单文件、ACID）——无服务、无外部数据库。

## 查看 / 检验

```bash
/engram status            # 各层条数、项目、冷库
/engram list              # 列全部
/engram recall <词>       # 检索（冷热都搜）
/engram render            # 预览会注入什么
```

## 状态栏

Claude Code 全局只允许**一个** statusLine。若你已在用别的（如 [claude-hud](https://github.com/jarrodwatts/claude-hud)），用 `statusline/engram-statusline.sh` 把两者**组合**——先渲染对方的行，再追加 `● Engram | …`。把 `settings.json` 的 `statusLine.command` 指向该脚本即可。

## 配置

- `ENGRAM_REVIEWER_CLI` —— `SessionEnd` 复盘者启动哪个 CLI（默认 `claude`）。若你的 CLI 不叫 `claude`（比如某个本地版），设这个变量指向它。

## 平台与发布

`bin/engram` 是个小启动器，按你的操作系统挑对应二进制。**打 tag（`v*`）会触发 GitHub Actions**（[`.github/workflows/release.yml`](./.github/workflows/release.yml)）交叉编译 **Windows / Linux x86_64 / macOS x86_64 / macOS arm64**，把二进制提交进 `bin/` 并发布到 GitHub Release。（Linux arm64 暂未构建——欢迎 PR。）

引擎源码在 [`engine/`](./engine)（Rust）。本地构建：在 `engine/` 里 `cargo build --release`。

Windows 上 hook 经 bash 执行，路径用正斜杠（随附启动器/脚本已处理）。

## 许可证

MIT
