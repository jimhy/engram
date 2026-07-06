//! engram-kb 二进制入口：知识库 sidecar 的 CLI 编排层。
//!
//! 与主引擎 CLI 同风格：clap derive + kebab-case、每参数中文 doc 注释、
//! `--json` 可选结构化输出（单行）、错误中文走 stderr + 非 0 退出、不 panic。
//!
//! 子命令（设计文档 §6）：
//! - `ingest`：文档入库 / 增量更新（分块 → 嵌入 → LanceDB → 重建 FTS 索引）；
//! - `search`：混合检索（向量 Cosine + ngram BM25，RRF 融合）；
//! - `list`：已入库文档清单（读 manifest）；
//! - `remove`：删除某文档的全部 chunk；
//! - `status`：库统计 + 模型就绪状态；
//! - `ensure-model`：预下载 / 校验嵌入模型。
//!
//! 知识库目录解析：`--db` 显式指定优先；否则从 cwd 向上锚定 `.engram/`（与主
//! 引擎同判据）取 `<锚点>/.engram/kb/`。模型缓存恒为 `<HOME>/.engram/kb/models/`
//! （全局共享，进程入口移除 `HF_HOME` 防静默覆盖）。

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};

use engram_kb::chunker::{Chunk, Chunker};
use engram_kb::embedder::{self, Embedder};
use engram_kb::manifest::{content_hash, DocEntry, Manifest, PIPELINE_VERSION};
use engram_kb::scope;
use engram_kb::store::{self, KbStore};

/// Engram 知识库 sidecar —— 本地 RAG：分块 + 中文嵌入 + 混合检索。
#[derive(Parser, Debug)]
#[command(name = "engram-kb", version, about = "Engram 知识库 sidecar（本地 RAG）")]
struct Cli {
    /// 子命令。
    #[command(subcommand)]
    command: Command,
}

/// 支持的子命令。
#[derive(Subcommand, Debug)]
enum Command {
    /// 文档入库 / 增量更新：分块 → 嵌入 → 写 LanceDB → 重建全文索引。
    Ingest {
        /// 知识库目录；缺省从 cwd 向上锚定 `.engram/` 取 `<锚点>/.engram/kb/`。
        #[arg(long)]
        db: Option<PathBuf>,
        /// 入库到用户级共享知识库（`<HOME>/.engram/kb/store/`）而非项目库；与 `--db` 互斥。
        #[arg(long)]
        shared: bool,
        /// 要入库的文件或目录（目录递归遍历），可给多个。
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// 接受的扩展名（逗号分隔，不带点）。
        #[arg(long, default_value = "md,txt")]
        ext: String,
        /// 忽略增量判断，强制全部重新入库。
        #[arg(long)]
        force: bool,
        /// 以单行 JSON 输出结果（默认输出可读文本）。
        #[arg(long)]
        json: bool,
    },
    /// 混合检索：向量（Cosine）+ 全文（ngram BM25），RRF 融合。
    Search {
        /// 知识库目录；缺省同 ingest 的锚定规则。
        #[arg(long)]
        db: Option<PathBuf>,
        /// 使用用户级共享知识库（`<HOME>/.engram/kb/store/`）而非项目库；与 `--db` 互斥。
        #[arg(long)]
        shared: bool,
        /// 查询文本（编码时自动加 BGE 官方指令前缀）。
        #[arg(long)]
        query: String,
        /// 最多返回的命中数。
        #[arg(long, default_value_t = 8)]
        limit: usize,
        /// 可选 SQL 过滤谓词（如 `doc_path LIKE 'docs/%'`）。
        #[arg(long)]
        filter: Option<String>,
        /// 以单行 JSON 输出命中（默认输出可读文本）。
        #[arg(long)]
        json: bool,
    },
    /// 已入库文档清单（读 manifest，不查表）。
    List {
        /// 知识库目录；缺省同 ingest 的锚定规则。
        #[arg(long)]
        db: Option<PathBuf>,
        /// 使用用户级共享知识库（`<HOME>/.engram/kb/store/`）而非项目库；与 `--db` 互斥。
        #[arg(long)]
        shared: bool,
        /// 以单行 JSON 输出（默认输出可读文本）。
        #[arg(long)]
        json: bool,
    },
    /// 删除某文档的全部 chunk（并更新 manifest 与全文索引）。
    Remove {
        /// 知识库目录；缺省同 ingest 的锚定规则。
        #[arg(long)]
        db: Option<PathBuf>,
        /// 使用用户级共享知识库（`<HOME>/.engram/kb/store/`）而非项目库；与 `--db` 互斥。
        #[arg(long)]
        shared: bool,
        /// 要删除的文档路径（须与 `list` 列出的 doc_path 完全一致）。
        #[arg(long)]
        doc: String,
        /// 以单行 JSON 输出（默认输出可读文本）。
        #[arg(long)]
        json: bool,
    },
    /// 知识库概况：文档数 / chunk 数 / 模型就绪状态。
    Status {
        /// 知识库目录；缺省同 ingest 的锚定规则。
        #[arg(long)]
        db: Option<PathBuf>,
        /// 使用用户级共享知识库（`<HOME>/.engram/kb/store/`）而非项目库；与 `--db` 互斥。
        #[arg(long)]
        shared: bool,
        /// 以单行 JSON 输出（默认输出可读文本）。
        #[arg(long)]
        json: bool,
    },
    /// 导出某文档的全部 chunk 原文（按 ord 顺序，含章节面包屑）。
    ///
    /// 纯读 LanceDB、不加载嵌入模型；供看板右侧内容区 / 外部工具「顺 doc_path 读原文」。
    Dump {
        /// 知识库目录；缺省同 ingest 的锚定规则。
        #[arg(long)]
        db: Option<PathBuf>,
        /// 从用户级共享知识库（`<HOME>/.engram/kb/store/`）导出而非项目库；与 `--db` 互斥。
        #[arg(long)]
        shared: bool,
        /// 要导出的文档路径（须与 `list` 列出的 doc_path 完全一致）。
        #[arg(long)]
        doc: String,
        /// 以单行 JSON 输出（默认输出可读文本）。
        #[arg(long)]
        json: bool,
    },
    /// 预下载 / 校验嵌入模型（bge-small-zh-v1.5，约 95MB，仅首次联网）。
    EnsureModel {
        /// 以单行 JSON 输出（默认输出可读文本）。
        #[arg(long)]
        json: bool,
    },
    /// SessionStart 注入用：只读 manifest.json 输出本项目知识库的一行概览
    /// （文档数 / chunk 数 / 文档清单），让会话一上来就知道知识库的存在与内容。
    ///
    /// 只读 manifest、**不启动 lancedb、不加载嵌入模型**（轻量、零等待）；不在任何
    /// engram 项目内或库为空时**静默**（输出空、退出 0），保证对未用知识库者零干扰。
    Digest {
        /// 知识库目录；缺省同 ingest 的锚定规则（不在项目内则静默无输出）。
        #[arg(long)]
        db: Option<PathBuf>,
        /// 对用户级共享知识库（`<HOME>/.engram/kb/store/`）出概览而非项目库；与 `--db` 互斥。
        #[arg(long)]
        shared: bool,
        /// 输出格式：text（人读概览，缺省）或 json（包成 Claude Code SessionStart
        /// hook 的 `additionalContext` 单行 JSON，供 kb-session-digest.sh 注入）。
        #[arg(long, default_value = "text")]
        emit: String,
    },
}

/// 进程入口：环境防御 → 构建 tokio runtime → 进入异步主逻辑。
fn main() -> ExitCode {
    // fastembed 的模型拉取逻辑里 HF_HOME 会**静默覆盖** with_cache_dir（源码实证，
    // 设计文档 §9.3），统一移除之，保证模型恒落 ~/.engram/kb/models/。
    // 必须在创建任何线程 / runtime 之前调用（本 crate 为 edition 2021，remove_var 为 safe）。
    std::env::remove_var("HF_HOME");

    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("初始化异步运行时失败：{e}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(dispatch(cli))
}

/// 把子命令分发到各 run_* 函数。
async fn dispatch(cli: Cli) -> ExitCode {
    let result = match cli.command {
        Command::Ingest { db, shared, paths, ext, force, json } => {
            run_ingest(IngestArgs { db, shared, paths, ext, force, json }).await
        }
        Command::Search { db, shared, query, limit, filter, json } => {
            run_search(SearchArgs { db, shared, query, limit, filter, json }).await
        }
        Command::List { db, shared, json } => run_list(db, shared, json),
        Command::Remove { db, shared, doc, json } => run_remove(db, shared, &doc, json).await,
        Command::Status { db, shared, json } => run_status(db, shared, json).await,
        Command::Dump { db, shared, doc, json } => run_dump(db, shared, &doc, json).await,
        Command::EnsureModel { json } => run_ensure_model(json),
        Command::Digest { db, shared, emit } => run_digest(db, shared, &emit),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// 取系统当前时间的 unix 秒（f64）；时钟异常时退化为 0（仅影响 manifest 时间戳展示）。
fn system_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// 去除 Windows `canonicalize` 产生的 verbatim 前缀 `\\?\`（与主引擎同款处理）。
fn strip_verbatim_prefix(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        if !rest.starts_with(r"UNC\") {
            return PathBuf::from(rest.to_string());
        }
    }
    p
}

/// best-effort 规范化路径：canonicalize 成功则剥 verbatim 前缀，失败原样返回。
fn canonicalize_lenient(p: &Path) -> PathBuf {
    match std::fs::canonicalize(p) {
        Ok(c) => strip_verbatim_prefix(c),
        Err(_) => p.to_path_buf(),
    }
}

/// 解析出的知识库上下文：库目录 + 锚点根（doc_path 相对化的基准）。
struct KbContext {
    /// 知识库根目录（manifest.json 所在；LanceDB 数据在其 `lancedb/` 子目录）。
    db_dir: PathBuf,
    /// 锚点根目录（用于把文档路径规范成相对路径；cwd 不在任何锚点内时为 None）。
    anchor_root: Option<PathBuf>,
}

impl KbContext {
    /// LanceDB 数据子目录（设计文档 §4：数据与 manifest.json 分层存放）。
    fn lance_dir(&self) -> PathBuf {
        self.db_dir.join("lancedb")
    }
}

/// 解析知识库目录：`--db` 显式优先；否则从 cwd 向上锚定。
///
/// 锚点**一律尝试推导**（即便 `--db` 显式给出）：锚点同时是 doc_path 相对化的
/// 基准，保证两种调用方式对同一文件生成一致的 doc_path（否则混用会在库里
/// 留下删不掉的重复文档）。
fn resolve_kb_context(db_override: Option<PathBuf>, shared: bool) -> Result<KbContext, String> {
    match resolve_kb_context_optional(db_override, shared)? {
        Some(ctx) => Ok(ctx),
        // 无 --db 且 cwd 不在任何 `.engram/` 锚点内：报错引导建锚点 / 显式指定。
        None => {
            let cwd = std::env::current_dir()
                .map(|c| canonicalize_lenient(&c).display().to_string())
                .unwrap_or_else(|_| "<未知>".to_string());
            Err(format!(
                "当前目录 {cwd} 不在任何 engram 项目内（向上未找到 .engram/ 锚点）。\
                 请先在项目根运行 /engram root 建立锚点，或用 --db 显式指定知识库目录。"
            ))
        }
    }
}

/// 解析知识库目录的**静默变体**（供 `digest` 用）：与 [`resolve_kb_context`] 同款
/// 锚定规则，但「无 `--db`、非共享、且 cwd 不在任何 engram 项目内」时返回 `None`
/// 而非报错——SessionStart 注入要求「不在项目内 → 静默」。`--db` 或 `--shared`
/// 显式给出时一律返回 `Some`。
///
/// `shared` 为真时走用户级共享库（`<HOME>/.engram/kb/store/`，anchor_root 为 None、
/// doc_path 用绝对路径），与 `--db` 互斥。
fn resolve_kb_context_optional(
    db_override: Option<PathBuf>,
    shared: bool,
) -> Result<Option<KbContext>, String> {
    if shared {
        if db_override.is_some() {
            return Err(
                "--shared 与 --db 不能同时使用（二者都在指定知识库位置，请只给其一）".to_string(),
            );
        }
        let dir = scope::resolve_shared_kb_dir(|k| std::env::var(k).ok())?;
        // 共享库不隶属任何锚点：doc_path 一律用绝对路径（anchor_root = None）。
        return Ok(Some(KbContext { db_dir: dir, anchor_root: None }));
    }
    let cwd = std::env::current_dir().map_err(|e| format!("获取当前工作目录失败：{e}"))?;
    let cwd = canonicalize_lenient(&cwd);
    let anchor = scope::find_anchor(&cwd);
    if let Some(db) = db_override {
        return Ok(Some(KbContext { db_dir: db, anchor_root: anchor }));
    }
    Ok(anchor.map(|a| KbContext {
        db_dir: a.join(scope::ENGRAM_DIR).join(scope::KB_DIR),
        anchor_root: Some(a),
    }))
}

/// manifest.json 在知识库目录下的路径。
fn manifest_path(db_dir: &Path) -> PathBuf {
    db_dir.join("manifest.json")
}

/// 把文档绝对路径规范成存储用 doc_path：锚点内取相对路径，否则用绝对路径；
/// 一律正斜杠（跨平台稳定，保证同一文件两次 ingest 的 doc_path 一致）。
fn normalize_doc_path(file: &Path, anchor_root: Option<&Path>) -> String {
    let canon = canonicalize_lenient(file);
    let rel = anchor_root
        .and_then(|root| canon.strip_prefix(root).ok())
        .unwrap_or(&canon);
    rel.to_string_lossy().replace('\\', "/")
}

/// 递归收集待入库文件：文件直接收（不限扩展名），目录递归并按扩展名过滤；
/// `.` 开头的隐藏目录（.git/.engram 等）整体跳过。
///
/// 防环：维护 canonicalize 后的已访问目录集合，symlink/junction 指回祖先目录
/// 不会无限递归（symlink 目录本身也直接跳过，双保险——Windows junction 在
/// `file_type().is_symlink()` 下可能不被识别，已访问集合才是兜底）。
fn collect_files(paths: &[PathBuf], exts: &[String]) -> Result<Vec<PathBuf>, String> {
    use std::collections::BTreeSet;

    fn walk(
        dir: &Path,
        exts: &[String],
        out: &mut Vec<PathBuf>,
        visited: &mut BTreeSet<String>,
    ) -> Result<(), String> {
        let canon = canonicalize_lenient(dir).to_string_lossy().to_string();
        if !visited.insert(canon) {
            return Ok(()); // 已访问过（链接环），跳过。
        }
        let entries = std::fs::read_dir(dir)
            .map_err(|e| format!("读取目录 {} 失败：{e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("遍历目录 {} 失败：{e}", dir.display()))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|e| format!("读取 {} 的类型失败：{e}", path.display()))?;
            if file_type.is_symlink() {
                continue; // 不跟随符号链接（防环 + 防把库外内容卷进来）。
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if file_type.is_dir() {
                if name.starts_with('.') {
                    continue; // 跳过 .git / .engram / .tmp 等隐藏目录。
                }
                walk(&path, exts, out, visited)?;
            } else if let Some(ext) = path.extension() {
                let ext = ext.to_string_lossy().to_ascii_lowercase();
                if exts.iter().any(|e| e == &ext) {
                    out.push(path);
                }
            }
        }
        Ok(())
    }

    let mut out = Vec::new();
    let mut visited = BTreeSet::new();
    for p in paths {
        if p.is_file() {
            out.push(p.clone());
        } else if p.is_dir() {
            walk(p, exts, &mut out, &mut visited)?;
        } else {
            return Err(format!("路径 {} 不存在或既非文件也非目录", p.display()));
        }
    }
    // 按规范化后的真实路径去重：同一文件经相对/绝对/重复传参等多种形式进来只留一份。
    let mut seen = BTreeSet::new();
    out.retain(|p| seen.insert(canonicalize_lenient(p).to_string_lossy().to_string()));
    out.sort();
    Ok(out)
}

/// 初始化嵌入器；模型未就绪时先在 stderr 给出体积预期提示再触发下载。
fn open_embedder() -> Result<(Embedder, PathBuf), String> {
    let cache_dir = scope::model_cache_dir(|k| std::env::var(k).ok())?;
    if !embedder::model_ready(&cache_dir) {
        eprintln!(
            "嵌入模型（bge-small-zh-v1.5，约 95MB）尚未就绪，开始下载（仅首次；\
             国内网络可设 HF_ENDPOINT=https://hf-mirror.com 加速）…"
        );
    }
    let emb = Embedder::new(&cache_dir, true)?;
    Ok((emb, cache_dir))
}

/// 按模型缓存可得性构造分块器：优先 bge tokenizer 精确计 token，缺则字符近似。
fn open_chunker(model_cache: &Path) -> Result<Chunker, String> {
    match embedder::find_tokenizer_json(model_cache) {
        Some(tk) => Chunker::with_tokenizer(&tk),
        None => {
            eprintln!("提示：未找到 bge tokenizer.json，分块退化为按字符计数（质量略降）。");
            Chunker::with_chars()
        }
    }
}

/// `ingest` 的参数集合。
struct IngestArgs {
    db: Option<PathBuf>,
    shared: bool,
    paths: Vec<PathBuf>,
    ext: String,
    force: bool,
    json: bool,
}

/// 执行 `ingest`：收集 → 一致性预检 → 增量比对 → 分块 → 嵌入 → 写库 → 重建 FTS。
///
/// 一致性保证（审查后强化）：
/// - **管线版本变更 → 全量重建**：旧向量与新管线不可比，drop 整张表 + 清空
///   manifest，绝不让两套不可比向量混在同一空间（设计文档 §9.8）；
/// - **manifest 与表互验**：表不存在但 manifest 非空（库被手动删除/迁移）时
///   重置 manifest，避免「为空 → ingest → 均为最新」的死循环；
/// - **逐文档落盘**：每个文档写库成功后立即保存 manifest；删旧之后任一步失败，
///   先把该文档从 manifest 移除再退出，维持「表里没有 ⇒ manifest 也没有」的
///   不变式（否则 --force 重入中途失败会造成永久静默丢块）。
async fn run_ingest(args: IngestArgs) -> Result<(), String> {
    let ctx = resolve_kb_context(args.db, args.shared)?;
    let exts: Vec<String> = args
        .ext
        .split(',')
        .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if exts.is_empty() {
        return Err("--ext 解析后为空（应形如 md,txt）".to_string());
    }

    let files = collect_files(&args.paths, &exts)?;
    if files.is_empty() {
        return Err(format!(
            "在给定路径下没有找到扩展名为 {} 的文件",
            exts.join("/")
        ));
    }

    let mpath = manifest_path(&ctx.db_dir);
    let mut manifest = Manifest::load(&mpath)?;

    // 打开（或创建）库——ingest 是写路径，建目录是其职责。
    let lance_dir = ctx.lance_dir();
    std::fs::create_dir_all(&lance_dir)
        .map_err(|e| format!("创建知识库目录 {} 失败：{e}", lance_dir.display()))?;
    let kb = KbStore::open(&lance_dir).await?;
    let table_exists = kb.table_exists().await?;

    // —— 一致性预检（必须在增量判断之前）——
    let pipeline_changed = manifest.pipeline_version != PIPELINE_VERSION;
    if pipeline_changed {
        eprintln!(
            "管线参数版本变化（{} → {PIPELINE_VERSION}）：旧向量与新管线不可比，\
             清空整库全量重建。本次未覆盖的文档需要重新 ingest 才会回到库中。",
            manifest.pipeline_version
        );
        if table_exists {
            kb.drop_table_if_exists().await?;
        }
        manifest.docs.clear();
        manifest.pipeline_version = PIPELINE_VERSION;
        manifest.save(&mpath)?;
    } else if !table_exists && !manifest.docs.is_empty() {
        eprintln!(
            "警告：manifest 记录了 {} 个文档但 chunks 表不存在（库可能被手动删除），\
             已重置清单、全部重新入库。",
            manifest.docs.len()
        );
        manifest.docs.clear();
        manifest.save(&mpath)?;
    }

    // —— 增量判断，决定本次要处理的文档集 ——
    let mut todo: Vec<(String, String, String)> = Vec::new(); // (doc_path, hash, content)
    let mut skipped = 0usize;
    for file in &files {
        let content = match std::fs::read_to_string(file) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("跳过 {}（读取失败：{e}）", file.display());
                continue;
            }
        };
        let doc_path = normalize_doc_path(file, ctx.anchor_root.as_deref());
        let hash = content_hash(&content);
        if !args.force && !manifest.needs_ingest(&doc_path, &hash) {
            skipped += 1;
            continue;
        }
        todo.push((doc_path, hash, content));
    }

    if todo.is_empty() {
        if args.json {
            println!(
                "{}",
                serde_json::json!({
                    "action": "ingest", "ingested": 0, "skipped": skipped, "chunks": 0,
                    "db": ctx.db_dir.to_string_lossy(),
                })
            );
        } else {
            println!("全部 {} 个文档均为最新，无需入库。", files.len());
        }
        return Ok(());
    }

    // 模型与分块器（首次会触发模型下载，stderr 有体积预期提示）。
    let (mut emb, model_cache) = open_embedder()?;
    let chunker = open_chunker(&model_cache)?;
    let table = kb.open_or_create_table().await?;

    let now = system_now();
    let mut total_chunks = 0usize;
    for (doc_path, hash, content) in &todo {
        match ingest_one(&table, &chunker, &mut emb, doc_path, hash, content).await {
            Ok(0) => {
                // 空文档：旧 chunk 已删，manifest 条目同步移除。
                eprintln!("提示：{doc_path} 没有可入库的内容（空文档？），已跳过。");
                manifest.docs.remove(doc_path.as_str());
                manifest.save(&mpath)?;
            }
            Ok(n) => {
                eprintln!("已入库 {doc_path}（{n} 个 chunk）");
                total_chunks += n;
                manifest.docs.insert(
                    doc_path.clone(),
                    DocEntry { hash: hash.clone(), chunks: n, ingested_at: now },
                );
                // 逐文档落盘：中途失败不丢已完成文档的状态。
                manifest.save(&mpath)?;
            }
            Err(e) => {
                // 旧 chunk 可能已被删除：维持「表里没有 ⇒ manifest 也没有」。
                manifest.docs.remove(doc_path.as_str());
                let _ = manifest.save(&mpath); // best-effort，主错误优先上抛。
                return Err(format!(
                    "{e}\n（文档 {doc_path} 本次入库失败，已从清单移除；重跑 ingest 会重新入库它。）"
                ));
            }
        }
    }

    // 重建全文索引（建索引后新增的行不在旧倒排里，重建保证全量可查）。
    store::rebuild_fts_index(&table).await?;

    if args.json {
        println!(
            "{}",
            serde_json::json!({
                "action": "ingest", "ingested": todo.len(), "skipped": skipped,
                "chunks": total_chunks, "db": ctx.db_dir.to_string_lossy(),
            })
        );
    } else {
        println!(
            "入库完成：{} 个文档（跳过 {} 个未变更），共 {} 个 chunk。库：{}",
            todo.len(),
            skipped,
            total_chunks,
            ctx.db_dir.display()
        );
    }
    Ok(())
}

/// 单文档入库：删旧 chunk → 分块 → 嵌入 → 写新。返回写入的 chunk 数（0 = 空文档）。
///
/// # Errors
/// 任一步失败时返回中文错误说明（调用方负责 manifest 的一致性补偿）。
async fn ingest_one(
    table: &lancedb::Table,
    chunker: &Chunker,
    emb: &mut Embedder,
    doc_path: &str,
    hash: &str,
    content: &str,
) -> Result<usize, String> {
    // 旧版本 chunk（含同名文档的历史内容）一律先删，幂等。
    store::delete_doc(table, doc_path).await?;

    let doc_name = doc_path.rsplit('/').next().unwrap_or(doc_path);
    let chunks: Vec<Chunk> = chunker.chunk_document(doc_name, content);
    if chunks.is_empty() {
        return Ok(0);
    }
    let embed_texts: Vec<String> = chunks.iter().map(Chunk::embedding_text).collect();
    let vectors = emb.embed_docs(embed_texts)?;
    store::add_chunks(table, doc_path, hash, &chunks, vectors).await?;
    Ok(chunks.len())
}

/// `search` 的参数集合。
struct SearchArgs {
    db: Option<PathBuf>,
    shared: bool,
    query: String,
    limit: usize,
    filter: Option<String>,
    json: bool,
}

/// 执行 `search`：编码查询（加指令前缀）→ 混合检索 → 输出命中。
async fn run_search(args: SearchArgs) -> Result<(), String> {
    let ctx = resolve_kb_context(args.db, args.shared)?;
    // 只读命令先探测目录：lancedb 的 connect 会隐式 create_dir_all，
    // 不探测会在从未 ingest 的项目里凭空制造杂散 kb 目录。
    if !ctx.lance_dir().is_dir() {
        return Err("知识库为空（尚未 ingest 过任何文档），请先执行 ingest 入库".to_string());
    }
    let kb = KbStore::open(&ctx.lance_dir()).await?;
    let table = kb.open_table().await?;

    let (mut emb, _cache) = open_embedder()?;
    let qvec = emb.embed_query(&args.query)?;
    let hits = store::search_hybrid(&table, &args.query, qvec, args.limit, args.filter.as_deref())
        .await?;

    if args.json {
        println!(
            "{}",
            serde_json::json!({ "action": "search", "query": args.query, "hits": hits })
        );
    } else if hits.is_empty() {
        println!("没有命中。可尝试换个说法，或确认相关文档已 ingest。");
    } else {
        for (i, h) in hits.iter().enumerate() {
            let score = h
                .score
                .map_or("-".to_string(), |s| format!("{s:.4}"));
            println!("{}. [{score}] {} | {}", i + 1, h.doc_path, h.breadcrumb);
            // 正文截断展示，完整内容引导顺指针读原文（与 engram 理念一致）。
            let preview: String = h.text.chars().take(160).collect();
            let ellipsis = if h.text.chars().count() > 160 { "…" } else { "" };
            println!("   {preview}{ellipsis}");
        }
        println!("（顺 doc_path + 面包屑去读原文获取完整上下文，不要只凭片段下结论。）");
    }
    Ok(())
}

/// 执行 `list`：读 manifest 输出文档清单。
fn run_list(db: Option<PathBuf>, shared: bool, json: bool) -> Result<(), String> {
    let ctx = resolve_kb_context(db, shared)?;
    let manifest = Manifest::load(&manifest_path(&ctx.db_dir))?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "action": "list",
                "db": ctx.db_dir.to_string_lossy(),
                "pipeline_version": manifest.pipeline_version,
                "docs": manifest.docs,
            })
        );
    } else if manifest.docs.is_empty() {
        println!("知识库为空（{}）。用 ingest 入库文档。", ctx.db_dir.display());
    } else {
        println!("知识库 {}（{} 个文档）：", ctx.db_dir.display(), manifest.docs.len());
        for (path, entry) in &manifest.docs {
            println!("  {path}  [{} chunks]", entry.chunks);
        }
    }
    Ok(())
}

/// 执行 `remove`：删 chunk + 更新 manifest + 重建全文索引。
///
/// 容忍表缺失：库被手动删除时仍允许清掉 manifest 里的幽灵条目（否则该条目
/// 永远删不掉、list 永远显示它）。
async fn run_remove(db: Option<PathBuf>, shared: bool, doc: &str, json: bool) -> Result<(), String> {
    let ctx = resolve_kb_context(db, shared)?;
    let mpath = manifest_path(&ctx.db_dir);
    let mut manifest = Manifest::load(&mpath)?;
    if manifest.docs.remove(doc).is_none() {
        return Err(format!(
            "manifest 中没有文档 {doc}（用 list 查看现有 doc_path，需完全一致）"
        ));
    }
    if ctx.lance_dir().is_dir() {
        let kb = KbStore::open(&ctx.lance_dir()).await?;
        if kb.table_exists().await? {
            let table = kb.open_table().await?;
            store::delete_doc(&table, doc).await?;
            store::rebuild_fts_index(&table).await?;
        }
    }
    manifest.save(&mpath)?;

    if json {
        println!("{}", serde_json::json!({ "action": "remove", "doc": doc }));
    } else {
        println!("已删除文档 {doc} 的全部 chunk。");
    }
    Ok(())
}

/// 执行 `status`：文档数 / chunk 数 / 模型就绪 / 路径概况。
async fn run_status(db: Option<PathBuf>, shared: bool, json: bool) -> Result<(), String> {
    let ctx = resolve_kb_context(db, shared)?;
    let manifest = Manifest::load(&manifest_path(&ctx.db_dir))?;
    let model_cache = scope::model_cache_dir(|k| std::env::var(k).ok())?;
    let model_ok = embedder::model_ready(&model_cache);

    // 库目录不存在（从未 ingest）按 0 处理；存在但打开失败则如实报错，
    // 不吞掉权限/损坏类故障（status 是诊断命令，必须说真话）。
    let chunk_count = if ctx.lance_dir().is_dir() {
        let kb = KbStore::open(&ctx.lance_dir()).await?;
        if kb.table_exists().await? {
            store::count_rows(&kb.open_table().await?).await?
        } else {
            0
        }
    } else {
        0
    };

    if json {
        println!(
            "{}",
            serde_json::json!({
                "action": "status",
                "db": ctx.db_dir.to_string_lossy(),
                "docs": manifest.docs.len(),
                "chunks": chunk_count,
                "model_ready": model_ok,
                "model_cache": model_cache.to_string_lossy(),
                "pipeline_version": manifest.pipeline_version,
            })
        );
    } else {
        println!("知识库：{}", ctx.db_dir.display());
        println!("  文档数：{}    chunk 数：{chunk_count}", manifest.docs.len());
        println!(
            "  嵌入模型：{}（缓存 {}）",
            if model_ok { "已就绪" } else { "未下载（首次 ingest/search 时自动下载约 95MB）" },
            model_cache.display()
        );
    }
    Ok(())
}

/// 执行 `dump`：读某文档的全部 chunk 原文（按 ord 排序），供看板右侧内容区 / 外部工具。
///
/// 纯读 LanceDB、不加载嵌入模型。库目录不存在（从未 ingest）时报中文用户级错误；
/// doc_path 无对应 chunk（未入库 / 名字不一致）时 JSON 出空数组、文本给出提示。
async fn run_dump(db: Option<PathBuf>, shared: bool, doc: &str, json: bool) -> Result<(), String> {
    let ctx = resolve_kb_context(db, shared)?;
    if !ctx.lance_dir().is_dir() {
        return Err("知识库为空（尚未 ingest 过任何文档），请先执行 ingest 入库".to_string());
    }
    let kb = KbStore::open(&ctx.lance_dir()).await?;
    let table = kb.open_table().await?;
    let chunks = store::fetch_doc_chunks(&table, doc).await?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "action": "dump",
                "db": ctx.db_dir.to_string_lossy(),
                "doc": doc,
                "chunks": chunks,
            })
        );
    } else if chunks.is_empty() {
        println!("文档 {doc} 在知识库中没有 chunk（确认 doc_path 与 list 列出的完全一致）。");
    } else {
        for c in &chunks {
            if !c.breadcrumb.is_empty() {
                println!("## {}", c.breadcrumb);
            }
            println!("{}\n", c.text);
        }
    }
    Ok(())
}

/// 执行 `ensure-model`：预下载 / 校验模型（含 tokenizer 可得性）。
fn run_ensure_model(json: bool) -> Result<(), String> {
    let (_emb, cache_dir) = open_embedder()?;
    let tokenizer = embedder::find_tokenizer_json(&cache_dir).is_some();
    if json {
        println!(
            "{}",
            serde_json::json!({
                "action": "ensure-model", "ready": true,
                "tokenizer": tokenizer, "cache": cache_dir.to_string_lossy(),
            })
        );
    } else {
        println!("嵌入模型已就绪（缓存 {}）。", cache_dir.display());
    }
    Ok(())
}

/// `digest` 的输出格式（由 `--emit` 解析而来；与主引擎 SessionStart 同款语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmitFormat {
    /// 逐行打到 stdout（人读概览，缺省）。
    Text,
    /// 把概览整段塞进 Claude Code SessionStart hook 的 `additionalContext`，打成一行 JSON。
    Json,
}

/// 把 `--emit` 字符串解析为 [`EmitFormat`]（大小写不敏感）。
///
/// # Errors
/// 既非 `text` 也非 `json` 时返回中文说明的错误（参数错该报错，不静默）。
fn parse_emit_format(s: &str) -> Result<EmitFormat, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "text" => Ok(EmitFormat::Text),
        "json" => Ok(EmitFormat::Json),
        other => Err(format!("无法识别的 --emit {other}（应为 text|json）")),
    }
}

/// 把整段注入文本包成 Claude Code SessionStart hook 的单行 JSON。
///
/// 形如 `{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"<文本>"}}`，
/// 用 [`serde_json`] 序列化保证转义与 UTF-8 正确（与主引擎 `build_hook_json` 同协议，
/// 保证两者注入格式一致、可被 Claude Code 同样消费）。
///
/// # Errors
/// 序列化失败（理论上不会发生，结构均为可序列化字面量）时返回其错误说明。
fn build_hook_json(context: &str) -> Result<String, String> {
    let value = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": context,
        }
    });
    serde_json::to_string(&value).map_err(|e| format!("序列化 hook JSON 失败：{e}"))
}

/// digest 概览里最多列出的文档数；超出则截断并提示剩余，避免文档很多时注入过长。
const DIGEST_MAX_DOCS: usize = 20;

/// 由 manifest 构造注入用的知识库概览文本（纯函数，便于单测）。
///
/// 返回「一句引导 + 文档清单」整段文本：让会话一上来就知道知识库的存在、规模与
/// 含哪些文档，相关问题主动去 `search` 而非凭印象。`db_dir` 仅用于展示库位置。
/// 文档按 doc_path 字典序（manifest 的 `BTreeMap` 天然有序），超过上限时截断。
fn build_digest_context(db_dir: &Path, manifest: &Manifest) -> String {
    let doc_count = manifest.docs.len();
    let chunk_count: usize = manifest.docs.values().map(|e| e.chunks).sum();
    let mut s = format!(
        "「本项目已建 engram-kb 本地知识库：{doc_count} 个文档、{chunk_count} 个 chunk 已向量化入库。\
         需要查项目文档/历史资料时，用 `/engram kb search <查询>` 做中文语义+全文混合检索，\
         不要凭印象作答。库位置：{}。」\n已入库文档：",
        db_dir.display()
    );
    for path in manifest.docs.keys().take(DIGEST_MAX_DOCS) {
        s.push_str(&format!("\n  - {path}"));
    }
    if doc_count > DIGEST_MAX_DOCS {
        s.push_str(&format!(
            "\n  …… 还有 {} 个（用 `/engram kb list` 查看完整清单）",
            doc_count - DIGEST_MAX_DOCS
        ));
    }
    s
}

/// 执行 `digest`：只读 manifest 输出知识库概览（供 SessionStart 注入）。
///
/// **不启动 lancedb、不加载嵌入模型**——只读 manifest.json，保证 hook 轻量零等待。
/// 静默约束（任一满足即输出空、退出 0，绝不报错刷屏 hook）：
/// - 不在任何 engram 项目内且未给 `--db`（[`resolve_kb_context_optional`] 返回 `None`）；
/// - manifest 不存在 / 库为空（无任何已入库文档）。
///
/// # Errors
/// `--emit` 非法、或 manifest 存在但损坏 / 序列化 hook JSON 失败时返回中文错误。
fn run_digest(db: Option<PathBuf>, shared: bool, emit: &str) -> Result<(), String> {
    let format = parse_emit_format(emit)?;
    let ctx = match resolve_kb_context_optional(db, shared)? {
        Some(c) => c,
        None => return Ok(()), // 不在项目内：静默。
    };
    let manifest = Manifest::load(&manifest_path(&ctx.db_dir))?;
    if manifest.docs.is_empty() {
        return Ok(()); // 空库 / manifest 不存在：静默。
    }
    let context = build_digest_context(&ctx.db_dir, &manifest);
    match format {
        EmitFormat::Text => println!("{context}"),
        EmitFormat::Json => println!("{}", build_hook_json(&context)?),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_kb::manifest::DocEntry;

    fn entry(chunks: usize) -> DocEntry {
        DocEntry { hash: "h".to_string(), chunks, ingested_at: 0.0 }
    }

    /// `--emit` 解析：text/json 大小写不敏感、含空白也接受，非法值报中文错。
    #[test]
    fn emit_format_parsing() {
        assert_eq!(parse_emit_format("text").unwrap(), EmitFormat::Text);
        assert_eq!(parse_emit_format(" JSON ").unwrap(), EmitFormat::Json);
        assert!(parse_emit_format("xml").is_err());
    }

    /// hook JSON：可被 serde_json 解析、协议字段正确、additionalContext 原样保留。
    #[test]
    fn hook_json_shape() {
        let json = build_hook_json("概览\"文本\"").unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).expect("应为合法 JSON");
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
        assert_eq!(v["hookSpecificOutput"]["additionalContext"], "概览\"文本\"");
    }

    /// 概览文本：含规模统计、引导语与按字典序排列的文档清单。
    #[test]
    fn digest_context_lists_docs() {
        let mut m = Manifest::default();
        m.docs.insert("docs/b.md".to_string(), entry(2));
        m.docs.insert("docs/a.md".to_string(), entry(3));
        let ctx = build_digest_context(Path::new("/kb"), &m);
        assert!(ctx.contains("2 个文档"), "实得：{ctx}");
        assert!(ctx.contains("5 个 chunk"), "实得：{ctx}");
        assert!(ctx.contains("docs/a.md") && ctx.contains("docs/b.md"));
        // BTreeMap 有序：a.md 应排在 b.md 之前。
        assert!(ctx.find("docs/a.md").unwrap() < ctx.find("docs/b.md").unwrap());
    }

    /// 文档数超过上限时截断，并提示剩余数量。
    #[test]
    fn digest_context_truncates_when_many() {
        let mut m = Manifest::default();
        for i in 0..(DIGEST_MAX_DOCS + 5) {
            m.docs.insert(format!("docs/{i:03}.md"), entry(1));
        }
        let ctx = build_digest_context(Path::new("/kb"), &m);
        assert!(ctx.contains("还有 5 个"), "实得：{ctx}");
    }
}
