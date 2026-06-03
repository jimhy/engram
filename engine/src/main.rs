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
//! 另有两个服务 Claude Code hook 的辅助子命令（把"路径解析约定"收进引擎，
//! 让 hook 侧近乎零逻辑，见 [`engram::commands::resolve_paths`]）：
//! - `session-start`：SessionStart hook 调用，按约定推导 general/project 库路径、
//!   确保父目录存在、打开两库合并后把热索引（含前言）打到 **stdout** 供注入；
//! - `resolve`：SessionEnd 脚本调用，按同一约定推导并 `create_dir_all` 两父目录，
//!   以 `env` 或 `json` 格式打印统一的 db 路径与项目名，供脚本再去调别的命令。
//!
//! 设计文档参考：§6 升降级、§7 降级去向、§13 交付形态（存储选定 redb、多库分置）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};

use engram::commands::{
    self, active_unchanged, build_merged_memory, decide_active, gc_should_delete, generate_id,
    home_dir, last_touch, list_visible, oneline_status, parse_level, parse_project_dbs,
    parse_status_filter, parse_tags, recall_candidates, resolve_paths, revived_level,
    tokenize_query, validate_merge_scope, MergeScope, ResolvedPaths,
};
use engram::consolidate::{consolidate, Transition, TransitionKind};
use engram::model::{Level, Memory, Pointer, Status};
use engram::render::{load_store_entries, render};
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
        /// 创建时间（unix 秒）；缺省取系统时间。
        #[arg(long)]
        now: Option<f64>,
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
        /// 当前时间（unix 秒）；缺省取系统时间。
        #[arg(long)]
        now: Option<f64>,
        /// 只报告将删除哪些条目，不真正删除。
        #[arg(long)]
        dry_run: bool,
    },
    /// hook 用：按约定推导库路径、确保父目录存在、打开两库合并后把热索引（含前言）打到 stdout。
    SessionStart {
        /// 项目根目录；缺省取当前工作目录。
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
    },
    /// hook 用：按约定推导并 create_dir_all 两父目录，以 env|json 打印统一的 db 路径与项目名。
    Resolve {
        /// 项目根目录；缺省取当前工作目录。
        #[arg(long)]
        project_dir: Option<PathBuf>,
        /// 公共库路径覆盖；缺省走 `<HOME>/.engram/general.redb` 约定。
        #[arg(long)]
        general_db: Option<PathBuf>,
        /// 输出格式：env（逐行 KEY=VALUE，缺省）或 json（单行对象）。
        #[arg(long, default_value = "env")]
        format: String,
    },
    /// hook 用：动态按需挂载「当前活跃子项目」的 L4——渲染 公共L1-3 + 根L4 +
    /// 活跃子项目L4，仅当活跃子项目相对上次**变化**时才输出（重注入）。
    HotIndex {
        /// 公共库路径覆盖；缺省走 `<HOME>/.engram/general.redb` 约定。
        #[arg(long)]
        general_db: Option<PathBuf>,
        /// 工作区根目录；缺省取 stdin 的 cwd，再缺省取当前工作目录。
        #[arg(long)]
        workspace_root: Option<PathBuf>,
        /// transcript 文件路径（用于「最近触碰」信号）；缺省取 stdin 的 transcript_path。
        #[arg(long)]
        transcript: Option<PathBuf>,
        /// 本次用户 prompt 文本（用于「显式提及」信号）；缺省取 stdin 的 prompt。
        #[arg(long)]
        prompt: Option<String>,
        /// 状态文件路径；给定时启用状态门控（活跃子项目未变则输出空、不重注入）。
        #[arg(long)]
        state: Option<PathBuf>,
        /// 状态栏小文件路径；给定时每次都把挂载集的一行状态串写入该文件（覆盖），
        /// 供状态栏只读该文件而不必开 redb。即便状态门控判定为「空、不注入」也照常写，
        /// 让状态栏始终最新。写失败静默忽略，不影响注入主流程。
        #[arg(long)]
        status_file: Option<PathBuf>,
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
        /// 当前时间（unix 秒）；缺省取系统时间。
        #[arg(long)]
        now: Option<f64>,
    },
    /// 概况：复用 hot-index 那套加载（公共库 + 工作区根 L4 + 各 --project-db），
    /// 把**传入的全部库**当作挂载集统计/展示（不做活跃子项目动态判定、不接 transcript）。
    Status {
        /// 公共库路径覆盖；缺省走 `<HOME>/.engram/general.redb` 约定。
        #[arg(long)]
        general_db: Option<PathBuf>,
        /// 工作区根目录；缺省取 stdin 的 cwd（仅 --from-hook-stdin 时），再缺省取当前工作目录。
        /// 其 `<root>/.claude/engram.redb` 作为根 L4 库一并计入挂载集。
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
fn resolve_now(now: Option<f64>) -> Option<f64> {
    match now {
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

/// 程序主逻辑。返回进程退出码。
fn main() -> ExitCode {
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
            now,
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
        Command::Gc {
            general_db,
            project_db,
            ttl_days,
            tombstone_ttl_days,
            now,
            dry_run,
        } => run_gc(GcArgs {
            general_db: &general_db,
            project_db: &project_db,
            ttl_days,
            tombstone_ttl_days,
            now,
            dry_run,
        }),
        Command::SessionStart {
            project_dir,
            general_db,
            now,
            emit,
            log,
        } => run_session_start(SessionStartArgs {
            project_dir: project_dir.as_deref(),
            general_db: general_db.as_deref(),
            now,
            emit: &emit,
            log: log.as_deref(),
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
            emit,
            hook_event,
            from_hook_stdin,
            log,
            now,
        } => run_hot_index(HotIndexArgs {
            general_db: general_db.as_deref(),
            workspace_root: workspace_root.as_deref(),
            transcript: transcript.as_deref(),
            prompt: prompt.as_deref(),
            state: state.as_deref(),
            status_file: status_file.as_deref(),
            emit: &emit,
            hook_event: &hook_event,
            from_hook_stdin,
            log: log.as_deref(),
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

/// 按约定解析 hook 辅助命令所需路径，并 `create_dir_all` 两个 db 的父目录。
///
/// 把 `session-start` 与 `resolve` 共用的"解析项目目录 → resolve_paths →
/// 建父目录"流程收成一处，保证两命令路径语义完全一致。
///
/// # Errors
/// 取当前目录失败、HOME 无法确定、或建父目录失败时，返回中文说明的错误字符串。
fn resolve_and_prepare(
    project_dir: Option<&Path>,
    general_override: Option<&Path>,
) -> Result<ResolvedPaths, String> {
    let project_dir = resolve_project_dir(project_dir)?;
    let resolved = resolve_paths(&project_dir, general_override, real_env_lookup)?;
    ensure_parent_dir(&resolved.general_db)?;
    ensure_parent_dir(&resolved.project_db)?;
    Ok(resolved)
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
/// 行格式：`<unix秒> session-start emit=<text|json> project=<name> general=<路径> project_db=<路径>\n`。
/// 父目录不存在时先 `create_dir_all`。
///
/// 调试日志是次要旁路——**任何错误都被静默吞掉、不上抛**：注入热索引才是主任务，
/// 不能因写日志失败而让命令失败。本函数因此返回 `()` 而非 `Result`。
///
/// # 参数
/// - `log_path`：日志文件路径（append 模式打开/创建）。
/// - `now`：当前时间（unix 秒），取整数秒写入。
/// - `emit`：本次的输出格式。
/// - `resolved`：已解析的库路径与项目名。
fn append_session_log(log_path: &Path, now: f64, emit: EmitFormat, resolved: &ResolvedPaths) {
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
        "{ts} session-start emit={emit_str} project={} general={} project_db={}\n",
        resolved.project_name,
        resolved.general_db.display(),
        resolved.project_db.display(),
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
    let resolved = match resolve_and_prepare(args.project_dir, args.general_db) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("session-start 失败：{e}");
            return ExitCode::FAILURE;
        }
    };

    // 打开公共库与当前项目库，各自读出后合并（项目库即作用域，挂载即包含）。
    let merged = match read_merged_two(&resolved) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("session-start 失败：{e}");
            return ExitCode::FAILURE;
        }
    };

    // 前言（一行）+ 热索引。前言提示按指针查 ground truth、勿凭印象。
    // 注意：text 分支须与历史输出逐字节一致（println! 前言 + print! render）。
    const PREAMBLE: &str = "「以下是你的 engram 长期记忆热索引。需要细节时按每条的指针去查 ground truth；不要凭印象。」";
    let rendered = render(&merged, now);

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
        append_session_log(log_path, now, emit, &resolved);
    }

    status
}

/// 打开 [`ResolvedPaths`] 的公共库 + 当前项目库，读出全部记忆合并为一个 `Vec`。
///
/// 项目记忆以 `project_name` 标注其归属（与 `--project-db name=path` 语义一致）。
///
/// # Errors
/// 任一库打开或读取失败时，返回带库类别说明的错误字符串。
fn read_merged_two(resolved: &ResolvedPaths) -> Result<Vec<Memory>, String> {
    let (_gdb, mut merged) = open_and_read(&resolved.general_db)
        .map_err(|e| format!("读取公共库 {} 失败：{e}", resolved.general_db.display()))?;
    let (_pdb, mut pmems) = open_and_read(&resolved.project_db).map_err(|e| {
        format!(
            "读取项目库 {}（{}）失败：{e}",
            resolved.project_name,
            resolved.project_db.display()
        )
    })?;
    merged.append(&mut pmems);
    Ok(merged)
}

/// 执行 `resolve` 子命令：按约定推导路径 → 建父目录 → 以 env|json 打印三项。
///
/// `--format env`（缺省）逐行打印 `ENGRAM_GENERAL_DB` / `ENGRAM_PROJECT_DB` /
/// `ENGRAM_PROJECT_NAME`；`--format json` 用 serde_json 打印单行对象。
/// 未知 format 值走 stderr + 非 0 退出。
fn run_resolve(project_dir: Option<&Path>, general_db: Option<&Path>, format: &str) -> ExitCode {
    let resolved = match resolve_and_prepare(project_dir, general_db) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("resolve 失败：{e}");
            return ExitCode::FAILURE;
        }
    };

    match format.trim().to_ascii_lowercase().as_str() {
        "env" => {
            println!("ENGRAM_GENERAL_DB={}", resolved.general_db.display());
            println!("ENGRAM_PROJECT_DB={}", resolved.project_db.display());
            println!("ENGRAM_PROJECT_NAME={}", resolved.project_name);
            ExitCode::SUCCESS
        }
        "json" => {
            // 用 serde_json 转义路径/名称，保证含特殊字符（反斜杠等）也合法。
            let obj = format!(
                r#"{{"general_db":{},"project_db":{},"project_name":{}}}"#,
                json_str(&resolved.general_db.to_string_lossy()),
                json_str(&resolved.project_db.to_string_lossy()),
                json_str(&resolved.project_name),
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

/// `hot-index` 命令的全部参数（聚成一个结构体，规避过多函数形参）。
struct HotIndexArgs<'a> {
    /// `--general-db` 覆盖；`None` 时走 `<HOME>/.engram/general.redb` 约定。
    general_db: Option<&'a Path>,
    /// `--workspace-root` 覆盖；`None` 时取 stdin 的 cwd，再缺省取当前目录。
    workspace_root: Option<&'a Path>,
    /// `--transcript`；`None` 时取 stdin 的 transcript_path。
    transcript: Option<&'a Path>,
    /// `--prompt`；`None` 时取 stdin 的 prompt。
    prompt: Option<&'a str>,
    /// `--state`；给定时启用状态门控。
    state: Option<&'a Path>,
    /// `--status-file`；给定时每次都把挂载集的一行状态串写入该文件（覆盖）。
    status_file: Option<&'a Path>,
    /// 输出格式：`text`（缺省）或 `json`。
    emit: &'a str,
    /// `--emit json` 时写入的 hookEventName。
    hook_event: &'a str,
    /// 是否从 stdin 读取整段 hook JSON 作兜底。
    from_hook_stdin: bool,
    /// 可选调试日志路径。
    log: Option<&'a Path>,
    /// 当前时间（unix 秒）；`None` 取系统时间。
    now: Option<f64>,
}

/// 从 stdin 解析出的 hook 兜底字段（任一缺失即为 `None`）。
#[derive(Debug, Default)]
struct HookStdin {
    /// hook 给的 transcript 文件路径。
    transcript_path: Option<PathBuf>,
    /// hook 给的当前工作目录。
    cwd: Option<PathBuf>,
    /// hook 给的本次 prompt 文本。
    prompt: Option<String>,
}

/// 读取整段 stdin、按 hook JSON 解析出 `transcript_path` / `cwd` / `prompt`。
///
/// stdin 为空、非法 JSON、或不是对象时一律返回 [`HookStdin::default`]（全 `None`）——
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
    let str_field = |key: &str| obj.get(key).and_then(|v| v.as_str()).map(|s| s.to_string());
    HookStdin {
        transcript_path: str_field("transcript_path").map(PathBuf::from),
        cwd: str_field("cwd").map(PathBuf::from),
        prompt: str_field("prompt"),
    }
}

/// 枚举 `root` 的**可挂载直接子目录名**（仅目录、仅名字）。读目录失败时返回空 `Vec`。
///
/// 用于活跃子项目判定的候选集与真实性校验：只有真实存在、且为「可挂载子项目」的
/// 子目录名才会成为候选。dotdir（`.claude`、`.git` 等）与基础设施目录
/// （`node_modules`、`target` 等）由 [`commands::is_mountable_subproject_name`]
/// 在此处一并过滤掉——它们虽是真实子目录，但绝不作为活跃子项目挂载。
fn list_subdirs(root: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                if commands::is_mountable_subproject_name(name) {
                    out.push(name.to_string());
                }
            }
        }
    }
    out
}

/// 取工作区根目录的「名字」（最后一段目录名）；取不到回退 `"root"`。
fn root_name_of(root: &Path) -> String {
    root.file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "root".to_string())
}

/// 读状态文件里上次记录的 active 名（去首尾空白）。文件不存在/读失败/为空 → `None`。
fn read_state(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 把本次 active 名写回状态文件（父目录不存在先创建）。失败时返回错误说明。
///
/// # Errors
/// 创建父目录或写文件失败时返回带路径说明的错误字符串。
fn write_state(path: &Path, active_name: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建状态目录 {} 失败：{e}", parent.display()))?;
        }
    }
    std::fs::write(path, active_name)
        .map_err(|e| format!("写状态文件 {} 失败：{e}", path.display()))
}

/// 向 `hot-index` 的调试日志**追加一行**（best-effort，失败静默忽略）。
///
/// 行格式：`<now> hot-index event=<hook_event> active=<active|none> root=<name> ws=<workspace_root>\n`。
fn append_hot_index_log(
    log_path: &Path,
    now: f64,
    hook_event: &str,
    active: Option<&str>,
    root_name: &str,
    workspace_root: &Path,
) {
    use std::io::Write as _;
    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() && std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let ts = now.max(0.0) as u64;
    let active_str = active.unwrap_or("none");
    let line = format!(
        "{ts} hot-index event={hook_event} active={active_str} root={root_name} ws={}\n",
        workspace_root.display()
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        let _ = f.write_all(line.as_bytes());
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

/// 读取「公共库 + 根 L4 库 + 活跃子项目 L4 库（若存在）」并合并为一个 `Vec`。
///
/// - 公共库：`general_db`（其全部记忆，含 L1-3 通用记忆）。
/// - 根 L4 库：`root_db`（工作区根项目自己的项目库，全部记忆）。
/// - 活跃子项目 L4 库：`<root>/<active>/.claude/engram.redb`，**仅当该文件存在**时加载。
///
/// # Errors
/// 任一应加载的库打开或读取失败时，返回带库类别说明的错误字符串。
fn read_hot_index_merged(
    general_db: &Path,
    root_db: &Path,
    active_db: Option<&Path>,
) -> Result<Vec<Memory>, String> {
    let (_gdb, mut merged) = open_and_read(general_db)
        .map_err(|e| format!("读取公共库 {} 失败：{e}", general_db.display()))?;
    let (_rdb, mut rmems) = open_and_read(root_db)
        .map_err(|e| format!("读取根项目库 {} 失败：{e}", root_db.display()))?;
    merged.append(&mut rmems);
    if let Some(path) = active_db {
        let (_adb, mut amems) = open_and_read(path)
            .map_err(|e| format!("读取活跃子项目库 {} 失败：{e}", path.display()))?;
        merged.append(&mut amems);
    }
    Ok(merged)
}

/// 执行 `hot-index` 子命令：动态按需挂载「当前活跃子项目」的 L4。
///
/// 流程（详见命令文档）：解析 general/workspace_root（含 stdin 兜底）→ 打开根项目库
/// → 判定活跃子项目（prompt 信号优先、否则 transcript 信号；须为真实子目录且非根）
/// → 合并 公共L1-3 + 根L4 + 活跃子项目L4（活跃库文件存在才加载）→ 状态门控（仅
/// `--state` 给出时：与上次 active 相同则输出空、exit 0；否则写回状态继续）→ 按
/// `--emit` 渲染输出（前言 + 热索引；门控判空时不打印任何东西）→ 可选追加日志。
///
/// 任何 IO/库错误走 stderr + 非 0 退出，不 panic。stdin 解析失败静默当作无。
fn run_hot_index(args: HotIndexArgs<'_>) -> ExitCode {
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

    // 1. stdin 兜底（仅当 --from-hook-stdin）。
    let hook = if args.from_hook_stdin {
        read_hook_stdin()
    } else {
        HookStdin::default()
    };

    // 2. general_db：--general-db，否则 <HOME>/.engram/general.redb。
    let general_db: PathBuf = match args.general_db {
        Some(p) => p.to_path_buf(),
        None => match home_dir(real_env_lookup) {
            Ok(home) => home.join(".engram").join("general.redb"),
            Err(e) => {
                eprintln!("hot-index 失败：{e}");
                return ExitCode::FAILURE;
            }
        },
    };

    // 3. workspace_root：--workspace-root，否则 stdin.cwd，否则 current_dir()。
    let workspace_root: PathBuf = match args.workspace_root {
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

    // transcript / prompt：显式优先，否则 stdin 兜底。
    let transcript_path: Option<PathBuf> = args
        .transcript
        .map(|p| p.to_path_buf())
        .or_else(|| hook.transcript_path.clone());
    let prompt: Option<String> = args
        .prompt
        .map(|s| s.to_string())
        .or_else(|| hook.prompt.clone());

    // 4. 根项目库：<workspace_root>/.claude/engram.redb；建父目录后 open（不存在即建）。
    let root_db = workspace_root.join(".claude").join("engram.redb");
    if let Err(e) = ensure_parent_dir(&root_db) {
        eprintln!("hot-index 失败：{e}");
        return ExitCode::FAILURE;
    }
    let root_name = root_name_of(&workspace_root);

    // 5. 判定活跃子项目（active）。
    let subdirs = list_subdirs(&workspace_root);
    let ws_str = workspace_root.to_string_lossy();
    let transcript_text: Option<String> = transcript_path
        .as_deref()
        .and_then(|p| std::fs::read_to_string(p).ok());
    let mut active = decide_active(
        prompt.as_deref(),
        transcript_text.as_deref(),
        ws_str.as_ref(),
        &subdirs,
    );
    // 校验：active 必须是真实存在的子目录；active==根 name 视为 None。
    if let Some(name) = &active {
        let sub_path = workspace_root.join(name);
        if !sub_path.is_dir() || name == &root_name {
            active = None;
        }
    }

    // 6. 活跃子项目 L4 库路径（仅当文件存在才加载）。
    let active_db: Option<PathBuf> = active.as_ref().and_then(|name| {
        let p = workspace_root
            .join(name)
            .join(".claude")
            .join("engram.redb");
        if p.is_file() {
            Some(p)
        } else {
            None
        }
    });

    // 7. 合并 公共L1-3 + 根L4 + 活跃子项目L4（在状态门控之前先读出，因为状态栏小文件
    //    无论是否门控判空都要写最新的一行状态串，需要先有挂载集）。
    let merged = match read_hot_index_merged(&general_db, &root_db, active_db.as_deref()) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("hot-index 失败：{e}");
            return ExitCode::FAILURE;
        }
    };

    // 8. 状态栏小文件（best-effort）：把挂载集的一行状态串写入 --status-file（覆盖）。
    //    无论后面状态门控是否判空、是否注入，都先把状态栏文件刷成最新；写失败静默忽略。
    if let Some(status_path) = args.status_file {
        write_status_file_silent(status_path, &oneline_status(&merged, now));
    }

    // 9. 状态门控（仅当 --state 给出）：与上次 active 相同则输出空、exit 0。
    if let Some(state_path) = args.state {
        let last = read_state(state_path);
        let unchanged = active_unchanged(active.as_deref(), last.as_deref(), &root_name);
        if unchanged {
            // 不打印任何东西（让 hook 不注入）；状态栏文件已在上一步写过、仍记日志（best-effort）。
            if let Some(log_path) = args.log {
                append_hot_index_log(
                    log_path,
                    now,
                    args.hook_event,
                    active.as_deref(),
                    &root_name,
                    &workspace_root,
                );
            }
            return ExitCode::SUCCESS;
        }
        // 有变化：把本次 active 名（None 记为根 name）写回状态文件后继续渲染。
        let to_write = active.as_deref().unwrap_or(&root_name);
        if let Err(e) = write_state(state_path, to_write) {
            eprintln!("hot-index 失败：{e}");
            return ExitCode::FAILURE;
        }
    }

    // 10. 渲染。
    const PREAMBLE: &str = "「以下是你的 engram 长期记忆热索引。需要细节时按每条的指针去查 ground truth；不要凭印象。」";
    let rendered = render(&merged, now);

    // 11. 输出。
    let status = match emit {
        EmitFormat::Text => {
            println!("{PREAMBLE}");
            print!("{rendered}");
            ExitCode::SUCCESS
        }
        EmitFormat::Json => {
            let context = format!("{PREAMBLE}\n{rendered}");
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

    // 12. 调试日志（best-effort）。
    if let Some(log_path) = args.log {
        append_hot_index_log(
            log_path,
            now,
            args.hook_event,
            active.as_deref(),
            &root_name,
            &workspace_root,
        );
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

/// `status` 命令的全部参数（聚成一个结构体，规避过多函数形参）。
struct StatusArgs<'a> {
    /// `--general-db` 覆盖；`None` 时走 `<HOME>/.engram/general.redb` 约定。
    general_db: Option<&'a Path>,
    /// `--workspace-root` 覆盖；`None` 时取 stdin 的 cwd（仅 from_hook_stdin），再缺省取当前目录。
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
/// - `memories`：挂载集（公共库 + 工作区根 L4 + 各 --project-db 的合并集）。
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

/// 执行 `status` 子命令：复用 hot-index 那套加载（公共库 + 工作区根 L4 + 各
/// `--project-db`），把**传入的全部库**当作挂载集统计/展示。
///
/// 注意：本命令只做统计/展示，**不做 active 子项目动态判定、不接 transcript**——
/// status 给的是「全貌」，把传入的所有库都算上即可。
///
/// `--format oneline`：打印 [`oneline_status`]（一行、不带多余空行）。
/// `--format full`（缺省）：打印 [`render_status_full`] 的可读多行概况。
///
/// 任何 IO/库错误走 stderr + 非 0 退出，不 panic。
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

    // stdin 兜底（仅 --from-hook-stdin）：只用其 cwd 作 workspace-root 兜底。
    let hook = if args.from_hook_stdin {
        read_hook_stdin()
    } else {
        HookStdin::default()
    };

    // general_db：--general-db，否则 <HOME>/.engram/general.redb。
    let general_db: PathBuf = match args.general_db {
        Some(p) => p.to_path_buf(),
        None => match home_dir(real_env_lookup) {
            Ok(home) => home.join(".engram").join("general.redb"),
            Err(e) => {
                eprintln!("status 失败：{e}");
                return ExitCode::FAILURE;
            }
        },
    };

    // workspace_root：--workspace-root，否则 stdin.cwd，否则 current_dir()。
    let workspace_root: PathBuf = match args.workspace_root {
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

    // 挂载集：公共库（全部）+ 工作区根 L4 库（若文件存在）+ 各 --project-db（若文件存在）。
    // 与 hot-index 一致，但不做活跃子项目判定——传入的库即全貌。缺失的库静默跳过
    // （状态命令是只读概况，库尚未创建时算作 0 条，不应报错）。
    let mut merged = match open_and_read(&general_db) {
        Ok((_db, mems)) => mems,
        Err(e) => {
            eprintln!("status 失败：读取公共库 {} 失败：{e}", general_db.display());
            return ExitCode::FAILURE;
        }
    };

    let root_db = workspace_root.join(".claude").join("engram.redb");
    if root_db.is_file() {
        match open_and_read(&root_db) {
            Ok((_db, mut mems)) => merged.append(&mut mems),
            Err(e) => {
                eprintln!("status 失败：读取根项目库 {} 失败：{e}", root_db.display());
                return ExitCode::FAILURE;
            }
        }
    }

    for (name, path) in &project_dbs {
        if !path.is_file() {
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

    match format {
        StatusFormat::Oneline => {
            // 一行串，println! 自带一个换行（无多余空行）。
            println!("{}", oneline_status(&merged, now));
        }
        StatusFormat::Full => {
            // render_status_full 末尾已自带换行，用 print! 避免多一个空行。
            print!("{}", render_status_full(&merged, now));
        }
    }
    ExitCode::SUCCESS
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
    now: Option<f64>,
}

/// 执行 `write` 子命令：校验 → 构造记忆 → 按层级路由写入正确的库 → 打印新 id。
///
/// 校验：importance ∈ [0,1]；L4.x 必须给 `--project NAME` 且 NAME 在项目库映射中；
/// L1-3 不得给 `--project`。任一校验失败即非 0 退出且不写入。
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
    };

    let db = match store::open(&target) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("打开数据库 {} 失败：{e}", target.display());
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = store::put(&db, &memory) {
        eprintln!("写入记忆失败：{e}");
        return ExitCode::FAILURE;
    }

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

    // 打开公共库并读出。
    let (gdb, gmems) = match open_and_read(general_db) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("读取公共库 {} 失败：{e}", general_db.display());
            return ExitCode::FAILURE;
        }
    };

    // 打开所有项目库并读出（保留各 Database 句柄用于写回）。
    let mut merged = gmems;
    let mut pdbs: BTreeMap<String, Database> = BTreeMap::new();
    for (name, path) in &project_dbs {
        match open_and_read(path) {
            Ok((db, mut pmems)) => {
                merged.append(&mut pmems);
                pdbs.insert(name.clone(), db);
            }
            Err(e) => {
                eprintln!("读取项目库 {name}（{}）失败：{e}", path.display());
                return ExitCode::FAILURE;
            }
        }
    }

    // 记录变更前的 (level, status)，用于只写回真正变化的记忆。
    let before: Vec<(Level, Status)> = merged.iter().map(|m| (m.level, m.status)).collect();

    // 跑状态机（纯算法，原地修改）。
    let transitions = consolidate(&mut merged, now);

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

    if dry_run {
        return ExitCode::SUCCESS;
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
        if let Err(e) = store::put_many(&gdb, &general_changed) {
            eprintln!("写回公共库失败：{e}");
            return ExitCode::FAILURE;
        }
    }

    // 各项目库分别单事务写回其变更集。
    for (name, changed) in &project_changed {
        let Some(db) = pdbs.get(name) else {
            // 不可达：project_changed 的键来自 pdbs.contains_key 过滤。
            eprintln!("内部错误：项目 {name} 无对应库句柄");
            return ExitCode::FAILURE;
        };
        if let Err(e) = store::put_many(db, changed) {
            eprintln!("写回项目库 {name} 失败：{e}");
            return ExitCode::FAILURE;
        }
    }

    // 无法路由的 L4 变更：记 stderr 提示（不致命）。
    if unrouted_l4 > 0 {
        eprintln!("警告：{unrouted_l4} 条 L4 记忆的所属项目不在 --project-db 映射中，已跳过其写回");
    }

    ExitCode::SUCCESS
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
        let m = entry.memory;
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

    println!("merge: 新记忆 {new_id} 合并了 {merged_count} 条源（源已转 Tombstone）");
    ExitCode::SUCCESS
}

/// `gc` 命令的全部参数（聚成一个结构体，规避过多函数形参）。
struct GcArgs<'a> {
    general_db: &'a Path,
    project_db: &'a [String],
    ttl_days: f64,
    tombstone_ttl_days: f64,
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
        "== gc (now={now}, ttl={} 天, tombstone_ttl={} 天){} ==",
        args.ttl_days,
        args.tombstone_ttl_days,
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
        if !gc_should_delete(m, now, args.ttl_days, args.tombstone_ttl_days) {
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
