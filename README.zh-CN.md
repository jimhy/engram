# Engram

> 为 Claude Code 打造的**类人长期记忆系统**——分层、会遗忘、自动巩固。会话开始注入相关记忆、会话结束自动复盘巩固。单 Rust 二进制、零依赖、不用向量数据库。

[English](./README.md) | **中文**

> **在用 Codex,或 Claude Code 和 Codex 一起用?** 也有 Codex 适配器:**[engram-codex](https://github.com/jimhy/engram-codex)**。同一引擎、**共用记忆库**(`~/.engram` + 各项目 `.engram/`),两个 CLI 的记忆互通。

> **在用 Kimi Code?** 也有 Kimi 适配器:**[engram-kimi](https://github.com/jimhy/engram-kimi)**。同一引擎、**共用记忆库**,多个 CLI 的记忆互通。

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
/reload-plugins
```

`/reload-plugins`（或重启一个会话）即可生效；若提示就在 `/hooks` 里批准。引擎是随插件携带的单个自包含 Rust 二进制——别的什么都不用装。

## 更新

```
/plugin marketplace update engram-marketplace
/plugin update engram
/reload-plugins
```

之后 `/doctor` 应无插件报错。（若 `/plugin update` 没拉到新版本，就 `/plugin uninstall engram` 再 `/plugin install engram` 重新克隆最新版。）

## 你会得到什么

**全自动记忆，读写双向：**

| Hook | 作用 |
|------|------|
| `SessionStart` | 把**热索引**（相关记忆）注入上下文；并**补跑**上次会话异常退出（崩溃/强关）未完成的巩固 |
| `UserPromptSubmit` | 按最近的 **`.engram/` 锚点**重解析当前项目，并重注入其 L4——仅当项目作用域（根）变化时才触发 |
| `SessionEnd` | 起一个**独立复盘者**，只巩固**自上次水位线以来的增量**（长会话/续接会话更省 token）：写入新记忆、升降级、标记取代、合并 |

**slash 命令：** `/engram list` · `/engram recall <词>` · `/engram status` · `/engram render` · `/engram root` · `/engram statusline [on|off]`

**状态栏指示**（可选，**默认关闭**）：`● Engram | L1:2 L2:0 L3:1`

## 工作原理

记忆**分层**，仿人脑：

| 层级 | 角色 | 衰减 |
|------|------|------|
| **L1** | "潜意识"——核心身份 / 偏好 | 几乎不忘（高 floor） |
| **L2** | 重要 | 慢 |
| **L3** | 普通 | 中等 |
| **L4** | 项目级，存于 `<项目>/.engram/engram.redb` | 项目作用域，按 `.engram/` 锚点定位 |

各层另有**字符预算**（token 治理）：巩固时条数上限与累计渲染字符预算**双约束**，先触发者生效——常驻注入成本被钳在常数。

- **activation = 重要度 + 近因 + 频率**（ACT-R base-level），每层带 floor，让 L1 站得住。
- **爬升要靠挣来的活跃度；下跌有 floor 和宽限期兜底**——新记忆、重要记忆不会被过早杀掉。
- **巩固**在会话结束由一个**独立**的 `claude -p` 复盘者读转录完成，所以"哪些真被用到、值得留"的判断不会自卖自夸。

### 每层存什么

一条记忆值不值得留，看它**能不能从代码 / 文档 / git 轻易找回**——engram 只存 artifact 的**补集**（意图、为什么、试过的死路、决策、未完成的开口），提炼成一句话 cue + 一个指向 ground truth 的指针。

**通用 —— 跨项目，存公共库（`~/.engram/general.redb`）：**
- **L1**——核心身份 & 常驻全局规矩：你是谁、怎么称呼、用什么语言、雷打不动的全局约定。极少、几乎不忘。
- **L2**——跨项目通用的重要知识（某工具的坑、长期偏好）。
- **L3**——一般、易忘的通用笔记。

**项目级 —— L4，存该项目的库（`<项目>/.engram/engram.redb`）：**
- **L4.1**——项目铁律：**本仓库**不可违反的约定 / 禁忌，来自你的"永远 / 绝不"指令或踩坑确立。**不是**照抄 CLAUDE.md / lint 配置（那些是会被自动加载的 artifact）；L4.1 只存它们**没写**的隐性铁律。
- **L4.2**——持久项目知识：这项目是干嘛的、**架构 / 模块心智地图**（各部分干嘛、为什么这么分——提炼版，不是 `ls` 罗列）、已定型 / 已辩论的决策（选了什么、否了什么及原因——免得后续会话重提死方案）。
- **L4.3**——快衰减层：重要度低、时效短的记忆（当前进度、短命开口、可交接的活自然落这里）。层级不绑定内容类型——重要的长期开口按重要度落更高层；做完 / 失效核实后即删。

> 黄金法则：**只存提炼的心智模型，绝不存单条 `grep` / `ls` 就能拿到的东西。** 文件位置放在指针里，不放进 cue。

## 记忆存哪

- **公共库**（跨项目 L1-3）：`~/.engram/general.redb`（首次自动建）
- **项目库**（L4）：`<项目>/.engram/engram.redb`（随项目走）

存储用 **[redb](https://github.com/cberner/redb)**（嵌入式、单文件、ACID）——无服务、无外部数据库。

## 查看 / 检验

```bash
/engram status            # 各层条数、项目、冷库、复盘健康
/engram list              # 列全部
/engram recall <词>       # 检索（冷热都搜）
/engram render            # 预览会注入什么
engram export --dir <目录>  # 全量导出为 JSON（备份/迁移，直接调引擎二进制）
engram doctor --general-db ~/.engram/general.redb [--json]  # 只读体检：库版本/各层分布/坏行/超预算层/未来时间戳/悬空指针/importance 偏离层锚/疑似重复（绝不写库）
```

## 备份与迁移

官方路径是 `export` / `import`（对称互逆，逐条 JSON、含全部字段与 `schema_version`）：

```bash
# 备份：把公共库 + 项目库全量导出到目录（每条记忆一个 <id>.json）
engram export --general-db ~/.engram/general.redb --project-db 名称=路径 --dir ./backup
# 可选过滤：--project <名称> 只导某项目；--status active 只导活跃层

# 恢复/迁移：在目标机上导回（L1-3 进公共库、L4 按 --project-db 路由）
engram import --general-db ~/.engram/general.redb --project-db 名称=路径 --from-json-dir ./backup
```

redb 单文件（`general.redb` / `engram.redb`）也可以直接拷贝，但**必须在无会话活动时进行**
（hook 随时可能持有写锁，热拷贝可能得到半截事务）；跨机迁移推荐走 export/import——
import 还会顺手把异常的未来时间戳钳到当前时间（时钟回拨/跨机时钟差消毒）。

**版本迁移（自动、无需手动）**：引擎数据版本升级后，**升级后首次会话（SessionStart）会自动迁移一次、迁移前自动备份**（备份到 `<公共库父目录>/backups/`，逐条 JSON，可用 `import` 原样导回）。迁移是 best-effort、绝不阻断会话，成功后不再重复。想先只读体检、或手动预演 / 执行：

```bash
engram doctor  --general-db ~/.engram/general.redb [--json]    # 只读体检，绝不写库
engram migrate --general-db ~/.engram/general.redb --if-needed # 一次性迁移（迁移前自动备份）；加 --dry-run 只预演不写库
```

## 状态栏（可选，默认关闭）

engram 状态栏**默认关闭**——装好插件不会自动显示。想常显 `● Engram | L1:n L2:n L3:n`，用开关启用 / 关闭：

```
/engram:statusline on      # 启用（不带参数等同 on）
/engram:statusline off     # 关闭（移除配置，不影响你为别的工具配的状态栏）
```

`on` 会把状态栏脚本固化到 `~/.engram/engram-statusline.sh`（与插件版本无关的稳定路径），把 `statusLine.command` 合并进你的**用户级** `settings.json`，再提示你重启。状态栏内容由 hooks 每轮刷新的 `~/.engram/status.txt` 驱动——不开数据库、极快。装了 [claude-hud](https://github.com/jarrodwatts/claude-hud) 会自动在前面拼上它的状态行，没装就只显示 Engram 段。`off` 只移除 engram 自己写的那条配置、不碰你为别的工具配的 `statusLine`。

> 为什么默认关闭、还要这一步：`statusLine` 是用户级配置，插件**无法**随分发自动写入（plugin.json 没有该字段）——这既是「默认关闭」的根本原因，也是为什么得由本命令替你写进 `settings.json`。之后插件升级**无需重配**——仅当某次升级改动了状态栏脚本本身时，重跑一次 `/engram:statusline on` 同步即可。

## 配置

- `ENGRAM_REVIEWER_CLI` —— `SessionEnd` 复盘者启动哪个 CLI（默认 `claude`）。若你的 CLI 不叫 `claude`（比如某个本地版），设这个变量指向它。
- `ENGRAM_REVIEWER_PROXY` —— 显式指定复盘者子进程使用的代理（如 `http://127.0.0.1:7897`），优先级高于环境里已有的 `HTTPS_PROXY`/`ALL_PROXY` 与系统代理。

### 代理环境（中国大陆等地区）

复盘者是 hook 起的 **headless `claude -p` 子进程**。若你的 `claude` 靠本地代理访问 Anthropic，务必让 **`HTTPS_PROXY`（不能只有 `all_proxy`）对 hook 子进程可见**——否则子进程会走直连、被 Anthropic 以「403 Request not allowed」按地区限制拒绝（表现为复盘静默失败，`~/.engram/hook.log` 里出现 `403` / `not allowed` 诊断行）。engram 起复盘者前会**自动派生代理**，优先级：`ENGRAM_REVIEWER_PROXY`（显式）> 已有 `HTTPS_PROXY`/`https_proxy`（继承）> `ALL_PROXY`/`all_proxy`（据以派生）>（仅 Windows）系统代理注册表 `HKCU\...\Internet Settings`。多数情况下你无需手动配置；若自动派生不到，用 `ENGRAM_REVIEWER_PROXY` 直接指定即可。

## 平台与发布

`bin/engram` 是个小启动器，按你的操作系统挑对应二进制。**打 tag（`v*`）会触发 GitHub Actions**（[`.github/workflows/release.yml`](./.github/workflows/release.yml)）交叉编译 **Windows / Linux x86_64 / macOS x86_64 / macOS arm64**，把二进制提交进 `bin/` 并发布到 GitHub Release。（Linux arm64 暂未构建——欢迎 PR。）

引擎源码在 [`engine/`](./engine)（Rust）。本地构建：在 `engine/` 里 `cargo build --release`。

Windows 上 hook 经 bash 执行，路径用正斜杠（随附启动器/脚本已处理）。

## 许可证

Apache License 2.0 —— 见 [LICENSE](./LICENSE)。第三方依赖的许可证清单见 [THIRD-PARTY-NOTICES.md](./THIRD-PARTY-NOTICES.md)。
