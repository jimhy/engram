//! 会话巩固的「水位线 + pending 标记 + 增量切片」支撑层。
//!
//! 解决两件事（见设计文档「补偿巩固 / 增量复盘」一节）：
//! 1. **增量复盘**：复盘者只看「自上次巩固水位线之后新增」的 transcript 行，
//!    而非每次从头读全量——续会话越长越省 token。
//! 2. **补偿巩固**：会话末起复盘者前先落一个 pending 标记，复盘者成功收尾才
//!    删除它；若进程被强杀 / 断电没删成，下次会话启动扫到残留 pending，对它
//!    补跑，既不丢增量也不重复。
//! 3. **领单互斥（claim）**：SessionEnd 复盘与 SessionStart 补偿巩固可能同时
//!    领走同一个 pending（两个复盘者各写一套高度重复的记忆）。每个 pending 配
//!    一个 `<sid>.claim` 锁文件，用 `create_new` 的 OS 级原子语义「谁先建谁
//!    拥有」互斥，输家不跑；陈旧锁（持有者崩溃残留）超 [`CLAIM_TTL_SECS`] 可夺取。
//!
//! 水位线以**行数**为单位（transcript 是 JSONL，一行一条消息），按行切不会切断
//! 半条记录。所有 IO / 序列化错误统一包进 [`SessionError`] 向上传播，不 panic。

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 水位线表：transcript 路径 → 已巩固到的行数（从 0 起，含义为「前 N 行已复盘」）。
pub type Watermark = BTreeMap<String, u64>;

/// 一条待巩固的会话标记，序列化为 JSON 落盘于 `<work-dir>/<session_id>.json`。
///
/// 字段一旦写盘即固定快照——补偿巩固时据 `transcript` + `start_line` + `end_line`
/// 可重切出与当初完全一致的增量，保证水位线推进与实际复盘范围对齐。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    /// 会话 id（用作 pending 文件名与切片文件名的稳定前缀）。
    pub session_id: String,
    /// 原始 transcript（JSONL）路径——作水位线的键，必要时据它重切增量。
    pub transcript: String,
    /// 增量切片文件路径（复盘者实际读这个，而非全量 transcript）。
    pub slice: String,
    /// 本次复盘的起始行（= 上次水位线，已巩固的行数；从 0 起）。
    pub start_line: u64,
    /// 本次复盘的结束行（切片时 transcript 的总行数）。
    pub end_line: u64,
    /// 公共库路径（透传给复盘者）。
    pub general_db: String,
    /// 项目库路径（透传给复盘者）。
    pub project_db: String,
    /// 项目名（透传给复盘者）。
    pub project_name: String,
    /// 创建时间（unix 秒）；catchup 据它挑「最近一场」残留。
    pub created_at: f64,
}

/// 本模块的错误：IO 与 JSON 序列化两类，均装箱以保持 `Result` 体积小。
#[derive(Debug)]
pub enum SessionError {
    /// 文件读写 / 目录操作失败。
    Io(Box<std::io::Error>),
    /// JSON 序列化 / 反序列化失败。
    Serde(Box<serde_json::Error>),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::Io(e) => write!(f, "文件操作失败：{e}"),
            SessionError::Serde(e) => write!(f, "JSON 序列化/反序列化失败：{e}"),
        }
    }
}

impl std::error::Error for SessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SessionError::Io(e) => Some(e.as_ref()),
            SessionError::Serde(e) => Some(e.as_ref()),
        }
    }
}

impl From<std::io::Error> for SessionError {
    fn from(e: std::io::Error) -> Self {
        SessionError::Io(Box::new(e))
    }
}

impl From<serde_json::Error> for SessionError {
    fn from(e: serde_json::Error) -> Self {
        SessionError::Serde(Box::new(e))
    }
}

/// 确保某文件路径的父目录存在（写盘前调用）。
///
/// 路径无父目录段或父目录为空串时视作无需创建。
///
/// # Errors
/// `create_dir_all` 失败（无权限等）时返回 [`SessionError::Io`]。
fn ensure_parent(path: &Path) -> Result<(), SessionError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

/// 数一个文本文件的行数（确定 transcript 当前进度）。文件不存在视作 0 行。
///
/// # Errors
/// 打开 / 读取文件失败（非「不存在」）时返回 [`SessionError::Io`]。
pub fn count_lines(path: &Path) -> Result<u64, SessionError> {
    if !path.exists() {
        return Ok(0);
    }
    let reader = BufReader::new(File::open(path)?);
    let mut n = 0u64;
    for line in reader.lines() {
        line?;
        n += 1;
    }
    Ok(n)
}

/// 把 `transcript` 的第 `from_line+1`..=`to_line` 行（1-indexed）写入 `out`。
///
/// 即跳过前 `from_line` 行、取到第 `to_line` 行——这正是「自上次水位线以来的增量」。
/// `to_line <= from_line` 时写一个空切片（无新增）。`out` 的父目录不存在先建。
///
/// # Errors
/// 建父目录、打开 transcript、或写 `out` 失败时返回 [`SessionError::Io`]。
pub fn slice_lines(
    transcript: &Path,
    from_line: u64,
    to_line: u64,
    out: &Path,
) -> Result<(), SessionError> {
    ensure_parent(out)?;
    let mut w = File::create(out)?;
    if to_line <= from_line {
        return Ok(());
    }
    let reader = BufReader::new(File::open(transcript)?);
    for (idx, line) in reader.lines().enumerate() {
        let lineno = idx as u64 + 1; // 1-indexed
        if lineno <= from_line {
            continue;
        }
        if lineno > to_line {
            break;
        }
        let content = line?;
        w.write_all(content.as_bytes())?;
        w.write_all(b"\n")?;
    }
    Ok(())
}

/// 读水位线表；文件缺失或解析失败时返回空表。
///
/// 容错语义：水位线只是「省 token 的优化」，丢了最坏不过全量重跑一次，绝不该让
/// hook 崩。故解析失败也静默退化为空表，而非上抛。
pub fn read_watermark(path: &Path) -> Watermark {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => Watermark::new(),
    }
}

/// 写水位线表（父目录不存在先建）。
///
/// # Errors
/// 建父目录、序列化或写文件失败时返回 [`SessionError`]。
pub fn write_watermark(path: &Path, wm: &Watermark) -> Result<(), SessionError> {
    ensure_parent(path)?;
    let bytes = serde_json::to_vec_pretty(wm)?;
    fs::write(path, bytes)?;
    Ok(())
}

/// 写一条 pending 标记到 `path`（父目录不存在先建）。
///
/// # Errors
/// 建父目录、序列化或写文件失败时返回 [`SessionError`]。
pub fn write_pending(path: &Path, p: &Pending) -> Result<(), SessionError> {
    ensure_parent(path)?;
    let bytes = serde_json::to_vec_pretty(p)?;
    fs::write(path, bytes)?;
    Ok(())
}

/// 从 `path` 读回一条 pending 标记。
///
/// # Errors
/// 读文件或反序列化失败时返回 [`SessionError`]。
pub fn read_pending(path: &Path) -> Result<Pending, SessionError> {
    let bytes = fs::read(path)?;
    let p = serde_json::from_slice(&bytes)?;
    Ok(p)
}

/// 列出 `dir` 下全部 pending 标记（仅 `*.json`，跳过 `*.slice.jsonl` 等），
/// 按 `created_at` **升序**返回 `(文件路径, 标记)`。
///
/// 目录不存在返回空。单个解析失败的文件被静默跳过（容错，不让一个坏标记拖垮扫描）。
///
/// # Errors
/// 读目录失败时返回 [`SessionError::Io`]。
pub fn list_pending(dir: &Path) -> Result<Vec<(PathBuf, Pending)>, SessionError> {
    let mut out: Vec<(PathBuf, Pending)> = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(p) = read_pending(&path) {
            out.push((path, p));
        }
    }
    out.sort_by(|a, b| {
        a.1.created_at
            .partial_cmp(&b.1.created_at)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(out)
}

/// 删除一条 pending 标记及其切片文件（best-effort，忽略不存在 / 删除失败）。
///
/// 清理是收尾旁路，失败无需上抛——残留文件下次扫描会被再次处理或覆盖。
pub fn remove_pending(pending_path: &Path, slice_path: &Path) {
    let _ = fs::remove_file(pending_path);
    let _ = fs::remove_file(slice_path);
}

/// claim 锁的陈旧阈值（秒）= 15 分钟。
///
/// 远大于一次正常复盘的耗时（分钟级），持有者活着绝不会被误夺；又足够短，
/// 持有者崩溃 / 被强杀残留的锁最多卡一个 TTL 就会被下一个领单者按陈旧夺取。
pub const CLAIM_TTL_SECS: f64 = 900.0;

/// claim 锁文件的内容：持有者 pid 与创建时间，序列化为 JSON。
///
/// `pid` 仅作诊断（看锁是谁建的）；陈旧判定只看 `created_at`——pid 复用 / 共享
/// 目录跨机的场景下「查 pid 是否存活」并不可靠，统一走 TTL 时间判定更稳。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimInfo {
    /// 持有者进程 id（诊断用，不参与陈旧判定）。
    pub pid: u32,
    /// claim 创建时间（unix 秒）；距今超过 [`CLAIM_TTL_SECS`] 即视为陈旧可夺取。
    pub created_at: f64,
}

/// [`try_claim`] 的两种结果：抢到了，或已被他人持有。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// 成功占用（含夺取陈旧 claim）——本进程现在是该 pending 的复盘 owner。
    Acquired,
    /// 已被他人持有且未陈旧——本进程不应再对该 pending 起复盘。
    Held,
}

/// 由 pending 标记路径推导其 claim 锁文件路径：`<sid>.json` → `<sid>.claim`。
///
/// claim 是「领单占用」标记：复盘者起跑前原子新建它宣示所有权，挡住 SessionEnd
/// 复盘与 SessionStart 补偿巩固同时领走同一个 pending 的竞态。
pub fn claim_path_for(pending_path: &Path) -> PathBuf {
    pending_path.with_extension("claim")
}

/// 原子新建 claim 文件并写入持有者信息（[`try_claim`] 的底层一步）。
///
/// `create_new` 保证「文件已存在则失败」在 OS 层是原子的（Windows / Unix 皆然），
/// 即跨进程互斥的「谁先建谁拥有」；已存在时返回 [`ClaimOutcome::Held`]。新建成功
/// 但写内容失败时回滚删除半成品，避免留下一个空锁迷惑后来者。
///
/// # Errors
/// 序列化 `info`、新建文件（非「已存在」）或写入失败时返回 [`SessionError`]。
fn create_claim(claim_path: &Path, info: &ClaimInfo) -> Result<ClaimOutcome, SessionError> {
    let bytes = serde_json::to_vec_pretty(info)?;
    let mut f = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(claim_path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(ClaimOutcome::Held),
        Err(e) => return Err(e.into()),
    };
    if let Err(e) = f.write_all(&bytes) {
        drop(f);
        let _ = fs::remove_file(claim_path);
        return Err(e.into());
    }
    Ok(ClaimOutcome::Acquired)
}

/// 判定既有 claim 是否陈旧（可被夺取）。
///
/// 正常路径：读回 [`ClaimInfo`] 的 `created_at`，距 `now` 超过 [`CLAIM_TTL_SECS`]
/// 即陈旧。内容解析失败（半截写入 / 损坏）时**不轻易夺取**——改用文件 mtime 兜底：
/// 同样超过 TTL 才算陈旧；mtime 也读不到则保守当作未陈旧（Held）。取舍：宁可这
/// 一轮不补跑（下次会话还有机会），也不抢一个状态完全未知的锁；坏文件最终会因
/// mtime 超时被夺取，不会永久卡死。
fn claim_is_stale(claim_path: &Path, now: f64) -> bool {
    if let Ok(bytes) = fs::read(claim_path) {
        if let Ok(info) = serde_json::from_slice::<ClaimInfo>(&bytes) {
            return now - info.created_at > CLAIM_TTL_SECS;
        }
    }
    // 解析失败：按文件 mtime 兜底判定。
    match fs::metadata(claim_path).and_then(|m| m.modified()) {
        Ok(mtime) => match mtime.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => now - d.as_secs_f64() > CLAIM_TTL_SECS,
            Err(_) => false,
        },
        Err(_) => false,
    }
}

/// 尝试领走（占用）一个 pending：对其 claim 路径原子建锁。
///
/// 互斥根基是 `OpenOptions::create_new`——「文件已存在则失败」由 OS 保证原子
/// （Windows / Unix 皆然），故两个进程同时领同一个 pending 时必恰有一方
/// [`ClaimOutcome::Acquired`]、另一方 [`ClaimOutcome::Held`]。
///
/// 已存在的 claim 若距 `now` 超过 [`CLAIM_TTL_SECS`]（持有者大概率已崩），按陈旧
/// **夺取**：先删旧锁再原子新建；夺取途中任一步竞争失败（他人抢先重建 / 删除受阻）
/// 一律退回 [`ClaimOutcome::Held`]，绝不出现双 owner。
///
/// # Errors
/// 建父目录、序列化 [`ClaimInfo`] 或写新锁文件失败时返回 [`SessionError`]；
/// 「锁被他人持有」不是错误（以 [`ClaimOutcome::Held`] 表达）。
pub fn try_claim(claim_path: &Path, now: f64, pid: u32) -> Result<ClaimOutcome, SessionError> {
    ensure_parent(claim_path)?;
    let info = ClaimInfo {
        pid,
        created_at: now,
    };
    // 第一步：直接抢——文件不存在时原子新建即获得。
    if create_claim(claim_path, &info)? == ClaimOutcome::Acquired {
        return Ok(ClaimOutcome::Acquired);
    }
    // 已存在：未陈旧则尊重现有持有者。
    if !claim_is_stale(claim_path, now) {
        return Ok(ClaimOutcome::Held);
    }
    // 陈旧 → 夺取：先删旧锁再原子新建。删除失败（除「已不存在」外）保守按 Held；
    // 新建撞上「已存在」（他人抢先重建）也按 Held——竞争输了就让对方跑。
    if let Err(e) = fs::remove_file(claim_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Ok(ClaimOutcome::Held);
        }
    }
    create_claim(claim_path, &info)
}

/// 释放 claim 锁（best-effort，忽略不存在 / 删除失败）。
///
/// 与 [`remove_pending`] 同风格：清理是收尾旁路，失败无需上抛——残留锁超过
/// [`CLAIM_TTL_SECS`] 后会被下一个领单者按陈旧夺取，不会永久卡死。
pub fn release_claim(claim_path: &Path) {
    let _ = fs::remove_file(claim_path);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 进程内唯一的临时目录（不引入 tempfile 依赖）。
    fn unique_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        let mut dir = std::env::temp_dir();
        dir.push(format!("engram_session_{tag}_{pid}_{nanos}"));
        dir
    }

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(path, contents).expect("写测试文件应成功");
    }

    fn sample_pending(id: &str, transcript: &str, slice: &str, created_at: f64) -> Pending {
        Pending {
            session_id: id.to_string(),
            transcript: transcript.to_string(),
            slice: slice.to_string(),
            start_line: 0,
            end_line: 3,
            general_db: "G".to_string(),
            project_db: "P".to_string(),
            project_name: "proj".to_string(),
            created_at,
        }
    }

    // 1. count_lines：3 行文件数 3；不存在文件数 0。
    #[test]
    fn count_lines_counts_and_handles_missing() {
        let dir = unique_dir("count");
        let f = dir.join("t.jsonl");
        write_file(&f, "a\nb\nc\n");
        assert_eq!(count_lines(&f).expect("应成功"), 3);
        assert_eq!(
            count_lines(&dir.join("nope.jsonl")).expect("不存在应为 0"),
            0
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // 2. slice_lines：5 行，from=2,to=5 → 切出第 3、4、5 行。
    #[test]
    fn slice_lines_extracts_increment() {
        let dir = unique_dir("slice");
        let src = dir.join("t.jsonl");
        write_file(&src, "l1\nl2\nl3\nl4\nl5\n");
        let out = dir.join("s.jsonl");
        slice_lines(&src, 2, 5, &out).expect("切片应成功");
        let got = fs::read_to_string(&out).expect("读切片应成功");
        assert_eq!(got, "l3\nl4\nl5\n", "应只含第 3-5 行");
        let _ = fs::remove_dir_all(&dir);
    }

    // 3. slice_lines：to<=from 写空切片（无新增）。
    #[test]
    fn slice_lines_empty_when_no_increment() {
        let dir = unique_dir("slice_empty");
        let src = dir.join("t.jsonl");
        write_file(&src, "l1\nl2\nl3\n");
        let out = dir.join("s.jsonl");
        slice_lines(&src, 3, 3, &out).expect("应成功");
        assert_eq!(fs::read_to_string(&out).expect("读应成功"), "", "应为空");
        let _ = fs::remove_dir_all(&dir);
    }

    // 4. watermark 往返：write→read 相等；缺失文件读为空表。
    #[test]
    fn watermark_roundtrip_and_missing() {
        let dir = unique_dir("wm");
        let wmf = dir.join("watermark.json");
        assert!(read_watermark(&wmf).is_empty(), "缺失文件应读为空表");
        let mut wm = Watermark::new();
        wm.insert("/path/to/t.jsonl".to_string(), 42);
        write_watermark(&wmf, &wm).expect("写水位线应成功");
        let got = read_watermark(&wmf);
        assert_eq!(got.get("/path/to/t.jsonl"), Some(&42));
        let _ = fs::remove_dir_all(&dir);
    }

    // 5. pending 往返：write→read 完全相等。
    #[test]
    fn pending_roundtrip() {
        let dir = unique_dir("pending");
        let pf = dir.join("sid.json");
        let p = sample_pending("sid", "/t.jsonl", "/sid.slice.jsonl", 100.0);
        write_pending(&pf, &p).expect("写 pending 应成功");
        let got = read_pending(&pf).expect("读 pending 应成功");
        assert_eq!(got, p, "往返应完全一致");
        let _ = fs::remove_dir_all(&dir);
    }

    // 6. list_pending：只认 *.json、按 created_at 升序、跳过切片与坏文件。
    #[test]
    fn list_pending_filters_and_sorts() {
        let dir = unique_dir("list");
        // 两条合法 pending（created_at 100、50）
        write_pending(
            &dir.join("a.json"),
            &sample_pending("a", "/a.jsonl", "/a.slice.jsonl", 100.0),
        )
        .expect("写 a 应成功");
        write_pending(
            &dir.join("b.json"),
            &sample_pending("b", "/b.jsonl", "/b.slice.jsonl", 50.0),
        )
        .expect("写 b 应成功");
        // 一个切片文件（.jsonl，应被忽略）、一个坏 json（应被跳过）
        write_file(&dir.join("a.slice.jsonl"), "x\n");
        write_file(&dir.join("bad.json"), "not json");

        let list = list_pending(&dir).expect("扫描应成功");
        assert_eq!(list.len(), 2, "应只返回 2 条合法 pending");
        assert_eq!(list[0].1.session_id, "b", "created_at 小的（50）排前");
        assert_eq!(list[1].1.session_id, "a", "created_at 大的（100）排后");
        let _ = fs::remove_dir_all(&dir);
    }

    // 7. remove_pending：删 pending + 切片，且不存在也不报错。
    #[test]
    fn remove_pending_deletes_both() {
        let dir = unique_dir("remove");
        let pf = dir.join("sid.json");
        let sf = dir.join("sid.slice.jsonl");
        write_pending(
            &pf,
            &sample_pending("sid", "/t.jsonl", sf.to_string_lossy().as_ref(), 1.0),
        )
        .expect("写应成功");
        write_file(&sf, "x\n");
        remove_pending(&pf, &sf);
        assert!(!pf.exists(), "pending 应被删");
        assert!(!sf.exists(), "切片应被删");
        // 再删一次（已不存在）不应 panic
        remove_pending(&pf, &sf);
        let _ = fs::remove_dir_all(&dir);
    }

    // 8. claim_path_for：<sid>.json → <sid>.claim。
    #[test]
    fn claim_path_for_swaps_extension() {
        assert_eq!(
            claim_path_for(Path::new("w/sid-1.json")),
            PathBuf::from("w/sid-1.claim")
        );
    }

    // 9. try_claim 原子互斥：首领 Acquired，紧接（同 now）二领 Held。
    #[test]
    fn try_claim_first_acquires_second_held() {
        let dir = unique_dir("claim");
        let cp = claim_path_for(&dir.join("sid.json"));
        assert_eq!(
            try_claim(&cp, 1000.0, 1).expect("首领应成功"),
            ClaimOutcome::Acquired
        );
        assert_eq!(
            try_claim(&cp, 1000.0, 2).expect("二领应正常返回"),
            ClaimOutcome::Held,
            "未陈旧的 claim 不应被二次领走"
        );
        // 锁内容应可读回且记录首位持有者。
        let info: ClaimInfo =
            serde_json::from_slice(&fs::read(&cp).expect("读锁应成功")).expect("解析锁应成功");
        assert_eq!(info.pid, 1, "锁应仍属首位持有者");
        let _ = fs::remove_dir_all(&dir);
    }

    // 10. 陈旧夺取：超 TTL 后 try_claim → Acquired，且锁归新持有者。
    #[test]
    fn try_claim_steals_stale() {
        let dir = unique_dir("claim_stale");
        let cp = claim_path_for(&dir.join("sid.json"));
        assert_eq!(
            try_claim(&cp, 1000.0, 1).expect("首领应成功"),
            ClaimOutcome::Acquired
        );
        let later = 1000.0 + CLAIM_TTL_SECS + 1.0;
        assert_eq!(
            try_claim(&cp, later, 2).expect("夺取应成功"),
            ClaimOutcome::Acquired,
            "超过 TTL 的陈旧 claim 应被夺取"
        );
        // 夺取后立刻再领（同 later）应 Held——锁已归新持有者且不陈旧。
        assert_eq!(
            try_claim(&cp, later, 3).expect("应正常返回"),
            ClaimOutcome::Held
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // 11. release_claim 后可再次 Acquired；释放不存在的锁不 panic。
    #[test]
    fn release_claim_allows_reacquire() {
        let dir = unique_dir("claim_release");
        let cp = claim_path_for(&dir.join("sid.json"));
        assert_eq!(
            try_claim(&cp, 1.0, 1).expect("首领应成功"),
            ClaimOutcome::Acquired
        );
        release_claim(&cp);
        assert!(!cp.exists(), "释放后锁文件应不存在");
        assert_eq!(
            try_claim(&cp, 2.0, 2).expect("释放后再领应成功"),
            ClaimOutcome::Acquired
        );
        release_claim(&cp);
        // 再释放一次（已不存在）不应 panic。
        release_claim(&cp);
        let _ = fs::remove_dir_all(&dir);
    }

    // 12. 内容损坏的 claim：TTL 内保守 Held；超 TTL（按 mtime 兜底）可夺取。
    #[test]
    fn try_claim_garbage_held_then_stolen_by_mtime() {
        let dir = unique_dir("claim_garbage");
        let cp = claim_path_for(&dir.join("sid.json"));
        write_file(&cp, "not json");
        // 文件刚写下，mtime ≈ 真实当前时间——TTL 内不可夺取。
        let wall = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .expect("系统时钟应晚于 unix 纪元");
        assert_eq!(
            try_claim(&cp, wall, 1).expect("应正常返回"),
            ClaimOutcome::Held,
            "TTL 内的坏 claim 应保守视作被持有"
        );
        assert_eq!(
            try_claim(&cp, wall + CLAIM_TTL_SECS + 60.0, 2).expect("应成功"),
            ClaimOutcome::Acquired,
            "坏 claim 超 TTL（按 mtime 兜底）应可夺取，不会永久卡死"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
