//! 孤儿 chunk 清理的端到端测试：驱动真实的 engram-kb 二进制。
//!
//! 修复目标：增量 ingest 只正向遍历磁盘上现存的文件——被删除的文档以过期内容
//! 继续命中检索，被改名的以新旧两份同内容 chunk 双倍挤占 top-k，`--force` 也
//! 救不了。修复后 ingest 对「本次路径范围内、manifest 有而磁盘无」的条目做差集
//! 清理；`prune` 子命令兜底清理全库孤儿（含范围外的）。
//!
//! 场景构造不经过嵌入模型：预先用库接口把「上一次 ingest 的结果」（chunk +
//! manifest 条目，假向量）种进临时知识库，再删 / 改名源文件后驱动真实二进制。
//! 磁盘上未变更文档的 hash 与 manifest 一致 → ingest 走「无需重嵌入」路径，
//! 不触发模型加载 / 下载，测试保持离线与快速。

use std::path::Path;
use std::process::{Command, Output};

use engram_kb::chunker::Chunk;
use engram_kb::embedder::VECTOR_DIM;
use engram_kb::manifest::{content_hash, DocEntry, Manifest};
use engram_kb::store::{self, KbStore};

/// 构造 tokio runtime（本 crate 未启 tokio 的 macros feature，手动 block_on）。
fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("构建 tokio runtime 失败")
}

/// 建一个假的 engram 项目锚点：`.engram/engram.redb` 占位文件（scope 只查存在性）。
fn make_anchor(root: &Path) {
    std::fs::create_dir_all(root.join(".engram")).expect("建 .engram 失败");
    std::fs::write(root.join(".engram").join("engram.redb"), b"x").expect("写锚点占位失败");
}

/// 把一份「已入库文档」种进库与 manifest（假向量，模拟上一次真实 ingest 的产物）。
/// manifest 里记录的 hash 与 `content` 一致：磁盘上同内容的文件再 ingest 会被跳过。
async fn seed_doc(
    table: &lancedb::Table,
    manifest: &mut Manifest,
    doc_path: &str,
    content: &str,
) {
    let hash = content_hash(content);
    let chunks = vec![Chunk { breadcrumb: String::new(), text: content.to_string(), ord: 0 }];
    let vectors = vec![vec![0.1f32; VECTOR_DIM as usize]];
    store::add_chunks(table, doc_path, &hash, &chunks, vectors)
        .await
        .expect("种入 chunk 失败");
    manifest
        .docs
        .insert(doc_path.to_string(), DocEntry { hash, chunks: 1, ingested_at: 0.0 });
}

/// 在 `cwd` 下运行 engram-kb 二进制（cargo 为集成测试提供 bin 路径环境变量）。
fn run_kb(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_engram-kb"))
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("启动 engram-kb 失败")
}

/// 解析二进制的单行 JSON 输出（stdout）。
fn parse_json(out: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout 不是合法 JSON（{e}）：{stdout}"))
}

/// 断言某文档在表里的 chunk 数。
async fn assert_chunk_count(lance_dir: &Path, doc: &str, expect: usize) {
    let kb = KbStore::open(lance_dir).await.expect("打开库失败");
    let table = kb.open_table().await.expect("打开表失败");
    let got = store::fetch_doc_chunks(&table, doc).await.expect("读 chunk 失败").len();
    assert_eq!(got, expect, "文档 {doc} 的 chunk 数不符");
}

/// 删除场景：入库两文档 → 删掉一个源文件 → 再 ingest → 旧 chunk 与 manifest
/// 条目应消失，幸存文档不受影响，且全程不触发重嵌入。
#[test]
fn ingest_cleans_deleted_doc() {
    let tmp = tempfile::tempdir().expect("建临时目录失败");
    let root = tmp.path();
    make_anchor(root);
    std::fs::create_dir_all(root.join("docs")).expect("建 docs 失败");
    let a_content = "# 甲\n\n甲文档正文。\n";
    std::fs::write(root.join("docs").join("a.md"), a_content).expect("写 a.md 失败");
    // b.md 不写盘：模拟「曾经入库、之后被从磁盘删除」。

    let lance_dir = root.join(".engram").join("kb").join("lancedb");
    std::fs::create_dir_all(&lance_dir).expect("建库目录失败");
    let mut manifest = Manifest::default();
    rt().block_on(async {
        let kb = KbStore::open(&lance_dir).await.expect("打开库失败");
        let table = kb.open_or_create_table().await.expect("建表失败");
        seed_doc(&table, &mut manifest, "docs/a.md", a_content).await;
        seed_doc(&table, &mut manifest, "docs/b.md", "# 乙\n\n已被删除的文档。\n").await;
    });
    let mpath = root.join(".engram").join("kb").join("manifest.json");
    manifest.save(&mpath).expect("保存 manifest 失败");

    let out = run_kb(root, &["ingest", "docs", "--json"]);
    assert!(out.status.success(), "ingest 失败：{}", String::from_utf8_lossy(&out.stderr));
    let v = parse_json(&out);
    assert_eq!(v["removed"], 1, "应清理 1 个孤儿：{v}");
    assert_eq!(v["ingested"], 0, "a.md 未变更，不应触发重嵌入：{v}");

    // manifest：b 消失、a 保留。
    let m = Manifest::load(&mpath).expect("读 manifest 失败");
    assert!(m.docs.contains_key("docs/a.md"), "幸存文档的 manifest 条目不应被动");
    assert!(!m.docs.contains_key("docs/b.md"), "孤儿的 manifest 条目应被移除");

    // 表：b 的 chunk 消失、a 的原样保留。
    rt().block_on(async {
        assert_chunk_count(&lance_dir, "docs/b.md", 0).await;
        assert_chunk_count(&lance_dir, "docs/a.md", 1).await;
    });
}

/// 改名场景：old.md 改名为 new.md 后，库里新旧两份同内容 chunk 并存（双倍挤占
/// top-k）→ 再 ingest → 旧路径的 chunk 与 manifest 条目应消失，新路径原样保留。
#[test]
fn ingest_cleans_renamed_doc() {
    let tmp = tempfile::tempdir().expect("建临时目录失败");
    let root = tmp.path();
    make_anchor(root);
    std::fs::create_dir_all(root.join("docs")).expect("建 docs 失败");
    let content = "# 改名文档\n\n改名前后内容完全相同。\n";
    // 磁盘上只有新名字；库里新旧两条都在（改名后跑过一次旧版 ingest 的真实惨状）。
    std::fs::write(root.join("docs").join("new.md"), content).expect("写 new.md 失败");

    let lance_dir = root.join(".engram").join("kb").join("lancedb");
    std::fs::create_dir_all(&lance_dir).expect("建库目录失败");
    let mut manifest = Manifest::default();
    rt().block_on(async {
        let kb = KbStore::open(&lance_dir).await.expect("打开库失败");
        let table = kb.open_or_create_table().await.expect("建表失败");
        seed_doc(&table, &mut manifest, "docs/old.md", content).await;
        seed_doc(&table, &mut manifest, "docs/new.md", content).await;
    });
    let mpath = root.join(".engram").join("kb").join("manifest.json");
    manifest.save(&mpath).expect("保存 manifest 失败");

    let out = run_kb(root, &["ingest", "docs", "--json"]);
    assert!(out.status.success(), "ingest 失败：{}", String::from_utf8_lossy(&out.stderr));
    let v = parse_json(&out);
    assert_eq!(v["removed"], 1, "应清理旧名字这 1 个孤儿：{v}");
    assert_eq!(v["ingested"], 0, "new.md 未变更，不应触发重嵌入：{v}");

    let m = Manifest::load(&mpath).expect("读 manifest 失败");
    assert!(!m.docs.contains_key("docs/old.md"), "旧名字的 manifest 条目应被移除");
    assert!(m.docs.contains_key("docs/new.md"), "新名字的 manifest 条目应保留");

    rt().block_on(async {
        assert_chunk_count(&lance_dir, "docs/old.md", 0).await;
        assert_chunk_count(&lance_dir, "docs/new.md", 1).await;
    });
}

/// prune：--dry-run 只列不删；正式运行清掉**全部**孤儿（含 ingest 范围外的），
/// 幸存文档不受影响。
#[test]
fn prune_lists_then_removes_all_orphans() {
    let tmp = tempfile::tempdir().expect("建临时目录失败");
    let root = tmp.path();
    make_anchor(root);
    std::fs::create_dir_all(root.join("notes")).expect("建 notes 失败");
    let keep_content = "# 保留\n\n仍在磁盘上的文档。\n";
    std::fs::write(root.join("notes").join("keep.md"), keep_content).expect("写 keep.md 失败");

    let lance_dir = root.join(".engram").join("kb").join("lancedb");
    std::fs::create_dir_all(&lance_dir).expect("建库目录失败");
    let mut manifest = Manifest::default();
    rt().block_on(async {
        let kb = KbStore::open(&lance_dir).await.expect("打开库失败");
        let table = kb.open_or_create_table().await.expect("建表失败");
        seed_doc(&table, &mut manifest, "notes/keep.md", keep_content).await;
        // 两个孤儿：一个在 notes/ 下，一个在完全不同的目录（模拟范围外残留）。
        seed_doc(&table, &mut manifest, "notes/gone.md", "# 已删\n\n没了。\n").await;
        seed_doc(&table, &mut manifest, "elsewhere/lost.md", "# 别处\n\n也没了。\n").await;
    });
    let mpath = root.join(".engram").join("kb").join("manifest.json");
    manifest.save(&mpath).expect("保存 manifest 失败");

    // --dry-run：列出两个孤儿，但 manifest 与 chunk 都原封不动。
    let out = run_kb(root, &["prune", "--dry-run", "--json"]);
    assert!(out.status.success(), "prune --dry-run 失败：{}", String::from_utf8_lossy(&out.stderr));
    let v = parse_json(&out);
    assert_eq!(v["removed"], 0, "--dry-run 不应删除任何东西：{v}");
    let orphans: Vec<&str> =
        v["orphans"].as_array().expect("orphans 应为数组").iter().filter_map(|o| o.as_str()).collect();
    assert_eq!(orphans.len(), 2, "应列出 2 个孤儿：{v}");
    assert!(orphans.contains(&"notes/gone.md") && orphans.contains(&"elsewhere/lost.md"));
    let m = Manifest::load(&mpath).expect("读 manifest 失败");
    assert_eq!(m.docs.len(), 3, "--dry-run 后 manifest 应原封不动");
    rt().block_on(async {
        assert_chunk_count(&lance_dir, "notes/gone.md", 1).await;
    });

    // 正式 prune：两个孤儿全清，幸存文档不受影响。
    let out = run_kb(root, &["prune", "--json"]);
    assert!(out.status.success(), "prune 失败：{}", String::from_utf8_lossy(&out.stderr));
    let v = parse_json(&out);
    assert_eq!(v["removed"], 2, "应清理 2 个孤儿：{v}");
    let m = Manifest::load(&mpath).expect("读 manifest 失败");
    assert_eq!(m.docs.len(), 1);
    assert!(m.docs.contains_key("notes/keep.md"));
    rt().block_on(async {
        assert_chunk_count(&lance_dir, "notes/gone.md", 0).await;
        assert_chunk_count(&lance_dir, "elsewhere/lost.md", 0).await;
        assert_chunk_count(&lance_dir, "notes/keep.md", 1).await;
    });
}
