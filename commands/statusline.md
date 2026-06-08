---
description: 开关 engram 状态栏（on 启用 / off 关闭，默认关闭）：● Engram | L1:.. L2:.. L3:..
allowed-tools: Bash, Read, Edit, Write
argument-hint: [on|off]
---

# /engram statusline [on|off]

开关 engram 状态栏 `● Engram | L1:n L2:n L3:n`。**默认关闭**——装上插件不会自动显示状态栏，需你主动启用；不想要了可随时关闭。

| 用法 | 作用 |
|---|---|
| `/engram:statusline on`（或不带参数） | **启用**：把状态栏写进用户级 `settings.json` |
| `/engram:statusline off` | **关闭**：从 `settings.json` 移除 engram 状态栏配置 |

**先解析参数 `$ARGUMENTS`**：去掉首尾空格后比较，若为 `off` / `disable` / `关` / `关闭` → 执行下面的【关闭流程】；其余一切（含空、`on` / `enable` / `开` / `启用`）→ 执行【启用流程】。两个流程互斥，**只执行其一**。

---

## 关闭流程（off / disable / 关 / 关闭）

只移除 **engram 自己**写入的状态栏配置，**绝不动**用户为别的工具配置的 `statusLine`。

1. 目标文件：`${CLAUDE_CONFIG_DIR:-$HOME/.claude}/settings.json`（用户级）。先用 **Read** 读它。
2. 文件不存在、或文件里没有 `statusLine` 键 → 告诉用户「engram 状态栏本就未启用，无需关闭」，**停止**。
3. 文件有 `statusLine` 键：
   - 若 `statusLine.command` 串里**含** `engram-statusline.sh` → 这是 engram 配的，用 **Edit** 把整个 `statusLine` 键连同其值删掉，**逐字保留**其余所有设置。
   - 若 `statusLine.command` 指向的**不是** engram 脚本（不含 `engram-statusline.sh`）→ 状态栏是别处配的，**不要改动**，告诉用户「当前状态栏不是 engram 配置的，已保持原样」，**停止**。
4. 删除后告诉用户：

   > ✅ 已移除 engram 状态栏配置。**请完全退出 Claude Code 再重开**——`statusLine` 改动要重启才生效。重开后状态栏不再显示 `● Engram`。

（固化在 `~/.engram/engram-statusline.sh` 的脚本本身保留不动，只是不再被 `settings.json` 引用；下次 `on` 时直接复用、无需重装。）

---

## 启用流程（on / enable / 启用 / 不带参数）

把 engram 状态栏一键写进**用户级** `settings.json`，让状态栏常显 `● Engram | L1:n L2:n L3:n`。

**为什么需要这个命令**：`statusLine.command` 是用户级配置，**插件无法随分发自动写入**（plugin.json 没有 statusLine 字段，这也正是 engram 状态栏「默认关闭」的根本原因）。hooks 已经在每轮刷新 `~/.engram/status.txt`，状态栏脚本也随插件携带——唯独「把 `statusLine.command` 指向脚本」这一步必须落到用户自己的 `settings.json` 里。本流程就替用户完成这步，并把脚本固化到一个**与插件版本无关的稳定路径**，升级插件后无需重配。

严格按下面的步骤执行，每步用真实命令做，不要臆测路径。

### Step 0：识别平台

用环境 context 里的 `Platform:` 与 `Shell:` 值判断，**不要**用 `uname` 之类临时探测。engram 的 hooks 全部经 bash 执行，所以装了 engram 的机器一定有 bash（Windows 上即 Git Bash）——本命令的所有步骤统一走 **Bash 工具**。

### Step 1：定位插件脚本、固化到稳定路径、生成命令串

用 **Bash 工具**跑这一段（它会：找到最新版插件里的状态栏脚本 → 复制到 `~/.engram/engram-statusline.sh` 固定路径 → 解析出写进 settings.json 的正斜杠绝对路径并打印 `COMMAND=...`）：

```bash
CFG="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
# marketplace 感知的 glob + 版本号排序，取最新装的 engram 版本目录（结尾带 /）。
SRC=$(ls -1d "$CFG"/plugins/cache/*/engram/*/ 2>/dev/null | sort -V | tail -1)
if [ -z "$SRC" ] || [ ! -f "${SRC}statusline/engram-statusline.sh" ]; then
  echo "ENGRAM_NOT_INSTALLED"; exit 0
fi
mkdir -p "$HOME/.engram"
cp -f "${SRC}statusline/engram-statusline.sh" "$HOME/.engram/engram-statusline.sh"
chmod +x "$HOME/.engram/engram-statusline.sh" 2>/dev/null
# 解析固定路径的绝对正斜杠形式（Windows 转成 C:/... 这样 powershell 与 bash 都能跑）。
DEST=$(cd "$HOME/.engram" && pwd)
case "$DEST" in
  /[a-z]/*) DEST=$(cygpath -m "$DEST" 2>/dev/null || echo "$DEST");;  # Git Bash: /c/.. -> C:/..
esac
echo "COMMAND=bash \"${DEST}/engram-statusline.sh\""
```

- 若输出 `ENGRAM_NOT_INSTALLED`：插件没装好。让用户先 `/plugin install engram` 再重跑本命令，**停止**。
- 否则记下输出里 `COMMAND=` 后面的整串（形如 `bash "C:/Users/你/.engram/engram-statusline.sh"`），这就是要写进 settings.json 的 `statusLine.command`。

### Step 2：先测命令能出东西

用 **Bash 工具**跑（把 Step 1 得到的脚本路径填进去）：

```bash
echo '{}' | bash "$HOME/.engram/engram-statusline.sh"
```

应当几秒内打印一行（至少 `● Engram (idle)`，若已有记忆则是 `● Engram | L1:.. L2:.. L3:..`）。报错就先排查，别往下写配置。

### Step 3：合并写入 settings.json（保留原有设置）

目标文件：`${CLAUDE_CONFIG_DIR:-$HOME/.claude}/settings.json`（用户级）。

1. 先 **Read** 该文件。
2. **合并**——只增改 `statusLine` 这一个键，**逐字保留其余所有设置**：
   ```json
   {
     "statusLine": {
       "type": "command",
       "command": "<Step 1 得到的 COMMAND 串>"
     }
   }
   ```
   - 文件已存在且是合法 JSON → 用 **Edit** 加入/替换 `statusLine` 键。
   - 文件不存在 → 用 **Write** 写一个只含 `statusLine` 的最小 JSON。
   - 文件存在但 JSON 非法 → **报错并停止**，不要覆盖（让用户先修）。
3. **JSON 安全**：`command` 串里有双引号，写入时必须正确转义（用编辑器/序列化，别手工拼字符串）。

### Step 4：收尾

写好后告诉用户：

> ✅ 已写入 `statusLine` 配置，脚本固化在 `~/.engram/engram-statusline.sh`（与插件版本无关）。**请完全退出 Claude Code 再重开**——`statusLine` 改动要重启才生效。重开后状态栏应显示 `● Engram | …`。

补充说明（按需转告）：
- 状态栏靠 hooks 每轮刷新 `~/.engram/status.txt`，不开数据库、极快。
- 装了 claude-hud 的话，脚本会自动在前面拼上 claude-hud 的状态行；没装就只显示 Engram 段。
- 插件以后升级，固定路径不变、**无需重配**；仅当某次升级**改动了状态栏脚本本身**时，重跑一次 `/engram:statusline on` 即可同步最新脚本。
- 不想要状态栏了，随时 `/engram:statusline off` 关闭。
