//! 持久化存储层（redb 嵌入式 ACID 单文件库）。
//!
//! 本模块是 Engram 的 IO 边界：把记忆条目 [`crate::model::Memory`] 持久化到
//! 单个 redb 数据库文件。general 记忆与各 project 记忆**同库**存放，靠
//! `Memory.project` 字段在逻辑上区分作用域（见设计文档 §13）。
//!
//! 表结构：单张表 `memories`，key = `Memory.id`（字符串），
//! value = `serde_json::to_vec(&memory)`（JSON 字节）。选 JSON 字节而非
//! 自定义二进制编码，是为了与前两切片的 JSON 序列化语义保持完全一致，
//! 迁移时行为零偏差。
//!
//! 所有函数都不做 `unwrap`/`expect`/`panic`，错误统一包进 [`StoreError`]
//! 向上传播。

use std::fmt;
use std::path::Path;
use std::time::Duration;

use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};

use crate::model::Memory;

/// 记忆表定义：key = `Memory.id`，value = 记忆的 JSON 字节序列。
const MEMORIES: TableDefinition<&str, &[u8]> = TableDefinition::new("memories");

/// 打开数据库时，遇到「锁被占用」最多重试的总尝试次数（含首次）。
///
/// 与 [`OPEN_RETRY_DELAY_MS`] 共同决定重试预算：60 × 50ms ≈ 3 秒。
const OPEN_MAX_ATTEMPTS: u32 = 60;

/// 打开数据库每次重试之间的退避间隔（毫秒）。
const OPEN_RETRY_DELAY_MS: u64 = 50;

/// 存储层错误。
///
/// 统一包裹底层 redb 各类错误与 `serde_json` 序列化/反序列化错误，
/// 实现 [`std::fmt::Display`] 与 [`std::error::Error`]，便于 `?` 向上传播。
///
/// 各底层错误一律 `Box` 装箱：redb 的错误类型体积较大（个别达 160+ 字节），
/// 不装箱会让整个 `StoreError` 以及返回它的 `Result` 都变得很大（触发 clippy
/// 的 `result_large_err`）。装箱后 `StoreError` 仅一个指针宽，错误路径也几乎
/// 不占栈——错误本就是冷路径，多一次堆分配可接受。
#[derive(Debug)]
pub enum StoreError {
    /// 打开 / 创建数据库失败。
    Database(Box<redb::DatabaseError>),
    /// 开启读写事务失败。
    Transaction(Box<redb::TransactionError>),
    /// 打开表失败。
    Table(Box<redb::TableError>),
    /// 表读写（insert/get/iter）过程中的存储错误。
    Storage(Box<redb::StorageError>),
    /// 提交事务失败。
    Commit(Box<redb::CommitError>),
    /// JSON 序列化 / 反序列化失败。
    Serde(Box<serde_json::Error>),
    /// 准备数据库父目录时的文件系统错误（`create_dir_all` 失败，如无权限）。
    Io(Box<std::io::Error>),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Database(e) => write!(f, "打开数据库失败：{e}"),
            StoreError::Transaction(e) => write!(f, "开启事务失败：{e}"),
            StoreError::Table(e) => write!(f, "打开表失败：{e}"),
            StoreError::Storage(e) => write!(f, "表读写失败：{e}"),
            StoreError::Commit(e) => write!(f, "提交事务失败：{e}"),
            StoreError::Serde(e) => write!(f, "JSON 序列化/反序列化失败：{e}"),
            StoreError::Io(e) => write!(f, "准备数据库目录失败：{e}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // 解引用 Box 取内层错误作为 source。
        match self {
            StoreError::Database(e) => Some(e.as_ref()),
            StoreError::Transaction(e) => Some(e.as_ref()),
            StoreError::Table(e) => Some(e.as_ref()),
            StoreError::Storage(e) => Some(e.as_ref()),
            StoreError::Commit(e) => Some(e.as_ref()),
            StoreError::Serde(e) => Some(e.as_ref()),
            StoreError::Io(e) => Some(e.as_ref()),
        }
    }
}

impl From<redb::DatabaseError> for StoreError {
    fn from(e: redb::DatabaseError) -> Self {
        StoreError::Database(Box::new(e))
    }
}

impl From<redb::TransactionError> for StoreError {
    fn from(e: redb::TransactionError) -> Self {
        StoreError::Transaction(Box::new(e))
    }
}

impl From<redb::TableError> for StoreError {
    fn from(e: redb::TableError) -> Self {
        StoreError::Table(Box::new(e))
    }
}

impl From<redb::StorageError> for StoreError {
    fn from(e: redb::StorageError) -> Self {
        StoreError::Storage(Box::new(e))
    }
}

impl From<redb::CommitError> for StoreError {
    fn from(e: redb::CommitError) -> Self {
        StoreError::Commit(Box::new(e))
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        StoreError::Serde(Box::new(e))
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(Box::new(e))
    }
}

impl StoreError {
    /// 判断本错误是否为 redb 的「数据库已被占用，无法获取文件锁」这一类锁竞争错误。
    ///
    /// redb 在底层把各平台的锁竞争（Unix `flock` 的 `EWOULDBLOCK`、Windows
    /// `LockFile` 的 `ERROR_IO_PENDING` / `ERROR_LOCK_VIOLATION`）统一归一为
    /// [`redb::DatabaseError::DatabaseAlreadyOpen`] 这一个变体；其它任何 io 错误
    /// （无权限、父目录不存在、磁盘错误等）都不会落到该变体。因此这里**精确匹配**
    /// 该变体即可，只有它才值得重试退避，其余错误一律立即返回、不重试。
    fn is_lock_contention(&self) -> bool {
        matches!(
            self,
            StoreError::Database(e) if matches!(e.as_ref(), redb::DatabaseError::DatabaseAlreadyOpen)
        )
    }
}

/// 单次尝试打开（不存在则创建）指定路径的 redb 数据库，不做任何重试。
///
/// 用 [`Database::create`]：文件已存在则打开，不存在则新建。这是 [`open`]
/// 的「一次尝试」内核，单独抽出以便重试逻辑复用与测试。
///
/// # 参数
/// - `path`：数据库文件路径。
///
/// # Errors
/// - [`StoreError::Io`] —— 父目录不存在且 `create_dir_all` 失败（如无权限）。
/// - [`StoreError::Database`] —— 路径无法创建/打开（文件已被其它进程占用、
///   或文件格式损坏等）。其中「已被占用」会被
///   [`StoreError::is_lock_contention`] 识别，供 [`open`] 据此重试。
fn open_once(path: &Path) -> Result<Database, StoreError> {
    // redb 只建文件、不建目录：父目录缺失时 `Database::create` 会以「系统找不到
    // 指定的路径」失败。全新安装时 `~/.engram/`（及项目库的 `<项目>/.engram/`）
    // 尚未建立，若不先补建，任何只读命令（list/status/recall/render）都会在打开
    // 公共库这一步直接报错退出——新用户会误以为插件坏了。这里先确保父目录存在，
    // 让空库就地建出、显示「0 条」而非红色报错。`create_dir_all` 对已存在目录是
    // no-op，对热路径无额外代价；任何 io 失败（无权限等）统一归 [`StoreError::Io`]。
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let db = Database::create(path)?;
    Ok(db)
}

/// 重试退避的通用内核：反复调用尝试闭包 `f`，仅当其错误满足 `is_retryable`
/// 时退避后再试，最多 `attempts` 次（含首次）。
///
/// 抽成接受闭包的纯函数，是为了让重试逻辑能脱离真实文件锁单测——测试中传入
/// 「前 K 次返回锁错、之后成功」的闭包即可验证次数与最终结果，无需多进程、
/// 也不依赖真实计时。
///
/// 行为约定：
/// - `f` 成功（`Ok`）→ 立即返回该成功值。
/// - `f` 失败但 `is_retryable(&e)` 为真，且还有剩余尝试次数 → `sleep(delay)`
///   后再试。
/// - `f` 失败且 `is_retryable(&e)` 为假 → 立即返回该错误，**不重试**。
/// - 最后一次尝试（第 `attempts` 次）即便错误可重试，也不再 sleep，直接返回该错误。
///
/// `attempts` 为 0 时视作 1（至少尝试一次），避免「零次尝试却要返回错误」的
/// 退化情形。
///
/// # 参数
/// - `attempts`：最大尝试次数（含首次）。
/// - `delay`：两次尝试之间的退避间隔。
/// - `is_retryable`：判断某次失败是否值得重试的谓词。
/// - `f`：单次尝试闭包。
///
/// # Errors
/// 透传最后一次 `f` 的错误（无论是不可重试的错误，还是重试用尽后仍未成功的
/// 锁竞争错误）。
fn retry_acquire<T, E, R, F>(
    attempts: u32,
    delay: Duration,
    is_retryable: R,
    mut f: F,
) -> Result<T, E>
where
    R: Fn(&E) -> bool,
    F: FnMut() -> Result<T, E>,
{
    let total = attempts.max(1);
    let mut last_err = None;
    for attempt in 1..=total {
        match f() {
            Ok(value) => return Ok(value),
            Err(e) => {
                let retryable = is_retryable(&e);
                last_err = Some(e);
                if !retryable {
                    break;
                }
                if attempt < total {
                    std::thread::sleep(delay);
                }
            }
        }
    }
    // 循环至少执行一次，故 last_err 必为 Some；用 match 兜底以彻底避开 unwrap。
    match last_err {
        Some(e) => Err(e),
        // 不可达：total >= 1 保证至少跑一次 f 并写入 last_err。
        None => unreachable!("retry_acquire 至少尝试一次，last_err 必被赋值"),
    }
}

/// 打开（不存在则创建）指定路径的 redb 数据库，对「锁竞争」自动重试退避。
///
/// # 为什么需要重试
/// redb 在打开数据库时对底层文件加**进程级排他锁**：同一时刻只允许一个进程
/// 持有该文件。Engram 的各 hook（SessionStart / UserPromptSubmit / SessionEnd
/// 复盘者等）是一批**短命、相互独立的进程**，它们会并发打开同一个库（尤其
/// `~/.engram/general.redb`）。若不处理，后到的进程会立即收到
/// [`redb::DatabaseError::DatabaseAlreadyOpen`]（「Database already open. Cannot
/// acquire lock.」）而失败。
///
/// 每个 hook 真正持锁的时间窗口极短（开库 → 几次读写 → 关库即释放），因此这里
/// 采用「重试退避」把并发打开**串行化**：撞锁就退避一小段再试，类比 SQLite 的
/// `busy_timeout`。OS 文件锁在进程退出时由内核自动释放，进程不会饿死，也**不会
/// 死锁**（持锁方只会越来越快地释放，不存在循环等待）。
///
/// 重试预算由 `OPEN_MAX_ATTEMPTS` × `OPEN_RETRY_DELAY_MS` 决定（约 3 秒）。
/// 仅对锁竞争错误重试；其它错误（无权限、父目录不存在、文件损坏等）立即返回。
/// 预算耗尽仍撞锁，则返回该锁错误，由调用方 log 并优雅失败（不 panic）。
///
/// # 参数
/// - `path`：数据库文件路径。
///
/// # Errors
/// - [`StoreError::Database`] —— 路径无法创建/打开。其中
///   [`redb::DatabaseError::DatabaseAlreadyOpen`]（锁竞争）会先经过重试退避，
///   重试用尽仍失败才返回；其余原因（无权限、父目录不存在、文件格式损坏等）
///   不重试、立即返回。
pub fn open(path: &Path) -> Result<Database, StoreError> {
    retry_acquire(
        OPEN_MAX_ATTEMPTS,
        Duration::from_millis(OPEN_RETRY_DELAY_MS),
        StoreError::is_lock_contention,
        || open_once(path),
    )
}

/// 把单条记忆写入数据库（写事务 insert + commit）。
///
/// 以 `m.id` 为 key、`serde_json::to_vec(m)` 为 value upsert；同 id 覆盖。
///
/// # 参数
/// - `db`：已打开的数据库。
/// - `m`：待写入的记忆。
///
/// # Errors
/// - [`StoreError::Serde`] —— 记忆序列化为 JSON 失败。
/// - [`StoreError::Transaction`] —— 开启写事务失败。
/// - [`StoreError::Table`] —— 打开 `memories` 表失败。
/// - [`StoreError::Storage`] —— insert 过程中的底层存储错误。
/// - [`StoreError::Commit`] —— 提交事务失败。
pub fn put(db: &Database, m: &Memory) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec(m)?;
    let write_txn = db.begin_write()?;
    {
        let mut table = write_txn.open_table(MEMORIES)?;
        table.insert(m.id.as_str(), bytes.as_slice())?;
    }
    write_txn.commit()?;
    Ok(())
}

/// 在**单个事务**内批量写入多条记忆（批量 upsert）。
///
/// 相比逐条 [`put`] 各自开事务，本函数把全部写入合并进一个写事务再一次性
/// commit，既快又保证原子性（要么全部落盘，要么因错误整体回滚）。
///
/// # 参数
/// - `db`：已打开的数据库。
/// - `ms`：待写入的记忆切片。空切片是合法输入，提交一个空事务。
///
/// # Errors
/// - [`StoreError::Serde`] —— 任一记忆序列化为 JSON 失败。
/// - [`StoreError::Transaction`] —— 开启写事务失败。
/// - [`StoreError::Table`] —— 打开 `memories` 表失败。
/// - [`StoreError::Storage`] —— insert 过程中的底层存储错误。
/// - [`StoreError::Commit`] —— 提交事务失败。
pub fn put_many(db: &Database, ms: &[Memory]) -> Result<(), StoreError> {
    let write_txn = db.begin_write()?;
    {
        let mut table = write_txn.open_table(MEMORIES)?;
        for m in ms {
            let bytes = serde_json::to_vec(m)?;
            table.insert(m.id.as_str(), bytes.as_slice())?;
        }
    }
    write_txn.commit()?;
    Ok(())
}

/// 按 id 读取单条记忆；不存在返回 `None`。
///
/// # 参数
/// - `db`：已打开的数据库。
/// - `id`：记忆 id。
///
/// # Errors
/// - [`StoreError::Transaction`] —— 开启读事务失败。
/// - [`StoreError::Table`] —— 打开 `memories` 表失败（表不存在时也归此类，
///   返回 `None` 而非错误，见下）。
/// - [`StoreError::Storage`] —— get 过程中的底层存储错误。
/// - [`StoreError::Serde`] —— 取出的字节反序列化为 [`Memory`] 失败。
pub fn get(db: &Database, id: &str) -> Result<Option<Memory>, StoreError> {
    let read_txn = db.begin_read()?;
    // 表尚不存在（全新库从未写入过）时，视作“无此记忆”返回 None，而非报错。
    let table = match read_txn.open_table(MEMORIES) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(StoreError::Table(Box::new(e))),
    };
    match table.get(id)? {
        Some(guard) => {
            let m: Memory = serde_json::from_slice(guard.value())?;
            Ok(Some(m))
        }
        None => Ok(None),
    }
}

/// 读取全表所有记忆（读事务遍历 + 反序列化）。
///
/// 遍历过程中，**单条反序列化失败的条目会被跳过并把错误写到 stderr**，
/// 不会中断整体加载、也不会 panic——与前两切片 JSON 目录扫描跳过坏文件的
/// 容错语义保持一致。表尚不存在（全新库）时返回空 `Vec`。
///
/// # 参数
/// - `db`：已打开的数据库。
///
/// # Errors
/// - [`StoreError::Transaction`] —— 开启读事务失败。
/// - [`StoreError::Table`] —— 打开 `memories` 表失败（表不存在除外，返回空）。
/// - [`StoreError::Storage`] —— 遍历过程中的底层存储错误。
pub fn all(db: &Database) -> Result<Vec<Memory>, StoreError> {
    let read_txn = db.begin_read()?;
    // 全新库尚无 memories 表时，视作空集返回。
    let table = match read_txn.open_table(MEMORIES) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(StoreError::Table(Box::new(e))),
    };
    let mut out = Vec::with_capacity(table.len()? as usize);
    for item in table.iter()? {
        let (key, value) = item?;
        match serde_json::from_slice::<Memory>(value.value()) {
            Ok(m) => out.push(m),
            Err(e) => {
                // 跳过反序列化失败项并记 stderr，不中断整体加载。
                eprintln!("跳过反序列化失败的记忆 id={}：{e}", key.value());
            }
        }
    }
    Ok(out)
}

/// 按 id 删除单条记忆（写事务 delete + commit），返回该 id 删除前是否存在。
///
/// 语义与 [`std::collections::BTreeMap::remove`] 类似：删除存在的 key 返回
/// `Ok(true)`；删除不存在的 key 返回 `Ok(false)`（不视作错误）。表尚不存在
/// （全新库从未写入过）时同样返回 `Ok(false)`，不报错。
///
/// 本函数供 `gc` 命令做 TTL 硬删除（§7.4）使用。
///
/// # 参数
/// - `db`：已打开的数据库。
/// - `id`：待删除记忆的 id。
///
/// # Errors
/// - [`StoreError::Transaction`] —— 开启写事务失败。
/// - [`StoreError::Table`] —— 打开 `memories` 表失败（表不存在除外，返回 `Ok(false)`）。
/// - [`StoreError::Storage`] —— remove 过程中的底层存储错误。
/// - [`StoreError::Commit`] —— 提交事务失败。
pub fn remove(db: &Database, id: &str) -> Result<bool, StoreError> {
    let write_txn = db.begin_write()?;
    // 表尚不存在时，视作“无此记忆”：write_txn 在函数返回时自动丢弃（未提交即回滚），
    // 无需也无法在 table 借用未结束时手动 drop 它，直接返回 false。
    let mut table = match write_txn.open_table(MEMORIES) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(StoreError::Table(Box::new(e))),
    };
    // remove 返回被删值的 guard（Some 表示曾存在）。`?` 取值后该临时 guard 即丢弃，
    // 故 `existed` 是个纯 bool，不再借用 table。
    let existed = table.remove(id)?.is_some();
    // 显式结束对 write_txn 的可变借用，方可 commit。
    drop(table);
    write_txn.commit()?;
    Ok(existed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Level, Pointer, Status};
    use std::path::PathBuf;

    /// 在系统临时目录下构造一个进程内唯一的 db 文件路径（不引入 tempfile 依赖）。
    ///
    /// 用「测试名 + 进程 id + 纳秒时间戳」拼出唯一文件名，避免并行测试互相踩。
    fn unique_db_path(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        dir.push(format!("engram_store_{tag}_{pid}_{nanos}.redb"));
        dir
    }

    /// 测试结束后清理 db 文件（忽略清理失败）。
    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    /// 构造一条用于测试的记忆。
    fn make(id: &str, level: Level, project: Option<&str>) -> Memory {
        Memory {
            id: id.to_string(),
            cue: format!("cue-{id}"),
            pointer: Pointer {
                kind: "none".to_string(),
                reference: None,
                detail: None,
            },
            level,
            project: project.map(|s| s.to_string()),
            importance: 0.5,
            pinned: false,
            access_log: vec![1_000_000_000.0],
            status: Status::Active,
            superseded_by: None,
            created_at: 1_000_000_000.0,
            tags: vec!["intent".to_string()],
        }
    }

    // 1. put → get 往返一致：写入后按 id 取回应与原记忆完全相等。
    #[test]
    fn put_get_roundtrip() {
        let path = unique_db_path("roundtrip");
        let db = open(&path).expect("应能创建数据库");
        let m = make("mem_a", Level::L1, None);
        put(&db, &m).expect("put 应成功");

        let got = get(&db, "mem_a").expect("get 应成功");
        assert_eq!(got, Some(m), "取回的记忆应与写入的完全一致");

        drop(db);
        cleanup(&path);
    }

    // 2. all 遍历条数正确：put_many 三条后 all 应恰好返回三条，id 集合一致。
    #[test]
    fn all_returns_all_entries() {
        let path = unique_db_path("all_count");
        let db = open(&path).expect("应能创建数据库");
        let mems = vec![
            make("m1", Level::L1, None),
            make("m2", Level::L3, None),
            make("m3", Level::L4_1, Some("alpha")),
        ];
        put_many(&db, &mems).expect("put_many 应成功");

        let all_mems = all(&db).expect("all 应成功");
        assert_eq!(all_mems.len(), 3, "应恰好读出 3 条");
        let mut ids: Vec<String> = all_mems.iter().map(|m| m.id.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["m1", "m2", "m3"], "id 集合应一致");

        drop(db);
        cleanup(&path);
    }

    // 3. get 不存在的 id 返回 None。
    #[test]
    fn get_missing_returns_none() {
        let path = unique_db_path("missing");
        let db = open(&path).expect("应能创建数据库");
        put(&db, &make("exists", Level::L2, None)).expect("put 应成功");

        let got = get(&db, "does_not_exist").expect("get 应成功");
        assert_eq!(got, None, "不存在的 id 应返回 None");

        drop(db);
        cleanup(&path);
    }

    // 4. 全新库（从未写入）：get 返回 None、all 返回空，均不报错。
    #[test]
    fn fresh_db_is_empty() {
        let path = unique_db_path("fresh");
        let db = open(&path).expect("应能创建数据库");

        assert_eq!(get(&db, "anything").expect("get 应成功"), None);
        assert!(all(&db).expect("all 应成功").is_empty(), "全新库应为空");

        drop(db);
        cleanup(&path);
    }

    // 5. put 同 id 覆盖：再次 put 同 id 应更新而非新增。
    #[test]
    fn put_same_id_overwrites() {
        let path = unique_db_path("overwrite");
        let db = open(&path).expect("应能创建数据库");
        let mut m = make("dup", Level::L3, None);
        put(&db, &m).expect("首次 put 应成功");
        // 改 level 后再写同 id。
        m.level = Level::L1;
        put(&db, &m).expect("覆盖 put 应成功");

        let all_mems = all(&db).expect("all 应成功");
        assert_eq!(all_mems.len(), 1, "同 id 覆盖后仍应只有 1 条");
        assert_eq!(all_mems[0].level, Level::L1, "应为覆盖后的新 level");

        drop(db);
        cleanup(&path);
    }

    // 6. remove 删除存在的记忆：返回 Ok(true)，删后 get 为 None。
    #[test]
    fn remove_existing_returns_true_and_gone() {
        let path = unique_db_path("remove_existing");
        let db = open(&path).expect("应能创建数据库");
        put(&db, &make("to_del", Level::L3, None)).expect("put 应成功");

        let existed = remove(&db, "to_del").expect("remove 应成功");
        assert!(existed, "删除存在的 id 应返回 true");
        assert_eq!(
            get(&db, "to_del").expect("get 应成功"),
            None,
            "删除后 get 应为 None"
        );

        drop(db);
        cleanup(&path);
    }

    // 7. remove 删除不存在的记忆：返回 Ok(false)，不报错。
    #[test]
    fn remove_missing_returns_false() {
        let path = unique_db_path("remove_missing");
        let db = open(&path).expect("应能创建数据库");
        put(&db, &make("keep", Level::L2, None)).expect("put 应成功");

        let existed = remove(&db, "never_existed").expect("remove 应成功");
        assert!(!existed, "删除不存在的 id 应返回 false");
        // 既有记忆不受影响。
        assert!(
            get(&db, "keep").expect("get 应成功").is_some(),
            "删除不存在的 id 不应影响其它记忆"
        );

        drop(db);
        cleanup(&path);
    }

    // 8. 全新库（无 memories 表）remove 返回 Ok(false)，不报错。
    #[test]
    fn remove_on_fresh_db_returns_false() {
        let path = unique_db_path("remove_fresh");
        let db = open(&path).expect("应能创建数据库");

        let existed = remove(&db, "anything").expect("全新库 remove 应成功且不报错");
        assert!(!existed, "全新库（无表）remove 应返回 false");

        drop(db);
        cleanup(&path);
    }

    // 8b. open 自动补建缺失的父目录：指向多级尚不存在目录也应成功建库并显示空。
    //     这是「全新安装 ~/.engram/ 尚未创建时，只读命令不该报错退出」的回归测试。
    #[test]
    fn open_creates_missing_parent_dirs() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        let mut base = std::env::temp_dir();
        base.push(format!("engram_store_mkparent_{pid}_{nanos}"));
        // 故意多套两层尚不存在的子目录，模拟 ~/.engram/ 这类缺失的父目录链。
        let nested = base.join("never").join("existed");
        let path = nested.join("general.redb");
        assert!(!nested.exists(), "前置：嵌套父目录应尚不存在");

        let db = open(&path).expect("父目录不存在时 open 也应补建并成功");
        assert!(path.exists(), "库文件应被就地创建");
        assert!(
            all(&db).expect("all 应成功").is_empty(),
            "全新建出的库应为空（0 条），而非报错"
        );

        drop(db);
        let _ = std::fs::remove_dir_all(&base);
    }

    // ---- retry_acquire 重试退避逻辑（用闭包注入，不依赖真实文件锁/计时）----
    //
    // 这些测试用 delay = 0，sleep 即为无副作用的近似空操作；用 Cell 计数器统计
    // 闭包被调用的次数，从而精确验证「重试到第几次」「调用了几次」。

    use std::cell::Cell;

    /// 9. 前 K 次返回可重试错误、第 K+1 次成功 → 最终成功，且恰好调用 K+1 次。
    #[test]
    fn retry_succeeds_after_transient_lock_errors() {
        let calls = Cell::new(0u32);
        // 设定前 3 次「撞锁」，第 4 次成功。
        let fail_until = 3u32;
        let result: Result<&str, &str> = retry_acquire(
            OPEN_MAX_ATTEMPTS,
            Duration::from_millis(0),
            |_e: &&str| true, // 全部视作可重试
            || {
                let n = calls.get() + 1;
                calls.set(n);
                if n <= fail_until {
                    Err("locked")
                } else {
                    Ok("opened")
                }
            },
        );
        assert_eq!(result, Ok("opened"), "撞锁数次后应最终成功");
        assert_eq!(
            calls.get(),
            fail_until + 1,
            "应恰好调用 K+1 次（前 K 次撞锁）"
        );
    }

    /// 10. 一直返回可重试错误 → 用尽 attempts 后返回该错误，且恰好调用 attempts 次。
    #[test]
    fn retry_exhausts_attempts_and_returns_lock_error() {
        let calls = Cell::new(0u32);
        let attempts = 5u32;
        let result: Result<&str, &str> = retry_acquire(
            attempts,
            Duration::from_millis(0),
            |_e: &&str| true,
            || {
                calls.set(calls.get() + 1);
                Err("locked")
            },
        );
        assert_eq!(result, Err("locked"), "一直撞锁应在用尽后返回锁错误");
        assert_eq!(calls.get(), attempts, "应恰好调用 attempts 次，不多不少");
    }

    /// 11. 首次返回不可重试错误 → 立即返回，绝不重试（只调用一次）。
    #[test]
    fn retry_does_not_retry_non_retryable_error() {
        let calls = Cell::new(0u32);
        let result: Result<&str, &str> = retry_acquire(
            OPEN_MAX_ATTEMPTS,
            Duration::from_millis(0),
            |_e: &&str| false, // 谓词恒假：任何错误都不可重试
            || {
                calls.set(calls.get() + 1);
                Err("fatal")
            },
        );
        assert_eq!(result, Err("fatal"), "不可重试错误应原样立即返回");
        assert_eq!(calls.get(), 1, "不可重试错误绝不重试，只应调用一次");
    }

    /// 12. 首次即成功 → 只调用一次，不进入退避。
    #[test]
    fn retry_returns_immediately_on_first_success() {
        let calls = Cell::new(0u32);
        let result: Result<&str, &str> = retry_acquire(
            OPEN_MAX_ATTEMPTS,
            Duration::from_millis(0),
            |_e: &&str| true,
            || {
                calls.set(calls.get() + 1);
                Ok("ok")
            },
        );
        assert_eq!(result, Ok("ok"), "首次成功应直接返回");
        assert_eq!(calls.get(), 1, "首次成功只应调用一次");
    }

    /// 13. attempts = 0 退化为「至少尝试一次」：调用恰好一次后返回错误。
    #[test]
    fn retry_with_zero_attempts_runs_once() {
        let calls = Cell::new(0u32);
        let result: Result<&str, &str> = retry_acquire(
            0,
            Duration::from_millis(0),
            |_e: &&str| true,
            || {
                calls.set(calls.get() + 1);
                Err("locked")
            },
        );
        assert_eq!(
            result,
            Err("locked"),
            "attempts=0 也应至少尝试一次并返回错误"
        );
        assert_eq!(calls.get(), 1, "attempts=0 应被夹为 1，恰好调用一次");
    }

    /// 14. is_lock_contention 精确识别 DatabaseAlreadyOpen，且只认这一个变体。
    #[test]
    fn is_lock_contention_matches_only_database_already_open() {
        let lock_err = StoreError::Database(Box::new(redb::DatabaseError::DatabaseAlreadyOpen));
        assert!(
            lock_err.is_lock_contention(),
            "DatabaseAlreadyOpen 应被识别为锁竞争"
        );

        // 另造一个非锁竞争的 DatabaseError（升级所需）确认不被误判为锁错误。
        let other_db_err = StoreError::Database(Box::new(redb::DatabaseError::UpgradeRequired(1)));
        assert!(
            !other_db_err.is_lock_contention(),
            "UpgradeRequired 不是锁竞争，不应重试"
        );

        // 非 Database 变体（如 Serde）也不应被判为锁竞争。
        let serde_err = match serde_json::from_str::<Memory>("not json") {
            Err(e) => StoreError::from(e),
            Ok(_) => panic!("构造测试用 serde 错误失败：非法 JSON 竟解析成功"),
        };
        assert!(
            !serde_err.is_lock_contention(),
            "Serde 错误不是锁竞争，不应重试"
        );
    }
}
