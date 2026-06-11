//! 已入库文档清单（manifest.json）：增量入库的依据。
//!
//! manifest 记录每个已入库文档的**内容 hash**与 chunk 数。再次 ingest 时：
//! hash 未变 → 跳过；已变 → 删旧 chunk 重入。另记录**管线参数版本**
//! （[`PIPELINE_VERSION`]）：分块参数 / 上下文头格式 / 嵌入模型任一变更都必须
//! 递增该常量——文档侧嵌入是「不对称」的（query 加指令前缀、文档不加），
//! 文档侧规则一变，旧向量与新向量不可比，必须全量重建（设计文档 §9.8）。

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 当前 RAG 管线参数版本。分块大小 / overlap / 面包屑格式 / 嵌入模型变更时递增。
pub const PIPELINE_VERSION: u32 = 1;

/// 单个已入库文档的记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocEntry {
    /// 文档内容的 SHA-256（hex 小写）。
    pub hash: String,
    /// 该文档产生的 chunk 数。
    pub chunks: usize,
    /// 入库时间（unix 秒）。
    pub ingested_at: f64,
}

/// 知识库文档清单。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// 写入该 manifest 时的管线参数版本。
    pub pipeline_version: u32,
    /// 相对路径（正斜杠规范化）→ 文档记录。
    pub docs: BTreeMap<String, DocEntry>,
}

impl Default for Manifest {
    fn default() -> Self {
        Manifest {
            pipeline_version: PIPELINE_VERSION,
            docs: BTreeMap::new(),
        }
    }
}

impl Manifest {
    /// 从 `path` 读取 manifest；文件不存在视为空清单（首次 ingest）。
    ///
    /// # Errors
    /// 文件存在但读取 / 解析失败时返回中文错误说明（不静默吞——manifest 损坏
    /// 时静默清空会导致全量重入且旧 chunk 残留）。
    pub fn load(path: &Path) -> Result<Manifest, String> {
        if !path.is_file() {
            return Ok(Manifest::default());
        }
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("读取 manifest {} 失败：{e}", path.display()))?;
        serde_json::from_str(&raw).map_err(|e| {
            format!(
                "解析 manifest {} 失败（文件可能已损坏）：{e}\n\
                 恢复办法：删除该文件后执行 ingest --force 全量重建知识库。",
                path.display()
            )
        })
    }

    /// 把 manifest 写回 `path`（原子覆盖：先写临时文件再 rename，崩溃不会留下
    /// 半截 JSON；父目录不存在则先创建）。
    ///
    /// # Errors
    /// 建目录、序列化、写临时文件或 rename 失败时返回中文错误说明。
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建目录 {} 失败：{e}", parent.display()))?;
        }
        let raw = serde_json::to_string_pretty(self)
            .map_err(|e| format!("序列化 manifest 失败：{e}"))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, raw)
            .map_err(|e| format!("写入临时文件 {} 失败：{e}", tmp.display()))?;
        // Windows / Unix 的 rename 对已存在目标均为原子替换。
        std::fs::rename(&tmp, path)
            .map_err(|e| format!("替换 manifest {} 失败：{e}", path.display()))
    }

    /// 判断某文档是否需要（重新）入库：清单里没有、hash 已变、或管线版本不符。
    pub fn needs_ingest(&self, rel_path: &str, content_hash: &str) -> bool {
        if self.pipeline_version != PIPELINE_VERSION {
            return true;
        }
        match self.docs.get(rel_path) {
            Some(entry) => entry.hash != content_hash,
            None => true,
        }
    }
}

/// 计算文本内容的 SHA-256（hex 小写）。
pub fn content_hash(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    let mut out = String::with_capacity(64);
    for b in digest {
        // 手写 hex 避免引入 hex crate（依赖极简原则，对齐主引擎不引 uuid/rand 的做法）。
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// hash 稳定且为 64 位 hex。
    #[test]
    fn hash_is_stable_hex() {
        let h = content_hash("你好，知识库");
        assert_eq!(h.len(), 64);
        assert_eq!(h, content_hash("你好，知识库"));
        assert_ne!(h, content_hash("你好，知识库!"));
    }

    /// 增量判断：新文档 / 变更文档需要入库，未变的不需要。
    #[test]
    fn needs_ingest_logic() {
        let mut m = Manifest::default();
        assert!(m.needs_ingest("a.md", "h1"));
        m.docs.insert(
            "a.md".to_string(),
            DocEntry { hash: "h1".to_string(), chunks: 3, ingested_at: 0.0 },
        );
        assert!(!m.needs_ingest("a.md", "h1"));
        assert!(m.needs_ingest("a.md", "h2"));
        // 管线版本不符 → 一律重入。
        m.pipeline_version = PIPELINE_VERSION - 1;
        assert!(m.needs_ingest("a.md", "h1"));
    }

    /// load/save 往返一致；不存在的文件给空清单。
    #[test]
    fn load_save_roundtrip() {
        let tmp = tempfile::tempdir().expect("建临时目录失败");
        let p = tmp.path().join("kb").join("manifest.json");
        let empty = Manifest::load(&p).expect("不存在应给空清单");
        assert!(empty.docs.is_empty());

        let mut m = Manifest::default();
        m.docs.insert(
            "docs/设计.md".to_string(),
            DocEntry { hash: content_hash("x"), chunks: 5, ingested_at: 1.0 },
        );
        m.save(&p).expect("保存失败");
        let back = Manifest::load(&p).expect("读取失败");
        assert_eq!(back.docs.len(), 1);
        assert_eq!(back.docs["docs/设计.md"].chunks, 5);
    }
}
