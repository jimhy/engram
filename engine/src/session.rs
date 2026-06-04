//! 会话巩固的「水位线 + pending 标记 + 增量切片」支撑层。
//!
//! 解决两件事（见设计文档「补偿巩固 / 增量复盘」一节）：
//! 1. **增量复盘**：复盘者只看「自上次巩固水位线之后新增」的 transcript 行，
//!    而非每次从头读全量——续会话越长越省 token。
//! 2. **补偿巩固**：会话末起复盘者前先落一个 pending 标记，复盘者成功收尾才
//!    删除它；若进程被强杀 / 断电没删成，下次会话启动扫到残留 pending，对它
//!    补跑，既不丢增量也不重复。
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
        assert!(
            read_watermark(&wmf).is_empty(),
            "缺失文件应读为空表"
        );
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
        write_pending(&dir.join("a.json"), &sample_pending("a", "/a.jsonl", "/a.slice.jsonl", 100.0))
            .expect("写 a 应成功");
        write_pending(&dir.join("b.json"), &sample_pending("b", "/b.jsonl", "/b.slice.jsonl", 50.0))
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
        write_pending(&pf, &sample_pending("sid", "/t.jsonl", sf.to_string_lossy().as_ref(), 1.0))
            .expect("写应成功");
        write_file(&sf, "x\n");
        remove_pending(&pf, &sf);
        assert!(!pf.exists(), "pending 应被删");
        assert!(!sf.exists(), "切片应被删");
        // 再删一次（已不存在）不应 panic
        remove_pending(&pf, &sf);
        let _ = fs::remove_dir_all(&dir);
    }
}
