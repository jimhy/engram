//! 嵌入：fastembed + bge-small-zh-v1.5（ONNX，Xenova 转换版，512 维）。
//!
//! 关键约束（设计文档 §3 / §9）：
//! - **缓存目录必须显式绝对路径**——fastembed 默认缓存是相对 CWD 的
//!   `.fastembed_cache`，会随调用目录乱跑；`HF_HOME` 还会静默覆盖
//!   `with_cache_dir`，进程入口已统一移除该变量（见 `main.rs`）；
//! - **嵌入不对称**：query 侧加 BGE 官方中文指令前缀 [`QUERY_PREFIX`]，
//!   文档侧裸文本嵌入（官方明确 passage 不加指令）；
//! - `embed()` 是 `&mut self`（ONNX session 状态），跨线程共享需调用方自行包裹；
//! - 推理线程数限制为 4，避免 sidecar 吃满全核抢占用户会话资源。

use std::path::{Path, PathBuf};

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

/// bge-small-zh-v1.5 的输出向量维度。
pub const VECTOR_DIM: i32 = 512;

/// BGE 中文系列 query 侧官方指令前缀（s2p 短查长文场景官方建议加；仅用于编码，
/// 不进索引、不展示）。
pub const QUERY_PREFIX: &str = "为这个句子生成表示以用于检索相关文章：";

/// 模型在 hf-hub 缓存布局下的仓库目录名。
const MODEL_REPO_DIR: &str = "models--Xenova--bge-small-zh-v1.5";

/// 推理用的 intra-op 线程数上限。
const INTRA_THREADS: usize = 4;

/// 嵌入器：bge-small-zh-v1.5 的 fastembed 包装。
pub struct Embedder {
    inner: TextEmbedding,
}

impl Embedder {
    /// 初始化嵌入模型。模型文件不在缓存时会**联网下载约 95MB**（走 hf-hub，
    /// 尊重 `HF_ENDPOINT` 环境变量，国内可设 hf-mirror.com 镜像）。
    ///
    /// # 参数
    /// - `cache_dir`：模型缓存目录（应为绝对路径，约定 `<HOME>/.engram/kb/models/`）；
    /// - `show_progress`：是否在 stderr 展示下载进度条。
    ///
    /// # Errors
    /// 模型下载或 ONNX session 初始化失败时返回中文错误说明。
    pub fn new(cache_dir: &Path, show_progress: bool) -> Result<Embedder, String> {
        let options = InitOptions::new(EmbeddingModel::BGESmallZHV15)
            .with_cache_dir(cache_dir.to_path_buf())
            .with_intra_threads(INTRA_THREADS)
            .with_show_download_progress(show_progress);
        let inner = TextEmbedding::try_new(options)
            .map_err(|e| format!("初始化嵌入模型失败（缓存目录 {}）：{e}", cache_dir.display()))?;
        Ok(Embedder { inner })
    }

    /// 文档侧批量嵌入（**不加**指令前缀）。
    ///
    /// # Errors
    /// 推理失败时返回中文错误说明。
    pub fn embed_docs(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
        self.inner
            .embed(texts, None)
            .map_err(|e| format!("文档嵌入失败：{e}"))
    }

    /// query 侧嵌入（自动拼接 [`QUERY_PREFIX`]）。
    ///
    /// # Errors
    /// 推理失败，或模型未返回任何向量（理论上不会）时返回中文错误说明。
    pub fn embed_query(&mut self, query: &str) -> Result<Vec<f32>, String> {
        let prefixed = format!("{QUERY_PREFIX}{query}");
        let mut vecs = self
            .inner
            .embed(vec![prefixed], None)
            .map_err(|e| format!("查询嵌入失败：{e}"))?;
        vecs.pop().ok_or_else(|| "嵌入模型未返回向量".to_string())
    }
}

/// 检查模型是否已在缓存中就绪（hf-hub 布局：`snapshots/<rev>/` 下同时存在
/// `onnx/model.onnx` 与 `tokenizer.json`）。用于在触发下载前给用户体积提示。
pub fn model_ready(cache_dir: &Path) -> bool {
    find_in_snapshots(cache_dir, &["onnx", "model.onnx"]).is_some()
        && find_in_snapshots(cache_dir, &["tokenizer.json"]).is_some()
}

/// 在缓存中定位 bge 的 tokenizer.json（供分块器按真实 token 计数）。
/// 模型未就绪时返回 `None`，调用方退化为字符计数。
pub fn find_tokenizer_json(cache_dir: &Path) -> Option<PathBuf> {
    find_in_snapshots(cache_dir, &["tokenizer.json"])
}

/// 在 `<cache>/<repo>/snapshots/*/<segments...>` 下找第一个存在的文件。
fn find_in_snapshots(cache_dir: &Path, segments: &[&str]) -> Option<PathBuf> {
    let snapshots = cache_dir.join(MODEL_REPO_DIR).join("snapshots");
    let entries = std::fs::read_dir(&snapshots).ok()?;
    for entry in entries.flatten() {
        let mut candidate = entry.path();
        for seg in segments {
            candidate = candidate.join(seg);
        }
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 缓存布局探测：构造假的 hf-hub 目录结构验证就绪判断与 tokenizer 定位。
    #[test]
    fn model_ready_and_tokenizer_lookup() {
        let tmp = tempfile::tempdir().expect("建临时目录失败");
        let cache = tmp.path();
        assert!(!model_ready(cache));
        assert!(find_tokenizer_json(cache).is_none());

        let snap = cache.join(MODEL_REPO_DIR).join("snapshots").join("abc123");
        fs::create_dir_all(snap.join("onnx")).expect("建目录失败");
        fs::write(snap.join("onnx").join("model.onnx"), b"x").expect("写文件失败");
        // 只有 model.onnx 还不算就绪。
        assert!(!model_ready(cache));
        fs::write(snap.join("tokenizer.json"), b"{}").expect("写文件失败");
        assert!(model_ready(cache));
        assert_eq!(
            find_tokenizer_json(cache).expect("应能找到"),
            snap.join("tokenizer.json")
        );
    }

    /// query 前缀为官方原文（防误改：该常量进了已建索引的语义空间，改了必须
    /// 递增 PIPELINE_VERSION 全量重建）。
    #[test]
    fn query_prefix_is_official() {
        assert_eq!(QUERY_PREFIX, "为这个句子生成表示以用于检索相关文章：");
    }
}
