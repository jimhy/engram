//! 混合检索 over-fetch 的集成测试。
//!
//! 修复目标：lancedb 0.30 的 `execute_hybrid` 把查询上的同一个 limit 克隆给
//! FTS 路与向量路各自执行——若直接用用户 limit（默认 8），单路秩在 8 开外、
//! 但两路综合相关的 chunk 永远进不了 RRF 融合。本测试手工构造一个「FTS 秩 9 +
//! 向量秩 9」的 chunk：修复前它连融合候选都进不了，修复后应稳定出现在结果里；
//! 同时验证融合结果在 Rust 侧截回用户 limit。
//!
//! 不依赖嵌入模型：向量与文本全部手工构造，直接走 store 层（离线、快速）。

use engram_kb::chunker::Chunk;
use engram_kb::embedder::VECTOR_DIM;
use engram_kb::store::{self, KbStore};

/// 构造 tokio runtime（本 crate 未启 tokio 的 macros feature，手动 block_on）。
fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("构建 tokio runtime 失败")
}

/// 稀疏向量：仅第 `i` 维为 1 的单位基向量 e_i。
fn basis(i: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; VECTOR_DIM as usize];
    v[i] = 1.0;
    v
}

/// e0 与 e_i 的线性组合（Cosine 距离与模长无关，不必归一化）。
fn blend(w0: f32, i: usize, wi: f32) -> Vec<f32> {
    let mut v = vec![0.0f32; VECTOR_DIM as usize];
    v[0] = w0;
    v[i] = wi;
    v
}

/// 造一个最小 chunk（面包屑留空，正文即全部内容）。
fn chunk(ord: i32, text: &str) -> Chunk {
    Chunk { breadcrumb: String::new(), text: text.to_string(), ord }
}

/// 双路秩 9 的 chunk 应被 over-fetch 捞回融合、进入最终 top-8；结果截回 limit。
#[test]
fn overfetch_recovers_rank9_chunk_and_truncates() {
    rt().block_on(async {
        let tmp = tempfile::tempdir().expect("建临时目录失败");
        let kb = KbStore::open(tmp.path()).await.expect("打开库失败");
        let table = kb.open_or_create_table().await.expect("建表失败");

        // 查询：文本「苹果」+ 向量 e0。
        //
        // A 组（8 个 chunk）：「苹果」高频短文本 → FTS 路 top-8；向量与查询正交。
        let a_chunks: Vec<Chunk> = (0..8)
            .map(|i| chunk(i, "苹果苹果苹果苹果苹果苹果苹果苹果苹果苹果"))
            .collect();
        let a_vecs: Vec<Vec<f32>> = (0..8).map(|i| basis(100 + i)).collect();
        store::add_chunks(&table, "docs/fts强.md", "hash-a", &a_chunks, a_vecs)
            .await
            .expect("写入 A 组失败");

        // B 组（8 个 chunk）：向量与查询夹角极小 → 向量路 top-8；不含查询词。
        let b_texts: Vec<String> = (0..8).map(|i| format!("与查询词毫不相干的内容第{i}号")).collect();
        let b_chunks: Vec<Chunk> =
            b_texts.iter().enumerate().map(|(i, t)| chunk(i as i32, t)).collect();
        let b_vecs: Vec<Vec<f32>> = (1..=8).map(|i| blend(1.0, i, 0.1)).collect();
        store::add_chunks(&table, "docs/向量强.md", "hash-b", &b_chunks, b_vecs)
            .await
            .expect("写入 B 组失败");

        // X：两路秩都是 9——「苹果」仅出现一次且正文更长（BM25 低于 A 组全部），
        // 向量与查询的夹角略大于 B 组全部但远小于其它行。RRF 里双路秩 9 的融合分
        // 高于任何单路秩 1，修复前它连候选都进不了（每路只取 top-limit）。
        let x_chunks = [chunk(
            0,
            "这一段落在讲水果的储存方法，其中苹果这个词只出现了一次，\
             正文写得相对较长一些，用来压低它在全文检索里的 BM25 得分。",
        )];
        let x_vecs = vec![blend(1.0, 9, 0.3)];
        store::add_chunks(&table, "docs/双路秩九.md", "hash-x", &x_chunks, x_vecs)
            .await
            .expect("写入 X 失败");

        // 填充 30 行：不含查询词；向量带微弱的 e0 分量（Cosine 距离略小于 A 组的
        // 正交向量），把 A 组挤出向量路的 top-40——避免 A 组「FTS 高秩 + 向量路
        // 平局擦边进榜」的双路加分把 X 挤出 top-8（RRF 分差在毫厘之间时平局顺序
        // 不可控，测试必须保证 X 的排位有确定余量）。
        let f_texts: Vec<String> = (0..30).map(|i| format!("纯粹的填充语料第{i}号")).collect();
        let f_chunks: Vec<Chunk> =
            f_texts.iter().enumerate().map(|(i, t)| chunk(i as i32, t)).collect();
        let f_vecs: Vec<Vec<f32>> = (10..40).map(|i| blend(0.05, i, 1.0)).collect();
        store::add_chunks(&table, "docs/填充.md", "hash-f", &f_chunks, f_vecs)
            .await
            .expect("写入填充失败");

        store::rebuild_fts_index(&table).await.expect("建 FTS 索引失败");

        // limit=8：内部召回放大到 max(8×5, 40)=40，X（两路秩 9）应被捞回并进入
        // 最终结果；修复前（每路只取 top-8）X 必然缺席。
        let hits = store::search_hybrid(&table, "苹果", basis(0), 8, None)
            .await
            .expect("混合检索失败");
        assert_eq!(hits.len(), 8, "融合后应截回用户 limit（8）");
        assert!(
            hits.iter().any(|h| h.doc_path == "docs/双路秩九.md"),
            "双路秩 9 的 chunk 应进入结果（over-fetch 修复点），实得：{:?}",
            hits.iter().map(|h| h.doc_path.as_str()).collect::<Vec<_>>()
        );

        // 截断行为：limit=3 应恰好返回 3 条（内部召回 40 条不外泄）。
        let hits3 = store::search_hybrid(&table, "苹果", basis(0), 3, None)
            .await
            .expect("混合检索失败");
        assert_eq!(hits3.len(), 3, "融合后应截回用户 limit（3）");
    });
}
