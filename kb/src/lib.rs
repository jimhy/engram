//! Engram 知识库 sidecar（engram-kb）库 crate。
//!
//! 本 crate 与主引擎（`plugin/engine/`）完全独立：不依赖 engine、不读写其 redb，
//! 仅共享「从 cwd 向上找 `.engram/` 锚点」的作用域约定（见 [`scope`]）。
//! 职责是本地 RAG 全链路：
//!
//! - [`chunker`]：markdown/纯文本 → 标题面包屑 + 语义分块（text-splitter，按 bge token 计数）；
//! - [`embedder`]：fastembed + bge-small-zh-v1.5 嵌入（query 侧加官方指令前缀，文档侧不加）；
//! - [`store`]：LanceDB 嵌入式存取（向量 flat 检索 + ngram FTS 倒排 + RRF 混合召回）;
//! - [`manifest`]：已入库文档清单与内容 hash（增量入库判断）；
//! - [`scope`]：作用域锚定与各路径约定；
//! - [`eval`]：检索质量评测（golden 集判定、recall@k / MRR 汇总，纯函数）。
//!
//! 设计文档：`docs/知识库设计文档.md`。

pub mod chunker;
pub mod embedder;
pub mod eval;
pub mod manifest;
pub mod scope;
pub mod store;
