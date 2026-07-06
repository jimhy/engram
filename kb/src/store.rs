//! LanceDB 存取层：建表 / 入块 / 删除 / FTS 索引 / 混合检索。
//!
//! 设计要点（设计文档 §3 / §5.2）：
//! - 嵌入式本地连接（`connect(目录)`），无任何服务进程；
//! - **向量检索不建 ANN 索引**——LanceDB 无索引时自动 flat 暴力扫，官方称
//!   几十万行内可接受；chunk 数上到十万级再上 `Index::Auto`（v2）；
//! - **FTS 必须有倒排索引才能查**（无索引直接报错），用 ngram(2,2) 分词器：
//!   零外部依赖的中文兜底（jieba 需外置词典，留 v2）；每次 ingest 尾部
//!   `replace=true` 重建——建索引后新增的行不在旧倒排里，重建保证全量可查；
//! - 混合检索：FTS(BM25) + 向量(Cosine) 双路召回，SDK 默认 RRF 融合。

use std::path::Path;
use std::sync::Arc;

use arrow_array::types::Float32Type;
use arrow_array::{
    Array, FixedSizeListArray, Float32Array, Int32Array, RecordBatch, StringArray,
};
use arrow_schema::{DataType, Field, Schema};
use futures::TryStreamExt;
use lancedb::index::scalar::{FtsIndexBuilder, FullTextSearchQuery};
use lancedb::index::Index;
use lancedb::query::{ExecutableQuery, QueryBase, QueryExecutionOptions};
use lancedb::{connect, Connection, DistanceType, Table};

use crate::chunker::Chunk;
use crate::embedder::VECTOR_DIM;

/// chunk 表名。
pub const TABLE_NAME: &str = "chunks";

/// 一条检索命中。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Hit {
    /// chunk id（`<doc_hash 前 12 位>-<序号>`）。
    pub id: String,
    /// 源文档相对路径（指针，引导去查 ground truth）。
    pub doc_path: String,
    /// 标题面包屑（指针的章节定位）。
    pub breadcrumb: String,
    /// chunk 正文。
    pub text: String,
    /// 融合相关性分（RRF；解析不到该列时为 None）。
    pub score: Option<f64>,
}

/// 已打开的知识库存储。
pub struct KbStore {
    conn: Connection,
}

impl KbStore {
    /// 连接（或创建）`db_dir` 下的 LanceDB 库目录。
    ///
    /// # Errors
    /// 路径非 UTF-8 或连接失败时返回中文错误说明。
    pub async fn open(db_dir: &Path) -> Result<KbStore, String> {
        let uri = db_dir
            .to_str()
            .ok_or_else(|| format!("知识库路径 {} 含非 UTF-8 字符", db_dir.display()))?;
        let conn = connect(uri)
            .execute()
            .await
            .map_err(|e| format!("打开知识库 {} 失败：{e}", db_dir.display()))?;
        Ok(KbStore { conn })
    }

    /// chunks 表是否已存在。
    ///
    /// # Errors
    /// 列举表名失败时返回中文错误说明。
    pub async fn table_exists(&self) -> Result<bool, String> {
        let names = self
            .conn
            .table_names()
            .execute()
            .await
            .map_err(|e| format!("列举知识库表失败：{e}"))?;
        Ok(names.iter().any(|n| n == TABLE_NAME))
    }

    /// 打开 chunks 表；不存在则按 schema 建空表。
    ///
    /// # Errors
    /// 打开 / 建表失败时返回中文错误说明。
    pub async fn open_or_create_table(&self) -> Result<Table, String> {
        if self.table_exists().await? {
            self.conn
                .open_table(TABLE_NAME)
                .execute()
                .await
                .map_err(|e| format!("打开表 {TABLE_NAME} 失败：{e}"))
        } else {
            self.conn
                .create_empty_table(TABLE_NAME, chunk_schema())
                .execute()
                .await
                .map_err(|e| format!("创建表 {TABLE_NAME} 失败：{e}"))
        }
    }

    /// 删除 chunks 表（若存在）。管线版本变更时全量重建用（设计文档 §9.8）。
    ///
    /// # Errors
    /// 删表失败时返回中文错误说明。
    pub async fn drop_table_if_exists(&self) -> Result<(), String> {
        if self.table_exists().await? {
            self.conn
                .drop_table(TABLE_NAME, &[])
                .await
                .map_err(|e| format!("删除表 {TABLE_NAME} 失败：{e}"))?;
        }
        Ok(())
    }

    /// 打开 chunks 表（必须已存在；不存在视为「知识库为空」的用户级错误）。
    ///
    /// # Errors
    /// 表不存在或打开失败时返回中文错误说明。
    pub async fn open_table(&self) -> Result<Table, String> {
        if !self.table_exists().await? {
            return Err("知识库为空（chunks 表不存在），请先执行 ingest 入库文档".to_string());
        }
        self.conn
            .open_table(TABLE_NAME)
            .execute()
            .await
            .map_err(|e| format!("打开表 {TABLE_NAME} 失败：{e}"))
    }
}

/// chunks 表的 arrow schema（设计文档 §4 表格）。
fn chunk_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("doc_path", DataType::Utf8, false),
        Field::new("doc_hash", DataType::Utf8, false),
        Field::new("breadcrumb", DataType::Utf8, true),
        Field::new("text", DataType::Utf8, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                VECTOR_DIM,
            ),
            true,
        ),
        Field::new("ord", DataType::Int32, false),
    ]))
}

/// 把一个文档的全部 chunk（连同向量）追加进表。
///
/// `chunks` 与 `vectors` 一一对应（调用方保证等长；这里再校验一次防御）。
///
/// # Errors
/// 两列表长度不一致、构造 RecordBatch 失败或写入失败时返回中文错误说明。
pub async fn add_chunks(
    table: &Table,
    doc_path: &str,
    doc_hash: &str,
    chunks: &[Chunk],
    vectors: Vec<Vec<f32>>,
) -> Result<(), String> {
    if chunks.len() != vectors.len() {
        return Err(format!(
            "内部错误：chunk 数（{}）与向量数（{}）不一致",
            chunks.len(),
            vectors.len()
        ));
    }
    if chunks.is_empty() {
        return Ok(());
    }
    // id 前缀掺入 doc_path：内容完全相同的两份不同文档（doc_hash 相同）也不会
    // 产生重复 id。12 位 hex 对同库文档量级（≪ 2^48）足以避免碰撞。
    let id_prefix: String = crate::manifest::content_hash(&format!("{doc_path}\n{doc_hash}"))
        .chars()
        .take(12)
        .collect();
    let n = chunks.len();

    let ids: Vec<String> = chunks.iter().map(|c| format!("{id_prefix}-{}", c.ord)).collect();
    let paths: Vec<&str> = std::iter::repeat_n(doc_path, n).collect();
    let hashes: Vec<&str> = std::iter::repeat_n(doc_hash, n).collect();
    let crumbs: Vec<&str> = chunks.iter().map(|c| c.breadcrumb.as_str()).collect();
    let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
    let ords: Vec<i32> = chunks.iter().map(|c| c.ord).collect();
    let vector_array = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        vectors
            .into_iter()
            .map(|v| Some(v.into_iter().map(Some).collect::<Vec<_>>())),
        VECTOR_DIM,
    );

    let batch = RecordBatch::try_new(
        chunk_schema(),
        vec![
            Arc::new(StringArray::from(ids)),
            Arc::new(StringArray::from(paths)),
            Arc::new(StringArray::from(hashes)),
            Arc::new(StringArray::from(crumbs)),
            Arc::new(StringArray::from(texts)),
            Arc::new(vector_array),
            Arc::new(Int32Array::from(ords)),
        ],
    )
    .map_err(|e| format!("构造 RecordBatch 失败：{e}"))?;

    table
        .add(batch)
        .execute()
        .await
        .map(|_| ())
        .map_err(|e| format!("写入文档 {doc_path} 的 chunk 失败：{e}"))
}

/// 删除某文档的全部 chunk（按 doc_path 精确匹配；单引号按 SQL 规则转义）。
///
/// # Errors
/// 删除失败时返回中文错误说明。
pub async fn delete_doc(table: &Table, doc_path: &str) -> Result<(), String> {
    let escaped = doc_path.replace('\'', "''");
    table
        .delete(&format!("doc_path = '{escaped}'"))
        .await
        .map(|_| ())
        .map_err(|e| format!("删除文档 {doc_path} 的 chunk 失败：{e}"))
}

/// （重）建 text 列的 FTS 倒排索引：ngram(2,2) 分词、关闭英文系处理。
///
/// 每次 ingest 尾部调用（`replace=true`）：建索引后新增的行不在旧倒排里，
/// 重建保证全量可查。
///
/// # Errors
/// 建索引失败时返回中文错误说明。
pub async fn rebuild_fts_index(table: &Table) -> Result<(), String> {
    let fts = FtsIndexBuilder::default()
        .base_tokenizer("ngram".to_owned())
        .ngram_min_length(2)
        .ngram_max_length(2)
        .stem(false)
        .remove_stop_words(false)
        .ascii_folding(false);
    table
        .create_index(&["text"], Index::FTS(fts))
        .replace(true)
        .execute()
        .await
        .map_err(|e| format!("构建全文索引失败：{e}"))
}

/// 混合检索：FTS(BM25) + 向量(Cosine) 双路召回，RRF 融合（SDK 默认 reranker）。
///
/// # 参数
/// - `query_text`：原始查询文本（FTS 用）；
/// - `query_vector`：查询向量（已带指令前缀编码）；
/// - `limit`：返回条数上限；
/// - `filter`：可选 SQL 谓词（如 `doc_path LIKE 'docs/%'`），prefilter 语义。
///
/// # Errors
/// 查询构造或执行失败时返回中文错误说明。
pub async fn search_hybrid(
    table: &Table,
    query_text: &str,
    query_vector: Vec<f32>,
    limit: usize,
    filter: Option<&str>,
) -> Result<Vec<Hit>, String> {
    let mut q = table
        .query()
        .full_text_search(FullTextSearchQuery::new(query_text.to_owned()))
        .nearest_to(query_vector)
        .map_err(|e| format!("构造向量查询失败：{e}"))?
        .distance_type(DistanceType::Cosine)
        .limit(limit);
    if let Some(f) = filter {
        q = q.only_if(f);
    }
    let batches: Vec<RecordBatch> = q
        .execute_hybrid(QueryExecutionOptions::default())
        .await
        .map_err(|e| format!("混合检索执行失败：{e}"))?
        .try_collect()
        .await
        .map_err(|e| format!("读取检索结果失败：{e}"))?;
    Ok(parse_hits(&batches))
}

/// 表内 chunk 总数。
///
/// # Errors
/// 统计失败时返回中文错误说明。
pub async fn count_rows(table: &Table) -> Result<usize, String> {
    table
        .count_rows(None)
        .await
        .map_err(|e| format!("统计 chunk 数失败：{e}"))
}

/// 取某文档的全部 chunk（按 `ord` 升序），供看板 / `dump` 读原文用。
///
/// 纯 SQL 谓词过滤（`doc_path` 精确匹配，单引号按 SQL 规则转义），**不走向量检索、
/// 不做重排**，故 [`Hit::score`] 恒为 `None`。LanceDB 查询不保证行序，结果在
/// Rust 侧按 `ord` 稳定排序还原文档内顺序。
///
/// # Errors
/// 查询构造或执行失败时返回中文错误说明。
pub async fn fetch_doc_chunks(table: &Table, doc_path: &str) -> Result<Vec<Hit>, String> {
    let escaped = doc_path.replace('\'', "''");
    let batches: Vec<RecordBatch> = table
        .query()
        .only_if(format!("doc_path = '{escaped}'"))
        .execute()
        .await
        .map_err(|e| format!("查询文档 {doc_path} 的 chunk 失败：{e}"))?
        .try_collect()
        .await
        .map_err(|e| format!("读取文档 {doc_path} 的 chunk 失败：{e}"))?;
    let mut rows = parse_doc_rows(&batches);
    rows.sort_by_key(|(ord, _)| *ord);
    Ok(rows.into_iter().map(|(_, hit)| hit).collect())
}

/// 从查询结果批里解析 `(ord, Hit)` 列表（容错：缺列给空串 / 0，不 panic）。
/// 与 [`parse_hits`] 的区别：额外取 `ord` 列供排序，且不解析融合分（纯过滤无该列）。
fn parse_doc_rows(batches: &[RecordBatch]) -> Vec<(i32, Hit)> {
    let mut rows = Vec::new();
    for batch in batches {
        let str_col = |name: &str| -> Option<&StringArray> {
            batch
                .column_by_name(name)
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        };
        let id = str_col("id");
        let doc_path = str_col("doc_path");
        let breadcrumb = str_col("breadcrumb");
        let text = str_col("text");
        let ord_col = batch
            .column_by_name("ord")
            .and_then(|c| c.as_any().downcast_ref::<Int32Array>());

        for row in 0..batch.num_rows() {
            let get = |col: Option<&StringArray>| -> String {
                col.map_or(String::new(), |a| {
                    if a.is_valid(row) { a.value(row).to_string() } else { String::new() }
                })
            };
            let ord = ord_col.map_or(0, |a| if a.is_valid(row) { a.value(row) } else { 0 });
            rows.push((
                ord,
                Hit {
                    id: get(id),
                    doc_path: get(doc_path),
                    breadcrumb: get(breadcrumb),
                    text: get(text),
                    score: None,
                },
            ));
        }
    }
    rows
}

/// 从查询结果批里解析 [`Hit`] 列表（容错：缺列给空串 / None，不 panic）。
fn parse_hits(batches: &[RecordBatch]) -> Vec<Hit> {
    let mut hits = Vec::new();
    for batch in batches {
        let str_col = |name: &str| -> Option<&StringArray> {
            batch
                .column_by_name(name)
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        };
        let id = str_col("id");
        let doc_path = str_col("doc_path");
        let breadcrumb = str_col("breadcrumb");
        let text = str_col("text");
        // RRF 融合分列名 `_relevance_score`（Float32）；取不到则保持 None。
        let score = batch
            .column_by_name("_relevance_score")
            .and_then(|c| c.as_any().downcast_ref::<Float32Array>());

        for row in 0..batch.num_rows() {
            let get = |col: Option<&StringArray>| -> String {
                col.map_or(String::new(), |a| {
                    if a.is_valid(row) { a.value(row).to_string() } else { String::new() }
                })
            };
            hits.push(Hit {
                id: get(id),
                doc_path: get(doc_path),
                breadcrumb: get(breadcrumb),
                text: get(text),
                score: score.and_then(|a| a.is_valid(row).then(|| f64::from(a.value(row)))),
            });
        }
    }
    hits
}
