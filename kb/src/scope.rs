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
}
