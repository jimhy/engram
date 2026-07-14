//! Engram 引擎二进制入口。
//!
//! 本切片把持久化从**公共库 + 单个项目库**扩展为**公共库 + 多个项目库**：
//! - **公共库（general）**：存通用记忆 L1/L2/L3（`Memory.project == None`），
//!   位于用户目录；CLI 用 `--general-db <path>` 指定（所有命令必填）。
//! - **项目库（project）**：每个目录（含工作区根）各一个项目库，存该项目的
//!   L4 记忆（L4.1/L4.2/L4.3，`Memory.project == Some(name)`）。一次会话可同时
//!   挂载**多个**活跃项目库；CLI 用**可重复**的 `--project-db <name>=<path>`
//!   指定（0..N 个），解析成 `名称 → 路径` 映射。**挂载了哪些库即为作用域本身**
//!   ——引擎不再按单一 scope 过滤，也不负责推导路径（那是将来 hook 的事）。
//!
//! 各命令：
//! - `render`：读公共库 + 所有挂载项目库 → 合并 → 渲染热索引（先通用 L1-3，
//!   再按项目名分组逐个输出各项目 L4）→ 打印；
//! - `consolidate`：读公共库 + 所有项目库 → 合并 → 跑升降级状态机 → 按
//!   层级 + 项目路由写回（L1-3 → 公共库，L4 → 其 `project` 对应的项目库）→
//!   打印变迁；某 L4 的 project 不在映射中则跳过其写回并记 stderr；
//! - `import`：扫描 JSON 目录 → 按层级 + 项目路由导入（L1-3 → 公共库，
//!   L4 → 其 `project` 对应的项目库；无对应映射则报错退出）；
//! - `write`：新建一条记忆，按层级路由写入正确的库（L4 必须 `--project NAME`
//!   且 NAME 在项目库映射中），打印新 id；
//! - `recall`：在合并集上词法检索返候选（默认搜 active+cold），**不写任何东西**；
//! - `list`：在合并集上按 status/level/project 过滤检视。
//!
//! 另有若干服务 Claude Code hook 的辅助子命令（把"作用域锚定约定"收进引擎，
//! 让 hook 侧近乎零逻辑，见 [`resolve_scope`] / [`engram::commands::resolve_project_scope`]）：
//! - `session-start` / `hot-index`：从 cwd 向上找 `.engram/` 锚点定出作用域，确保
//!   父目录存在、打开「公共库 + 作用域库」合并后把热索引（含前言）打到 **stdout** 供注入；
//! - `resolve`：脚本调用，按同一约定锚定作用域并 `create_dir_all` 两父目录，以 `env`
//!   或 `json` 格式打印 general/作用域库路径、作用域名与 kind，供脚本再去调别的命令；
//! - `root`：把某目录设为 engram「项目管理目录」（在其 `.engram/` 下写 workspace 标记）。
//!
//! 设计文档参考：§6 升降级、§7 降级去向、§13 交付形态（存储选定 redb、多库分置）。

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};

use engram::commands::{
    self, build_merged_memory, gc_should_delete, generate_id, home_dir, last_touch, list_visible,
    oneline_status_with_health, parse_level, parse_project_dbs, parse_status_filter, parse_tags,
    recall_candidates, resolve_project_scope, revived_level, tokenize_query, validate_merge_scope,
    MergeScope, ProjectScope, ScopeKind, ENGRAM_DB_FILE, ENGRAM_DIR, WORKSPACE_MARKER,
};
use engram::consolidate::{consolidate, Transition, TransitionKind};
use engram::health::{self, HealthReport};
use engram::model::{
    EngramConfig, Level, Memory, Pointer, Status, ENGRAM_DATA_VERSION, MEMORY_SCHEMA_VERSION,
};
use engram::render::{load_store_entries, render};
use engram::serve::{self, ServeConfig};
use engram::session::{self, Pending};
use engram::store::{self, StoreError};
use redb::Database;

/// Engram —— 类人分层记忆系统引擎。
#[derive(Parser, Debug)]
#[command(name = "engram", version, about = "Engram 类人分层记忆系统引擎")]
struct Cli {
    /// 子命令。
    #[command(subcommand)]
    command: Command,
}

/// 引擎支持的子命令。
#[derive(Subcommand, Debug)]
enum Command {
    /// 从公共库 + 所有挂载项目库读全部记忆，合并后渲染热索引。
    Render {
        /// 公共库 redb 文件路径（必填，存 L1/L2/L3 通用记忆）。
        #[arg(long)]
        general_db: PathBuf,
        /// 项目库映射 `name=path`，可重复给 0..N 个（每个存对应项目的 L4 记忆）。
        #[arg(long = "project-db")]
        project_db: Vec<String>,
        /// 当前时间（unix 秒）；缺省取系统时间。
        #[arg(long)]
        now: Option<f64>,
    },
    /// 显式召回：在合并集上词法检索返候选（默认搜 active+cold），不写任何东西。
    Recall {
        /// 公共库 redb 文件路径（必填）。
        #[arg(long)]
        general_db: PathBuf,
        /// 项目库映射 `name=path`，可重复给 0..N 个。
        #[arg(long = "project-db")]
        project_db: Vec<String>,
        /// 查询串（按空白分词、小写后做子串匹配）。
        #[arg(long)]
        query: String,
        /// 最多返回的候选数。
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// 只搜 active；缺省同时搜 active 与 cold。
        #[arg(long)]
        active_only: bool,
        /// 当前时间（unix 秒）；缺省取系统时间。
        #[arg(long)]
        now: Option<f64>,
        /// 以 JSON 数组输出候选（默认输出可读表格）。
        #[arg(long)]
        json: bool,
    },
    /// 写入新记忆：按层级路由到正确的库（L1-3 → 公共库，L4 → 对应项目库），打印新 id。
    Write {
        /// 公共库 redb 文件路径（必填）。
        #[arg(long)]
        general_db: PathBuf,
        /// 项目库映射 `name=path`，可重复给 0..N 个（L4 记忆需其归属项目在此映射中）。
        #[arg(long = "project-db")]
        project_db: Vec<String>,
        /// 记忆层级：L1|L2|L3|L4.1|L4.2|L4.3。
        #[arg(long)]
        level: String,
        /// 一句话总结（cue）。
        #[arg(long)]
        cue: String,
        /// 所属项目名（L4 记忆必需，且必须是项目库映射中的某个 name；L1-3 不得给）。
        #[arg(long)]
        project: Option<String>,
        /// 重要度，取值 [0,1]；缺省 0.0。
        #[arg(long, default_value_t = 0.0)]
        importance: f64,
        /// 是否置顶。
        #[arg(long)]
        pinned: bool,
        /// 指针种类：file|doc|url|none；缺省 none。
        #[arg(long, default_value = "none")]
        pointer_kind: String,
        /// 指针引用位置（文件路径:行号 / 文档 id / url）。
        #[arg(long)]
        pointer_ref: Option<String>,
        /// 指针细节正文（仅当无法从 artifact 恢复时使用）。
        #[arg(long)]
        pointer_detail: Option<String>,
        /// 逗号分隔标签，如 `a,b,c`。
        #[arg(long)]
        tags: Option<String>,
        /// 显式指定 id；缺省自动生成唯一 id。
        #[arg(long)]
        id: Option<String>,
        /// 目标库已存在同 id 记忆时允许覆盖；缺省拒绝（防静默覆盖丢数据）。
        #[arg(long)]
        overwrite: bool,
        /// 创建时间（unix 秒）；缺省取系统时间。
        #[arg(long)]
        now: Option<f64>,
    },
    /// 备份/迁移导出：把合并集逐条完整序列化为 JSON 文件（文件名=id.json），与 import 对称互逆。
    Export {
        /// 公共库 redb 文件路径（必填）。
        #[arg(long)]
        general_db: PathBuf,
        /// 项目库映射 `name=path`，可重复给 0..N 个。
        #[arg(long = "project-db")]
        project_db: Vec<String>,
        /// 输出目录（不存在则创建；每条记忆一个 `<id>.json`）。
        #[arg(long)]
        dir: PathBuf,
        /// 项目过滤（可选；给定则仅导出 `project == Some(name)` 的记忆）。
        #[arg(long)]
        project: Option<String>,
        /// 状态过滤：active|cold|superseded|tombstone|all；缺省 all（全量备份）。
        #[arg(long, default_value = "all")]
        status: String,
    },
    /// 可读检视：在合并集上按 status/level/project 过滤并排序输出。
    List {
        /// 公共库 redb 文件路径（必填）。
        #[arg(long)]
        general_db: PathBuf,
        /// 项目库映射 `name=path`，可重复给 0..N 个。
        #[arg(long = "project-db")]
        project_db: Vec<String>,
        /// 项目过滤（可选；给定则仅含 `project == Some(name)` 的 L4）。
        #[arg(long)]
        project: Option<String>,
        /// 状态过滤：active|cold|superseded|tombstone|all；缺省 all。
        #[arg(long, default_value = "all")]
        status: String,
        /// 层级过滤：L1|L2|L3|L4.1|L4.2|L4.3；缺省不过滤。
        #[arg(long)]
        level: Option<String>,
        /// 当前时间（unix 秒）；缺省取系统时间。
        #[arg(long)]
        now: Option<f64>,
        /// 以 JSON 数组输出（每条附计算出的 effective）。
        #[arg(long)]
        json: bool,
    },
    /// 会话末巩固：跑升降级状态机，打印变迁摘要并按层级 + 项目路由写回。
    Consolidate {
        /// 公共库 redb 文件路径（必填）。
        #[arg(long)]
        general_db: PathBuf,
        /// 项目库映射 `name=path`，可重复给 0..N 个。
        #[arg(long = "project-db")]
        project_db: Vec<String>,
        /// 当前时间（unix 秒）；缺省取系统时间。
        #[arg(long)]
        now: Option<f64>,
        /// 只计算并打印，不写回任何数据库。
        #[arg(long)]
        dry_run: bool,
    },
    /// 把旧的 JSON 目录按层级 + 项目路由导入多库（L1-3 → 公共库，L4 → 对应项目库）。
    Import {
        /// 目标公共库 redb 文件路径（不存在则创建；接收 L1/L2/L3）。
        #[arg(long)]
        general_db: PathBuf,
        /// 项目库映射 `name=path`，可重复给 0..N 个（接收对应项目的 L4）。
        #[arg(long = "project-db")]
        project_db: Vec<String>,
        /// 源 JSON 目录（包含若干 `*.json` 记忆文件）。
        #[arg(long)]
        from_json_dir: PathBuf,
    },
    /// 确认真使用：给候选记忆追加一次真使用时间戳（加固）；若为 Cold 则复活。
    ConfirmUse {
        /// 公共库 redb 文件路径（必填）。
        #[arg(long)]
        general_db: PathBuf,
        /// 项目库映射 `name=path`，可重复给 0..N 个。
        #[arg(long = "project-db")]
        project_db: Vec<String>,
        /// 逗号分隔的记忆 id 列表，如 `id1,id2,id3`。
        #[arg(long)]
        ids: String,
        /// 当前时间（unix 秒）；缺省取系统时间。
        #[arg(long)]
        now: Option<f64>,
    },
    /// 纠错硬拽：把 OLD 记忆标记为 Superseded（被 NEW 取代），移出热层。
    Supersede {
        /// 公共库 redb 文件路径（必填）。
        #[arg(long)]
        general_db: PathBuf,
        /// 项目库映射 `name=path`，可重复给 0..N 个。
        #[arg(long = "project-db")]
        project_db: Vec<String>,
        /// 被取代的旧记忆 id。
        #[arg(long)]
        id: String,
        /// 取代它的新记忆 id（即便此 id 当前不存在也照常标记，仅 stderr 警告）。
        #[arg(long)]
        by: String,
        /// 当前时间（unix 秒）；缺省取系统时间（本命令未直接用 now，仅为接口一致）。
        #[arg(long)]
        now: Option<f64>,
    },
    /// 合并聚类：把多条同作用域源记忆合并为一条新记忆，源全部转 Tombstone。
    Merge {
        /// 公共库 redb 文件路径（必填）。
        #[arg(long)]
        general_db: PathBuf,
        /// 项目库映射 `name=path`，可重复给 0..N 个。
        #[arg(long = "project-db")]
        project_db: Vec<String>,
        /// 逗号分隔的源记忆 id 列表（须全部同作用域），如 `id1,id2`。
        #[arg(long)]
        from: String,
        /// 合并记忆的一句话总结（cue）。
        #[arg(long)]
        cue: String,
        /// 显式指定合并记忆层级；缺省取源中最重要的层。
        #[arg(long)]
        level: Option<String>,
        /// 显式指定合并记忆重要度 [0,1]；缺省取源中最大值。
        #[arg(long)]
        importance: Option<f64>,
        /// 显式指定合并记忆所属项目；给定时须与源的共同项目一致。
        #[arg(long)]
        project: Option<String>,
        /// 显式指定合并记忆 id；缺省自动生成。
        #[arg(long)]
        id: Option<String>,
        /// 当前时间（unix 秒）；缺省取系统时间。
        #[arg(long)]
        now: Option<f64>,
    },
    /// 直接遗忘：按 id 在所有挂载库中查找并**物理删除**记忆，不留 superseded/tombstone 痕迹。
    Forget {
        /// 公共库 redb 文件路径（必填）。
        #[arg(long)]
        general_db: PathBuf,
        /// 项目库映射 `name=path`，可重复给 0..N 个。
        #[arg(long = "project-db")]
        project_db: Vec<String>,
        /// 要删除的记忆 id，可重复给 0..N 个；未找到的 id 仅报告、不算失败。
        #[arg(long = "id")]
        id: Vec<String>,
        /// 当前时间（unix 秒）；缺省取系统时间（本命令未直接用 now，仅为接口一致）。
        #[arg(long)]
        now: Option<f64>,
    },
    /// 垃圾回收：按 §7.4 TTL 硬删除过期的 Cold / Tombstone 记忆。
    Gc {
        /// 公共库 redb 文件路径（必填）。
        #[arg(long)]
        general_db: PathBuf,
        /// 项目库映射 `name=path`，可重复给 0..N 个。
        #[arg(long = "project-db")]
        project_db: Vec<String>,
        /// 冷条目存活上限（天）；缺省 180。
        #[arg(long, default_value_t = 180.0)]
        ttl_days: f64,
        /// 墓碑存活上限（天）；缺省 3650。
        #[arg(long, default_value_t = 3650.0)]
        tombstone_ttl_days: f64,
        /// 与门第二臂阈值：真使用次数（access_log 长度）不超过此值才算「极少使用」，
        /// 须与「超 TTL」同时成立方可删除；缺省 1（至多被 confirm-use 过一次）。
        #[arg(long, default_value_t = 1)]
        min_uses: usize,
        /// 当前时间（unix 秒）；缺省取系统时间。
        #[arg(long)]
        now: Option<f64>,
        /// 只报告将删除哪些条目，不真正删除。
        #[arg(long)]
        dry_run: bool,
    },
    /// hook 用：从 cwd 锚定作用域、确保父目录存在、打开「公共库 + 作用域库」合并后把
    /// 热索引（含前言）打到 stdout。
    SessionStart {
        /// 项目目录（即 cwd）；缺省取当前工作目录，从其向上锚定作用域。
        #[arg(long)]
        project_dir: Option<PathBuf>,
        /// 公共库路径覆盖；缺省走 `<HOME>/.engram/general.redb` 约定。
        #[arg(long)]
        general_db: Option<PathBuf>,
        /// 当前时间（unix 秒）；缺省取系统时间。
        #[arg(long)]
        now: Option<f64>,
        /// 输出格式：text（逐行打到 stdout，缺省、保持原行为）或 json
        /// （把前言 + 热索引整段文本塞进 Claude Code SessionStart hook 的
        /// `additionalContext`，打成一行 JSON）。
        #[arg(long, default_value = "text")]
        emit: String,
        /// 可选调试日志文件路径；给定时向其**追加一行**本次调用的记录
        /// （父目录不存在会先创建）。写日志失败被静默忽略，不影响主输出。
        #[arg(long)]
        log: Option<PathBuf>,
        /// 渲染字符预算（0=不限）；缺省 24000 字符 ≈ 8k token，见
        /// [`engram::render::HOT_INDEX_CHAR_BUDGET`]。
        #[arg(long, default_value_t = engram::render::HOT_INDEX_CHAR_BUDGET)]
        budget: usize,
    },
    /// hook 用：从 cwd 锚定作用域、create_dir_all 两父目录，以 env|json 打印 general/
    /// 作用域库路径、作用域名与 kind（project|workspace）。
    Resolve {
        /// 项目目录（即 cwd）；缺省取当前工作目录，从其向上锚定作用域。
        #[arg(long)]
        project_dir: Option<PathBuf>,
        /// 公共库路径覆盖；缺省走 `<HOME>/.engram/general.redb` 约定。
        #[arg(long)]
        general_db: Option<PathBuf>,
        /// 输出格式：env（逐行 KEY=VALUE，缺省）或 json（单行对象）。
        #[arg(long, default_value = "env")]
        format: String,
    },
    /// hook 用：从 cwd 向上找 `.engram/` 锚点定出作用域，挂载「公共库 + 作用域库」并渲染；
    /// 仅当作用域根相对上次**变化**时才输出（重注入）。
    HotIndex {
        /// 公共库路径覆盖；缺省走 `<HOME>/.engram/general.redb` 约定。
        #[arg(long)]
        general_db: Option<PathBuf>,
        /// 工作区根目录（作 cwd override）；缺省取 stdin 的 cwd，再缺省取当前工作目录。
        #[arg(long)]
        workspace_root: Option<PathBuf>,
        /// transcript 文件路径（历史信号字段，新作用域模型已不使用；仅为兼容保留接收）。
        #[arg(long)]
        transcript: Option<PathBuf>,
        /// 本次用户 prompt 文本（历史信号字段，新作用域模型已不使用；仅为兼容保留接收）。
        #[arg(long)]
        prompt: Option<String>,
        /// 状态门控基准路径；给定时启用状态门控（作用域根未变则输出空、不重注入）。
        /// 兼容说明：本参数继续接受历史的单文件路径（如 `~/.engram/active.state`），
        /// 但**旧单文件不再读写**——门控状态改按会话分文件存到
        /// `<其父目录>/active-state/<session_id>.state`，避免多窗口跨项目交替时
        /// 共享一个门控互相翻转、每条 prompt 都全量重注入。
        #[arg(long)]
        state: Option<PathBuf>,
        /// 状态栏小文件路径；给定时每次都把挂载集的一行状态串写入该文件（覆盖），
        /// 供状态栏只读该文件而不必开 redb。即便状态门控判定为「空、不注入」也照常写，
        /// 让状态栏始终最新。写失败静默忽略，不影响注入主流程。
        #[arg(long)]
        status_file: Option<PathBuf>,
        /// 状态栏目录；给定时把状态串写入 `<dir>/<session_id>.txt`（按会话分文件，
        /// 多窗口/多项目各读各的、互不覆盖）。优先于 `--status-file`；从 hook stdin 取
        /// session_id，缺失时回退 `<dir>/default.txt`。顺手清理目录内过期的旧会话文件。
        #[arg(long)]
        status_dir: Option<PathBuf>,
        /// 输出格式：text（前言 + 热索引，缺省）或 json（合成 hook 单行 JSON）。
        #[arg(long, default_value = "text")]
        emit: String,
        /// `--emit json` 时写入的 hookEventName；缺省 `SessionStart`。
        #[arg(long, default_value = "SessionStart")]
        hook_event: String,
        /// 从 stdin 读取整段 hook JSON（取 transcript_path / cwd / prompt 兜底）。
        #[arg(long)]
        from_hook_stdin: bool,
        /// 可选调试日志文件路径；给定时向其追加一行本次调用记录（失败静默忽略）。
        #[arg(long)]
        log: Option<PathBuf>,
        /// 渲染字符预算（0=不限）；缺省 24000 字符 ≈ 8k token，见
        /// [`engram::render::HOT_INDEX_CHAR_BUDGET`]。
        #[arg(long, default_value_t = engram::render::HOT_INDEX_CHAR_BUDGET)]
        budget: usize,
        /// 当前时间（unix 秒）；缺省取系统时间。
        #[arg(long)]
        now: Option<f64>,
    },
    /// 概况：从 cwd 锚定作用域，挂载「公共库 + 作用域库 + 各 --project-db」，把这批库
    /// 当作挂载集统计/展示（不做活跃子项目动态判定、不接 transcript）。
    Status {
        /// 公共库路径覆盖；缺省走 `<HOME>/.engram/general.redb` 约定。
        #[arg(long)]
        general_db: Option<PathBuf>,
        /// 工作区根目录（作 cwd override）；缺省取 stdin 的 cwd（仅 --from-hook-stdin 时），
        /// 再缺省取当前工作目录。从该目录向上锚定作用域，其作用域库一并计入挂载集。
        #[arg(long)]
        workspace_root: Option<PathBuf>,
        /// 额外项目库映射 `name=path`，可重复给 0..N 个（一并计入挂载集）。
        #[arg(long = "project-db")]
        project_db: Vec<String>,
        /// 从 stdin 读取整段 hook JSON（仅用于取 cwd 作 workspace-root 兜底）。
        #[arg(long)]
        from_hook_stdin: bool,
        /// 输出格式：full（缺省，可读多行概况）或 oneline（状态栏一行串）。
        #[arg(long, default_value = "full")]
        format: String,
        /// 当前时间（unix 秒）；缺省取系统时间。
        #[arg(long)]
        now: Option<f64>,
    },
    /// hook 用（SessionEnd）：算 transcript 相对水位线的增量，切片 + 落 pending 标记，
    /// 输出复盘所需单行 JSON（`action`=`review` 带切片/库路径，或 `skip` 表无新增）。
    ReviewPrepare {
        /// 原始 transcript（JSONL）路径。
        #[arg(long)]
        transcript: PathBuf,
        /// 会话 id（pending / 切片文件名前缀）。
        #[arg(long)]
        session_id: String,
        /// 水位线文件路径（记录各 transcript 已巩固行数）。
        #[arg(long)]
        watermark: PathBuf,
        /// pending 与切片的存放目录（如 `~/.engram/pending`）。
        #[arg(long)]
        work_dir: PathBuf,
        /// 公共库路径（透传给复盘者）。
        #[arg(long)]
        general_db: PathBuf,
        /// 项目库路径（透传给复盘者）。
        #[arg(long)]
        project_db: PathBuf,
        /// 项目名（透传给复盘者）。
        #[arg(long)]
        project_name: String,
    },
    /// hook 用（SessionStart）：扫 `work-dir` 残留 pending（上次起了复盘者却没收尾），
    /// 挑**最近一场**补跑、清掉更早的，输出复盘所需单行 JSON（`action`=`review`|`none`）。
    ///
    /// 另给 `--sessions-dir` + `--watermark` 时先做**孤儿会话补登**：对「登记过
    /// （UserPromptSubmit 落的 active-sessions 登记）、无对应 pending、且 transcript
    /// 现有行数 > 水位线」的最近一个孤儿 transcript 现场补建 pending，再按原流程领取
    /// ——覆盖强杀/断电时 SessionEnd 未触发、连 pending 都没落下的场景（README
    /// 「崩溃/强关可补」承诺的闭环）。补跑仍保持「只补最近一场」策略。
    CatchupScan {
        /// pending 与切片的存放目录（如 `~/.engram/pending`）。
        #[arg(long)]
        work_dir: PathBuf,
        /// 活跃会话登记目录（如 `~/.engram/active-sessions`）；缺省不做孤儿补登。
        #[arg(long)]
        sessions_dir: Option<PathBuf>,
        /// 水位线文件路径（孤儿判定「行数 > 水位线」用）；缺省不做孤儿补登。
        #[arg(long)]
        watermark: Option<PathBuf>,
    },
    /// 复盘者收尾：把水位线推进到 pending 的 `end_line`，删除该 pending 与其切片。
    ConsolidateDone {
        /// 待收尾的 pending 标记文件路径。
        #[arg(long)]
        pending: PathBuf,
        /// 水位线文件路径。
        #[arg(long)]
        watermark: PathBuf,
    },
    /// 把某目录设为 engram「项目管理目录」（workspace）：在其 `.engram/` 下写 workspace
    /// 标记 + 建空项目库，并往公共库追加一条 L2「管理目录」记忆。幂等、防嵌套。
    Root {
        /// 待设为项目管理目录的目录；缺省取当前工作目录。
        #[arg(long)]
        project_dir: Option<PathBuf>,
        /// 公共库路径覆盖；缺省走 `<HOME>/.engram/general.redb` 约定。
        #[arg(long)]
        general_db: Option<PathBuf>,
    },
    /// 毕业通道（§6 L4 → L1-3）：把一条项目记忆（L4.x）提拔进通用层；原 L4 记忆转
    /// superseded 留作「已上浮」指针（移动 + 留指针、不复制，避免两层各存一份打架）。
    Graduate {
        /// 公共库 redb 文件路径（必填，新通用记忆写这里）。
        #[arg(long)]
        general_db: PathBuf,
        /// 项目库映射 `name=path`，可重复给 0..N 个（源 L4 记忆所属项目须在此映射中）。
        #[arg(long = "project-db")]
        project_db: Vec<String>,
        /// 要毕业的源项目记忆 id（须为 L4.x 且 active）。
        #[arg(long)]
        id: String,
        /// 目标通用层 L1|L2|L3；缺省按子层同构 L4.1→L1 / L4.2→L2 / L4.3→L3。
        #[arg(long)]
        to_level: Option<String>,
        /// 覆盖新通用记忆的 cue；缺省沿用源 cue。
        #[arg(long)]
        cue: Option<String>,
        /// 覆盖新通用记忆的重要度 [0,1]；缺省沿用源 importance。
        #[arg(long)]
        importance: Option<f64>,
        /// 显式指定新通用记忆 id；缺省自动生成。
        #[arg(long)]
        new_id: Option<String>,
        /// 当前时间（unix 秒）；缺省取系统时间。
        #[arg(long)]
        now: Option<f64>,
    },
    /// 启动本地网页看板：从 `.engram/` 锚点扫描本机全部项目库 + 公共库，在浏览器里
    /// 浏览全部记忆。默认只监听回环地址（127.0.0.1），不对外网开放。
    Serve {
        /// 公共库路径覆盖；缺省走 `<HOME>/.engram/general.redb` 约定。
        #[arg(long)]
        general_db: Option<PathBuf>,
        /// 额外项目库映射 `name=path`，可重复；与自动发现的库合并去重。
        #[arg(long = "project-db")]
        project_db: Vec<String>,
        /// 额外的项目管理目录（扫其管理库与直接子项目库），可重复；与从 cwd 自动
        /// 锚定的管理目录合并。
        #[arg(long = "scan-root")]
        scan_root: Vec<PathBuf>,
        /// 监听端口（缺省 8765）。
        #[arg(long, default_value_t = 8765)]
        port: u16,
        /// 监听主机（缺省 127.0.0.1，仅本机可访问）。
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// 启动后自动用系统默认浏览器打开看板页面。
        #[arg(long)]
        open: bool,
    },
    /// 版本触发的一次性数据迁移：把老库存量记忆按新一代规则重洗。
    ///
    /// 严格顺序：① 备份优先（先把全库完整 JSON 导出到 `--backup-dir`，失败即中止不动库）；
    /// ② 结构性清洗（补 `schema_version`、钳未来时间戳、`access_log` 去重排序，自动）；
    /// ③ 重分层 + 容量（跑 consolidate，把旧尖峰升上来的层降回、超预算层挤下去，自动）；
    /// ④ 语义性问题（importance 偏离层锚 / 悬空 `superseded_by` / 疑似重复）**只收进报告、
    /// 绝不自动改**；⑤ 写库级 `data_version`。写库前用 `<general_db>.migrate.lock` 加库级
    /// 迁移锁防并发，拿不到锁则跳过（下次再迁）。
    Migrate {
        /// 公共库 redb 文件路径（必填）。
        #[arg(long)]
        general_db: PathBuf,
        /// 项目库映射 `name=path`，可重复给 0..N 个（一并迁移）。
        #[arg(long = "project-db")]
        project_db: Vec<String>,
        /// 仅当库级 `data_version` 落后于当前 [`ENGRAM_DATA_VERSION`] 才迁移；已最新则跳过退出 0。
        #[arg(long)]
        if_needed: bool,
        /// 备份输出根目录；缺省取「公共库父目录 / backups」。
        #[arg(long)]
        backup_dir: Option<PathBuf>,
        /// 只报告将做什么 + 语义问题，不备份、不写库。
        #[arg(long)]
        dry_run: bool,
        /// 当前时间（unix 秒）；缺省取系统时间。备份目录时间戳、重分层判定均用它。
        #[arg(long)]
        now: Option<f64>,
    },
    /// 只读健康体检：报告库版本、层分布、schema 分布、坏行、超预算层、未来时间戳、
    /// 悬空 `superseded_by`、importance 偏离层锚、疑似重复、孤儿。**绝不写库**。
    Doctor {
        /// 公共库 redb 文件路径（必填）。
        #[arg(long)]
        general_db: PathBuf,
        /// 项目库映射 `name=path`，可重复给 0..N 个。
        #[arg(long = "project-db")]
        project_db: Vec<String>,
        /// 以 JSON 输出（缺省输出可读文本报告）。
        #[arg(long)]
        json: bool,
        /// 当前时间（unix 秒）；缺省取系统时间（未来时间戳判定用）。
        #[arg(long)]
        now: Option<f64>,
    },
}

/// 取系统当前时间的 unix 秒（f64）。
///
/// # Errors
/// 当系统时钟早于 unix 纪元（极端异常）时返回错误说明。
fn system_now() -> Result<f64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .map_err(|e| format!("系统时钟早于 UNIX 纪元：{e}"))
}

/// 解析 `now`：给定则用给定值，否则取系统时间。出错时打印并返回 `None`。
///
/// 对显式给定的 `--now` 做输入消毒：必须是**有限且非负**的 unix 秒——否则
/// NaN/±inf/负数会经 `created_at` / `access_log` 写进库里，污染所有依赖时间差的
/// 懒计算（activation / TTL / 排序），且一旦落盘极难清理。非法值直接报错退出。
fn resolve_now(now: Option<f64>) -> Option<f64> {
    match now {
        Some(n) if !n.is_finite() || n < 0.0 => {
            eprintln!("--now 非法：{n}（须为有限且非负的 unix 秒）");
            None
        }
        Some(n) => Some(n),
        None => match system_now() {
            Ok(n) => Some(n),
            Err(e) => {
                eprintln!("获取系统时间失败：{e}");
                None
            }
        },
    }
}

/// 向指定 hook 日志文件**追加一行**带时间戳的记录（best-effort，全程失败静默）。
///
/// 供 hook 链路（hot-index 降级、review-prepare / catchup-scan / consolidate-done
/// 的关键失败）留痕：stderr 在 hook 场景常被吞掉，hook.log 是事后可查的唯一线索。
fn append_hook_log(log_path: &Path, now: f64, msg: &str) {
    use std::io::Write as _;
    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() && std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let ts = now.max(0.0) as u64;
    let line = format!("{ts} {msg}\n");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// 推导 `<engram_dir>/hook.log` 路径：`engram_dir` 给定用给定值，否则回退
/// `<HOME>/.engram/hook.log`（HOME 取不到时返回 `None`，调用方放弃写日志）。
fn hook_log_path(engram_dir: Option<&Path>) -> Option<PathBuf> {
    match engram_dir {
        Some(d) => Some(d.join("hook.log")),
        None => home_dir(real_env_lookup)
            .ok()
            .map(|h| h.join(ENGRAM_DIR).join("hook.log")),
    }
}

/// 把任意字符串净化为文件名安全形式：只留 ASCII 字母数字与 `-`/`_`，其余替换为
/// `_`；空串回退 `default`。杜绝 `/`、`..` 等穿出目录（状态栏 / 门控 / 登记 /
/// export 文件名共用此规则）。
fn sanitize_file_stem(raw: &str) -> String {
    let safe: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        "default".to_string()
    } else {
        safe
    }
}

/// 把 `content` **原子地**写入 `path`（tmp+rename，模式同 `session::write_atomic`）。
///
/// 供 main.rs 侧的小状态文件（门控 state、last-review-ok、会话登记）使用：
/// 裸 `fs::write` 截断后写，另一进程并发读会读到空/半截。
///
/// # Errors
/// 建父目录、写临时文件或 rename 失败时返回中文说明的错误字符串。
fn write_atomic_str(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建目录 {} 失败：{e}", parent.display()))?;
        }
    }
    let tmp = path.with_extension("atomic.tmp");
    std::fs::write(&tmp, content).map_err(|e| format!("写临时文件 {} 失败：{e}", tmp.display()))?;
    // Windows / Unix 的 rename 对已存在目标均为原子替换。
    std::fs::rename(&tmp, path).map_err(|e| format!("替换文件 {} 失败：{e}", path.display()))
}

/// 解析 `--project-db name=path` 原始参数；出错时打印并返回 `None`。
fn resolve_project_dbs(raw: &[String]) -> Option<BTreeMap<String, PathBuf>> {
    match parse_project_dbs(raw) {
        Ok(m) => Some(m),
        Err(e) => {
            eprintln!("解析 --project-db 失败：{e}");
            None
        }
    }
}

/// 打开一个库并读出其全部记忆。
///
/// # Errors
/// - [`StoreError`] —— 打开或读取库失败。
fn open_and_read(path: &Path) -> Result<(Database, Vec<Memory>), StoreError> {
    let db = store::open(path)?;
    let mems = store::all(&db)?;
    Ok((db, mems))
}

/// 读取公共库 + 所有挂载项目库，并把全部记忆合并为一个 `Vec`。
///
/// 用于 `render` / `recall` / `list`：它们都只读、合并后在内存里过滤排序。
///
/// # Errors
/// 任一库打开或读取失败时，返回带库类别说明的错误字符串。
fn read_merged(
    general_db: &Path,
    project_dbs: &BTreeMap<String, PathBuf>,
) -> Result<Vec<Memory>, String> {
    let (_gdb, mut merged) = open_and_read(general_db)
        .map_err(|e| format!("读取公共库 {} 失败：{e}", general_db.display()))?;
    for (name, path) in project_dbs {
        let (_db, mut pmems) = open_and_read(path)
            .map_err(|e| format!("读取项目库 {name}（{}）失败：{e}", path.display()))?;
        merged.append(&mut pmems);
    }
    Ok(merged)
}

/// 一组已打开的库句柄：公共库 + 各项目库（按项目名索引）。
///
/// 维护命令（confirm-use / supersede / merge / gc）需要**读全部库为一个合并集、
/// 再按记忆的 `project` 字段把写回/删除路由到正确的库**，故把句柄一并保留。
struct DbSet {
    /// 公共库句柄（存 `project == None` 的记忆）。
    general: Database,
    /// 项目名 → 项目库句柄（存对应项目的 L4 记忆）。
    projects: BTreeMap<String, Database>,
}

impl DbSet {
    /// 打开公共库与所有项目库，保留句柄。
    ///
    /// # Errors
    /// 任一库打开失败时，返回带库类别说明的错误字符串。
    fn open(general_db: &Path, project_dbs: &BTreeMap<String, PathBuf>) -> Result<Self, String> {
        let general = store::open(general_db)
            .map_err(|e| format!("打开公共库 {} 失败：{e}", general_db.display()))?;
        let mut projects = BTreeMap::new();
        for (name, path) in project_dbs {
            let db = store::open(path)
                .map_err(|e| format!("打开项目库 {name}（{}）失败：{e}", path.display()))?;
            projects.insert(name.clone(), db);
        }
        Ok(DbSet { general, projects })
    }

    /// 读取公共库 + 所有项目库的全部记忆，合并为一个 `Vec`。
    ///
    /// # Errors
    /// 任一库读取失败时，返回带库类别说明的错误字符串。
    fn read_all(&self) -> Result<Vec<Memory>, String> {
        let mut merged = store::all(&self.general).map_err(|e| format!("读取公共库失败：{e}"))?;
        for (name, db) in &self.projects {
            let mut pmems = store::all(db).map_err(|e| format!("读取项目库 {name} 失败：{e}"))?;
            merged.append(&mut pmems);
        }
        Ok(merged)
    }

    /// 取一条记忆应写入/删除的目标库句柄：`project == None` 路由公共库，
    /// `Some(name)` 路由对应项目库；无对应映射时返回 `None`（调用方记 stderr 跳过）。
    fn route(&self, project: Option<&str>) -> Option<&Database> {
        match project {
            None => Some(&self.general),
            Some(name) => self.projects.get(name),
        }
    }
}

/// 进程入口：把真正的分发逻辑放到一个**显式大栈**的工作线程上执行。
///
/// Windows 主线程默认栈仅约 1 MiB；本二进制的命令分发（`dispatch` 里那个覆盖全部
/// 子命令的巨型 `match`）在 **debug 构建**下单个栈帧就接近该上限，极易栈溢出
/// （release 优化后栈帧大幅收缩，无此问题）。集成测试直接 spawn 的正是 debug 二进制，
/// 故用一个 16 MiB 栈的工作线程承载逻辑，与优化级别无关地保证稳定，也为后续继续新增
/// 子命令留足余量。
fn main() -> ExitCode {
    match std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(dispatch)
    {
        Ok(handle) => handle.join().unwrap_or_else(|_| {
            eprintln!("engram 工作线程异常终止");
            ExitCode::FAILURE
        }),
        Err(e) => {
            eprintln!("engram 启动工作线程失败：{e}");
            ExitCode::FAILURE
        }
    }
}

/// 程序主逻辑（跑在 [`main`] 起的大栈工作线程上）。返回进程退出码。
fn dispatch() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Render {
            general_db,
            project_db,
            now,
        } => run_render(&general_db, &project_db, now),
        Command::Recall {
            general_db,
            project_db,
            query,
            limit,
            active_only,
            now,
            json,
        } => run_recall(RecallArgs {
            general_db: &general_db,
            project_db: &project_db,
            query: &query,
            limit,
            active_only,
            now,
            json,
        }),
        Command::Write {
            general_db,
            project_db,
            level,
            cue,
            project,
            importance,
            pinned,
            pointer_kind,
            pointer_ref,
            pointer_detail,
            tags,
            id,
            overwrite,
            now,
        } => run_write(WriteArgs {
            general_db: &general_db,
            project_db: &project_db,
            level: &level,
            cue: &cue,
            project: project.as_deref(),
            importance,
            pinned,
            pointer_kind: &pointer_kind,
            pointer_ref: pointer_ref.as_deref(),
            pointer_detail: pointer_detail.as_deref(),
            tags: tags.as_deref(),
            id: id.as_deref(),
            overwrite,
            now,
        }),
        Command::Export {
            general_db,
            project_db,
            dir,
            project,
            status,
        } => run_export(ExportArgs {
            general_db: &general_db,
            project_db: &project_db,
            dir: &dir,
            project: project.as_deref(),
            status: &status,
        }),
        Command::List {
            general_db,
            project_db,
            project,
            status,
            level,
            now,
            json,
        } => run_list(ListArgs {
            general_db: &general_db,
            project_db: &project_db,
            project: project.as_deref(),
            status: &status,
            level: level.as_deref(),
            now,
            json,
        }),
        Command::Consolidate {
            general_db,
            project_db,
            now,
            dry_run,
        } => run_consolidate(&general_db, &project_db, now, dry_run),
        Command::Import {
            general_db,
            project_db,
            from_json_dir,
        } => run_import(&general_db, &project_db, &from_json_dir),
        Command::ConfirmUse {
            general_db,
            project_db,
            ids,
            now,
        } => run_confirm_use(&general_db, &project_db, &ids, now),
        Command::Supersede {
            general_db,
            project_db,
            id,
            by,
            now,
        } => run_supersede(&general_db, &project_db, &id, &by, now),
        Command::Merge {
            general_db,
            project_db,
            from,
            cue,
            level,
            importance,
            project,
            id,
            now,
        } => run_merge(MergeArgs {
            general_db: &general_db,
            project_db: &project_db,
            from: &from,
            cue: &cue,
            level: level.as_deref(),
            importance,
            project: project.as_deref(),
            id: id.as_deref(),
            now,
        }),
        Command::Forget {
            general_db,
            project_db,
            id,
            now,
        } => run_forget(ForgetArgs {
            general_db: &general_db,
            project_db: &project_db,
            ids: &id,
            now,
        }),
        Command::Gc {
            general_db,
            project_db,
            ttl_days,
            tombstone_ttl_days,
            min_uses,
            now,
            dry_run,
        } => run_gc(GcArgs {
            general_db: &general_db,
            project_db: &project_db,
            ttl_days,
            tombstone_ttl_days,
            min_uses,
            now,
            dry_run,
        }),
        Command::SessionStart {
            project_dir,
            general_db,
            now,
            emit,
            log,
            budget,
        } => run_session_start(SessionStartArgs {
            project_dir: project_dir.as_deref(),
            general_db: general_db.as_deref(),
            now,
            emit: &emit,
            log: log.as_deref(),
            budget,
        }),
        Command::Resolve {
            project_dir,
            general_db,
            format,
        } => run_resolve(project_dir.as_deref(), general_db.as_deref(), &format),
        Command::HotIndex {
            general_db,
            workspace_root,
            transcript,
            prompt,
            state,
            status_file,
            status_dir,
            emit,
            hook_event,
            from_hook_stdin,
            log,
            budget,
            now,
        } => run_hot_index(HotIndexArgs {
            general_db: general_db.as_deref(),
            workspace_root: workspace_root.as_deref(),
            transcript: transcript.as_deref(),
            prompt: prompt.as_deref(),
            state: state.as_deref(),
            status_file: status_file.as_deref(),
            status_dir: status_dir.as_deref(),
            emit: &emit,
            hook_event: &hook_event,
            from_hook_stdin,
            log: log.as_deref(),
            budget,
            now,
        }),
        Command::Status {
            general_db,
            workspace_root,
            project_db,
            from_hook_stdin,
            format,
            now,
        } => run_status(StatusArgs {
            general_db: general_db.as_deref(),
            workspace_root: workspace_root.as_deref(),
            project_db: &project_db,
            from_hook_stdin,
            format: &format,
            now,
        }),
        Command::ReviewPrepare {
            transcript,
            session_id,
            watermark,
            work_dir,
            general_db,
            project_db,
            project_name,
        } => run_review_prepare(ReviewPrepareArgs {
            transcript: &transcript,
            session_id: &session_id,
            watermark: &watermark,
            work_dir: &work_dir,
            general_db: &general_db,
            project_db: &project_db,
            project_name: &project_name,
        }),
        Command::CatchupScan {
            work_dir,
            sessions_dir,
            watermark,
        } => run_catchup_scan(&work_dir, sessions_dir.as_deref(), watermark.as_deref()),
        Command::ConsolidateDone { pending, watermark } => {
            run_consolidate_done(&pending, &watermark)
        }
        Command::Root {
            project_dir,
            general_db,
        } => run_root(project_dir.as_deref(), general_db.as_deref()),
        Command::Graduate {
            general_db,
            project_db,
            id,
            to_level,
            cue,
            importance,
            new_id,
            now,
        } => run_graduate(GraduateArgs {
            general_db: &general_db,
            project_db: &project_db,
            id: &id,
            to_level: to_level.as_deref(),
            cue: cue.as_deref(),
            importance,
            new_id: new_id.as_deref(),
            now,
        }),
        Command::Serve {
            general_db,
            project_db,
            scan_root,
            port,
            host,
            open,
        } => run_serve(ServeCliArgs {
            general_db: general_db.as_deref(),
            project_db: &project_db,
            scan_root: &scan_root,
            port,
            host,
            open,
        }),
        Command::Migrate {
            general_db,
            project_db,
            if_needed,
            backup_dir,
            dry_run,
            now,
        } => run_migrate(MigrateCliArgs {
            general_db: &general_db,
            project_db: &project_db,
            if_needed,
            backup_dir: backup_dir.as_deref(),
            dry_run,
            now,
        }),
        Command::Doctor {
            general_db,
            project_db,
            json,
            now,
        } => run_doctor(DoctorArgs {
            general_db: &general_db,
            project_db: &project_db,
            json,
            now,
        }),
    }
}

/// 生产环境的环境变量查找闭包：读真实进程环境变量（`USERPROFILE` / `HOME`）。
fn real_env_lookup(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// 解析 hook 辅助命令的项目目录：给定则用给定值，否则取 [`std::env::current_dir`]。
///
/// # Errors
/// 取当前工作目录失败（如所在目录已被删除、无权限）时返回错误说明。
fn resolve_project_dir(project_dir: Option<&Path>) -> Result<PathBuf, String> {
    match project_dir {
        Some(p) => Ok(p.to_path_buf()),
        None => std::env::current_dir().map_err(|e| format!("获取当前工作目录失败：{e}")),
    }
}

/// 去除 Windows 规范化路径可能带的 verbatim 前缀 `\\?\`（如 `\\?\C:\foo` → `C:\foo`）。
///
/// [`std::fs::canonicalize`] 在 Windows 上返回带 `\\?\` 前缀的 verbatim 路径，该写法
/// 虽合法但对用户不友好、且与项目里别处拼出的普通路径不一致（影响 `--state` 的字符串
/// 比较、日志可读性）。这里仅做字符串层面的前缀剥离，**不引入新依赖**；非 Windows
/// 或本就无该前缀的路径原样返回。UNC verbatim（`\\?\UNC\...`）较罕见，保守地只剥离
/// 普通盘符形态的 `\\?\`，其余情况保持原值。
fn strip_verbatim_prefix(p: PathBuf) -> PathBuf {
    // 用 lossy 转字符串判断前缀；仅当确为 `\\?\` 开头且其后不是 `UNC\` 时才剥离。
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        if !rest.starts_with(r"UNC\") {
            return PathBuf::from(rest.to_string());
        }
    }
    p
}

/// 把 `cwd` 规范化为绝对路径并剥离 Windows verbatim 前缀（best-effort）。
///
/// 先尝试 [`std::fs::canonicalize`]（解析符号链接、`.`/`..`、相对路径）；成功则剥离
/// 可能的 `\\?\` 前缀，失败（路径不存在、无权限等）则原样退回传入的 `cwd`——作用域
/// 锚定不应因为路径暂不可规范化而整体失败。
fn canonicalize_cwd(cwd: &Path) -> PathBuf {
    match std::fs::canonicalize(cwd) {
        Ok(canon) => strip_verbatim_prefix(canon),
        Err(_) => cwd.to_path_buf(),
    }
}

/// 从 `cwd` 向上锚定作用域，并算出公共库路径（生产 helper，供各 hook 命令复用）。
///
/// 流程：
/// 1. 用 [`canonicalize_cwd`] 把 `cwd` 规范化为绝对路径（best-effort）。
/// 2. 用「锚点必须带 redb 或 workspace 标记」的收紧判据从规范化 cwd 向上锚定作用域
///    （见 [`resolve_project_scope`]）：空的 / 残留的 `.engram/` 目录**不算**锚点。
/// 3. 公共库 `general_db` = `general_override` 若给定，否则 `<HOME>/.engram/general.redb`
///    （HOME 由 [`home_dir`] 用真实环境变量推导）。
///
/// 返回 `(general_db, scope)`。本函数会探测文件系统（canonicalize、`is_file`），但不
/// 创建任何目录、不打开任何库——建父目录、开库是调用方的职责。
///
/// # 参数
/// - `cwd`：当前工作目录（可为相对路径，会被规范化）。
/// - `general_override`：`--general-db` 显式指定的公共库路径（`None` 时走默认约定）。
///
/// # Errors
/// 当 `general_override` 为 `None` 且 [`home_dir`] 无法确定主目录时，返回其错误说明。
fn resolve_scope(
    cwd: &Path,
    general_override: Option<&Path>,
) -> Result<(PathBuf, ProjectScope), String> {
    let canonical_cwd = canonicalize_cwd(cwd);

    // 锚点判据（收紧）：`.engram/` 下必须有 redb 库或 workspace 标记才算真锚点，
    // 空的 / 残留的 `.engram/` 目录不算——否则误删后残留的空目录会污染锚定。
    let has_engram = |d: &Path| {
        d.join(ENGRAM_DIR).join(ENGRAM_DB_FILE).is_file()
            || d.join(ENGRAM_DIR).join(WORKSPACE_MARKER).is_file()
    };
    // workspace 判据：`.engram/workspace` 标记文件存在即为项目管理目录。
    let is_workspace = |d: &Path| d.join(ENGRAM_DIR).join(WORKSPACE_MARKER).is_file();

    let scope = resolve_project_scope(&canonical_cwd, has_engram, is_workspace);

    let general_db = match general_override {
        Some(p) => p.to_path_buf(),
        None => {
            let home = home_dir(real_env_lookup)?;
            home.join(ENGRAM_DIR).join("general.redb")
        }
    };

    Ok((general_db, scope))
}

/// 确保某 db 文件路径的父目录存在（redb 只建文件不建目录）。
///
/// 路径无父目录段（极端情况）时视作无需创建，直接返回 `Ok`。
///
/// # Errors
/// `create_dir_all` 失败（无权限等）时返回带路径说明的错误字符串。
fn ensure_parent_dir(db_path: &Path) -> Result<(), String> {
    if let Some(parent) = db_path.parent() {
        // parent 为空串（如纯文件名）时 create_dir_all 是 no-op，无害。
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建目录 {} 失败：{e}", parent.display()))?;
    }
    Ok(())
}

/// 按 cwd 锚定作用域并 `create_dir_all` **公共库**的父目录（项目库父目录**不**在此建）。
///
/// 把 `session-start` / `resolve` 共用的"解析项目目录 → resolve_scope → 建公共库父目录"
/// 流程收成一处，保证两命令的作用域语义完全一致。
///
/// **为何不建项目库父目录**：只读命令（resolve / session-start）若在此 `create_dir_all`
/// 项目库的 `.engram/` 目录、再配合 `store::open` 建出空库，就会在任意子目录里凭空造出
/// 杂散锚点，把一个项目碎片化成多个（曾踩：在 plugin/ 子目录开会话，plugin 被识别成独立
/// 项目而非 engram 的一部分）。项目锚点只应由 `write` / `root` 显式建立。
///
/// # 参数
/// - `project_dir`：项目目录（即 cwd 语义）；`None` 时取当前工作目录。
/// - `general_override`：`--general-db` 覆盖；`None` 时走默认约定。
///
/// # Errors
/// 取当前目录失败、HOME 无法确定、或建公共库父目录失败时，返回中文说明的错误字符串。
fn resolve_scope_and_prepare(
    project_dir: Option<&Path>,
    general_override: Option<&Path>,
) -> Result<(PathBuf, ProjectScope), String> {
    let project_dir = resolve_project_dir(project_dir)?;
    let (general_db, scope) = resolve_scope(&project_dir, general_override)?;
    ensure_parent_dir(&general_db)?;
    Ok((general_db, scope))
}

/// `session-start` 命令的全部参数（聚成一个结构体，规避过多函数形参）。
struct SessionStartArgs<'a> {
    project_dir: Option<&'a Path>,
    general_db: Option<&'a Path>,
    now: Option<f64>,
    /// 输出格式：`text`（缺省，逐行打到 stdout）或 `json`（合成 hook JSON 一行）。
    emit: &'a str,
    /// 可选调试日志文件路径（追加一行）；`None` 时不写日志。
    log: Option<&'a Path>,
    /// 渲染字符预算（0=不限）；注入路径缺省 [`engram::render::HOT_INDEX_CHAR_BUDGET`]。
    budget: usize,
}

/// `session-start` 的输出格式（由 `--emit` 解析而来）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmitFormat {
    /// 逐行打到 stdout（前言行 + render 热索引），与历史行为一致。
    Text,
    /// 把整段文本塞进 Claude Code SessionStart hook 的 `additionalContext`，打成一行 JSON。
    Json,
}

/// 把 `--emit` 字符串解析为 [`EmitFormat`]（大小写不敏感）。
///
/// # Errors
/// 当字符串既非 `text` 也非 `json` 时，返回中文说明的错误字符串。
fn parse_emit_format(s: &str) -> Result<EmitFormat, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "text" => Ok(EmitFormat::Text),
        "json" => Ok(EmitFormat::Json),
        other => Err(format!("无法识别的 --emit {other}（应为 text|json）")),
    }
}

/// 把整段注入文本包成 Claude Code SessionStart hook 的单行 JSON。
///
/// 形如 `{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"<整段文本>"}}`，
/// 用 [`serde_json`] 序列化保证转义与 UTF-8 正确（不手拼字符串）。
///
/// # 参数
/// - `context`：要注入的整段文本（前言行 + render 热索引）。
///
/// # Errors
/// 序列化失败（理论上不会发生，字符串与字面量结构均可序列化）时返回其错误说明。
fn build_hook_json(context: &str) -> Result<String, String> {
    let value = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": context,
        }
    });
    serde_json::to_string(&value).map_err(|e| format!("序列化 hook JSON 失败：{e}"))
}

/// 向调试日志文件**追加一行**本次 `session-start` 调用的记录（best-effort）。
///
/// 行格式：`<unix秒> session-start emit=<text|json> kind=<project|workspace> project=<name> general=<路径> scope_db=<路径>\n`。
/// 父目录不存在时先 `create_dir_all`。
///
/// 调试日志是次要旁路——**任何错误都被静默吞掉、不上抛**：注入热索引才是主任务，
/// 不能因写日志失败而让命令失败。本函数因此返回 `()` 而非 `Result`。
///
/// # 参数
/// - `log_path`：日志文件路径（append 模式打开/创建）。
/// - `now`：当前时间（unix 秒），取整数秒写入。
/// - `emit`：本次的输出格式。
/// - `general_db`：本次解析出的公共库路径。
/// - `scope`：本次锚定出的作用域（取其 kind / name / db）。
fn append_session_log(
    log_path: &Path,
    now: f64,
    emit: EmitFormat,
    general_db: &Path,
    scope: &ProjectScope,
) {
    use std::io::Write as _;

    // 父目录不存在先创建；失败则直接放弃写日志（不报错、不影响主输出）。
    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() && std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }

    let emit_str = match emit {
        EmitFormat::Text => "text",
        EmitFormat::Json => "json",
    };
    // now 取非负整数秒（防御异常负值），与文档约定的「unix 秒」一致。
    let ts = now.max(0.0) as u64;
    let line = format!(
        "{ts} session-start emit={emit_str} kind={} project={} general={} scope_db={}\n",
        scope_kind_str(scope.kind),
        scope.name,
        general_db.display(),
        scope.db.display(),
    );

    // append 模式打开/创建并写入；任一步失败都静默忽略。
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// 把 [`ScopeKind`] 渲染为日志/输出用的小写短串。
fn scope_kind_str(kind: ScopeKind) -> &'static str {
    match kind {
        ScopeKind::Project => "project",
        ScopeKind::Workspace => "workspace",
    }
}

/// 执行 `session-start` 子命令：按约定推导库路径 → 建父目录 → 打开两库合并 →
/// 按 `--emit` 输出（`text` 逐行打到 stdout；`json` 合成 hook 单行 JSON）→
/// 若给了 `--log` 则追加一行调试记录（失败静默忽略）。
///
/// 任何 IO/库错误走 stderr + 非 0 退出，不 panic。空库渲染为空索引（不报错）。
/// `--emit text`（缺省）的 stdout 与历史行为逐字节一致。
fn run_session_start(args: SessionStartArgs<'_>) -> ExitCode {
    let emit = match parse_emit_format(args.emit) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("session-start 失败：{e}");
            return ExitCode::FAILURE;
        }
    };
    let Some(now) = resolve_now(args.now) else {
        return ExitCode::FAILURE;
    };
    let (general_db, scope) = match resolve_scope_and_prepare(args.project_dir, args.general_db) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("session-start 失败：{e}");
            return ExitCode::FAILURE;
        }
    };

    // 版本触发的一次性迁移（best-effort，绝不阻断热索引注入）：在渲染热索引之前，
    // 若库级 data_version 落后于当前引擎，尝试一次 migrate --if-needed；失败只留痕、
    // 照常注入（下次会话重试）。只在 session-start 触发，迁移只跑一次。
    maybe_migrate_on_session_start(&general_db, &scope, now, args.log);

    // 打开公共库与本作用域库，各自读出后合并（作用域库即挂载集）。
    let merged = match read_merged_scope(&general_db, &scope) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("session-start 失败：{e}");
            return ExitCode::FAILURE;
        }
    };

    // 前言（一行）+ 热索引。前言提示按指针查 ground truth、勿凭印象。
    // 注意：text 分支在预算内时须与历史输出逐字节一致（println! 前言 + print! render）。
    // 渲染带字符预算（--budget，0=不限）：注入成本封顶，超出部分从低优先级整段截断。
    const PREAMBLE: &str = "「以下是你的 engram 长期记忆热索引。需要细节时按每条的指针去查 ground truth；不要凭印象。」";
    let rendered = engram::render::render_budgeted(&merged, now, &[], args.budget);

    let status = match emit {
        EmitFormat::Text => {
            println!("{PREAMBLE}");
            print!("{rendered}");
            ExitCode::SUCCESS
        }
        EmitFormat::Json => {
            // additionalContext = 「前言行 + 换行 + render 输出」整段文本。
            // 与 text 分支语义对齐：println! 的前言后带一个换行，render 自带其换行。
            let context = format!("{PREAMBLE}\n{rendered}");
            match build_hook_json(&context) {
                Ok(json) => {
                    println!("{json}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("session-start 失败：{e}");
                    ExitCode::FAILURE
                }
            }
        }
    };

    // 调试日志（best-effort）：在主输出之后追加一行，失败不影响退出码。
    if let Some(log_path) = args.log {
        append_session_log(log_path, now, emit, &general_db, &scope);
    }

    status
}

/// 打开公共库 + 本作用域库（`scope.db`），读出全部记忆合并为一个 `Vec`。
///
/// 作用域库即本次挂载集（具体项目库或项目管理库），其记忆已自带 `project` 标注。
///
/// # Errors
/// 任一库打开或读取失败时，返回带库类别说明的错误字符串。
fn read_merged_scope(general_db: &Path, scope: &ProjectScope) -> Result<Vec<Memory>, String> {
    let (_gdb, mut merged) = open_and_read(general_db)
        .map_err(|e| format!("读取公共库 {} 失败：{e}", general_db.display()))?;
    // 作用域(项目)库：**仅当文件已存在才读，不存在则当空、绝不创建**。只读 hook
    // (hot-index / session-start) 不该在子目录里凭空建出杂散锚点把一个项目碎片化成
    // 多个——项目锚点只由 write / root 显式建立（与 run_status 的处理保持一致）。
    if scope.db.is_file() {
        let (_sdb, mut smems) = open_and_read(&scope.db).map_err(|e| {
            format!(
                "读取作用域库 {}（{}）失败：{e}",
                scope.name,
                scope.db.display()
            )
        })?;
        merged.append(&mut smems);
    }
    Ok(merged)
}

/// 执行 `resolve` 子命令：从 cwd 锚定作用域 → 建父目录 → 以 env|json 打印各项。
///
/// 输出语义（作用域库 `scope.db` 充当 project_db、作用域名 `scope.name` 充当
/// project_name，便于调用脚本沿用旧字段名透传）：
/// - `--format env`（缺省）逐行打印 `ENGRAM_GENERAL_DB` / `ENGRAM_PROJECT_DB` /
///   `ENGRAM_PROJECT_NAME` / `ENGRAM_SCOPE_KIND`（`project|workspace`）；
/// - `--format json` 用 serde_json 打印单行对象，含 `general_db` / `project_db` /
///   `project_name` / `kind` 四个字段。
///
/// 未知 format 值走 stderr + 非 0 退出。
fn run_resolve(project_dir: Option<&Path>, general_db: Option<&Path>, format: &str) -> ExitCode {
    let (general_db, scope) = match resolve_scope_and_prepare(project_dir, general_db) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("resolve 失败：{e}");
            return ExitCode::FAILURE;
        }
    };
    let kind = scope_kind_str(scope.kind);

    match format.trim().to_ascii_lowercase().as_str() {
        "env" => {
            println!("ENGRAM_GENERAL_DB={}", general_db.display());
            println!("ENGRAM_PROJECT_DB={}", scope.db.display());
            println!("ENGRAM_PROJECT_NAME={}", scope.name);
            println!("ENGRAM_SCOPE_KIND={kind}");
            ExitCode::SUCCESS
        }
        "json" => {
            // 用 serde_json 转义路径/名称，保证含特殊字符（反斜杠等）也合法。
            let obj = format!(
                r#"{{"general_db":{},"project_db":{},"project_name":{},"kind":{}}}"#,
                json_str(&general_db.to_string_lossy()),
                json_str(&scope.db.to_string_lossy()),
                json_str(&scope.name),
                json_str(kind),
            );
            println!("{obj}");
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("resolve 失败：未知 --format {other}（应为 env|json）");
            ExitCode::FAILURE
        }
    }
}

/// `review-prepare` 子命令的全部参数（聚成结构体，规避过多形参）。
struct ReviewPrepareArgs<'a> {
    transcript: &'a Path,
    session_id: &'a str,
    watermark: &'a Path,
    work_dir: &'a Path,
    general_db: &'a Path,
    project_db: &'a Path,
    project_name: &'a str,
}

/// 执行 `review-prepare`（SessionEnd 调）：算增量 → 切片 → 落 pending → 打印复盘 JSON。
///
/// 无新增（transcript 行数 ≤ 水位线）时打印 `{"action":"skip"}` 并成功退出；否则把
/// `[水位线+1 .. 末尾]` 切到 `<work-dir>/<sid>.slice.jsonl`、写 `<work-dir>/<sid>.json`
/// pending 标记，再打印 `{"action":"review", ...}`（含切片、pending、库路径等），供
/// 调用脚本据以起复盘者。
fn run_review_prepare(args: ReviewPrepareArgs<'_>) -> ExitCode {
    // hook.log 与复盘产物同目录（watermark 的父目录，生产即 ~/.engram）——
    // 关键失败除 stderr 外同步留痕（hook 场景 stderr 常被吞，事后只能靠它）。
    let engram_dir = args.watermark.parent().map(Path::to_path_buf);
    let log_fail = |msg: &str| {
        if let Some(p) = hook_log_path(engram_dir.as_deref()) {
            let ts = system_now().unwrap_or(0.0);
            append_hook_log(&p, ts, msg);
        }
    };

    let end = match session::count_lines(args.transcript) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("review-prepare 失败：数 transcript 行数失败：{e}");
            log_fail(&format!("review-prepare 数 transcript 行数失败：{e}"));
            return ExitCode::FAILURE;
        }
    };
    let transcript_key = args.transcript.to_string_lossy().to_string();
    let wm = session::read_watermark(args.watermark);
    let start = wm.get(&transcript_key).copied().unwrap_or(0);
    if end <= start {
        println!(r#"{{"action":"skip"}}"#);
        return ExitCode::SUCCESS;
    }

    let slice_path = args
        .work_dir
        .join(format!("{}.slice.jsonl", args.session_id));
    if let Err(e) = session::slice_lines(args.transcript, start, end, &slice_path) {
        eprintln!("review-prepare 失败：切片失败：{e}");
        log_fail(&format!("review-prepare 切片失败：{e}"));
        return ExitCode::FAILURE;
    }

    let Some(now) = resolve_now(None) else {
        return ExitCode::FAILURE;
    };
    let pending_path = args.work_dir.join(format!("{}.json", args.session_id));
    let pending = Pending {
        session_id: args.session_id.to_string(),
        transcript: transcript_key,
        slice: slice_path.to_string_lossy().to_string(),
        start_line: start,
        end_line: end,
        general_db: args.general_db.to_string_lossy().to_string(),
        project_db: args.project_db.to_string_lossy().to_string(),
        project_name: args.project_name.to_string(),
        created_at: now,
    };
    if let Err(e) = session::write_pending(&pending_path, &pending) {
        eprintln!("review-prepare 失败：写 pending 失败：{e}");
        log_fail(&format!("review-prepare 写 pending 失败：{e}"));
        return ExitCode::FAILURE;
    }

    // 领单：对该 pending 原子建 claim 锁，宣示本场复盘所有权——挡住 SessionStart
    // catchup 同时领走同一个 pending 的竞态。本进程是新建 pending 的天然 owner，
    // 正常必 Acquired；极罕见撞上 Held（同 sid 已有人在复盘）或建锁出错也不失败，
    // 照常输出复盘 JSON，仅记一行 stderr 供诊断（claim 是保护层，不是硬前置）。
    let claim_path = session::claim_path_for(&pending_path);
    match session::try_claim(&claim_path, now, std::process::id()) {
        Ok(session::ClaimOutcome::Acquired) => {}
        Ok(session::ClaimOutcome::Held) => {
            eprintln!(
                "review-prepare：{} 的 claim 已被他人持有（同 sid 复盘进行中？），照常输出",
                pending_path.display()
            );
        }
        Err(e) => {
            eprintln!("review-prepare：建 claim 失败（不影响复盘输出）：{e}");
        }
    }

    print_review_json(&pending, &pending_path)
}

/// 执行 `catchup-scan`（SessionStart 调）：补最近一场残留 pending、清掉更早的。
///
/// 无残留时打印 `{"action":"none"}`。有残留时取 `created_at` 最大的一条，先对其
/// claim 锁 [`session::try_claim`] 领单——领不到（已被在跑的复盘者持有）说明这场
/// 正有人复盘，打印 `{"action":"none"}` 直接返回，**这就是挡住双复盘竞态的关口**；
/// 领到才补跑（其余更早残留连切片、claim 一并删除——轻量策略只补最近一场）；若其
/// 切片已不在则据 pending 区间重切，再打印 `{"action":"review", ...}`。
///
/// 给了 `sessions_dir` + `watermark` 时，扫 pending 之前先做**孤儿会话补登**
/// （[`rebuild_orphan_pending`]）：强杀/断电场景 SessionEnd 不触发、pending 根本
/// 没落下，靠 UserPromptSubmit 留下的活跃会话登记现场补建 pending，再按原流程领取
/// ——补跑仍保持「只补最近一场」策略（补建的 pending 与既有残留一起按 created_at
/// 排序取最近）。这使 README「崩溃/强关可补」的承诺覆盖到「连 pending 都没有」的场景。
fn run_catchup_scan(
    work_dir: &Path,
    sessions_dir: Option<&Path>,
    watermark: Option<&Path>,
) -> ExitCode {
    // 孤儿会话补登（best-effort）：失败只记 stderr + hook.log，不阻断原有补跑流程。
    if let (Some(sdir), Some(wm_path)) = (sessions_dir, watermark) {
        if let Err(e) = rebuild_orphan_pending(work_dir, sdir, wm_path) {
            eprintln!("catchup-scan: 孤儿会话补登失败（继续原流程）：{e}");
            if let Some(p) = hook_log_path(work_dir.parent()) {
                append_hook_log(
                    &p,
                    system_now().unwrap_or(0.0),
                    &format!("catchup-scan 孤儿会话补登失败：{e}"),
                );
            }
        }
    }

    let mut list = match session::list_pending(work_dir) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("catchup-scan 失败：扫描 pending 失败：{e}");
            if let Some(p) = hook_log_path(work_dir.parent()) {
                append_hook_log(
                    &p,
                    system_now().unwrap_or(0.0),
                    &format!("catchup-scan 扫描 pending 失败：{e}"),
                );
            }
            return ExitCode::FAILURE;
        }
    };
    // 升序排列，末尾即最近一场。
    let Some((pending_path, pending)) = list.pop() else {
        println!(r#"{{"action":"none"}}"#);
        return ExitCode::SUCCESS;
    };

    // 领单：claim 被持有（未陈旧）= 另一个复盘者正在处理这场（典型：SessionEnd
    // 刚起的 reviewer 还在跑）——本次不补跑，输出 none，避免两套重复记忆入库。
    // 更早的残留也先不动：等在跑的那场收尾后，下次 catchup 再统一清。
    let Some(now) = resolve_now(None) else {
        return ExitCode::FAILURE;
    };
    let claim_path = session::claim_path_for(&pending_path);
    match session::try_claim(&claim_path, now, std::process::id()) {
        Ok(session::ClaimOutcome::Acquired) => {}
        Ok(session::ClaimOutcome::Held) => {
            println!(r#"{{"action":"none"}}"#);
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("catchup-scan 失败：建 claim 失败：{e}");
            return ExitCode::FAILURE;
        }
    }

    // 其余（更早的）残留连切片、claim 一并清掉：轻量策略只补最近一场，不留孤儿锁。
    for (pp, p) in &list {
        session::remove_pending(pp, Path::new(&p.slice));
        session::release_claim(&session::claim_path_for(pp));
    }

    // 切片可能已被清理；不在就据 pending 记录的区间重切，保证复盘者有增量可读。
    let slice_path = PathBuf::from(&pending.slice);
    if !slice_path.exists() {
        let transcript = PathBuf::from(&pending.transcript);
        if let Err(e) = session::slice_lines(
            &transcript,
            pending.start_line,
            pending.end_line,
            &slice_path,
        ) {
            eprintln!("catchup-scan 失败：重切增量失败：{e}");
            if let Some(p) = hook_log_path(work_dir.parent()) {
                append_hook_log(
                    &p,
                    system_now().unwrap_or(0.0),
                    &format!("catchup-scan 重切增量失败：{e}"),
                );
            }
            // 补跑没起成，释放刚领的 claim，让下次会话能立即重试（不必等 TTL）。
            session::release_claim(&claim_path);
            return ExitCode::FAILURE;
        }
    }

    print_review_json(&pending, &pending_path)
}

/// 孤儿会话补登：把「登记过、无对应 pending、且 transcript 现有行数 > 水位线」的
/// **最近一个**孤儿 transcript 现场补建成 pending（行区间 = [水位线, 当前行数]），
/// 供随后的原流程领取。
///
/// 背景：pending 只在 SessionEnd 落；强杀/断电时 SessionEnd 不触发，整场增量丢失。
/// UserPromptSubmit 路径的 hot-index 每条 prompt 都会把
/// `{transcript, session_id, 库路径, 时间}` 登记到 `sessions_dir`（见
/// [`register_active_session`]），本函数据此闭环。补建成功后删除该登记文件
/// （避免重复补建；正常收尾的会话其登记因「行数 ≤ 水位线」永远不会被选中，
/// 由 7 天 TTL 自然清掉）。只补最近一个孤儿——与「只补最近一场」的轻量策略一致。
///
/// # Errors
/// 读登记目录 / 扫 pending / 写补建 pending 失败时返回中文错误说明（调用方降级继续）。
fn rebuild_orphan_pending(
    work_dir: &Path,
    sessions_dir: &Path,
    watermark_path: &Path,
) -> Result<(), String> {
    // 登记目录不存在 = 无孤儿（老版本 hook 或从未有过会话），静默返回。
    if !sessions_dir.is_dir() {
        return Ok(());
    }
    // 读全部登记（坏文件静默跳过，不让一个坏登记拖垮补登）。
    let mut regs: Vec<(PathBuf, SessionRegistration)> = Vec::new();
    let rd = std::fs::read_dir(sessions_dir)
        .map_err(|e| format!("读登记目录 {} 失败：{e}", sessions_dir.display()))?;
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(reg) = serde_json::from_slice::<SessionRegistration>(&bytes) {
                regs.push((path, reg));
            }
        }
    }
    if regs.is_empty() {
        return Ok(());
    }

    // 已有 pending 的 transcript 集合：登记若已有对应 pending（SessionEnd 正常落过），
    // 不重复补建——那是原流程的事。
    let existing: HashSet<String> = session::list_pending(work_dir)
        .map_err(|e| format!("扫描 pending 失败：{e}"))?
        .into_iter()
        .map(|(_, p)| p.transcript)
        .collect();
    let wm = session::read_watermark(watermark_path);

    // 最近优先（登记 ts 降序），找第一个满足「无对应 pending 且行数 > 水位线」的孤儿。
    regs.sort_by(|a, b| {
        b.1.ts
            .partial_cmp(&a.1.ts)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for (reg_path, reg) in &regs {
        if existing.contains(&reg.transcript_path) {
            continue;
        }
        let transcript = PathBuf::from(&reg.transcript_path);
        let end = match session::count_lines(&transcript) {
            Ok(n) => n,
            Err(_) => continue, // transcript 已不可读（被清理等）：跳过该孤儿。
        };
        let start = wm.get(&reg.transcript_path).copied().unwrap_or(0);
        if end <= start {
            continue; // 无未巩固增量：不是孤儿（或已被别的路径巩固过）。
        }

        // 补建 pending（文件名前缀 = 净化后的会话 id，与 review-prepare 的产物同构；
        // created_at 取登记时间，保证与既有残留按「最近一场」正确排序）。
        let sid = sanitize_file_stem(&reg.session_id);
        let slice_path = work_dir.join(format!("{sid}.slice.jsonl"));
        let pending = Pending {
            session_id: reg.session_id.clone(),
            transcript: reg.transcript_path.clone(),
            slice: slice_path.to_string_lossy().to_string(),
            start_line: start,
            end_line: end,
            general_db: reg.general_db.clone(),
            project_db: reg.project_db.clone(),
            project_name: reg.project_name.clone(),
            created_at: reg.ts,
        };
        let pending_path = work_dir.join(format!("{sid}.json"));
        session::write_pending(&pending_path, &pending)
            .map_err(|e| format!("写补建 pending 失败：{e}"))?;
        // 切片交给原流程的「切片不在则重切」逻辑，这里不切（少一处重复实现）。
        eprintln!(
            "catchup-scan: 已为孤儿会话 {} 补建 pending（行 {}..{}）",
            reg.session_id, start, end
        );
        // 删除登记，避免下次重复补建（best-effort）。
        let _ = std::fs::remove_file(reg_path);
        break; // 只补最近一个孤儿。
    }
    Ok(())
}

/// 执行 `consolidate-done`（复盘者收尾调）：推进水位线 + 删 pending、切片与 claim。
///
/// 读 pending → 对水位线加粗粒度文件锁（`watermark.lock`，持锁毫秒级）→ 把
/// `watermark[transcript]` 抬到 `end_line`（取较大值，幂等）→ 原子写回 → 释放锁 →
/// 原子写 `last-review-ok` 健康标记 → 删 pending 与切片 → 释放 claim 锁（领单收尾
/// 闭环），最后打印 `{"action":"done", ...}`。关键失败除 stderr 外同步追加 hook.log。
fn run_consolidate_done(pending_path: &Path, watermark: &Path) -> ExitCode {
    // hook.log 与复盘产物同目录（watermark 的父目录，生产即 ~/.engram）。
    let engram_dir = watermark.parent().map(Path::to_path_buf);
    let log_fail = |now: f64, msg: &str| {
        if let Some(p) = hook_log_path(engram_dir.as_deref()) {
            append_hook_log(&p, now, msg);
        }
    };
    let Some(now) = resolve_now(None) else {
        return ExitCode::FAILURE;
    };

    let pending = match session::read_pending(pending_path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("consolidate-done 失败：读 pending 失败：{e}");
            log_fail(now, &format!("consolidate-done 读 pending 失败：{e}"));
            return ExitCode::FAILURE;
        }
    };

    // 确定性巩固：复盘者一旦收尾，就在推进水位线前把升降级/容量/淘汰状态机跑一遍
    // ——升降级本是纯算法、无需 LLM 判断，故不再依赖复盘者是否记得手动跑
    // `consolidate`（confirm-use/supersede/merge 这类需语义判断的仍留给复盘者）。
    // best-effort：巩固失败只记 stderr，绝不阻断水位线推进与 pending 清理。
    let mut transition_count = 0usize;
    {
        let mut pmap: BTreeMap<String, PathBuf> = BTreeMap::new();
        if !pending.project_name.is_empty() && !pending.project_db.is_empty() {
            pmap.insert(
                pending.project_name.clone(),
                PathBuf::from(&pending.project_db),
            );
        }
        match consolidate_libs(Path::new(&pending.general_db), &pmap, now, false) {
            Ok(ts) => {
                transition_count = ts.len();
                if !ts.is_empty() {
                    eprintln!("consolidate-done: 确定性巩固 {} 条变迁", ts.len());
                }
            }
            Err(e) => eprintln!("consolidate-done: 巩固跳过（{e}）"),
        }
    }

    // 水位线读-改-写加粗粒度文件锁：并发的两个收尾者（如 SessionEnd 复盘 + 手动
    // consolidate-done）同时读-改-写会互相丢更新。锁用 create_new claim 原语
    // （watermark.lock），持锁毫秒级；stale 超 30 秒可夺（持有者崩溃不至长期卡死）。
    let lock_path = watermark.with_extension("lock");
    let mut locked = false;
    for _ in 0..40 {
        match session::try_claim_ttl(
            &lock_path,
            now,
            std::process::id(),
            session::WATERMARK_LOCK_TTL_SECS,
        ) {
            Ok(session::ClaimOutcome::Acquired) => {
                locked = true;
                break;
            }
            Ok(session::ClaimOutcome::Held) => {
                // 对方持锁毫秒级，稍等重试（总预算约 2 秒）。
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("consolidate-done: 建 watermark 锁失败（无锁继续）：{e}");
                break;
            }
        }
    }
    if !locked {
        // 重试预算用尽仍拿不到锁：退化为旧行为（无锁写回）并留痕——收尾绝不能
        // 因为锁而放弃（否则 pending 永不清、水位线永不推进）。
        eprintln!("consolidate-done: 未取得 watermark 锁，按旧行为无锁写回（冲突概率极低）");
        log_fail(now, "consolidate-done 未取得 watermark 锁，无锁写回");
    }

    let mut wm = session::read_watermark(watermark);
    let cur = wm.get(&pending.transcript).copied().unwrap_or(0);
    if pending.end_line > cur {
        wm.insert(pending.transcript.clone(), pending.end_line);
    }
    let write_result = session::write_watermark(watermark, &wm);
    if locked {
        session::release_claim(&lock_path);
    }
    if let Err(e) = write_result {
        eprintln!("consolidate-done 失败：写水位线失败：{e}");
        log_fail(now, &format!("consolidate-done 写水位线失败：{e}"));
        return ExitCode::FAILURE;
    }

    // 成功收尾 → 原子写 last-review-ok 健康标记（status 的复盘健康面据此判停摆）。
    // best-effort：健康标记是旁路，写失败只记 stderr，不影响收尾成功语义。
    if let Some(dir) = &engram_dir {
        let ok = serde_json::json!({
            "ts": now,
            "transcript": pending.transcript,
            "start_line": pending.start_line,
            "end_line": pending.end_line,
            "transitions": transition_count,
        });
        if let Ok(content) = serde_json::to_string_pretty(&ok) {
            if let Err(e) = write_atomic_str(&dir.join("last-review-ok"), &content) {
                eprintln!("consolidate-done: 写 last-review-ok 失败（不影响收尾）：{e}");
            }
        }
    }

    session::remove_pending(pending_path, Path::new(&pending.slice));
    // 释放本场的 claim 锁（与领单配对的收尾；best-effort，同 remove_pending 风格）。
    session::release_claim(&session::claim_path_for(pending_path));

    let obj = serde_json::json!({
        "action": "done",
        "transcript": pending.transcript,
        "watermark_line": pending.end_line,
    });
    match serde_json::to_string(&obj) {
        Ok(s) => {
            println!("{s}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("consolidate-done 失败：序列化输出失败：{e}");
            ExitCode::FAILURE
        }
    }
}

/// 执行 `root` 子命令：把 `--project-dir`（缺省当前目录）设为 engram 项目管理目录。
///
/// 步骤（任一失败走 stderr + 非 0 退出，不 panic）：
/// 1. 规范化 `project_dir`（[`canonicalize_cwd`]，含剥离 Windows verbatim 前缀）。
/// 2. 防嵌套：沿其**父链（不含自己）**向上，若任一祖先已是项目管理目录
///    （`<祖先>/.engram/workspace` 是文件）→ 失败返回（不允许嵌套管理目录）。
/// 3. 幂等：若本目录 `.engram/workspace` 已存在 → 打印「已是」并成功返回。
/// 4. 否则：建 `.engram/`（已存在则复用）→ 写 `workspace` 标记（`engram-workspace v1
///    created_at=<秒>`）→ 用 [`store::open`] 打开/建项目库（已有库则保留其内容）→
///    往公共库追加一条 L2「管理目录」记忆 → 打印该管理目录的**绝对路径**。
///
/// 注：唯一的拒绝条件是「防嵌套」（父链已有管理目录，第 2 步）。本目录已存在
/// `.engram/` 或项目库**不**构成拒绝——hot-index hook 会在任意目录自动建
/// `.engram/engram.redb`，若据此拒绝，则任何开过会话的目录都再也无法设为管理目录。
fn run_root(project_dir: Option<&Path>, general_db: Option<&Path>) -> ExitCode {
    // 1. 解析并规范化目标目录。
    let raw_dir = match resolve_project_dir(project_dir) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("engram root 失败：{e}");
            return ExitCode::FAILURE;
        }
    };
    let dir = canonicalize_cwd(&raw_dir);

    // 2. 防嵌套：检查父链（不含自己）上是否已有项目管理目录。
    for ancestor in dir.ancestors().skip(1) {
        let marker = ancestor.join(ENGRAM_DIR).join(WORKSPACE_MARKER);
        if marker.is_file() {
            eprintln!(
                "engram root 失败：不能嵌套——父目录 {} 已是 engram 项目管理目录",
                ancestor.display()
            );
            return ExitCode::FAILURE;
        }
    }

    let engram_dir = dir.join(ENGRAM_DIR);
    let workspace_marker = engram_dir.join(WORKSPACE_MARKER);
    let scope_db = engram_dir.join(ENGRAM_DB_FILE);

    // 3. 幂等：已是管理目录则直接成功。
    if workspace_marker.is_file() {
        println!("{} 已是 engram 项目管理目录", dir.display());
        return ExitCode::SUCCESS;
    }

    // 取当前时间（用于 workspace 标记内容、记忆 created_at / id）。
    let Some(now) = resolve_now(None) else {
        return ExitCode::FAILURE;
    };

    // 4a. 建 .engram/ 目录（已存在则复用）。
    if let Err(e) = std::fs::create_dir_all(&engram_dir) {
        eprintln!(
            "engram root 失败：创建目录 {} 失败：{e}",
            engram_dir.display()
        );
        return ExitCode::FAILURE;
    }

    // 4b. 写 workspace 标记文件。
    let marker_content = format!("engram-workspace v1 created_at={}", now.max(0.0) as u64);
    if let Err(e) = std::fs::write(&workspace_marker, marker_content) {
        eprintln!(
            "engram root 失败：写 workspace 标记 {} 失败：{e}",
            workspace_marker.display()
        );
        return ExitCode::FAILURE;
    }

    // 4c. 用 store::open 打开/建项目库（已有库则保留其内容）。
    if let Err(e) = store::open(&scope_db) {
        eprintln!(
            "engram root 失败：创建项目库 {} 失败：{e}",
            scope_db.display()
        );
        return ExitCode::FAILURE;
    }

    // 4d. 往公共库追加一条 L2「管理目录」记忆。
    let general_db: PathBuf = match general_db {
        Some(p) => p.to_path_buf(),
        None => match home_dir(real_env_lookup) {
            Ok(home) => home.join(ENGRAM_DIR).join("general.redb"),
            Err(e) => {
                eprintln!("engram root 失败：{e}");
                return ExitCode::FAILURE;
            }
        },
    };
    let abs = dir.display().to_string();
    let cue = format!("{abs} 是 engram 项目管理目录，不可直接当项目用，请在其下建具体项目目录");
    let memory = Memory {
        id: generate_id(&cue, now),
        cue,
        pointer: Pointer {
            kind: "none".to_string(),
            reference: None,
            detail: None,
        },
        level: Level::L2,
        project: None,
        importance: 0.6,
        pinned: false,
        access_log: Vec::new(),
        status: Status::Active,
        superseded_by: None,
        created_at: now,
        tags: vec!["workspace".to_string()],
        schema_version: MEMORY_SCHEMA_VERSION,
    };
    let gdb = match store::open(&general_db) {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "engram root 失败：打开公共库 {} 失败：{e}",
                general_db.display()
            );
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = store::put(&gdb, &memory) {
        eprintln!("engram root 失败：写入管理目录记忆失败：{e}");
        return ExitCode::FAILURE;
    }

    // 5e. 打印管理目录绝对路径（供调用脚本使用）。
    println!("{abs}");
    ExitCode::SUCCESS
}

/// 打印 `action=review` 的复盘 JSON（`review-prepare` 与 `catchup-scan` 共用）。
///
/// 含复盘者所需的一切：增量切片路径、待收尾的 pending 路径、库路径与项目名、行区间。
fn print_review_json(pending: &Pending, pending_path: &Path) -> ExitCode {
    let obj = serde_json::json!({
        "action": "review",
        "slice": pending.slice,
        "pending": pending_path.to_string_lossy(),
        "general_db": pending.general_db,
        "project_db": pending.project_db,
        "project_name": pending.project_name,
        "start_line": pending.start_line,
        "end_line": pending.end_line,
    });
    match serde_json::to_string(&obj) {
        Ok(s) => {
            println!("{s}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("序列化复盘 JSON 失败：{e}");
            ExitCode::FAILURE
        }
    }
}

/// `hot-index` 命令的全部参数（聚成一个结构体，规避过多函数形参）。
struct HotIndexArgs<'a> {
    /// `--general-db` 覆盖；`None` 时走 `<HOME>/.engram/general.redb` 约定。
    general_db: Option<&'a Path>,
    /// `--workspace-root`（作 cwd override）；`None` 时取 stdin 的 cwd，再缺省取当前目录。
    workspace_root: Option<&'a Path>,
    /// `--transcript`（历史信号字段，新作用域模型已不使用；仅为兼容保留接收）。
    transcript: Option<&'a Path>,
    /// `--prompt`（历史信号字段，新作用域模型已不使用；仅为兼容保留接收）。
    prompt: Option<&'a str>,
    /// `--state`；给定时启用状态门控（按作用域根路径比较）。
    state: Option<&'a Path>,
    /// `--status-file`；给定时每次都把挂载集的一行状态串写入该文件（覆盖）。
    status_file: Option<&'a Path>,
    /// `--status-dir`；给定时按 `<dir>/<session_id>.txt` 分会话写状态串（优先于
    /// `status_file`，多窗口互不覆盖）。
    status_dir: Option<&'a Path>,
    /// 输出格式：`text`（缺省）或 `json`。
    emit: &'a str,
    /// `--emit json` 时写入的 hookEventName。
    hook_event: &'a str,
    /// 是否从 stdin 读取整段 hook JSON 作兜底。
    from_hook_stdin: bool,
    /// 可选调试日志路径。
    log: Option<&'a Path>,
    /// 渲染字符预算（0=不限）。
    budget: usize,
    /// 当前时间（unix 秒）；`None` 取系统时间。
    now: Option<f64>,
}

/// 从 stdin 解析出的 hook 兜底字段（缺失即为 `None`）。
///
/// 新作用域模型需 hook 给的 `cwd`（用于从其向上锚定作用域）、`session_id`
/// （状态栏 / 门控按会话分文件用，避免多窗口共用单文件互相覆盖）与
/// `transcript_path`（活跃会话登记用，供崩溃场景的孤儿补建）；历史的
/// `prompt` 不再参与判定，故不再解析。
#[derive(Debug, Default)]
struct HookStdin {
    /// hook 给的当前工作目录。
    cwd: Option<PathBuf>,
    /// hook 给的会话 id（状态栏 / 门控按会话分文件用）。
    session_id: Option<String>,
    /// hook 给的 transcript 路径（活跃会话登记用）。
    transcript_path: Option<String>,
}

/// 读取整段 stdin、按 hook JSON 解析出 `cwd`。
///
/// stdin 为空、非法 JSON、或不是对象时一律返回 [`HookStdin::default`]（`cwd` 为 `None`）——
/// **静默当作无**，不报错、不 panic（UserPromptSubmit hook 偶尔无 stdin 是正常的）。
///
/// 取字段时只接受字符串值；非字符串或缺字段对应项为 `None`。
fn read_hook_stdin() -> HookStdin {
    use std::io::Read as _;
    let mut buf = String::new();
    if std::io::stdin().read_to_string(&mut buf).is_err() {
        return HookStdin::default();
    }
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        return HookStdin::default();
    }
    let value: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return HookStdin::default(),
    };
    let obj = match value.as_object() {
        Some(o) => o,
        None => return HookStdin::default(),
    };
    let cwd = obj.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from);
    // session_id：状态栏 / 门控按会话分文件用（避免多窗口共用单文件互相覆盖）；空串视作无。
    let session_id = obj
        .get("session_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    // transcript_path：活跃会话登记用（崩溃场景孤儿补建的线索）；空串视作无。
    let transcript_path = obj
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    HookStdin {
        cwd,
        session_id,
        transcript_path,
    }
}

/// 读状态文件里上次记录的作用域根路径（去首尾空白）。文件不存在/读失败/为空 → `None`。
fn read_state(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 把本次作用域根的绝对路径字符串**原子地**写回状态文件（tmp+rename，父目录不存在
/// 先创建）。失败时返回错误说明。
///
/// # Errors
/// 创建父目录、写临时文件或 rename 失败时返回带路径说明的错误字符串。
fn write_state(path: &Path, scope_root: &str) -> Result<(), String> {
    write_atomic_str(path, scope_root)
}

/// 由 `--state` 基准路径 + 会话 id 推导**按会话分文件**的门控状态路径：
/// `<base 父目录>/active-state/<净化后 session_id>.state`。
///
/// 背景：hooks.json 的 UserPromptSubmit 传全局单文件 `--state ~/.engram/active.state`，
/// 两个窗口跨项目交替时 scope.root 在单文件里反复翻转、门控失效、每条 prompt 都全量
/// 重注入。改为按会话分文件后各窗口互不干扰；`--state` 原路径仅作定位基准保留兼容，
/// 旧单文件不再读写。无 session_id（如手动调试）时回退 `default.state`。
fn session_state_path(state_base: &Path, session_id: Option<&str>) -> PathBuf {
    let dir = state_base
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let sid = sanitize_file_stem(session_id.unwrap_or("default"));
    dir.join("active-state").join(format!("{sid}.state"))
}

/// 一条活跃会话登记（`~/.engram/active-sessions/<sid>.json`），UserPromptSubmit
/// 路径的 hot-index 每条 prompt 原子刷写。
///
/// 用途（崩溃场景补漏）：pending 只在 SessionEnd 落，强杀/断电时 SessionEnd 不触发、
/// 整场增量丢失。有了登记，下次 SessionStart 的 catchup-scan 能据
/// `transcript_path` + 水位线现场补建 pending（见 `rebuild_orphan_pending`）。
/// 库路径与项目名一并登记，补建的 pending 才能透传给复盘者。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SessionRegistration {
    /// 会话 id（登记文件名与补建 pending 的文件名前缀）。
    session_id: String,
    /// transcript（JSONL）路径——孤儿判定与补切的依据。
    transcript_path: String,
    /// 最近一次刷写时间（unix 秒）；catchup 挑「最近一个孤儿」用。
    ts: f64,
    /// 公共库路径（透传给补建的 pending）。
    general_db: String,
    /// 作用域（项目）库路径（透传给补建的 pending）。
    project_db: String,
    /// 作用域（项目）名（透传给补建的 pending）。
    project_name: String,
}

/// 登记目录 / 门控目录里旧文件的保留窗口（秒）：7 天，与状态栏会话文件一致。
const ACTIVE_FILE_TTL_SECS: u64 = 7 * 24 * 3600;

/// 删 `dir` 内 mtime 早于「现在 − `ttl_secs`」且扩展名为 `ext` 的文件
/// （best-effort，全程静默）。[`cleanup_stale_status`] 的通用化版本。
fn cleanup_stale_files(dir: &Path, ext: &str, ttl_secs: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(ext) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let Ok(age) = modified.elapsed() else {
            continue;
        };
        if age.as_secs() > ttl_secs {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// 把本会话登记到 `<state_base 父目录>/active-sessions/<sid>.json`（原子写，
/// best-effort：登记是崩溃兜底旁路，任何失败都静默、绝不影响注入主流程）。
/// 顺手按 7 天 TTL 清理目录内过期登记。
fn register_active_session(state_base: &Path, reg: &SessionRegistration) {
    let dir = state_base
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("active-sessions");
    let sid = sanitize_file_stem(&reg.session_id);
    let Ok(content) = serde_json::to_string_pretty(reg) else {
        return;
    };
    let _ = write_atomic_str(&dir.join(format!("{sid}.json")), &content);
    cleanup_stale_files(&dir, "json", ACTIVE_FILE_TTL_SECS);
}

/// 向 `hot-index` 的调试日志**追加一行**（best-effort，失败静默忽略）。
///
/// 行格式：`<now> hot-index event=<hook_event> kind=<project|workspace> name=<scope.name> root=<scope.root>\n`。
fn append_hot_index_log(log_path: &Path, now: f64, hook_event: &str, scope: &ProjectScope) {
    use std::io::Write as _;
    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() && std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let ts = now.max(0.0) as u64;
    let line = format!(
        "{ts} hot-index event={hook_event} kind={} name={} root={}\n",
        scope_kind_str(scope.kind),
        scope.name,
        scope.root.display(),
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// hot-index 打库失败时的**降级注入**：stderr 报错、hook.log 留痕，stdout 按
/// emit 格式输出一段最小 additionalContext（格式与成功路径一致），exit 0。
///
/// 设计取舍：hook 失败时它的 stderr 几乎没人看，若只报错退出，该会话就是
/// 「零记忆注入 + 模型全然不知」——模型会自信地凭空行事。降级注入让模型至少
/// 知道「记忆缺席」这一事实并能提示用户自查；exit 0 是 hook 语义（降级但不装死）。
fn degrade_hot_index_unavailable(
    err: &str,
    emit: EmitFormat,
    hook_event: &str,
    log: Option<&Path>,
    now: f64,
) -> ExitCode {
    // stderr 照旧完整报错（供手动调试时看全文）。
    eprintln!("hot-index 失败（降级注入提示后 exit 0）：{err}");

    // 原因摘要：截到 200 字符以内，避免把冗长底层错误整段塞进模型上下文。
    let summary: String = err.chars().take(200).collect();
    let context = format!(
        "⚠ engram 记忆库暂不可用（{summary}），本会话记忆缺席；可稍后用 /engram:status 检查"
    );

    // 失败事件追加 hook.log（--log 给定用给定路径，否则回退 ~/.engram/hook.log）。
    let log_path = match log {
        Some(p) => Some(p.to_path_buf()),
        None => hook_log_path(None),
    };
    if let Some(p) = log_path {
        append_hook_log(&p, now, &format!("hot-index 打库失败降级：{err}"));
    }

    match emit {
        EmitFormat::Text => {
            println!("{context}");
            ExitCode::SUCCESS
        }
        EmitFormat::Json => match build_hot_index_json(hook_event, &context) {
            Ok(json) => {
                println!("{json}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                // 连包 JSON 都失败（理论不可达）：只能放弃注入，但仍按降级语义 exit 0。
                eprintln!("hot-index 降级注入失败：{e}");
                ExitCode::SUCCESS
            }
        },
    }
}

/// 复盘健康快照（`gather_review_health` 的产物）。
struct ReviewHealth {
    /// pending 目录里的残留标记数（`*.json`）。
    pending_count: usize,
    /// 上次成功巩固（last-review-ok）的时间戳（unix 秒）；读不到为 `None`。
    last_ok_ts: Option<f64>,
    /// 疑似停摆判定（见 [`engram::commands::reviewer_stalled`]）。
    stalled: bool,
}

/// 从 `<engram_dir>`（即 `~/.engram`，与公共库同目录）收集复盘健康信号：
/// `pending/` 残留数 + `last-review-ok` 时间戳 → 停摆判定。全程容错，
/// 读不到一律按「无」处理（不让健康检查本身把 status 弄挂）。
fn gather_review_health(engram_dir: &Path, now: f64) -> ReviewHealth {
    // pending 残留数：数 pending/ 下的 *.json 标记文件（不解析内容，坏文件也算残留）。
    let pending_count = std::fs::read_dir(engram_dir.join("pending"))
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
                .count()
        })
        .unwrap_or(0);

    // last-review-ok：consolidate-done 成功收尾时原子写下的 JSON（取其 ts 字段）。
    let last_ok_ts = std::fs::read_to_string(engram_dir.join("last-review-ok"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("ts").and_then(serde_json::Value::as_f64));

    let stalled = engram::commands::reviewer_stalled(pending_count, last_ok_ts, now);
    ReviewHealth {
        pending_count,
        last_ok_ts,
        stalled,
    }
}

/// 把整段注入文本包成 Claude Code hook 的单行 JSON，hookEventName 由调用方给定。
///
/// # Errors
/// 序列化失败（理论上不会发生）时返回其错误说明。
fn build_hot_index_json(hook_event: &str, context: &str) -> Result<String, String> {
    let value = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": hook_event,
            "additionalContext": context,
        }
    });
    serde_json::to_string(&value).map_err(|e| format!("序列化 hook JSON 失败：{e}"))
}

/// 执行 `hot-index` 子命令：从 cwd 向上锚定作用域（找最近的 `.engram/` 锚点），
/// 挂载「公共库 + 该作用域库」并渲染热索引。
///
/// 流程：
/// 1. 解析 emit / now / stdin 兜底（仅 `--from-hook-stdin`）。
/// 2. cwd 取值：`--workspace-root` 优先，否则 stdin.cwd，否则 current_dir。
/// 3. [`resolve_scope`] 锚定作用域（含 cwd 规范化、收紧的锚点判据）→ `(general_db, scope)`。
///    **不为 `scope.db` 建父目录**：只读 hook 不该凭空建出杂散锚点把项目碎片化（项目库
///    缺失即按空处理，锚点只由 write/root 显式建）；公共库父目录由 [`store::open`] 补建。
/// 4. 合并读取改为 **公共库 + `scope.db` 两库**（[`read_merged_scope`]）。
/// 5. 状态栏小文件：照常写 [`oneline_status`]（best-effort，门控判空也写）。
/// 6. `--state` 门控：state 文件存**上次 `scope.root` 的绝对路径字符串**；本次相同则
///    输出空、exit 0；不同则写回后继续渲染。
/// 7. 若 `scope.kind == Workspace`：在注入前言之后追加一行管理目录提示。
/// 8. 按 `--emit` 渲染输出（前言 + 热索引；门控判空时不打印任何东西）→ 可选追加日志。
///
/// 任何 IO/库错误走 stderr + 非 0 退出，不 panic。stdin 解析失败静默当作无。
///
/// 注意：`--transcript` / `--prompt`（及 stdin 的同名字段）在新作用域模型下不再参与
/// 判定，仅为兼容历史 hook 调用契约而保留接收，本函数不读取它们。
fn run_hot_index(args: HotIndexArgs<'_>) -> ExitCode {
    // 历史 hook 调用仍可能传 --prompt；新模型不再用它做任何判定，显式忽略以表意图。
    // --transcript 仍接收：作活跃会话登记的 transcript 兜底（stdin 缺该字段时）。
    let _ = args.prompt;

    // 输出格式复用 session-start 的解析。
    let emit = match parse_emit_format(args.emit) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("hot-index 失败：{e}");
            return ExitCode::FAILURE;
        }
    };
    let Some(now) = resolve_now(args.now) else {
        return ExitCode::FAILURE;
    };

    // 1. stdin 兜底（仅当 --from-hook-stdin）：只用其 cwd 作 cwd 兜底。
    let hook = if args.from_hook_stdin {
        read_hook_stdin()
    } else {
        HookStdin::default()
    };

    // 2. cwd：--workspace-root 优先，否则 stdin.cwd，否则 current_dir()。
    let cwd: PathBuf = match args.workspace_root {
        Some(p) => p.to_path_buf(),
        None => match hook.cwd.clone() {
            Some(c) => c,
            None => match std::env::current_dir() {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("hot-index 失败：获取当前工作目录失败：{e}");
                    return ExitCode::FAILURE;
                }
            },
        },
    };

    // 3. 从 cwd 向上锚定作用域，得 (general_db, scope)。不为作用域(项目)库建父目录：
    //    只读 hook 不该凭空建出杂散锚点把项目碎片化（项目库缺失即按空处理，锚点只由
    //    write/root 显式建）；公共库父目录由 read_merged_scope→store::open 补建。
    let (general_db, scope) = match resolve_scope(&cwd, args.general_db) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("hot-index 失败：{e}");
            return ExitCode::FAILURE;
        }
    };

    // 4. 合并 公共库 + 作用域库（在状态门控之前先读出，因为状态栏小文件无论是否
    //    门控判空都要写最新的一行状态串，需要先有挂载集）。
    //
    //    打库失败**降级而非装死**：此前只 eprintln + 非 0 退出——该会话零记忆注入
    //    且模型上下文里没有任何信号（hook 的 stderr 用户通常看不到）。改为：stderr
    //    照旧报错、hook.log 留痕，但 stdout 按 emit 格式输出一段最小提示注入，让
    //    模型知道「记忆缺席」这一事实，并 exit 0（hook 语义：降级但不失败）。
    let merged = match read_merged_scope(&general_db, &scope) {
        Ok(m) => m,
        Err(e) => {
            return degrade_hot_index_unavailable(&e, emit, args.hook_event, args.log, now);
        }
    };

    // 复盘健康：pending 残留 + last-review-ok 距今判定「疑似停摆」，停摆时状态栏
    // 串尾加 ⚠reviewer（复盘产物与公共库同在 <general_db 父目录>，即 ~/.engram）。
    let stalled = general_db
        .parent()
        .map(|dir| {
            let h = gather_review_health(dir, now);
            h.stalled
        })
        .unwrap_or(false);

    // 5. 状态栏小文件（best-effort）：无论后面状态门控是否判空、是否注入，都先把状态栏
    //    刷成最新；写失败静默忽略。优先按会话分文件（--status-dir/<session_id>.txt），让每个
    //    窗口只读自己会话的状态、不被别的窗口的 hook 覆盖；否则回退单文件（--status-file）。
    let status_line = oneline_status_with_health(&merged, now, stalled);
    if let Some(dir) = args.status_dir {
        let sid = hook.session_id.as_deref().unwrap_or("default");
        write_session_status_silent(dir, sid, &status_line);
    } else if let Some(status_path) = args.status_file {
        write_status_file_silent(status_path, &status_line);
    }

    // 5b. 活跃会话登记（仅带 --state 的 UserPromptSubmit 路径；best-effort）：
    //     把 {transcript, session_id, 时间, 库路径} 原子写 active-sessions/<sid>.json，
    //     强杀/断电时 SessionEnd 不触发也能靠它在下次启动补建 pending（崩溃补漏闭环）。
    if let Some(state_base) = args.state {
        let transcript = hook
            .transcript_path
            .clone()
            .or_else(|| args.transcript.map(|p| p.to_string_lossy().to_string()));
        if let Some(transcript) = transcript {
            register_active_session(
                state_base,
                &SessionRegistration {
                    session_id: hook
                        .session_id
                        .clone()
                        .unwrap_or_else(|| "default".to_string()),
                    transcript_path: transcript,
                    ts: now,
                    general_db: general_db.to_string_lossy().to_string(),
                    project_db: scope.db.to_string_lossy().to_string(),
                    project_name: scope.name.clone(),
                },
            );
        }
    }

    // 6. 状态门控（仅当 --state 给出）：与上次 scope.root 相同则输出空、exit 0。
    //    门控状态按会话分文件存于 <state 父目录>/active-state/<sid>.state（见
    //    [`session_state_path`]）；--state 原路径仅作定位基准，旧单文件不再读写。
    let scope_root_str = scope.root.to_string_lossy().to_string();
    if let Some(state_base) = args.state {
        let state_path = session_state_path(state_base, hook.session_id.as_deref());
        let last = read_state(&state_path);
        let unchanged = last.as_deref() == Some(scope_root_str.as_str());
        if unchanged {
            // 不打印任何东西（让 hook 不注入）；状态栏文件已在上一步写过、仍记日志（best-effort）。
            if let Some(log_path) = args.log {
                append_hot_index_log(log_path, now, args.hook_event, &scope);
            }
            return ExitCode::SUCCESS;
        }
        // 有变化：把本次 scope.root 绝对路径写回状态文件后继续渲染，
        // 并按 7 天 TTL 顺手清理门控目录里过期的旧会话状态。
        if let Err(e) = write_state(&state_path, &scope_root_str) {
            eprintln!("hot-index 失败：{e}");
            return ExitCode::FAILURE;
        }
        if let Some(dir) = state_path.parent() {
            cleanup_stale_files(dir, "state", ACTIVE_FILE_TTL_SECS);
        }
    }

    // 7. 渲染。前言后若作用域是项目管理目录，追加一行提示（勿在此直接写项目记忆）。
    const PREAMBLE: &str = "「以下是你的 engram 长期记忆热索引。需要细节时按每条的指针去查 ground truth；不要凭印象。」";
    const WORKSPACE_NOTE: &str = "（注意：当前目录是 engram 项目管理目录，不要直接在此写项目记忆；请在其下的具体项目目录里工作，项目记忆写到该项目的库。）";
    // 当前作用域是具体项目时，即使其 L4 为空也显式渲染 [<项目名>] L4.x 段，
    // 作为「该项目已识别/挂载」的可视确认（空项目否则会在热索引里完全隐身）。
    let active_projects: Vec<&str> = match scope.kind {
        ScopeKind::Project => vec![scope.name.as_str()],
        ScopeKind::Workspace => Vec::new(),
    };
    // 渲染带字符预算（--budget，0=不限）：注入成本封顶，超出部分从低优先级整段截断。
    let rendered = engram::render::render_budgeted(&merged, now, &active_projects, args.budget);
    let is_workspace = scope.kind == ScopeKind::Workspace;

    // 8. 输出。
    let status = match emit {
        EmitFormat::Text => {
            println!("{PREAMBLE}");
            if is_workspace {
                println!("{WORKSPACE_NOTE}");
            }
            print!("{rendered}");
            ExitCode::SUCCESS
        }
        EmitFormat::Json => {
            // additionalContext = 前言（+ 管理目录提示行）+ render 输出。
            let context = if is_workspace {
                format!("{PREAMBLE}\n{WORKSPACE_NOTE}\n{rendered}")
            } else {
                format!("{PREAMBLE}\n{rendered}")
            };
            match build_hot_index_json(args.hook_event, &context) {
                Ok(json) => {
                    println!("{json}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("hot-index 失败：{e}");
                    ExitCode::FAILURE
                }
            }
        }
    };

    // 9. 调试日志（best-effort）。
    if let Some(log_path) = args.log {
        append_hot_index_log(log_path, now, args.hook_event, &scope);
    }

    status
}

/// 把状态栏一行串写入 `path`（**覆盖**），best-effort：父目录不存在先 `create_dir_all`，
/// 任一步失败都静默忽略、不上抛——状态栏小文件是旁路，绝不能因写它失败而影响注入主流程。
///
/// 与 [`write_state`] 的区别：本函数面向状态栏的「展示快照」，故采用「失败静默吞掉、
/// 返回 `()`」的语义；而 `--state` 的门控文件写失败需让命令失败（语义不同）。
///
/// # 参数
/// - `path`：状态栏小文件路径（覆盖写入）。
/// - `content`：要写入的一行状态串（[`oneline_status`] 的产物）。
fn write_status_file_silent(path: &Path, content: &str) {
    // 父目录不存在先创建；失败则直接放弃（不报错、不影响主输出）。
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    // 覆盖写入；失败静默忽略。
    let _ = std::fs::write(path, content);
}

/// 状态栏会话文件的保留窗口（秒）：超过此时长未更新的旧会话状态文件，在下次写入时被清理。
/// 取 7 天——足够覆盖跨天的长会话，又不至于让 `~/.engram/status/` 无限堆积。
const STATUS_SESSION_TTL_SECS: u64 = 7 * 24 * 3600;

/// 按会话把状态栏一行串写入 `<dir>/<session_id>.txt`（**覆盖**），best-effort：全程失败静默。
///
/// 分会话写是为了让每个 Claude Code 窗口的状态栏只读到**自己会话**的快照，不被别的窗口的
/// hook 覆盖（旧版所有窗口共用单文件 `status.txt`，导致多窗口显示雷同）。写完顺手清理目录内
/// 过期的旧会话文件（见 [`cleanup_stale_status`]），避免长期堆积。
///
/// # 参数
/// - `dir`：状态栏会话目录（如 `~/.engram/status`）。
/// - `session_id`：会话 id；非文件名安全字符会被替换为 `_`，空则记为 `default`。
/// - `content`：要写入的一行状态串（[`oneline_status`] 的产物）。
fn write_session_status_silent(dir: &Path, session_id: &str, content: &str) {
    // 文件名净化（共用 [`sanitize_file_stem`]）：杜绝 `/`、`..` 穿出目录。
    let safe = sanitize_file_stem(session_id);
    // 目录不存在先建；失败则放弃（不报错、不影响主输出）。
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let _ = std::fs::write(dir.join(format!("{safe}.txt")), content);
    cleanup_stale_status(dir);
}

/// 删 `dir` 内 mtime 早于「现在 − [`STATUS_SESSION_TTL_SECS`]」的 `*.txt`（best-effort，全程静默）。
///
/// 用文件 mtime 的 [`std::fs::Metadata::modified`] + `elapsed()` 判龄，避免与外部逻辑时钟耦合；
/// 读目录 / 取元数据 / 删文件任一步失败或时钟异常都跳过（保守不删），绝不上抛。
/// 通用内核见 [`cleanup_stale_files`]（门控 / 登记目录复用同一模式）。
fn cleanup_stale_status(dir: &Path) {
    cleanup_stale_files(dir, "txt", STATUS_SESSION_TTL_SECS);
}

/// `status` 命令的全部参数（聚成一个结构体，规避过多函数形参）。
struct StatusArgs<'a> {
    /// `--general-db` 覆盖；`None` 时走 `<HOME>/.engram/general.redb` 约定。
    general_db: Option<&'a Path>,
    /// `--workspace-root`（作 cwd override）；`None` 时取 stdin 的 cwd（仅 from_hook_stdin），
    /// 再缺省取当前目录；从该目录向上锚定作用域。
    workspace_root: Option<&'a Path>,
    /// 额外 `--project-db name=path`（0..N 个）。
    project_db: &'a [String],
    /// 是否从 stdin 读取整段 hook JSON 作 cwd 兜底。
    from_hook_stdin: bool,
    /// 输出格式：`full`（缺省）或 `oneline`。
    format: &'a str,
    /// 当前时间（unix 秒）；`None` 取系统时间。
    now: Option<f64>,
}

/// `status` 命令的输出格式（由 `--format` 解析而来）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusFormat {
    /// 可读多行概况（供 `/engram status` 用）。
    Full,
    /// 状态栏一行紧凑串（[`oneline_status`]）。
    Oneline,
}

/// 把 `--format` 字符串解析为 [`StatusFormat`]（大小写不敏感）。
///
/// # Errors
/// 当字符串既非 `full` 也非 `oneline` 时，返回中文说明的错误字符串。
fn parse_status_format(s: &str) -> Result<StatusFormat, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "full" => Ok(StatusFormat::Full),
        "oneline" => Ok(StatusFormat::Oneline),
        other => Err(format!("无法识别的 --format {other}（应为 full|oneline）")),
    }
}

/// 渲染 `status --format full` 的可读多行概况。
///
/// 统计**传入的全部挂载集**（不做 active 子项目动态判定），逐项给出：
/// - 顶行：总 active 数；
/// - 通用三层 L1/L2/L3 各自的 active 条数；
/// - 各项目各 L4 子层（L4.1/L4.2/L4.3）的 active 条数（按项目名升序，再按子层）；
/// - 全集的 cold 数、superseded 数、tombstone 数。
///
/// 纯函数，不做 IO。
///
/// # 参数
/// - `memories`：挂载集（公共库 + 作用域库 + 各 --project-db 的合并集）。
/// - `now`：当前时间（unix 秒），仅用于打印抬头（统计本身不依赖时间）。
fn render_status_full(memories: &[Memory], now: f64) -> String {
    // 通用三层 active 计数。
    let count_level = |level: Level| -> usize {
        memories
            .iter()
            .filter(|m| m.status == Status::Active && m.project.is_none() && m.level == level)
            .count()
    };
    let l1 = count_level(Level::L1);
    let l2 = count_level(Level::L2);
    let l3 = count_level(Level::L3);

    // 状态计数（全集）。
    let count_status = |st: Status| -> usize { memories.iter().filter(|m| m.status == st).count() };
    let active_total = count_status(Status::Active);
    let cold = count_status(Status::Cold);
    let superseded = count_status(Status::Superseded);
    let tombstone = count_status(Status::Tombstone);

    // 各项目各 L4 子层 active 计数：project 名 → (l4_1, l4_2, l4_3)。按名升序（BTreeMap）。
    let mut projects: BTreeMap<String, [usize; 3]> = BTreeMap::new();
    for m in memories {
        if m.status != Status::Active {
            continue;
        }
        if let Some(name) = &m.project {
            let slot = match m.level {
                Level::L4_1 => Some(0),
                Level::L4_2 => Some(1),
                Level::L4_3 => Some(2),
                Level::L1 | Level::L2 | Level::L3 => None,
            };
            if let Some(idx) = slot {
                projects.entry(name.clone()).or_insert([0, 0, 0])[idx] += 1;
            }
        }
    }

    let mut buf = String::new();
    buf.push_str(&format!("== Engram 概况 (now={now}) ==\n"));
    buf.push_str(&format!("active 总数：{active_total}\n"));
    buf.push_str(&format!("通用层：L1:{l1} L2:{l2} L3:{l3}\n"));
    if projects.is_empty() {
        buf.push_str("项目层：（无）\n");
    } else {
        buf.push_str("项目层：\n");
        for (name, [a1, a2, a3]) in &projects {
            buf.push_str(&format!("  [{name}] L4.1:{a1} L4.2:{a2} L4.3:{a3}\n"));
        }
    }
    buf.push_str(&format!(
        "其它：cold:{cold} superseded:{superseded} tombstone:{tombstone}\n"
    ));
    buf
}

/// 执行 `status` 子命令：从 cwd 锚定作用域，挂载「公共库 + 作用域库 + 各
/// `--project-db`」，把这批库当作挂载集统计/展示。
///
/// 注意：本命令只做统计/展示，**不做 active 子项目动态判定、不接 transcript**——
/// status 给的是「全貌」，把传入的所有库都算上即可。`--workspace-root` 在此作为
/// cwd override，用于从其向上锚定作用域。
///
/// `--format oneline`：打印 [`oneline_status`]（一行、不带多余空行）。
/// `--format full`（缺省）：打印 [`render_status_full`] 的可读多行概况。
///
/// 任何 IO/库错误走 stderr + 非 0 退出，不 panic。作用域库 / 各项目库缺失时静默跳过
/// （只读概况，库尚未创建时算作 0 条，不应报错）。
/// `serve` 子命令的 CLI 参数（聚成结构体，规避过多形参）。
struct ServeCliArgs<'a> {
    general_db: Option<&'a Path>,
    project_db: &'a [String],
    scan_root: &'a [PathBuf],
    port: u16,
    host: String,
    open: bool,
}

/// 从 `cwd` 向上收集所有「项目管理目录」（带 `.engram/workspace` 标记）的祖先路径。
///
/// 看板据此扫描各管理目录的直接子项目库，配合注册表累积，实现"本机全部项目库"的发现。
fn collect_workspace_ancestors(cwd: &Path) -> Vec<PathBuf> {
    let canon = canonicalize_cwd(cwd);
    let mut roots = Vec::new();
    for anc in canon.ancestors() {
        if anc.join(ENGRAM_DIR).join(WORKSPACE_MARKER).is_file() {
            roots.push(anc.to_path_buf());
        }
    }
    roots
}

/// 执行 `serve` 子命令：锚定作用域 → 组装 [`ServeConfig`] → 起看板服务（阻塞在监听循环）。
///
/// 发现范围 = 公共库 + cwd 之上各项目管理目录的子项目库 + 注册表累积项 +
/// `--project-db`/`--scan-root` 显式项 + 当前作用域库（兜底独立项目）。
fn run_serve(args: ServeCliArgs<'_>) -> ExitCode {
    let Some(extra_map) = resolve_project_dbs(args.project_db) else {
        return ExitCode::FAILURE;
    };

    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("serve 失败：获取当前工作目录失败：{e}");
            return ExitCode::FAILURE;
        }
    };

    // 锚定作用域：拿公共库路径 + 当前作用域库（供发现兜底当前项目/管理目录）。
    let (general_db, scope) = match resolve_scope(&cwd, args.general_db) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("serve 失败：{e}");
            return ExitCode::FAILURE;
        }
    };

    // 扫描根：cwd 之上所有管理目录 + 显式 --scan-root（最终去重由 serve 侧按路径完成）。
    let mut scan_roots = collect_workspace_ancestors(&cwd);
    scan_roots.extend(args.scan_root.iter().cloned());

    // 额外库：--project-db 显式项 + 当前作用域库（覆盖不在任何管理目录下的独立项目）。
    let mut extra_project_dbs: Vec<(String, PathBuf)> = extra_map.into_iter().collect();
    extra_project_dbs.push((scope.name.clone(), scope.db.clone()));

    let cfg = ServeConfig {
        host: args.host,
        port: args.port,
        general_db,
        scan_roots,
        extra_project_dbs,
        open: args.open,
    };

    match serve::run(cfg) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("serve 失败：{e}");
            ExitCode::FAILURE
        }
    }
}

fn run_status(args: StatusArgs<'_>) -> ExitCode {
    let format = match parse_status_format(args.format) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("status 失败：{e}");
            return ExitCode::FAILURE;
        }
    };
    let Some(now) = resolve_now(args.now) else {
        return ExitCode::FAILURE;
    };
    let Some(project_dbs) = resolve_project_dbs(args.project_db) else {
        return ExitCode::FAILURE;
    };

    // stdin 兜底（仅 --from-hook-stdin）：只用其 cwd 作 cwd 兜底。
    let hook = if args.from_hook_stdin {
        read_hook_stdin()
    } else {
        HookStdin::default()
    };

    // cwd：--workspace-root 优先，否则 stdin.cwd，否则 current_dir()。
    let cwd: PathBuf = match args.workspace_root {
        Some(p) => p.to_path_buf(),
        None => match hook.cwd.clone() {
            Some(c) => c,
            None => match std::env::current_dir() {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("status 失败：获取当前工作目录失败：{e}");
                    return ExitCode::FAILURE;
                }
            },
        },
    };

    // 从 cwd 向上锚定作用域，得 (general_db, scope)。
    let (general_db, scope) = match resolve_scope(&cwd, args.general_db) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("status 失败：{e}");
            return ExitCode::FAILURE;
        }
    };

    // 挂载集：公共库（全部）+ 作用域库（若文件存在）+ 各 --project-db（若文件存在）。
    // 缺失的库静默跳过（只读概况，库尚未创建时算作 0 条，不应报错）。
    //
    // 按规范化绝对路径去重：cwd 锚定的作用域库与显式 --project-db 可能指向**同一个**
    // redb 文件（/engram:list 等 skill 的官方用法就是先 resolve 拿到路径、再用
    // --project-db 显式传进来）。若不去重，同一个项目库会被读两遍、概况数字虚高一倍
    // （唯独 status 有 cwd 锚定 + 显式挂载两条来源，故只有它会踩到）。以规范化路径为
    // 准跳过重复来源，与 list/render/recall 的单次挂载语义对齐。
    let mut mounted: HashSet<PathBuf> = HashSet::new();

    let mut merged = match open_and_read(&general_db) {
        Ok((_db, mems)) => mems,
        Err(e) => {
            eprintln!("status 失败：读取公共库 {} 失败：{e}", general_db.display());
            return ExitCode::FAILURE;
        }
    };
    mounted.insert(canonicalize_cwd(&general_db));

    if scope.db.is_file() && mounted.insert(canonicalize_cwd(&scope.db)) {
        match open_and_read(&scope.db) {
            Ok((_db, mut mems)) => merged.append(&mut mems),
            Err(e) => {
                eprintln!(
                    "status 失败：读取作用域库 {}（{}）失败：{e}",
                    scope.name,
                    scope.db.display()
                );
                return ExitCode::FAILURE;
            }
        }
    }

    for (name, path) in &project_dbs {
        if !path.is_file() || !mounted.insert(canonicalize_cwd(path)) {
            continue;
        }
        match open_and_read(path) {
            Ok((_db, mut mems)) => merged.append(&mut mems),
            Err(e) => {
                eprintln!(
                    "status 失败：读取项目库 {name}（{}）失败：{e}",
                    path.display()
                );
                return ExitCode::FAILURE;
            }
        }
    }

    // 复盘健康：复盘产物（pending / last-review-ok）与公共库同在 <general_db 父目录>
    // （生产即 ~/.engram；测试传临时目录时随之隔离，保持可测）。
    let health = general_db
        .parent()
        .map(|dir| gather_review_health(dir, now));

    match format {
        StatusFormat::Oneline => {
            // 一行串，println! 自带一个换行（无多余空行）；疑似停摆时串尾加 ⚠reviewer。
            let stalled = health.as_ref().map(|h| h.stalled).unwrap_or(false);
            println!("{}", oneline_status_with_health(&merged, now, stalled));
        }
        StatusFormat::Full => {
            // render_status_full 末尾已自带换行，用 print! 避免多一个空行。
            print!("{}", render_status_full(&merged, now));
            if let Some(h) = &health {
                print!("{}", render_review_health(h, now));
            }
        }
    }
    ExitCode::SUCCESS
}

/// 渲染 `status --format full` 的复盘健康段（多行，自带末尾换行）。
///
/// 内容：上次成功巩固时间/距今、pending 目录残留数；疑似停摆（存在 pending 且
/// last-review-ok 距今 > 48h）时额外输出一行原因与自查指引。
fn render_review_health(h: &ReviewHealth, now: f64) -> String {
    let mut buf = String::new();
    let last = match h.last_ok_ts {
        Some(ts) => {
            let hours = ((now - ts) / 3600.0).max(0.0);
            format!("{ts:.0}（距今 {hours:.1} 小时）")
        }
        None => "（从未记录）".to_string(),
    };
    buf.push_str(&format!(
        "复盘健康：上次成功巩固 {last}；pending 残留 {} 条\n",
        h.pending_count
    ));
    if h.stalled {
        buf.push_str(
            "⚠ 疑似停摆：存在 pending 残留且上次成功巩固距今超 48 小时。\
             自查：① 看 ~/.engram/hook.log 里 review/catchup 的失败记录；\
             ② 确认 SessionEnd 复盘者能启动（ENGRAM_REVIEWER_CLI 指向的 CLI 可用）；\
             ③ 手动跑 engram catchup-scan --work-dir ~/.engram/pending 补跑最近一场。\n",
        );
    }
    buf
}

/// 执行 `render` 子命令：读公共库 + 所有项目库 → 合并 → 渲染 → 打印。
///
/// 渲染**全部合并集**（作用域由挂载了哪些项目库决定，render 不再按单一 scope 过滤）。
fn run_render(general_db: &Path, project_db_raw: &[String], now: Option<f64>) -> ExitCode {
    let Some(now) = resolve_now(now) else {
        return ExitCode::FAILURE;
    };
    let Some(project_dbs) = resolve_project_dbs(project_db_raw) else {
        return ExitCode::FAILURE;
    };

    let merged = match read_merged(general_db, &project_dbs) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let out = render(&merged, now);
    print!("{out}");
    ExitCode::SUCCESS
}

/// 把 [`Level`] 渲染为带点号的字符串（用于 JSON 输出，与 serde 表示一致）。
fn level_repr(level: Level) -> &'static str {
    match level {
        Level::L1 => "L1",
        Level::L2 => "L2",
        Level::L3 => "L3",
        Level::L4_1 => "L4.1",
        Level::L4_2 => "L4.2",
        Level::L4_3 => "L4.3",
    }
}

/// 把 [`Status`] 渲染为小写字符串（与 serde 表示一致）。
fn status_repr(status: Status) -> &'static str {
    match status {
        Status::Active => "active",
        Status::Cold => "cold",
        Status::Superseded => "superseded",
        Status::Tombstone => "tombstone",
    }
}

/// 把 `effective` 格式化为稳定字符串（处理无穷大），供文本输出用。
fn fmt_eff(eff: f64) -> String {
    if eff == f64::INFINITY {
        "INF".to_string()
    } else if eff == f64::NEG_INFINITY {
        "-INF".to_string()
    } else {
        format!("{eff:.3}")
    }
}

/// `recall` 命令的全部参数（聚成一个结构体，规避过多函数形参）。
struct RecallArgs<'a> {
    general_db: &'a Path,
    project_db: &'a [String],
    query: &'a str,
    limit: usize,
    active_only: bool,
    now: Option<f64>,
    json: bool,
}

/// 执行 `recall` 子命令：合并多库 → 词法打分检索 → 打印候选（不写任何库）。
///
/// 默认检索 active+cold；`active_only` 时只搜 active。superseded/tombstone 永不返回。
fn run_recall(args: RecallArgs<'_>) -> ExitCode {
    let Some(now) = resolve_now(args.now) else {
        return ExitCode::FAILURE;
    };
    let Some(project_dbs) = resolve_project_dbs(args.project_db) else {
        return ExitCode::FAILURE;
    };
    let merged = match read_merged(args.general_db, &project_dbs) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let tokens = tokenize_query(args.query);
    let cands = recall_candidates(&merged, &tokens, args.active_only, args.limit, now);

    if args.json {
        // 手工拼 JSON 数组：避免为输出额外引入序列化辅助类型。
        let mut buf = String::from("[");
        for (i, c) in cands.iter().enumerate() {
            if i > 0 {
                buf.push(',');
            }
            let m = c.memory;
            buf.push_str(&format!(
                r#"{{"id":{},"level":{},"status":{},"score":{},"effective":{},"cue":{},"pointer":{}}}"#,
                json_str(&m.id),
                json_str(level_repr(m.level)),
                json_str(status_repr(m.status)),
                c.score,
                json_f64(c.effective),
                json_str(&m.cue),
                json_pointer(&m.pointer),
            ));
        }
        buf.push(']');
        println!("{buf}");
    } else {
        println!(
            "== recall (now={now}, query=\"{}\") == 共 {} 条候选",
            args.query,
            cands.len()
        );
        for c in &cands {
            let m = c.memory;
            let reference = m.pointer.reference.as_deref().unwrap_or("-");
            println!(
                "  {} | {} | {} | score={} | eff={} | {} | {}",
                m.id,
                level_repr(m.level),
                status_repr(m.status),
                c.score,
                fmt_eff(c.effective),
                m.cue,
                reference,
            );
        }
    }
    ExitCode::SUCCESS
}

/// cue 长度源头治理的警告阈值（字符数）。
///
/// cue 应是一句话检索线索；超过此长度大概率是把细节正文塞进了 cue——超长 cue
/// 会挤占该层字符预算（见 `TierParams::char_budget` 的条数+预算双约束），把
/// 同层其它记忆提前挤下去。仅警告、不拒绝：由写入方自行裁决。
const CUE_WARN_CHARS: usize = 240;

/// 若 cue 超长（字符数 > [`CUE_WARN_CHARS`]）则向 stderr 打警告（不拒绝）。
///
/// write（含 --overwrite）/ merge / graduate 等所有产生新 cue 的路径共用。
fn warn_long_cue(cue: &str) {
    let n = cue.chars().count();
    if n > CUE_WARN_CHARS {
        eprintln!(
            "警告：cue 应是一句话检索线索（当前 {n} 字符），细节请放指针/detail——超长 cue 会挤占该层字符预算"
        );
    }
}

/// `write` 命令的全部参数（聚成一个结构体，规避过多函数形参）。
struct WriteArgs<'a> {
    general_db: &'a Path,
    project_db: &'a [String],
    level: &'a str,
    cue: &'a str,
    project: Option<&'a str>,
    importance: f64,
    pinned: bool,
    pointer_kind: &'a str,
    pointer_ref: Option<&'a str>,
    pointer_detail: Option<&'a str>,
    tags: Option<&'a str>,
    id: Option<&'a str>,
    overwrite: bool,
    now: Option<f64>,
}

/// 执行 `write` 子命令：校验 → 构造记忆 → 按层级路由写入正确的库 → 打印新 id。
///
/// 校验：importance ∈ [0,1]；L4.x 必须给 `--project NAME` 且 NAME 在项目库映射中；
/// L1-3 不得给 `--project`。任一校验失败即非 0 退出且不写入。
///
/// 防覆盖：目标库已存在同 id 记忆时**默认拒绝**（store::put 是 upsert，静默覆盖
/// 等于无痕丢旧记忆），提示改用 `--overwrite` 显式确认（对齐 graduate 的
/// 新 id 冲突拦截）。只查目标库：跨库同 id 不会互相覆盖，不在拦截范围。
fn run_write(args: WriteArgs<'_>) -> ExitCode {
    // 解析层级。
    let level = match parse_level(args.level) {
        Ok(lv) => lv,
        Err(e) => {
            eprintln!("write 失败：{e}");
            return ExitCode::FAILURE;
        }
    };

    // 校验 importance 范围。
    if !(0.0..=1.0).contains(&args.importance) {
        eprintln!("write 失败：importance={} 超出范围 [0,1]", args.importance);
        return ExitCode::FAILURE;
    }

    // 解析项目库映射。
    let Some(project_dbs) = resolve_project_dbs(args.project_db) else {
        return ExitCode::FAILURE;
    };

    let l4 = commands::is_l4(level);
    // L4 路由前置校验：必须给 --project，且该 NAME 必须在项目库映射中。
    // 否则（L1-3）不得携带 project。确定写入目标库路径。
    let target: PathBuf = if l4 {
        let Some(name) = args.project else {
            eprintln!("write 失败：{} 记忆必须提供 --project", args.level);
            return ExitCode::FAILURE;
        };
        match project_dbs.get(name) {
            Some(path) => path.clone(),
            None => {
                eprintln!(
                    "write 失败：项目 {name} 不在 --project-db 映射中（无法路由 {} 记忆）",
                    args.level
                );
                return ExitCode::FAILURE;
            }
        }
    } else {
        if args.project.is_some() {
            eprintln!("write 失败：{} 通用记忆不得提供 --project", args.level);
            return ExitCode::FAILURE;
        }
        args.general_db.to_path_buf()
    };

    // 解析 now（缺省取系统时间）。
    let Some(now) = resolve_now(args.now) else {
        return ExitCode::FAILURE;
    };

    // 生成或采用 id。
    let id = match args.id {
        Some(s) => s.to_string(),
        None => generate_id(args.cue, now),
    };

    // 构造记忆：created_at=now、access_log=[]、status=active、superseded_by=None。
    let memory = Memory {
        id: id.clone(),
        cue: args.cue.to_string(),
        pointer: Pointer {
            kind: args.pointer_kind.to_string(),
            reference: args.pointer_ref.map(|s| s.to_string()),
            detail: args.pointer_detail.map(|s| s.to_string()),
        },
        level,
        project: args.project.map(|s| s.to_string()),
        importance: args.importance,
        pinned: args.pinned,
        access_log: Vec::new(),
        status: Status::Active,
        superseded_by: None,
        created_at: now,
        tags: parse_tags(args.tags),
        schema_version: MEMORY_SCHEMA_VERSION,
    };

    let db = match store::open(&target) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("打开数据库 {} 失败：{e}", target.display());
            return ExitCode::FAILURE;
        }
    };
    // 防覆盖拦截：同 id 已存在且未给 --overwrite → 拒绝（不写入）。
    match store::get(&db, &id) {
        Ok(Some(_)) if !args.overwrite => {
            eprintln!("write 失败：id {id} 在目标库已存在；如确要覆盖旧记忆请加 --overwrite");
            return ExitCode::FAILURE;
        }
        Ok(_) => {}
        Err(e) => {
            eprintln!("write 失败：检查 id 冲突时读库失败：{e}");
            return ExitCode::FAILURE;
        }
    }
    if let Err(e) = store::put(&db, &memory) {
        eprintln!("写入记忆失败：{e}");
        return ExitCode::FAILURE;
    }

    // cue 长度源头治理：超长仅警告（不拒绝），覆盖普通写入与 --overwrite 路径。
    warn_long_cue(args.cue);

    println!("{id}");
    ExitCode::SUCCESS
}

/// `list` 命令的全部参数（聚成一个结构体，规避过多函数形参）。
struct ListArgs<'a> {
    general_db: &'a Path,
    project_db: &'a [String],
    project: Option<&'a str>,
    status: &'a str,
    level: Option<&'a str>,
    now: Option<f64>,
    json: bool,
}

/// 执行 `list` 子命令：合并多库 → 按 status/level/project 过滤排序 → 打印（不写库）。
fn run_list(args: ListArgs<'_>) -> ExitCode {
    let Some(now) = resolve_now(args.now) else {
        return ExitCode::FAILURE;
    };
    let Some(project_dbs) = resolve_project_dbs(args.project_db) else {
        return ExitCode::FAILURE;
    };

    // 解析状态过滤。
    let status_filter = match parse_status_filter(args.status) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("list 失败：{e}");
            return ExitCode::FAILURE;
        }
    };

    // 解析可选层级过滤。
    let level_filter = match args.level {
        None => None,
        Some(s) => match parse_level(s) {
            Ok(lv) => Some(lv),
            Err(e) => {
                eprintln!("list 失败：{e}");
                return ExitCode::FAILURE;
            }
        },
    };

    let merged = match read_merged(args.general_db, &project_dbs) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let items = list_visible(&merged, status_filter, level_filter, args.project, now);

    if args.json {
        // 输出 Memory 数组，每条附加计算出的 effective。
        let mut buf = String::from("[");
        for (i, m) in items.iter().enumerate() {
            if i > 0 {
                buf.push(',');
            }
            // 先序列化 Memory 本体，再在对象末尾补 effective 字段。
            let base = match serde_json::to_string(m) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("list 序列化记忆 {} 失败：{e}", m.id);
                    return ExitCode::FAILURE;
                }
            };
            // base 形如 `{...}`，去掉末尾 `}` 后追加 effective 再补回。
            let eff = engram::activation::effective(m, now);
            match base.strip_suffix('}') {
                Some(head) => {
                    buf.push_str(head);
                    buf.push_str(&format!(",\"effective\":{}}}", json_f64(eff)));
                }
                None => {
                    // 不可达：serde 对象一定以 } 结尾。保底直接放原串。
                    buf.push_str(&base);
                }
            }
        }
        buf.push(']');
        println!("{buf}");
    } else {
        println!(
            "== list (now={now}, status={}) == 共 {} 条",
            args.status,
            items.len()
        );
        for m in &items {
            let proj = m.project.as_deref().unwrap_or("-");
            println!(
                "  {} | {} | {} | {} | imp={:.2} | eff={} | {}",
                m.id,
                level_repr(m.level),
                status_repr(m.status),
                proj,
                m.importance,
                fmt_eff(engram::activation::effective(m, now)),
                m.cue,
            );
        }
    }
    ExitCode::SUCCESS
}

/// 把一个字符串转义为 JSON 字符串字面量（含外层双引号）。
///
/// 借 `serde_json` 做转义，保证 cue/id 里若含引号、反斜杠、控制字符也合法。
/// 序列化字符串理论上不会失败；极端情况下退化为一个空 JSON 串 `""`。
fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// 把 `f64` 渲染为合法 JSON 数值；无穷/NaN 用 `null`（JSON 无对应字面量）。
fn json_f64(v: f64) -> String {
    if v.is_finite() {
        format!("{v}")
    } else {
        "null".to_string()
    }
}

/// 把 [`Pointer`] 序列化为 JSON 对象字符串（用于 recall 的 `--json` 输出）。
fn json_pointer(p: &Pointer) -> String {
    serde_json::to_string(p).unwrap_or_else(|_| "null".to_string())
}

/// 把层级渲染为简短字符串（用于变迁摘要）。
fn level_str(level: Level) -> &'static str {
    match level {
        Level::L1 => "L1",
        Level::L2 => "L2",
        Level::L3 => "L3",
        Level::L4_1 => "L4.1",
        Level::L4_2 => "L4.2",
        Level::L4_3 => "L4.3",
    }
}

/// 把状态渲染为简短字符串（用于变迁摘要）。
fn status_str(status: Status) -> &'static str {
    match status {
        Status::Active => "active",
        Status::Cold => "cold",
        Status::Superseded => "superseded",
        Status::Tombstone => "tombstone",
    }
}

/// 把变迁种类渲染为中文短语。
fn kind_str(kind: TransitionKind) -> &'static str {
    match kind {
        TransitionKind::Promote => "升级",
        TransitionKind::Demote => "降级",
        TransitionKind::Evict => "淘汰",
        TransitionKind::Overflow => "溢出下推",
    }
}

/// 打印一条变迁摘要：`id | 种类 | from→to`。
fn print_transition(t: &Transition) {
    // 目标用层级（若变）或状态（若变）描述。
    let to = match (t.to_level, t.to_status) {
        (Some(lv), _) => level_str(lv).to_string(),
        (None, Some(st)) => status_str(st).to_string(),
        (None, None) => "(无变化)".to_string(),
    };
    println!(
        "  {} | {} | {}→{}",
        t.id,
        kind_str(t.kind),
        level_str(t.from),
        to
    );
}

/// 执行 `consolidate` 子命令。
///
/// 从公共库 + 所有项目库读出记忆合并 → 跑状态机 → 打印变迁 → **按层级 + 项目路由
/// 写回**：变更后 `level ∈ {L1,L2,L3}` 的写回公共库；`level ∈ {L4.x}` 的写回其
/// `project` 在映射中对应的项目库（各项目 L4 本就是独立容量池）。只写状态有变化者，
/// 各库分别单事务批量写。`--dry-run` 不写。
///
/// 某 L4 记忆的 `project` 不在项目库映射中（正常不会，因 L4 只来自挂载的项目库），
/// 跳过其写回并记 stderr。
fn run_consolidate(
    general_db: &Path,
    project_db_raw: &[String],
    now: Option<f64>,
    dry_run: bool,
) -> ExitCode {
    let Some(now) = resolve_now(now) else {
        return ExitCode::FAILURE;
    };
    let Some(project_dbs) = resolve_project_dbs(project_db_raw) else {
        return ExitCode::FAILURE;
    };

    let transitions = match consolidate_libs(general_db, &project_dbs, now, dry_run) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    // 打印变迁摘要。
    if transitions.is_empty() {
        println!("== consolidate (now={now}) == 无变迁");
    } else {
        println!(
            "== consolidate (now={now}) == 共 {} 条变迁{}",
            transitions.len(),
            if dry_run {
                "（dry-run，不写回）"
            } else {
                ""
            }
        );
        for t in &transitions {
            print_transition(t);
        }
    }

    ExitCode::SUCCESS
}

/// 载入公共库 + 各项目库、跑 consolidate 状态机、按层级/项目路由写回。
///
/// [`run_consolidate`] 与 [`run_consolidate_done`] 共用的执行核：纯执行、**不打印**
/// 变迁摘要（交给调用方决定是打到 stdout 还是静默），故可在 consolidate-done 的
/// 收尾里静默复用而不污染其 stdout JSON。`dry_run` 为真时只算变迁、不写回。
///
/// # 返回
/// 本次状态机产生的全部变迁；载入/写回失败时返回 `Err(错误说明)`。无法路由的 L4
/// 变更（其 `project` 不在映射中）仅记 stderr 警告、不算失败。
fn consolidate_libs(
    general_db: &Path,
    project_dbs: &BTreeMap<String, PathBuf>,
    now: f64,
    dry_run: bool,
) -> Result<Vec<Transition>, String> {
    // 打开公共库并读出。
    let (gdb, gmems) = open_and_read(general_db)
        .map_err(|e| format!("读取公共库 {} 失败：{e}", general_db.display()))?;

    // 打开所有项目库并读出（保留各 Database 句柄用于写回）。
    let mut merged = gmems;
    let mut pdbs: BTreeMap<String, Database> = BTreeMap::new();
    for (name, path) in project_dbs {
        let (db, mut pmems) = open_and_read(path)
            .map_err(|e| format!("读取项目库 {name}（{}）失败：{e}", path.display()))?;
        merged.append(&mut pmems);
        pdbs.insert(name.clone(), db);
    }

    // 记录变更前的 (level, status)，用于只写回真正变化的记忆。
    let before: Vec<(Level, Status)> = merged.iter().map(|m| (m.level, m.status)).collect();

    // 跑状态机（纯算法，原地修改）。
    let transitions = consolidate(&mut merged, now);

    if dry_run {
        return Ok(transitions);
    }

    // 挑出 level 或 status 有变化的记忆，按当前（变更后）层级 + 项目路由：
    // L1-3 归公共库；L4 按其 project 名归入对应项目库的写回桶。
    let mut general_changed: Vec<Memory> = Vec::new();
    let mut project_changed: BTreeMap<String, Vec<Memory>> = BTreeMap::new();
    // project 不在映射中的 L4 记忆（无法路由），仅计数用于 stderr 提示。
    let mut unrouted_l4 = 0usize;
    for (i, m) in merged.into_iter().enumerate() {
        let (old_level, old_status) = before[i];
        if m.level == old_level && m.status == old_status {
            continue;
        }
        if commands::is_l4(m.level) {
            match &m.project {
                Some(name) if pdbs.contains_key(name) => {
                    project_changed.entry(name.clone()).or_default().push(m);
                }
                _ => {
                    unrouted_l4 += 1;
                }
            }
        } else {
            general_changed.push(m);
        }
    }

    // 写回公共库（仅在有变更时开事务）。
    if !general_changed.is_empty() {
        store::put_many(&gdb, &general_changed).map_err(|e| format!("写回公共库失败：{e}"))?;
    }

    // 各项目库分别单事务写回其变更集。
    for (name, changed) in &project_changed {
        let db = pdbs
            .get(name)
            .ok_or_else(|| format!("内部错误：项目 {name} 无对应库句柄"))?;
        store::put_many(db, changed).map_err(|e| format!("写回项目库 {name} 失败：{e}"))?;
    }

    // 无法路由的 L4 变更：记 stderr 提示（不致命）。
    if unrouted_l4 > 0 {
        eprintln!("警告：{unrouted_l4} 条 L4 记忆的所属项目不在 --project-db 映射中，已跳过其写回");
    }

    Ok(transitions)
}

/// 执行 `import` 子命令：扫描 JSON 目录 → **按层级 + 项目路由**导入多库，打印各库条数。
///
/// L1/L2/L3 写入公共库；L4.1/L4.2/L4.3 写入其 `project` 在映射中对应的项目库。
/// 若某 L4 记忆的 `project` 无对应 `--project-db` 映射（或为 `None`），报错退出
/// （非 0），不做任何写入。
fn run_import(general_db: &Path, project_db_raw: &[String], json_dir: &Path) -> ExitCode {
    let Some(project_dbs) = resolve_project_dbs(project_db_raw) else {
        return ExitCode::FAILURE;
    };
    // 取当前时间：未来时间戳消毒（跨机拷库 / 时钟回拨）用。
    let Some(now) = resolve_now(None) else {
        return ExitCode::FAILURE;
    };

    let entries = match load_store_entries(json_dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("扫描 JSON 目录 {} 失败：{e}", json_dir.display());
            return ExitCode::FAILURE;
        }
    };

    // 按层级 + 项目路由分流：L1-3 → 公共桶；L4 → 各项目桶。
    let mut general_mems: Vec<Memory> = Vec::new();
    let mut project_mems: BTreeMap<String, Vec<Memory>> = BTreeMap::new();
    for entry in entries {
        let mut m = entry.memory;
        // 时间输入消毒：created_at / access_log 里超过 now+60s 的未来时间戳一律
        // 钳到 now（60s 容忍正常时钟偏差）。未来时间戳会让 Δt 为负、activation
        // 计算失真，且在源头（另一台机器的时钟）已不可考，钳制是最保守的修复。
        if sanitize_future_times(&mut m, now) {
            eprintln!(
                "import: 记忆 {} 含超过 now+60s 的未来时间戳，已钳制到当前时间（跨机拷库/时钟回拨消毒）",
                m.id
            );
        }
        if commands::is_l4(m.level) {
            // L4 必须有所属项目，且该项目在映射中。
            match &m.project {
                Some(name) if project_dbs.contains_key(name) => {
                    project_mems.entry(name.clone()).or_default().push(m);
                }
                Some(name) => {
                    eprintln!(
                        "导入失败：L4 记忆 {} 的所属项目 {name} 不在 --project-db 映射中",
                        m.id
                    );
                    return ExitCode::FAILURE;
                }
                None => {
                    eprintln!("导入失败：L4 记忆 {} 缺少所属项目（project 为空）", m.id);
                    return ExitCode::FAILURE;
                }
            }
        } else {
            general_mems.push(m);
        }
    }

    // 写入公共库（L1-3）。
    let gdb = match store::open(general_db) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("打开公共库 {} 失败：{e}", general_db.display());
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = store::put_many(&gdb, &general_mems) {
        eprintln!("导入写入公共库失败：{e}");
        return ExitCode::FAILURE;
    }

    // 各项目库分别写入其 L4 记忆。
    let mut project_total = 0usize;
    for (name, mems) in &project_mems {
        let Some(path) = project_dbs.get(name) else {
            // 不可达：project_mems 的键来自 project_dbs.contains_key 过滤。
            eprintln!("内部错误：项目 {name} 无对应库路径");
            return ExitCode::FAILURE;
        };
        let pdb = match store::open(path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("打开项目库 {name}（{}）失败：{e}", path.display());
                return ExitCode::FAILURE;
            }
        };
        if let Err(e) = store::put_many(&pdb, mems) {
            eprintln!("导入写入项目库 {name} 失败：{e}");
            return ExitCode::FAILURE;
        }
        project_total += mems.len();
        println!("import: 项目库 {name} 导入 {} 条", mems.len());
    }

    println!(
        "import: 已从 {} 导入公共库 {} 条、项目库共 {} 条记忆（{} 个项目）",
        json_dir.display(),
        general_mems.len(),
        project_total,
        project_mems.len(),
    );
    ExitCode::SUCCESS
}

/// `export` 命令的全部参数（聚成一个结构体，规避过多函数形参）。
struct ExportArgs<'a> {
    general_db: &'a Path,
    project_db: &'a [String],
    dir: &'a Path,
    project: Option<&'a str>,
    status: &'a str,
}

/// 把一批记忆逐条完整序列化为 `<dir>/<净化id>.json`（`export` 与 `migrate` 备份的
/// 共用落盘逻辑）。目录不存在则创建；同名文件覆盖。返回成功写出的条数。
///
/// 文件名取净化后的 id（[`sanitize_file_stem`]，防异常 id 穿出目录），与 `import`
/// 对称互逆（一文件一记忆、含全部字段与 `schema_version`）。
///
/// # Errors
/// 创建目录、序列化任一记忆或写文件失败时返回中文说明的错误字符串。
fn write_memories_as_json(dir: &Path, mems: &[&Memory]) -> Result<usize, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("创建目录 {} 失败：{e}", dir.display()))?;
    let mut written = 0usize;
    for m in mems {
        let json = serde_json::to_string_pretty(m)
            .map_err(|e| format!("序列化记忆 {} 失败：{e}", m.id))?;
        let file = dir.join(format!("{}.json", sanitize_file_stem(&m.id)));
        std::fs::write(&file, json).map_err(|e| format!("写文件 {} 失败：{e}", file.display()))?;
        written += 1;
    }
    Ok(written)
}

/// 执行 `export` 子命令：合并集过滤后逐条完整序列化为 `<dir>/<id>.json`。
///
/// 与 `import` 对称互逆（import 扫目录 `*.json`、一文件一记忆），是备份/迁移/审查
/// 的官方路径：JSON 含 Memory 全部字段与 `schema_version`，export → import 往返
/// 后记忆内容逐字段一致。文件名取净化后的 id（[`sanitize_file_stem`]，防异常 id
/// 穿出目录）；输出目录不存在则创建，同名文件覆盖（重复导出即刷新备份）。
///
/// 过滤：`--project` 仅导出该项目的记忆；`--status` 支持
/// active|cold|superseded|tombstone|all（缺省 all，全量备份）。
fn run_export(args: ExportArgs<'_>) -> ExitCode {
    let status_filter = match parse_status_filter(args.status) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("export 失败：{e}");
            return ExitCode::FAILURE;
        }
    };
    let Some(project_dbs) = resolve_project_dbs(args.project_db) else {
        return ExitCode::FAILURE;
    };
    let merged = match read_merged(args.general_db, &project_dbs) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("export 失败：{e}");
            return ExitCode::FAILURE;
        }
    };

    // 过滤（状态 + 项目）后取引用集，交给共用落盘逻辑写出。
    let selected: Vec<&Memory> = merged
        .iter()
        .filter(|m| match status_filter {
            commands::StatusFilter::All => true,
            commands::StatusFilter::Only(s) => m.status == s,
        })
        .filter(|m| match args.project {
            Some(p) => m.project.as_deref() == Some(p),
            None => true,
        })
        .collect();

    let exported = match write_memories_as_json(args.dir, &selected) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("export 失败：{e}");
            return ExitCode::FAILURE;
        }
    };

    println!("export: 已导出 {exported} 条记忆到 {}", args.dir.display());
    ExitCode::SUCCESS
}

/// 对一条记忆的时间字段做未来时间戳消毒：`created_at` 与 `access_log` 里超过
/// `now + 60s` 的值钳到 `now`。返回是否发生过钳制（供调用方打警告）。
///
/// 钳制后 `access_log` 重新升序排序，维持 [`Memory::access_log`]「升序」不变量
/// （钳制可能打乱原有顺序）。
fn sanitize_future_times(m: &mut Memory, now: f64) -> bool {
    // 与 doctor 体检的未来时间戳判定同阈（[`health::FUTURE_SKEW_SECS`] = 60s），
    // 保证「体检报的未来时间戳数」与「迁移/import 会钳制的条数」判据一致。
    let limit = now + health::FUTURE_SKEW_SECS;
    let mut touched = false;
    if m.created_at > limit {
        m.created_at = now;
        touched = true;
    }
    for t in &mut m.access_log {
        if *t > limit {
            *t = now;
            touched = true;
        }
    }
    if touched {
        m.access_log
            .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    }
    touched
}

/// 把逗号分隔的 id 列表解析为去空白、去空项的 `Vec<String>`（保持给定顺序、去重）。
fn parse_id_list(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in s.split(',') {
        let id = raw.trim();
        if !id.is_empty() && !out.iter().any(|x| x == id) {
            out.push(id.to_string());
        }
    }
    out
}

/// 执行 `confirm-use` 子命令：对每个 id 追加一次真使用时间戳（加固）；Cold 则复活。
///
/// 跨库载入全部记忆 → 逐 id 在合并集里查找：
/// - 命中：向其 `access_log` 追加 `now`（真使用加固）。若原 `status == Cold` 则
///   **复活**：`status = Active`，`level` 置为复活层（通用 → L3、项目 → L4.3，见
///   [`revived_level`]）。把更新后的记忆写回其 `project` 所路由的库；该 project
///   无对应映射时记 stderr 跳过其写回（不 panic）。
/// - 未命中：记 stderr 并继续处理后续 id。
///
/// 逐 id 打印结果（追加使用 / 复活 / 未找到 / 无法路由）。
fn run_confirm_use(
    general_db: &Path,
    project_db_raw: &[String],
    ids_raw: &str,
    now: Option<f64>,
) -> ExitCode {
    let Some(now) = resolve_now(now) else {
        return ExitCode::FAILURE;
    };
    let Some(project_dbs) = resolve_project_dbs(project_db_raw) else {
        return ExitCode::FAILURE;
    };
    let dbs = match DbSet::open(general_db, &project_dbs) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let merged = match dbs.read_all() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let ids = parse_id_list(ids_raw);
    for id in &ids {
        let Some(mut m) = merged.iter().find(|m| &m.id == id).cloned() else {
            eprintln!("confirm-use: 未找到 id {id}，跳过");
            continue;
        };
        // 追加一次真使用（保持 access_log 升序：now 通常是最大值，直接 push 后排序兜底）。
        m.access_log.push(now);
        m.access_log
            .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let revived = m.status == Status::Cold;
        if revived {
            m.status = Status::Active;
            m.level = revived_level(m.project.as_deref());
        }
        // 路由写回。
        match dbs.route(m.project.as_deref()) {
            Some(db) => {
                if let Err(e) = store::put(db, &m) {
                    eprintln!("confirm-use: 写回 {id} 失败：{e}");
                    return ExitCode::FAILURE;
                }
                if revived {
                    println!(
                        "confirm-use: {id} 复活（status→active, level={}）并加固",
                        level_repr(m.level)
                    );
                } else {
                    println!("confirm-use: {id} 追加使用（共 {} 次）", m.access_log.len());
                }
            }
            None => {
                let pname = m.project.as_deref().unwrap_or("-");
                eprintln!("confirm-use: {id} 所属项目 {pname} 不在 --project-db 映射中，跳过写回");
            }
        }
    }
    ExitCode::SUCCESS
}

/// 执行 `supersede` 子命令：把 OLD 记忆标记为 Superseded（被 NEW 取代），写回其库。
///
/// 跨库找到 OLD → `status = Superseded`、`superseded_by = Some(NEW)`，写回其 `project`
/// 路由的库。这一步**无视层级/floor**把它移出热层（render 本就只展示 active）。
/// NEW 不存在于合并集时仅 stderr 警告、仍照常标记。OLD 找不到则报错退出（非 0）。
fn run_supersede(
    general_db: &Path,
    project_db_raw: &[String],
    old_id: &str,
    new_id: &str,
    now: Option<f64>,
) -> ExitCode {
    // now 当前命令未直接使用，但仍解析以统一接口并校验系统时钟。
    let Some(_now) = resolve_now(now) else {
        return ExitCode::FAILURE;
    };
    let Some(project_dbs) = resolve_project_dbs(project_db_raw) else {
        return ExitCode::FAILURE;
    };
    let dbs = match DbSet::open(general_db, &project_dbs) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let merged = match dbs.read_all() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let Some(mut old) = merged.iter().find(|m| m.id == old_id).cloned() else {
        eprintln!("supersede 失败：未找到 OLD 记忆 {old_id}");
        return ExitCode::FAILURE;
    };
    // NEW 不存在仅警告，不阻断标记。
    if !merged.iter().any(|m| m.id == new_id) {
        eprintln!("supersede: 警告——NEW 记忆 {new_id} 当前不存在（仍照常标记 OLD）");
    }

    old.status = Status::Superseded;
    old.superseded_by = Some(new_id.to_string());

    match dbs.route(old.project.as_deref()) {
        Some(db) => {
            if let Err(e) = store::put(db, &old) {
                eprintln!("supersede: 写回 {old_id} 失败：{e}");
                return ExitCode::FAILURE;
            }
            println!("supersede: {old_id} → superseded（被 {new_id} 取代），已移出热层");
            ExitCode::SUCCESS
        }
        None => {
            let pname = old.project.as_deref().unwrap_or("-");
            eprintln!(
                "supersede 失败：{old_id} 所属项目 {pname} 不在 --project-db 映射中，无法写回"
            );
            ExitCode::FAILURE
        }
    }
}

/// `forget` 命令的全部参数（聚成一个结构体，规避过多函数形参）。
struct ForgetArgs<'a> {
    general_db: &'a Path,
    project_db: &'a [String],
    ids: &'a [String],
    now: Option<f64>,
}

/// 执行 `forget` 子命令：按 id **物理删除**记忆，不留任何痕迹。
///
/// 与软删除家族语义不同：`supersede` 把旧记忆转 Superseded、`merge` 把源转
/// Tombstone、`gc` 只按 TTL 删过期的 Cold/Tombstone——而 `forget` 是**直接
/// 硬删除**：在「公共库 + 所有挂载项目库」里逐库找每个 id，命中哪个库就从哪个
/// 库 [`store::remove`]，不写 superseded、不写 tombstone。路由不预设 id 属于
/// 哪层/哪库（不依赖记忆的 `project` 字段），按「哪个库真有这个 id」来删；同一
/// id 若（异常地）同时存在于多个库，每个命中的库都会删掉。
///
/// 逐 id 打印「已删除（来自哪个库）/ 未找到」，末尾汇总条数。未找到不算失败、
/// 不影响其它 id 的删除；即便全部 id 都未找到也 exit 0（幂等友好）。仅当解析
/// 参数、打开库或删除时发生底层存储错误才非 0 退出。
fn run_forget(args: ForgetArgs<'_>) -> ExitCode {
    // now 当前命令未直接使用，但仍解析以统一接口并校验系统时钟。
    let Some(_now) = resolve_now(args.now) else {
        return ExitCode::FAILURE;
    };
    let Some(project_dbs) = resolve_project_dbs(args.project_db) else {
        return ExitCode::FAILURE;
    };
    let dbs = match DbSet::open(args.general_db, &project_dbs) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    // 去空白、去空项、按给定顺序去重（同一 id 重复给只删一次、只报一次）。
    let mut ids: Vec<&str> = Vec::new();
    for raw in args.ids {
        let id = raw.trim();
        if !id.is_empty() && !ids.contains(&id) {
            ids.push(id);
        }
    }

    let mut deleted = 0usize;
    let mut missing = 0usize;
    for id in &ids {
        // 在所有挂载库里找：命中哪个库就从哪个库物理删除。
        let mut hit_libs: Vec<String> = Vec::new();
        match store::remove(&dbs.general, id) {
            Ok(true) => hit_libs.push("(general)".to_string()),
            Ok(false) => {}
            Err(e) => {
                eprintln!("forget: 从公共库删除 {id} 失败：{e}");
                return ExitCode::FAILURE;
            }
        }
        for (name, db) in &dbs.projects {
            match store::remove(db, id) {
                Ok(true) => hit_libs.push(name.clone()),
                Ok(false) => {}
                Err(e) => {
                    eprintln!("forget: 从项目库 {name} 删除 {id} 失败：{e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        if hit_libs.is_empty() {
            println!("forget: 未找到 {id}");
            missing += 1;
        } else {
            println!("forget: 已删除 {id}（来自 {}）", hit_libs.join("、"));
            deleted += 1;
        }
    }

    println!("forget: 共删除 {deleted} 条 / {missing} 个未找到");
    ExitCode::SUCCESS
}

/// `graduate` 命令的全部参数（聚成一个结构体，规避过多函数形参）。
struct GraduateArgs<'a> {
    general_db: &'a Path,
    project_db: &'a [String],
    id: &'a str,
    to_level: Option<&'a str>,
    cue: Option<&'a str>,
    importance: Option<f64>,
    new_id: Option<&'a str>,
    now: Option<f64>,
}

/// 执行 `graduate` 子命令（§6 毕业通道 L4 → L1-3）：把一条 active 的项目记忆（L4.x）
/// 提拔进通用层——在公共库写入一条新通用记忆（[`commands::build_graduated_memory`]），并把
/// 原 L4 记忆转 [`Status::Superseded`]、`superseded_by = 新 id`，作为留在项目库里的
/// 「已上浮」指针（§6：移动 + 留指针、不复制，避免两层各存一份打架）。
///
/// 校验（任一失败即非 0 退出且不写任何库）：
/// - 源 id 存在且 `status == Active`；
/// - 源是 L4.x、`--to-level`（若给）为通用层 L1/L2/L3（[`commands::graduate_target_level`] 把关）；
/// - `--importance`（若给）落在 `[0,1]`；
/// - 源所属项目在 `--project-db` 映射中（否则无法写回「已上浮」指针）；
/// - 新 id 不与任何已有记忆冲突。
///
/// 写入顺序：先把新通用记忆落公共库，确认成功后再把源转 superseded 写回项目库——
/// 即便第二步失败，毕业目标也已存在、不会丢失上浮内容。
fn run_graduate(args: GraduateArgs<'_>) -> ExitCode {
    let Some(now) = resolve_now(args.now) else {
        return ExitCode::FAILURE;
    };

    // 解析可选 --to-level 覆盖（仅校验格式；是否落通用层由 graduate_target_level 把关）。
    let to_override = match args.to_level {
        None => None,
        Some(s) => match parse_level(s) {
            Ok(lv) => Some(lv),
            Err(e) => {
                eprintln!("graduate 失败：{e}");
                return ExitCode::FAILURE;
            }
        },
    };

    // 校验可选 --importance 覆盖范围。
    if let Some(imp) = args.importance {
        if !(0.0..=1.0).contains(&imp) {
            eprintln!("graduate 失败：importance={imp} 超出范围 [0,1]");
            return ExitCode::FAILURE;
        }
    }

    let Some(project_dbs) = resolve_project_dbs(args.project_db) else {
        return ExitCode::FAILURE;
    };
    let dbs = match DbSet::open(args.general_db, &project_dbs) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let merged = match dbs.read_all() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    // 定位源记忆：必须存在且 active（superseded/cold/tombstone 的不再毕业）。
    let Some(mut src) = merged.iter().find(|m| m.id == args.id).cloned() else {
        eprintln!("graduate 失败：未找到源记忆 {}", args.id);
        return ExitCode::FAILURE;
    };
    if src.status != Status::Active {
        eprintln!(
            "graduate 失败：源记忆 {} 状态为 {:?}，仅 active 的项目记忆可毕业",
            args.id, src.status
        );
        return ExitCode::FAILURE;
    }

    // 算目标通用层（内部校验源须为 L4.x、--to-level 须落通用层）。
    let to_level = match commands::graduate_target_level(src.level, to_override) {
        Ok(lv) => lv,
        Err(e) => {
            eprintln!("graduate 失败：{e}");
            return ExitCode::FAILURE;
        }
    };

    // 源所属项目必须在 --project-db 映射中（既已读到、也要能写回「已上浮」指针）。
    let Some(project_name) = src.project.as_deref() else {
        // L4 记忆按不变量必带 project；走到这里说明数据异常，防御性报错。
        eprintln!(
            "graduate 失败：源记忆 {} 是项目记忆却无所属项目，数据异常",
            args.id
        );
        return ExitCode::FAILURE;
    };
    if dbs.route(Some(project_name)).is_none() {
        eprintln!(
            "graduate 失败：源记忆所属项目 {project_name} 不在 --project-db 映射中，无法写回"
        );
        return ExitCode::FAILURE;
    }

    // 决定新通用记忆 id，并防其与任何已有记忆冲突（--new-id 可能撞）。
    let new_id = match args.new_id {
        Some(s) => s.to_string(),
        None => generate_id(args.cue.unwrap_or(&src.cue), now),
    };
    if merged.iter().any(|m| m.id == new_id) {
        eprintln!("graduate 失败：新通用记忆 id {new_id} 与已有记忆冲突");
        return ExitCode::FAILURE;
    }

    // 构造毕业后的新通用记忆（脱离项目、继承使用痕迹/资历/指针，补 graduated 血缘）。
    let graduated =
        commands::build_graduated_memory(&new_id, &src, to_level, args.cue, args.importance);

    // 第一步：把新通用记忆写入公共库。
    if let Err(e) = store::put(&dbs.general, &graduated) {
        eprintln!("graduate 失败：写入新通用记忆 {new_id} 失败：{e}");
        return ExitCode::FAILURE;
    }

    // 第二步：把源 L4 记忆转 superseded、superseded_by=新 id，写回其项目库（留作已上浮指针）。
    src.status = Status::Superseded;
    src.superseded_by = Some(new_id.clone());
    match dbs.route(src.project.as_deref()) {
        Some(db) => {
            if let Err(e) = store::put(db, &src) {
                eprintln!(
                    "graduate 失败：写回源记忆 {} 的已上浮指针失败：{e}",
                    args.id
                );
                return ExitCode::FAILURE;
            }
        }
        None => {
            // 前已校验映射存在，理论不可达；防御性报错。
            eprintln!("graduate 失败：源记忆所属项目不在 --project-db 映射中，无法写回");
            return ExitCode::FAILURE;
        }
    }

    // cue 长度源头治理：对毕业后新通用记忆实际采用的 cue（--cue 覆盖或沿用源 cue）警告。
    warn_long_cue(&graduated.cue);

    println!(
        "graduate: {} ({:?}) → 毕业为通用记忆 {new_id} ({:?})；原记忆已转 superseded 留作已上浮指针",
        args.id, src.level, graduated.level
    );
    ExitCode::SUCCESS
}

/// `merge` 命令的全部参数（聚成一个结构体，规避过多函数形参）。
struct MergeArgs<'a> {
    general_db: &'a Path,
    project_db: &'a [String],
    from: &'a str,
    cue: &'a str,
    level: Option<&'a str>,
    importance: Option<f64>,
    project: Option<&'a str>,
    id: Option<&'a str>,
    now: Option<f64>,
}

/// 执行 `merge` 子命令：把多条同作用域源记忆合并为一条新记忆，源全部转 Tombstone。
///
/// 跨库载入所有 `--from` 源 → 校验同作用域（[`validate_merge_scope`]）→ 构造新合并
/// 记忆（[`build_merged_memory`]）→ 每个源标记 `status = Tombstone`、
/// `superseded_by = Some(NEWID)`（保留负知识痕迹）→ 把新记忆与各源都写回该作用域
/// 对应的库 → 打印新 id 与合并条数。
///
/// 校验失败（源缺失 / 混作用域 / `--project` 与源作用域冲突 / importance 越界 /
/// level 非法）一律非 0 退出且不写任何库。
fn run_merge(args: MergeArgs<'_>) -> ExitCode {
    let Some(now) = resolve_now(args.now) else {
        return ExitCode::FAILURE;
    };
    let Some(project_dbs) = resolve_project_dbs(args.project_db) else {
        return ExitCode::FAILURE;
    };

    // 解析可选 level 覆盖。
    let level_override = match args.level {
        None => None,
        Some(s) => match parse_level(s) {
            Ok(lv) => Some(lv),
            Err(e) => {
                eprintln!("merge 失败：{e}");
                return ExitCode::FAILURE;
            }
        },
    };
    // 校验可选 importance 覆盖范围。
    if let Some(imp) = args.importance {
        if !(0.0..=1.0).contains(&imp) {
            eprintln!("merge 失败：importance={imp} 超出范围 [0,1]");
            return ExitCode::FAILURE;
        }
    }

    let dbs = match DbSet::open(args.general_db, &project_dbs) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let merged = match dbs.read_all() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    // 收集 --from 源（保持给定顺序、去重）。缺失任一源即报错。
    let from_ids = parse_id_list(args.from);
    if from_ids.is_empty() {
        eprintln!("merge 失败：--from 未给任何源 id");
        return ExitCode::FAILURE;
    }
    let mut sources: Vec<Memory> = Vec::with_capacity(from_ids.len());
    for id in &from_ids {
        match merged.iter().find(|m| &m.id == id) {
            Some(m) => sources.push(m.clone()),
            None => {
                eprintln!("merge 失败：未找到源记忆 {id}");
                return ExitCode::FAILURE;
            }
        }
    }

    // 校验同作用域。
    let scope = match validate_merge_scope(&sources) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    // 若显式给了 --project，须与源的共同作用域一致。
    if let Some(p) = args.project {
        let ok = matches!(&scope, MergeScope::Project(name) if name == p);
        if !ok {
            eprintln!(
                "merge 失败：--project {p} 与源的共同作用域不一致（源{}）",
                match &scope {
                    MergeScope::General => "为通用，不应给 --project".to_string(),
                    MergeScope::Project(name) => format!("属项目 {name}"),
                }
            );
            return ExitCode::FAILURE;
        }
    }

    // 决定新 id。
    let new_id = match args.id {
        Some(s) => s.to_string(),
        None => generate_id(args.cue, now),
    };

    // 构造合并记忆。
    let merged_mem = build_merged_memory(
        &new_id,
        args.cue,
        &sources,
        &scope,
        level_override,
        args.importance,
        now,
    );

    // 路由库句柄：新记忆与各源同作用域，写同一个目标库。
    let scope_project: Option<&str> = match &scope {
        MergeScope::General => None,
        MergeScope::Project(name) => Some(name.as_str()),
    };
    let Some(db) = dbs.route(scope_project) else {
        let pname = scope_project.unwrap_or("-");
        eprintln!("merge 失败：作用域项目 {pname} 不在 --project-db 映射中，无法写回");
        return ExitCode::FAILURE;
    };

    // 把各源标记为 Tombstone、superseded_by=新 id；与新记忆一起在同一库批量写回。
    let mut batch: Vec<Memory> = Vec::with_capacity(sources.len() + 1);
    for mut s in sources {
        s.status = Status::Tombstone;
        s.superseded_by = Some(new_id.clone());
        batch.push(s);
    }
    let merged_count = batch.len();
    batch.push(merged_mem);

    if let Err(e) = store::put_many(db, &batch) {
        eprintln!("merge: 写回失败：{e}");
        return ExitCode::FAILURE;
    }

    // cue 长度源头治理：合并生成的新 cue 同样受源头警告约束。
    warn_long_cue(args.cue);

    println!("merge: 新记忆 {new_id} 合并了 {merged_count} 条源（源已转 Tombstone）");
    ExitCode::SUCCESS
}

/// `gc` 命令的全部参数（聚成一个结构体，规避过多函数形参）。
struct GcArgs<'a> {
    general_db: &'a Path,
    project_db: &'a [String],
    ttl_days: f64,
    tombstone_ttl_days: f64,
    min_uses: usize,
    now: Option<f64>,
    dry_run: bool,
}

/// 执行 `gc` 子命令：按 §7.4 TTL 硬删除过期的 Cold / Tombstone 记忆。
///
/// 跨库载入全部记忆 → 对每条用 [`gc_should_delete`] 判定是否应删
/// （Cold 超 `ttl_days`、Tombstone 超 `tombstone_ttl_days`；Active/Superseded 永不删，
/// `last_touch` 见 [`last_touch`]）→ 命中者从其 `project` 路由的库 [`store::remove`]
/// 删除。`--dry-run` 只报告不删。打印删除清单与各库删除条数。
fn run_gc(args: GcArgs<'_>) -> ExitCode {
    let Some(now) = resolve_now(args.now) else {
        return ExitCode::FAILURE;
    };
    let Some(project_dbs) = resolve_project_dbs(args.project_db) else {
        return ExitCode::FAILURE;
    };
    let dbs = match DbSet::open(args.general_db, &project_dbs) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let merged = match dbs.read_all() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    println!(
        "== gc (now={now}, ttl={} 天, tombstone_ttl={} 天, min_uses={}){} ==",
        args.ttl_days,
        args.tombstone_ttl_days,
        args.min_uses,
        if args.dry_run {
            "（dry-run，不删除）"
        } else {
            ""
        }
    );

    // 按库（项目名键，公共库用 None）统计删除条数。用 BTreeMap 保证输出稳定。
    let mut deleted_by_scope: BTreeMap<String, usize> = BTreeMap::new();
    let mut total = 0usize;

    for m in &merged {
        if !gc_should_delete(
            m,
            now,
            args.ttl_days,
            args.tombstone_ttl_days,
            args.min_uses,
        ) {
            continue;
        }
        let scope_label = m.project.as_deref().unwrap_or("(general)").to_string();
        let age_days = (now - last_touch(m)) / engram::model::SECS_PER_DAY;
        println!(
            "  删除 {} | {} | {} | age={:.1}天 | {}",
            m.id,
            status_repr(m.status),
            scope_label,
            age_days,
            m.cue,
        );

        if !args.dry_run {
            match dbs.route(m.project.as_deref()) {
                Some(db) => match store::remove(db, &m.id) {
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("gc: 删除 {} 失败：{e}", m.id);
                        return ExitCode::FAILURE;
                    }
                },
                None => {
                    let pname = m.project.as_deref().unwrap_or("-");
                    eprintln!(
                        "gc: {} 所属项目 {pname} 不在 --project-db 映射中，跳过删除",
                        m.id
                    );
                    continue;
                }
            }
        }
        *deleted_by_scope.entry(scope_label).or_default() += 1;
        total += 1;
    }

    if total == 0 {
        println!("  （无可删除条目）");
    } else {
        for (scope, count) in &deleted_by_scope {
            println!(
                "gc: {} {} {} 条",
                scope,
                if args.dry_run { "将删" } else { "已删" },
                count
            );
        }
        println!(
            "gc: 共 {} {} 条",
            if args.dry_run { "将删" } else { "已删" },
            total
        );
    }

    ExitCode::SUCCESS
}

// ============================================================================
// migrate：版本触发的一次性数据迁移（结构自动洗 + 语义只标记）
// ============================================================================

/// 迁移的运行选项（由 CLI 参数解析而来）。
struct MigrateOptions {
    /// 仅当库级 `data_version` 落后才迁移。
    if_needed: bool,
    /// 只报告、不备份不写库。
    dry_run: bool,
    /// 备份输出根目录覆盖（缺省取「公共库父目录 / backups」）。
    backup_dir: Option<PathBuf>,
}

/// 一次迁移（或 dry-run 预演）的结果报告。
struct MigrationReport {
    /// 迁移前的库级数据版本（各挂载库取最小值）。
    from_version: u32,
    /// 迁移目标版本（= [`ENGRAM_DATA_VERSION`]）。
    to_version: u32,
    /// 迁移涉及的记忆总数。
    total: usize,
    /// 结构性清洗中被修正的记忆条数（schema_version / 未来时间戳 / access_log）。
    structural_changed: usize,
    /// 重分层 + 容量步（consolidate）产生的变迁条数。
    transitions: usize,
    /// 实际写出的备份目录（dry-run 为 `None`）。
    backup_dir: Option<PathBuf>,
    /// 迁移后（dry-run 为预演后）的健康扫描——语义性问题取自此。
    health: HealthReport,
}

/// 迁移的三种归宿。
enum MigrationOutcome {
    /// 跳过（已最新 / 迁移锁被他人持有），附带原因说明。
    Skipped(String),
    /// dry-run 预演（未写库），附带预演报告。
    DryRun(MigrationReport),
    /// 真迁移完成，附带迁移报告。
    Migrated(MigrationReport),
}

/// 库级迁移锁文件路径：`<general_db>.migrate.lock`。
///
/// 用 [`session::try_claim`] 的 `create_new` OS 级原子语义防两个会话同时迁移同一批库；
/// 与 watermark.lock / 复盘 claim 复用**同一套**领单原语（见
/// [`session::try_claim_ttl`]），陈旧阈值取 [`session::CLAIM_TTL_SECS`]。
fn migrate_lock_path(general_db: &Path) -> PathBuf {
    general_db.with_extension("migrate.lock")
}

/// 推导本次迁移的备份子目录：`<root>/pre-migrate-v<from>-to-v<to>-<时间戳>`。
///
/// `root` 取 `override_dir`，否则「公共库父目录 / backups」；时间戳用引擎 `now`
/// （[`resolve_now`] 生成）取整数秒。
fn resolve_backup_dir(
    general_db: &Path,
    override_dir: Option<&Path>,
    from: u32,
    to: u32,
    now: f64,
) -> PathBuf {
    let root = match override_dir {
        Some(d) => d.to_path_buf(),
        None => general_db
            .parent()
            .map(|p| p.join("backups"))
            .unwrap_or_else(|| PathBuf::from("backups")),
    };
    let ts = now.max(0.0) as u64;
    root.join(format!("pre-migrate-v{from}-to-v{to}-{ts}"))
}

/// 对一条记忆做**结构性清洗**（迁移步②）：补 `schema_version`、钳未来时间戳、
/// `access_log` 去重并升序排序。返回是否发生过任何改动。
///
/// **严格只碰结构性字段**——绝不读写 `importance` / `status` / `superseded_by`
/// （那些是语义性问题，只能进报告，不在本函数）。这是「语义只标记零泄漏进结构路径」
/// 的关键一环：本函数在编译期就没有触碰语义字段的任何语句。
fn structural_clean(m: &mut Memory, now: f64) -> bool {
    let mut touched = false;
    // 补 schema_version：显式为 0 的旧行 → 当前 schema 版本（缺字段的旧行反序列化
    // 时已按 1 补齐，写回时自然持久化该字段）。
    if m.schema_version == 0 {
        m.schema_version = MEMORY_SCHEMA_VERSION;
        touched = true;
    }
    // 未来时间戳钳制（复用 import 的 clamp：created_at / access_log > now+60s → now，
    // 并重排升序）。
    if sanitize_future_times(m, now) {
        touched = true;
    }
    // access_log 去重 + 升序（clamp 可能引入重复 / 打乱顺序）。
    let before = m.access_log.len();
    m.access_log
        .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    m.access_log.dedup();
    if m.access_log.len() != before {
        touched = true;
    }
    touched
}

/// 对整批记忆逐条做结构性清洗，返回被修正的条数。
fn clean_all(mems: &mut [Memory], now: f64) -> usize {
    let mut changed = 0usize;
    for m in mems.iter_mut() {
        if structural_clean(m, now) {
            changed += 1;
        }
    }
    changed
}

/// 只读打开公共库 + 各项目库，读出各库版本的最小值与合并记忆集（不保留句柄）。
///
/// 供迁移的 dry-run 路径用（只读、不写、不加锁）。
///
/// # Errors
/// 任一库打开或读取失败时返回带库类别说明的错误字符串。
fn read_min_version_and_merged(
    general_db: &Path,
    project_dbs: &BTreeMap<String, PathBuf>,
) -> Result<(u32, Vec<Memory>), String> {
    let gdb = store::open(general_db)
        .map_err(|e| format!("打开公共库 {} 失败：{e}", general_db.display()))?;
    let mut min_version =
        store::read_data_version(&gdb).map_err(|e| format!("读取公共库版本失败：{e}"))?;
    let mut merged = store::all(&gdb).map_err(|e| format!("读取公共库失败：{e}"))?;
    for (name, path) in project_dbs {
        let db = store::open(path)
            .map_err(|e| format!("打开项目库 {name}（{}）失败：{e}", path.display()))?;
        let v = store::read_data_version(&db)
            .map_err(|e| format!("读取项目库 {name} 版本失败：{e}"))?;
        min_version = min_version.min(v);
        let mut pmems = store::all(&db).map_err(|e| format!("读取项目库 {name} 失败：{e}"))?;
        merged.append(&mut pmems);
    }
    Ok((min_version, merged))
}

/// 把迁移后的合并集**按层级 + 项目路由**写回各库（L1-3 → 公共库，L4 → 对应项目库）。
///
/// consolidate 只在同一条轨道内移动层级（通用 L1↔L2↔L3、项目 L4.1↔L4.2↔L4.3），
/// 不跨轨道，故记忆的归属库稳定：从公共库读来的通用记忆写回公共库、从项目库读来的
/// L4 写回原项目库，不会串库。project 不在映射中的 L4 记忆记 stderr 并跳过写回（不删）。
///
/// # Errors
/// 任一库写回失败时返回错误说明。
fn write_back_merged(
    gdb: &Database,
    pdbs: &BTreeMap<String, Database>,
    merged: Vec<Memory>,
) -> Result<(), String> {
    let mut general_bucket: Vec<Memory> = Vec::new();
    let mut project_buckets: BTreeMap<String, Vec<Memory>> = BTreeMap::new();
    let mut unrouted = 0usize;
    for m in merged {
        if commands::is_l4(m.level) {
            match &m.project {
                Some(name) if pdbs.contains_key(name) => {
                    project_buckets.entry(name.clone()).or_default().push(m);
                }
                _ => unrouted += 1,
            }
        } else {
            general_bucket.push(m);
        }
    }
    if !general_bucket.is_empty() {
        store::put_many(gdb, &general_bucket).map_err(|e| format!("写回公共库失败：{e}"))?;
    }
    for (name, bucket) in &project_buckets {
        let db = pdbs
            .get(name)
            .ok_or_else(|| format!("内部错误：项目 {name} 无对应库句柄"))?;
        store::put_many(db, bucket).map_err(|e| format!("写回项目库 {name} 失败：{e}"))?;
    }
    if unrouted > 0 {
        eprintln!(
            "migrate: 警告：{unrouted} 条 L4 记忆的所属项目不在 --project-db 映射中，已跳过其写回（未删除）"
        );
    }
    Ok(())
}

/// 迁移的**持锁临界区**：已持有迁移锁，执行备份 → 清洗 → 重分层 → 语义扫描 → 写回 →
/// 写版本。调用方（[`migrate_libs`]）负责在本函数返回后释放锁。
///
/// # Errors
/// 备份失败（迁移中止、不动库）、任一库读写失败时返回错误说明。
fn migrate_locked(
    general_db: &Path,
    project_dbs: &BTreeMap<String, PathBuf>,
    now: f64,
    opts: &MigrateOptions,
    to_version: u32,
) -> Result<MigrationOutcome, String> {
    // 打开公共库 + 各项目库，保留句柄用于写回；读版本最小值与合并集。
    let gdb = store::open(general_db)
        .map_err(|e| format!("打开公共库 {} 失败：{e}", general_db.display()))?;
    let mut min_version =
        store::read_data_version(&gdb).map_err(|e| format!("读取公共库版本失败：{e}"))?;
    let mut merged = store::all(&gdb).map_err(|e| format!("读取公共库失败：{e}"))?;
    let mut pdbs: BTreeMap<String, Database> = BTreeMap::new();
    for (name, path) in project_dbs {
        let db = store::open(path)
            .map_err(|e| format!("打开项目库 {name}（{}）失败：{e}", path.display()))?;
        let v = store::read_data_version(&db)
            .map_err(|e| format!("读取项目库 {name} 版本失败：{e}"))?;
        min_version = min_version.min(v);
        let mut pmems = store::all(&db).map_err(|e| format!("读取项目库 {name} 失败：{e}"))?;
        merged.append(&mut pmems);
        pdbs.insert(name.clone(), db);
    }
    let from_version = min_version;

    // 持锁后再确认一次：--if-needed 且已最新则不动库（防在等锁期间别人已迁完）。
    if opts.if_needed && from_version >= to_version {
        return Ok(MigrationOutcome::Skipped(format!(
            "已是最新（data_version={from_version} ≥ {to_version}），跳过"
        )));
    }

    // 空库：没有任何按旧规则分布的数据，无需备份/清洗/重分层——直接把版本标记为最新
    // 即可（幂等自愈：全新库首次 session-start 会走到这里被静默标记，之后不再触发）。
    if merged.is_empty() {
        store::write_data_version(&gdb, to_version)
            .map_err(|e| format!("写公共库 data_version 失败：{e}"))?;
        for (name, db) in &pdbs {
            store::write_data_version(db, to_version)
                .map_err(|e| format!("写项目库 {name} data_version 失败：{e}"))?;
        }
        return Ok(MigrationOutcome::Skipped(format!(
            "库为空，已标记为 v{to_version}，无需迁移"
        )));
    }

    // ① 备份优先：把**原始**合并集（清洗前）完整导出。失败即中止，绝不动库。
    let backup_dir = resolve_backup_dir(
        general_db,
        opts.backup_dir.as_deref(),
        from_version,
        to_version,
        now,
    );
    let refs: Vec<&Memory> = merged.iter().collect();
    write_memories_as_json(&backup_dir, &refs)
        .map_err(|e| format!("备份失败（迁移已中止，未改动任何库）：{e}"))?;

    // ② 结构性清洗（自动）。
    let structural_changed = clean_all(&mut merged, now);

    // ③ 重分层 + 容量（自动）：默认 EngramConfig，按虚拟视界重评层级、按字符预算 +
    //    条数双约束执行容量——旧尖峰升上来的层降回、超预算层挤下去（可能转 Cold，
    //    属结构性容量执行，允许）。
    let transitions = consolidate(&mut merged, now).len();

    // ④ 语义性问题：**只扫描收进报告，绝不自动改**（health::scan 取不可变借用）。
    let health = health::scan(&merged, &EngramConfig::default(), now);

    // 写回（按层级 + 项目路由）。
    let total = merged.len();
    write_back_merged(&gdb, &pdbs, merged)?;

    // ⑤ 写库级 data_version（公共库 + 各项目库都标记为最新）。
    store::write_data_version(&gdb, to_version)
        .map_err(|e| format!("写公共库 data_version 失败：{e}"))?;
    for (name, db) in &pdbs {
        store::write_data_version(db, to_version)
            .map_err(|e| format!("写项目库 {name} data_version 失败：{e}"))?;
    }

    Ok(MigrationOutcome::Migrated(MigrationReport {
        from_version,
        to_version,
        total,
        structural_changed,
        transitions,
        backup_dir: Some(backup_dir),
        health,
    }))
}

/// 迁移的执行核（[`run_migrate`] 与 session-start 自动触发共用）。
///
/// dry-run 只读预演、不加锁；实迁移先抢库级迁移锁（拿不到即 [`MigrationOutcome::Skipped`]，
/// 下次再迁），持锁执行 [`migrate_locked`] 后无论成败都释放锁。
///
/// # Errors
/// 备份失败或库读写失败时返回错误说明（dry-run 只读失败同理）。
fn migrate_libs(
    general_db: &Path,
    project_dbs: &BTreeMap<String, PathBuf>,
    now: f64,
    opts: &MigrateOptions,
) -> Result<MigrationOutcome, String> {
    let to_version = ENGRAM_DATA_VERSION;

    // dry-run：只读、不加锁、不写、不备份，只算「将做什么 + 语义问题」。
    if opts.dry_run {
        let (from_version, merged) = read_min_version_and_merged(general_db, project_dbs)?;
        if opts.if_needed && from_version >= to_version {
            return Ok(MigrationOutcome::Skipped(format!(
                "已是最新（data_version={from_version} ≥ {to_version}），跳过"
            )));
        }
        let mut work = merged;
        let structural_changed = clean_all(&mut work, now);
        let transitions = consolidate(&mut work, now).len();
        let health = health::scan(&work, &EngramConfig::default(), now);
        return Ok(MigrationOutcome::DryRun(MigrationReport {
            from_version,
            to_version,
            total: work.len(),
            structural_changed,
            transitions,
            backup_dir: None,
            health,
        }));
    }

    // 实迁移：抢库级迁移锁（拿不到即跳过，下次再迁——绝不阻塞等待）。
    let lock_path = migrate_lock_path(general_db);
    match session::try_claim(&lock_path, now, std::process::id()) {
        Ok(session::ClaimOutcome::Acquired) => {}
        Ok(session::ClaimOutcome::Held) => {
            return Ok(MigrationOutcome::Skipped(
                "另一进程正在迁移（迁移锁被持有），本次跳过".to_string(),
            ));
        }
        Err(e) => return Err(format!("获取迁移锁失败：{e}")),
    }
    // 持锁执行，无论成败都释放锁（与领单收尾配对）。
    let result = migrate_locked(general_db, project_dbs, now, opts, to_version);
    session::release_claim(&lock_path);
    result
}

/// `migrate` 命令的全部参数（聚成结构体，规避过多形参）。
struct MigrateCliArgs<'a> {
    general_db: &'a Path,
    project_db: &'a [String],
    if_needed: bool,
    backup_dir: Option<&'a Path>,
    dry_run: bool,
    now: Option<f64>,
}

/// 执行 `migrate` 子命令。
fn run_migrate(args: MigrateCliArgs<'_>) -> ExitCode {
    let Some(now) = resolve_now(args.now) else {
        return ExitCode::FAILURE;
    };
    let Some(project_dbs) = resolve_project_dbs(args.project_db) else {
        return ExitCode::FAILURE;
    };
    let opts = MigrateOptions {
        if_needed: args.if_needed,
        dry_run: args.dry_run,
        backup_dir: args.backup_dir.map(Path::to_path_buf),
    };
    match migrate_libs(args.general_db, &project_dbs, now, &opts) {
        Ok(MigrationOutcome::Skipped(reason)) => {
            println!("migrate: {reason}");
            ExitCode::SUCCESS
        }
        Ok(MigrationOutcome::DryRun(rep)) => {
            print_migration_report(&rep, true);
            ExitCode::SUCCESS
        }
        Ok(MigrationOutcome::Migrated(rep)) => {
            print_migration_report(&rep, false);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("migrate 失败：{e}");
            ExitCode::FAILURE
        }
    }
}

/// 打印迁移（或 dry-run 预演）报告到 stdout。
fn print_migration_report(rep: &MigrationReport, dry_run: bool) {
    if dry_run {
        println!("== migrate --dry-run（只报告，不备份、不写库）==");
    } else {
        println!("== migrate 完成 ==");
    }
    println!(
        "  data_version: v{} → v{}",
        rep.from_version, rep.to_version
    );
    println!("  记忆总数: {}", rep.total);
    println!(
        "  结构性清洗: {} 条被修正（schema_version / 未来时间戳 / access_log 去重排序）",
        rep.structural_changed
    );
    println!("  重分层+容量: consolidate 产生 {} 条变迁", rep.transitions);
    if let Some(dir) = &rep.backup_dir {
        println!("  备份目录: {}", dir.display());
    }
    print_semantic_issues(&rep.health);
}

/// 打印语义性问题清单（迁移只标记、绝不自动改的三类）。
fn print_semantic_issues(h: &HealthReport) {
    if !h.has_semantic_issues() {
        println!("  语义性问题: 无");
        return;
    }
    println!("  语义性问题（仅标记，未修改任何 importance/status，也未删除任何记忆）:");
    for o in &h.off_anchor_importance {
        println!(
            "    [层锚偏离] {} 在 {} importance={:.2} 不在层锚区间 [{:.2},{:.2}]",
            o.id,
            level_repr(o.level),
            o.importance,
            o.band_lo,
            o.band_hi
        );
    }
    for d in &h.dangling_superseded {
        println!(
            "    [悬空指针] {} 的 superseded_by 指向库中不存在的 {}",
            d.id, d.target
        );
    }
    for g in &h.duplicate_cues {
        println!(
            "    [疑似重复] cue「{}」共 {} 条：{}",
            g.cue,
            g.ids.len(),
            g.ids.join(", ")
        );
    }
}

// ============================================================================
// doctor：只读健康体检
// ============================================================================

/// `doctor` 命令的全部参数（聚成结构体）。
struct DoctorArgs<'a> {
    general_db: &'a Path,
    project_db: &'a [String],
    json: bool,
    now: Option<f64>,
}

/// 执行 `doctor` 子命令：只读打开各库，产出健康报告（文本或 JSON）。**绝不写库**
/// （不 put/remove、不写 data_version、不触发迁移）。
fn run_doctor(args: DoctorArgs<'_>) -> ExitCode {
    let Some(now) = resolve_now(args.now) else {
        return ExitCode::FAILURE;
    };
    let Some(project_dbs) = resolve_project_dbs(args.project_db) else {
        return ExitCode::FAILURE;
    };

    // 公共库：读版本、原始行数、已解析记忆。
    let gdb = match store::open(args.general_db) {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "doctor 失败：打开公共库 {} 失败：{e}",
                args.general_db.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let mut versions: Vec<(String, u32)> = Vec::new();
    let mut bad_rows = 0usize;
    let mut merged: Vec<Memory> = Vec::new();
    match (
        store::read_data_version(&gdb),
        store::raw_len(&gdb),
        store::all(&gdb),
    ) {
        (Ok(v), Ok(raw), Ok(mut mems)) => {
            versions.push((health::GENERAL_SCOPE_LABEL.to_string(), v));
            bad_rows += (raw as usize).saturating_sub(mems.len());
            merged.append(&mut mems);
        }
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
            eprintln!("doctor 失败：读取公共库出错：{e}");
            return ExitCode::FAILURE;
        }
    }
    // 各项目库同理。
    for (name, path) in &project_dbs {
        let db = match store::open(path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!(
                    "doctor 失败：打开项目库 {name}（{}）失败：{e}",
                    path.display()
                );
                return ExitCode::FAILURE;
            }
        };
        match (
            store::read_data_version(&db),
            store::raw_len(&db),
            store::all(&db),
        ) {
            (Ok(v), Ok(raw), Ok(mut mems)) => {
                versions.push((name.clone(), v));
                bad_rows += (raw as usize).saturating_sub(mems.len());
                merged.append(&mut mems);
            }
            (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
                eprintln!("doctor 失败：读取项目库 {name} 出错：{e}");
                return ExitCode::FAILURE;
            }
        }
    }

    let mut report = health::scan(&merged, &EngramConfig::default(), now);
    report.bad_rows = bad_rows;
    let min_version = versions
        .iter()
        .map(|(_, v)| *v)
        .min()
        .unwrap_or(ENGRAM_DATA_VERSION);
    let needs_migration = min_version < ENGRAM_DATA_VERSION;

    if args.json {
        print_doctor_json(&versions, needs_migration, &report, now);
    } else {
        print_doctor_text(&versions, needs_migration, &report, now);
    }
    ExitCode::SUCCESS
}

/// 打印 doctor 的可读文本报告。
fn print_doctor_text(
    versions: &[(String, u32)],
    needs_migration: bool,
    h: &HealthReport,
    now: f64,
) {
    println!("== Engram doctor (now={now}) ==");
    println!("库数据版本（当前引擎数据版本 v{ENGRAM_DATA_VERSION}）:");
    for (scope, v) in versions {
        let tag = if *v < ENGRAM_DATA_VERSION {
            format!("需迁移 → v{ENGRAM_DATA_VERSION}")
        } else {
            "最新".to_string()
        };
        println!("  {scope}: v{v}  [{tag}]");
    }
    println!("需迁移: {}", if needs_migration { "是" } else { "否" });
    println!("记忆总数: {}（无法解析的坏行 {}）", h.total, h.bad_rows);

    // 各层分布。
    let dist: Vec<String> = h
        .level_counts
        .iter()
        .map(|(lvl, c)| format!("{}={c}", level_repr(*lvl)))
        .collect();
    println!("各层分布: {}", dist.join(" "));

    // schema_version 分布。
    let sv: Vec<String> = h
        .schema_versions
        .iter()
        .map(|(v, c)| format!("{v}={c}"))
        .collect();
    println!(
        "schema_version 分布: {}",
        if sv.is_empty() {
            "（空）".to_string()
        } else {
            sv.join(" ")
        }
    );

    // 超预算层。
    if h.over_budget_layers.is_empty() {
        println!("超字符预算的层: 无");
    } else {
        println!("超字符预算的层:");
        for o in &h.over_budget_layers {
            println!(
                "  {} {} 常驻 {} 字符 > 预算 {}（{} 条）",
                o.scope,
                level_repr(o.level),
                o.used,
                o.budget,
                o.count
            );
        }
    }

    println!(
        "未来时间戳: {} 条{}",
        h.future_timestamps.len(),
        fmt_id_tail(&h.future_timestamps)
    );

    if h.dangling_superseded.is_empty() {
        println!("悬空 superseded_by: 0 条");
    } else {
        println!("悬空 superseded_by: {} 条", h.dangling_superseded.len());
        for d in &h.dangling_superseded {
            println!("  {} → 不存在的 {}", d.id, d.target);
        }
    }

    if h.off_anchor_importance.is_empty() {
        println!("importance 偏离层锚: 0 条");
    } else {
        println!("importance 偏离层锚: {} 条", h.off_anchor_importance.len());
        for o in &h.off_anchor_importance {
            println!(
                "  {} 在 {} importance={:.2} 不在 [{:.2},{:.2}]",
                o.id,
                level_repr(o.level),
                o.importance,
                o.band_lo,
                o.band_hi
            );
        }
    }

    if h.duplicate_cues.is_empty() {
        println!("疑似重复: 0 组");
    } else {
        println!("疑似重复: {} 组", h.duplicate_cues.len());
        for g in &h.duplicate_cues {
            println!("  cue「{}」: {}", g.cue, g.ids.join(", "));
        }
    }

    println!(
        "孤儿（Superseded 无后继指针）: {} 条{}",
        h.orphans.len(),
        fmt_id_tail(&h.orphans)
    );
}

/// 把一小串 id 格式化成「（id1, id2, ...）」尾巴，空则返回空串。
fn fmt_id_tail(ids: &[String]) -> String {
    if ids.is_empty() {
        String::new()
    } else {
        format!("（{}）", ids.join(", "))
    }
}

/// 打印 doctor 的 JSON 报告（单行对象），用 [`serde_json`] 保证转义正确。
fn print_doctor_json(
    versions: &[(String, u32)],
    needs_migration: bool,
    h: &HealthReport,
    now: f64,
) {
    let version_objs: Vec<serde_json::Value> = versions
        .iter()
        .map(|(scope, v)| {
            serde_json::json!({
                "scope": scope,
                "data_version": v,
                "needs_migration": *v < ENGRAM_DATA_VERSION,
            })
        })
        .collect();
    let level_dist: Vec<serde_json::Value> = h
        .level_counts
        .iter()
        .map(|(lvl, c)| serde_json::json!({ "level": level_repr(*lvl), "count": c }))
        .collect();
    let schema_dist: Vec<serde_json::Value> = h
        .schema_versions
        .iter()
        .map(|(v, c)| serde_json::json!({ "schema_version": v, "count": c }))
        .collect();
    let over_budget: Vec<serde_json::Value> = h
        .over_budget_layers
        .iter()
        .map(|o| {
            serde_json::json!({
                "scope": o.scope,
                "level": level_repr(o.level),
                "used": o.used,
                "budget": o.budget,
                "count": o.count,
            })
        })
        .collect();
    let dangling: Vec<serde_json::Value> = h
        .dangling_superseded
        .iter()
        .map(|d| serde_json::json!({ "id": d.id, "target": d.target }))
        .collect();
    let off_anchor: Vec<serde_json::Value> = h
        .off_anchor_importance
        .iter()
        .map(|o| {
            serde_json::json!({
                "id": o.id,
                "level": level_repr(o.level),
                "importance": o.importance,
                "band": [o.band_lo, o.band_hi],
            })
        })
        .collect();
    let duplicates: Vec<serde_json::Value> = h
        .duplicate_cues
        .iter()
        .map(|g| serde_json::json!({ "cue": g.cue, "ids": g.ids }))
        .collect();

    let obj = serde_json::json!({
        "now": now,
        "engine_data_version": ENGRAM_DATA_VERSION,
        "needs_migration": needs_migration,
        "libraries": version_objs,
        "total": h.total,
        "bad_rows": h.bad_rows,
        "level_distribution": level_dist,
        "schema_version_distribution": schema_dist,
        "over_budget_layers": over_budget,
        "future_timestamps": h.future_timestamps,
        "dangling_superseded": dangling,
        "off_anchor_importance": off_anchor,
        "duplicate_cues": duplicates,
        "orphans": h.orphans,
    });
    match serde_json::to_string(&obj) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("doctor: 序列化 JSON 失败：{e}"),
    }
}

/// SessionStart 自动触发迁移（best-effort，绝不阻断热索引注入）。
///
/// 先廉价预检公共库（及存在的作用域库）的 `data_version`：任一落后于
/// [`ENGRAM_DATA_VERSION`] 才尝试 `migrate --if-needed`。迁移失败只 `eprintln` +
/// 追加 hook.log，**不上抛、不影响调用方**（下次会话重试）；成功则记一行
/// 「已迁移 v<from>→v<to>，备份于 <path>」。只在 session-start 调用，迁移只跑一次
/// （版本标记后 --if-needed 自然跳过）。
fn maybe_migrate_on_session_start(
    general_db: &Path,
    scope: &ProjectScope,
    now: f64,
    log: Option<&Path>,
) {
    // 预检公共库版本；读失败（库损坏等）就放弃迁移，绝不阻断注入。
    let g_version = match store::open(general_db).and_then(|db| store::read_data_version(&db)) {
        Ok(v) => v,
        Err(_) => return,
    };
    let mut min_version = g_version;
    let mut project_dbs: BTreeMap<String, PathBuf> = BTreeMap::new();
    // 作用域库：仅当已存在才纳入（不创建杂散锚点，与 read_merged_scope 同规矩）。
    if scope.db.is_file() {
        if let Ok(v) = store::open(&scope.db).and_then(|db| store::read_data_version(&db)) {
            min_version = min_version.min(v);
        }
        project_dbs.insert(scope.name.clone(), scope.db.clone());
    }
    if min_version >= ENGRAM_DATA_VERSION {
        return; // 已最新，不触发（新库不迁移）。
    }

    let opts = MigrateOptions {
        if_needed: true,
        dry_run: false,
        backup_dir: None,
    };
    let log_line = |msg: &str| {
        eprintln!("{msg}");
        if let Some(p) = hook_log_path(general_db.parent()) {
            append_hook_log(&p, now, msg);
        } else if let Some(p) = log {
            append_hook_log(p, now, msg);
        }
    };
    match migrate_libs(general_db, &project_dbs, now, &opts) {
        Ok(MigrationOutcome::Migrated(rep)) => {
            let backup = rep
                .backup_dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            log_line(&format!(
                "session-start：已迁移 v{}→v{}，备份于 {}",
                rep.from_version, rep.to_version, backup
            ));
        }
        // 已最新 / 锁被持有 / dry-run：静默（预检已过滤大多数「已最新」）。
        Ok(MigrationOutcome::Skipped(_)) | Ok(MigrationOutcome::DryRun(_)) => {}
        Err(e) => {
            log_line(&format!(
                "session-start：迁移失败（不阻断热索引注入，下次会话重试）：{e}"
            ));
        }
    }
}
