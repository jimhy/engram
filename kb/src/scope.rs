//! 作用域锚定与路径约定。
//!
//! 与主引擎共享同一锚点判据（约定层面共享，代码不依赖 engine）：
//! 从某目录向上找第一个含 `.engram/` 的祖先目录，且该 `.engram/` 下**必须**
//! 有 `engram.redb` 库文件或 `workspace` 标记文件才算真锚点——空的 / 残留的
//! `.engram/` 目录不算（主引擎踩过的坑：误删后残留空目录会污染锚定）。
//!
//! 知识库的各路径约定（设计文档 §4）：
//! - 项目知识库目录：`<锚点>/.engram/kb/`（LanceDB 数据 + manifest.json）；
//! - 模型缓存（全局共享）：`<HOME>/.engram/kb/models/`；
//! - 本模块只做路径推导与探测，**不创建任何目录**——建目录是写入路径调用方的职责。

use std::path::{Path, PathBuf};

/// 锚点目录名（与主引擎一致）。
pub const ENGRAM_DIR: &str = ".engram";
/// 锚点判据之一：项目记忆库文件名（与主引擎一致）。
pub const ENGRAM_DB_FILE: &str = "engram.redb";
/// 锚点判据之二：项目管理目录（workspace）标记文件名（与主引擎一致）。
pub const WORKSPACE_MARKER: &str = "workspace";
/// 知识库子目录名（位于锚点 `.engram/` 之下）。
pub const KB_DIR: &str = "kb";
/// 共享知识库数据子目录名（位于 `<HOME>/.engram/kb/` 之下，与 `models/` 同级分开）。
pub const SHARED_KB_SUBDIR: &str = "store";

/// 判断目录 `d` 是否为 engram 锚点（`.engram/` 下有 redb 库或 workspace 标记）。
fn is_anchor(d: &Path) -> bool {
    d.join(ENGRAM_DIR).join(ENGRAM_DB_FILE).is_file()
        || d.join(ENGRAM_DIR).join(WORKSPACE_MARKER).is_file()
}

/// 从 `cwd` 向上找最近的 engram 锚点目录。
///
/// # 参数
/// - `cwd`：起始目录（调用方应传规范化后的绝对路径；相对路径也能工作，但
///   向上遍历会止于相对根）。
///
/// 返回锚点目录（含 `.engram/` 的那一层）；找不到返回 `None`。
pub fn find_anchor(cwd: &Path) -> Option<PathBuf> {
    let mut cur = Some(cwd);
    while let Some(d) = cur {
        if is_anchor(d) {
            return Some(d.to_path_buf());
        }
        cur = d.parent();
    }
    None
}

/// 推导项目知识库目录：`<锚点>/.engram/kb/`。
///
/// # Errors
/// `cwd` 不在任何 engram 项目内（向上找不到锚点）时返回中文错误说明，
/// 提示用户先用主引擎 `/engram root` 建立锚点或用 `--db` 显式指定目录。
pub fn resolve_kb_dir(cwd: &Path) -> Result<PathBuf, String> {
    match find_anchor(cwd) {
        Some(anchor) => Ok(anchor.join(ENGRAM_DIR).join(KB_DIR)),
        None => Err(format!(
            "当前目录 {} 不在任何 engram 项目内（向上未找到 .engram/ 锚点）。\
             请先在项目根运行 /engram root 建立锚点，或用 --db 显式指定知识库目录。",
            cwd.display()
        )),
    }
}

/// 推导用户主目录：Windows 取 `USERPROFILE`，其余取 `HOME`。
///
/// # 参数
/// - `env_lookup`：环境变量查找闭包（可注入，便于纯单测）。
///
/// # Errors
/// 两个变量都取不到时返回中文错误说明。
pub fn home_dir(env_lookup: impl Fn(&str) -> Option<String>) -> Result<PathBuf, String> {
    env_lookup("USERPROFILE")
        .or_else(|| env_lookup("HOME"))
        .map(PathBuf::from)
        .ok_or_else(|| "无法确定用户主目录（USERPROFILE 与 HOME 均未设置）".to_string())
}

/// 推导模型缓存目录：`<HOME>/.engram/kb/models/`。
///
/// # Errors
/// 主目录无法确定时返回其错误说明。
pub fn model_cache_dir(env_lookup: impl Fn(&str) -> Option<String>) -> Result<PathBuf, String> {
    Ok(home_dir(env_lookup)?
        .join(ENGRAM_DIR)
        .join(KB_DIR)
        .join("models"))
}

/// 推导共享（用户级）知识库目录：`<HOME>/.engram/kb/store/`。
///
/// 与项目知识库（`<锚点>/.engram/kb/`）并列的跨项目共用库，类比公共记忆库
/// （`<HOME>/.engram/general.redb`）：不隶属任何项目、任意目录下都可访问。放
/// `store/` 子目录而非直接落在 `<HOME>/.engram/kb/`，是为了与同级的模型缓存
/// `<HOME>/.engram/kb/models/` 分开、互不干扰（其下再有 `lancedb/` 与
/// `manifest.json`，与项目库同构）。共享库无锚点，doc_path 一律用绝对路径。
///
/// # Errors
/// 主目录无法确定时返回其错误说明。
pub fn resolve_shared_kb_dir(
    env_lookup: impl Fn(&str) -> Option<String>,
) -> Result<PathBuf, String> {
    Ok(home_dir(env_lookup)?
        .join(ENGRAM_DIR)
        .join(KB_DIR)
        .join(SHARED_KB_SUBDIR))
}

/// 把 manifest 里的 doc_path 还原为绝对路径（孤儿清理的定位步骤）。
///
/// doc_path 的两种形态（见 main.rs 的 normalize_doc_path）：
/// - 绝对路径（共享库、或锚点外的文档）→ 原样返回；
/// - 相对路径（锚点内的文档）→ 拼到锚点根之下。
///
/// 锚点缺失且路径为相对时返回 `None`——无从定位源文件，调用方应跳过该条目
/// （孤儿判定宁可漏判也不误删）。
pub fn resolve_doc_abs(doc_path: &str, anchor_root: Option<&Path>) -> Option<PathBuf> {
    let p = Path::new(doc_path);
    if p.is_absolute() {
        return Some(p.to_path_buf());
    }
    anchor_root.map(|root| root.join(p))
}

/// 判断绝对路径 `doc_abs` 是否落在一次 ingest 输入路径 `root` 的覆盖范围内。
///
/// 覆盖语义与 ingest 的文件收集规则（main.rs 的 collect_files）保持一致：
/// - 输入是**文件**（`root_is_dir=false`）→ 精确匹配该文件（收集时不限扩展名，
///   判定同样不限）；
/// - 输入是**目录** → 位于其真子路径下、且扩展名（忽略大小写）在 `exts` 中。
///
/// 判定比真实收集略宽（不排除隐藏子目录与符号链接）：本函数只用于「磁盘上已
/// 不存在」的孤儿判定，宽一点最多多清理本就该清的残留条目，无害。路径比较走
/// `Path` 的按组件比较，正/反斜杠混用（doc_path 存储用正斜杠）不影响结果。
pub fn ingest_covers(root: &Path, root_is_dir: bool, doc_abs: &Path, exts: &[String]) -> bool {
    if !root_is_dir {
        return doc_abs == root;
    }
    if !doc_abs.starts_with(root) || doc_abs == root {
        return false;
    }
    match doc_abs.extension() {
        Some(ext) => {
            let ext = ext.to_string_lossy().to_ascii_lowercase();
            exts.iter().any(|e| e == &ext)
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 锚点判据：`.engram/` 下有 redb 文件才算锚点，空目录不算。
    ///
    /// 注意环境无关性：临时目录的**祖先链**上可能存在真实锚点（开发机的
    /// `~/.engram/engram.redb`，temp 目录在 HOME 之下），所以「空目录不算」
    /// 只能断言*不会停在 root*，不能断言整体为 None。
    #[test]
    fn anchor_requires_db_or_marker() {
        let tmp = tempfile::tempdir().expect("建临时目录失败");
        let root = tmp.path();
        let sub = root.join("a").join("b");
        fs::create_dir_all(&sub).expect("建子目录失败");

        // 空 .engram/ 不算锚点：向上查找绝不应停在 root（祖先里有无真锚点不论）。
        fs::create_dir_all(root.join(ENGRAM_DIR)).expect("建 .engram 失败");
        assert_ne!(find_anchor(&sub), Some(root.to_path_buf()));

        // 放入 redb 文件后成为锚点（最近优先，必然命中 root，不受祖先影响）。
        fs::write(root.join(ENGRAM_DIR).join(ENGRAM_DB_FILE), b"x").expect("写文件失败");
        assert_eq!(find_anchor(&sub), Some(root.to_path_buf()));

        // kb 目录推导。
        let kb = resolve_kb_dir(&sub).expect("应能推导 kb 目录");
        assert_eq!(kb, root.join(ENGRAM_DIR).join(KB_DIR));
    }

    /// workspace 标记同样构成锚点。
    #[test]
    fn workspace_marker_is_anchor() {
        let tmp = tempfile::tempdir().expect("建临时目录失败");
        let root = tmp.path();
        fs::create_dir_all(root.join(ENGRAM_DIR)).expect("建 .engram 失败");
        fs::write(root.join(ENGRAM_DIR).join(WORKSPACE_MARKER), b"").expect("写标记失败");
        assert_eq!(find_anchor(root), Some(root.to_path_buf()));
    }

    /// home_dir 注入式查找：USERPROFILE 优先，HOME 兜底，都缺报错。
    #[test]
    fn home_dir_lookup_order() {
        let win = |k: &str| (k == "USERPROFILE").then(|| "C:/Users/x".to_string());
        assert_eq!(home_dir(win).expect("应取到"), PathBuf::from("C:/Users/x"));
        let unix = |k: &str| (k == "HOME").then(|| "/home/x".to_string());
        assert_eq!(home_dir(unix).expect("应取到"), PathBuf::from("/home/x"));
        let none = |_: &str| None;
        assert!(home_dir(none).is_err());
    }

    /// 共享库目录：`<HOME>/.engram/kb/store/`，与模型缓存 `.../kb/models/` 同级分开。
    #[test]
    fn shared_kb_dir_layout() {
        let env = |k: &str| (k == "USERPROFILE").then(|| "C:/Users/x".to_string());
        let shared = resolve_shared_kb_dir(env).expect("应取到共享库目录");
        assert_eq!(
            shared,
            PathBuf::from("C:/Users/x").join(".engram").join("kb").join("store")
        );
        // 与模型缓存同级、目录名不同（不会互相污染）。
        let models = model_cache_dir(env).expect("应取到模型缓存目录");
        assert_eq!(shared.parent(), models.parent());
        assert_ne!(shared, models);
    }

    /// doc_path 还原：相对路径拼锚点、绝对路径原样、无锚点且相对时给 None。
    #[test]
    fn doc_abs_resolution() {
        let root = if cfg!(windows) { PathBuf::from(r"F:\proj") } else { PathBuf::from("/proj") };
        assert_eq!(
            resolve_doc_abs("docs/a.md", Some(&root)),
            Some(root.join("docs").join("a.md"))
        );
        let abs = if cfg!(windows) { "C:/other/b.md" } else { "/other/b.md" };
        assert_eq!(resolve_doc_abs(abs, Some(&root)), Some(PathBuf::from(abs)));
        assert_eq!(resolve_doc_abs("docs/a.md", None), None);
    }

    /// 覆盖判定：目录输入看「真子路径 + 扩展名」，文件输入看精确匹配；
    /// 正/反斜杠混用（doc_path 存储用正斜杠）不影响按组件比较。
    #[test]
    fn ingest_coverage_semantics() {
        let root = if cfg!(windows) { PathBuf::from(r"F:\proj\docs") } else { PathBuf::from("/proj/docs") };
        let exts = vec!["md".to_string()];

        // 目录输入：其下的 .md 覆盖；扩展名不符 / 目录外 / 目录自身不覆盖。
        assert!(ingest_covers(&root, true, &root.join("a.md"), &exts));
        assert!(ingest_covers(&root, true, &root.join("sub").join("b.MD"), &exts));
        assert!(!ingest_covers(&root, true, &root.join("a.txt"), &exts));
        assert!(!ingest_covers(&root, true, &root.join("无扩展名"), &exts));
        assert!(!ingest_covers(&root, true, &root, &exts));
        let outside =
            if cfg!(windows) { PathBuf::from(r"F:\proj\other\a.md") } else { PathBuf::from("/proj/other/a.md") };
        assert!(!ingest_covers(&root, true, &outside, &exts));

        // 文件输入：精确匹配、不限扩展名（与 collect_files 的收集规则一致）。
        let file = root.join("单发.rst");
        assert!(ingest_covers(&file, false, &file, &exts));
        assert!(!ingest_covers(&file, false, &root.join("别的.rst"), &exts));

        // 分隔符混用：doc_path 是正斜杠拼出来的路径也能按组件匹配。
        #[cfg(windows)]
        {
            let mixed = PathBuf::from(r"F:\proj\docs/sub/c.md");
            assert!(ingest_covers(&root, true, &mixed, &exts));
        }
    }
}
