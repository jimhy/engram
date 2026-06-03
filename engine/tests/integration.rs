//! 集成测试：覆盖 redb **多库**持久化层 + CLI 端到端。
//!
//! 本切片把存储从「公共库 + 单个项目库」扩展为「公共库 + 多个项目库」：
//! - **公共库（general）**：存通用记忆 L1/L2/L3（`project == None`）；
//! - **项目库（project）**：每个项目一个库，存该项目的 L4 记忆
//!   （`project == Some(name)`）。一次调用可同时挂载多个项目库，CLI 用可重复的
//!   `--project-db <name>=<path>` 指定。
//!
//! 测试相应基于公共库 + **两个**临时项目库文件：
//! - 用 `engram::store::put_many` 直接把记忆灌入对应库（L1-3 灌 general、L4 灌
//!   各自项目库）；
//! - 再以子进程方式调用编译出的 `engram` 二进制跑 `render` / `consolidate` /
//!   `import` / `recall` / `list` / `write`，用 `CARGO_BIN_EXE_engram` 定位；
//! - 对会写回的命令，重新用 `store::get` 读出对应库，断言新状态确已
//!   **路由到正确的项目库**、互不串库。
//!
//! 注意：redb 文件需独占访问。凡是要让子进程打开同一个 db 的用例，**必须先
//! `drop` 掉本进程持有的 `Database`**，再 spawn 子进程；反之亦然。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use engram::model::{Level, Memory, Pointer, Status};
use engram::store;

/// 全局测试串行锁。
///
/// 本集成测试在**单进程内多线程**地（libtest 默认按 CPU 数并行）反复
/// `store::open` 打开大量 redb 库文件；redb 在 Windows 上用内存映射文件，
/// 高并行下多个线程同时 mmap/munmap 会触发底层访问冲突（STATUS_ACCESS_VIOLATION，
/// 与具体用例逻辑无关、纯属并发下的 mmap 竞态）。每个测试在开头取本锁串行执行，
/// 即可彻底规避该竞态、保证套件稳定全绿（单测本就快，串行开销可忽略）。
fn test_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    // 锁被 poison（某测试 panic 时）也照常取用：本锁只为串行化，不保护共享状态。
    match LOCK.get_or_init(|| Mutex::new(())).lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 进程内单调递增计数器，给每个临时路径再补一份**绝对唯一**的后缀。
///
/// 仅靠「纳秒时间戳」在高并行下仍可能撞名（两个线程同纳秒取值），叠加本计数器
/// 后即可保证进程内任意两次调用得到不同路径，杜绝并行测试因撞名互相踩。
static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 取一个进程内唯一的后缀串：`<纳秒>_<自增序号>`。
fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos}_{seq}")
}

/// 在系统临时目录下构造一个进程内唯一的 db 文件路径（不引入 tempfile 依赖）。
///
/// 用「测试名 + 进程 id + 纳秒时间戳 + 自增序号」拼出唯一文件名，避免并行测试互相踩。
fn unique_db_path(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let pid = std::process::id();
    dir.push(format!("engram_it_{tag}_{pid}_{}.redb", unique_suffix()));
    dir
}

/// 在系统临时目录下构造一个进程内唯一的 JSON 目录（用于 `import` 用例）。
fn unique_json_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let pid = std::process::id();
    dir.push(format!("engram_it_json_{tag}_{pid}_{}", unique_suffix()));
    std::fs::create_dir_all(&dir).expect("应能创建临时 JSON 目录");
    dir
}

/// 清理 db 文件（忽略失败）。
fn cleanup_file(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// 构造一条测试记忆。
fn make(
    id: &str,
    level: Level,
    project: Option<&str>,
    status: Status,
    importance: f64,
    created_at: f64,
    access_log: Vec<f64>,
) -> Memory {
    Memory {
        id: id.to_string(),
        cue: format!("cue-{id}"),
        pointer: Pointer {
            kind: "file".to_string(),
            reference: Some(format!("src/{id}.rs:1")),
            detail: None,
        },
        level,
        project: project.map(|s| s.to_string()),
        importance,
        pinned: false,
        access_log,
        status,
        superseded_by: None,
        created_at,
        tags: vec![],
    }
}

/// 把 `&[(name, path)]` 追加为若干 `--project-db name=path` 参数。
fn push_project_dbs(cmd: &mut Command, project_dbs: &[(&str, &Path)]) {
    for (name, path) in project_dbs {
        cmd.arg("--project-db")
            .arg(format!("{name}={}", path.display()));
    }
}

/// 调用 `engram render --general-db G [--project-db name=path ...]`，返回 stdout。
fn run_render(general_db: &Path, project_dbs: &[(&str, &Path)], now: f64) -> String {
    let exe = env!("CARGO_BIN_EXE_engram");
    let mut cmd = Command::new(exe);
    cmd.arg("render")
        .arg("--general-db")
        .arg(general_db)
        .arg("--now")
        .arg(now.to_string());
    push_project_dbs(&mut cmd, project_dbs);
    let output = cmd.output().expect("运行 engram 二进制失败");
    assert!(
        output.status.success(),
        "render 退出码非 0，stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("stdout 非 UTF-8")
}

/// 调用 `engram consolidate --general-db G [--project-db name=path ...]`，
/// `dry_run` 为真时追加 `--dry-run`。返回完整 Output（调用方自行断言）。
fn run_consolidate_raw(
    general_db: &Path,
    project_dbs: &[(&str, &Path)],
    now: f64,
    dry_run: bool,
) -> std::process::Output {
    let exe = env!("CARGO_BIN_EXE_engram");
    let mut cmd = Command::new(exe);
    cmd.arg("consolidate")
        .arg("--general-db")
        .arg(general_db)
        .arg("--now")
        .arg(now.to_string());
    push_project_dbs(&mut cmd, project_dbs);
    if dry_run {
        cmd.arg("--dry-run");
    }
    cmd.output().expect("运行 engram 二进制失败")
}

/// 调用 consolidate 并断言成功，返回 stdout。
fn run_consolidate(
    general_db: &Path,
    project_dbs: &[(&str, &Path)],
    now: f64,
    dry_run: bool,
) -> String {
    let output = run_consolidate_raw(general_db, project_dbs, now, dry_run);
    assert!(
        output.status.success(),
        "consolidate 退出码非 0，stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("stdout 非 UTF-8")
}

/// 调用 `engram import --general-db G [--project-db name=path ...] --from-json-dir D`，
/// 返回完整 Output。
fn run_import_raw(
    general_db: &Path,
    project_dbs: &[(&str, &Path)],
    json_dir: &Path,
) -> std::process::Output {
    let exe = env!("CARGO_BIN_EXE_engram");
    let mut cmd = Command::new(exe);
    cmd.arg("import")
        .arg("--general-db")
        .arg(general_db)
        .arg("--from-json-dir")
        .arg(json_dir);
    push_project_dbs(&mut cmd, project_dbs);
    cmd.output().expect("运行 engram 二进制失败")
}

/// 调用 import 并断言成功，返回 stdout 文本。
fn run_import(general_db: &Path, project_dbs: &[(&str, &Path)], json_dir: &Path) -> String {
    let output = run_import_raw(general_db, project_dbs, json_dir);
    assert!(
        output.status.success(),
        "import 退出码非 0，stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("stdout 非 UTF-8")
}

/// 把若干记忆灌入一个临时 db 文件后**关闭** db，返回该路径（供子进程接管）。
fn seed_db(tag: &str, mems: &[Memory]) -> PathBuf {
    let path = unique_db_path(tag);
    let db = store::open(&path).expect("应能创建数据库");
    store::put_many(&db, mems).expect("put_many 应成功");
    drop(db); // 释放独占文件锁，便于子进程打开。
    path
}

// 1. import 双库路由：L1-3 灌共享库、L4 灌项目库，各库条数正确。
#[test]
fn import_routes_by_level() {
    let _guard = test_guard();
    let json_dir = unique_json_dir("import_route");
    // 两条通用（L1、L3）+ 两条项目 L4（L4.1、L4.2）+ 一条损坏 + 一个非 json。
    let g1 = r#"{
        "id": "g_1",
        "cue": "通用一",
        "pointer": { "kind": "file", "reference": "src/a.rs:1", "detail": null },
        "level": "L1",
        "project": null,
        "importance": 0.7,
        "pinned": false,
        "access_log": [1000000000.0],
        "status": "active",
        "superseded_by": null,
        "created_at": 1000000000.0,
        "tags": ["intent"]
    }"#;
    let g2 = r#"{
        "id": "g_2",
        "cue": "通用二",
        "pointer": { "kind": "none", "reference": null, "detail": null },
        "level": "L3",
        "project": null,
        "importance": 0.1,
        "pinned": false,
        "access_log": [1000000000.0],
        "status": "active",
        "superseded_by": null,
        "created_at": 1000000000.0,
        "tags": []
    }"#;
    let p1 = r#"{
        "id": "p_1",
        "cue": "项目一",
        "pointer": { "kind": "file", "reference": "src/p.rs:1", "detail": null },
        "level": "L4.1",
        "project": "engram",
        "importance": 0.6,
        "pinned": false,
        "access_log": [1000000000.0],
        "status": "active",
        "superseded_by": null,
        "created_at": 1000000000.0,
        "tags": []
    }"#;
    // p2 属于另一个项目 beta，验证 import 能把不同项目的 L4 路由到不同项目库。
    let p2 = r#"{
        "id": "p_2",
        "cue": "项目二",
        "pointer": { "kind": "none", "reference": null, "detail": null },
        "level": "L4.2",
        "project": "beta",
        "importance": 0.3,
        "pinned": false,
        "access_log": [1000000000.0],
        "status": "active",
        "superseded_by": null,
        "created_at": 1000000000.0,
        "tags": []
    }"#;
    std::fs::write(json_dir.join("g_1.json"), g1).expect("写入 g1 失败");
    std::fs::write(json_dir.join("g_2.json"), g2).expect("写入 g2 失败");
    std::fs::write(json_dir.join("p_1.json"), p1).expect("写入 p1 失败");
    std::fs::write(json_dir.join("p_2.json"), p2).expect("写入 p2 失败");
    std::fs::write(json_dir.join("broken.json"), "{ not json").expect("写入 broken 失败");
    std::fs::write(json_dir.join("notes.txt"), "无关内容").expect("写入 notes 失败");

    let general_path = unique_db_path("import_route_g");
    let pa_path = unique_db_path("import_route_pa");
    let pb_path = unique_db_path("import_route_pb");
    let out = run_import(
        &general_path,
        &[("engram", &pa_path), ("beta", &pb_path)],
        &json_dir,
    );
    assert!(out.contains("公共库 2 条"), "应公共库 2 条，实得：{out}");
    assert!(
        out.contains("项目库 engram 导入 1 条") && out.contains("项目库 beta 导入 1 条"),
        "engram/beta 各应导入 1 条，实得：{out}"
    );

    // 公共库应恰好含 g_1、g_2。
    let gdb = store::open(&general_path).expect("应能打开公共库");
    let gmems = store::all(&gdb).expect("all 应成功");
    let mut gids: Vec<String> = gmems.iter().map(|m| m.id.clone()).collect();
    gids.sort();
    assert_eq!(gids, vec!["g_1", "g_2"], "公共库应只含 L1-3 通用记忆");
    drop(gdb);

    // engram 项目库应只含 p_1。
    let pa_db = store::open(&pa_path).expect("应能打开 engram 项目库");
    let pa_mems = store::all(&pa_db).expect("all 应成功");
    let pa_ids: Vec<String> = pa_mems.iter().map(|m| m.id.clone()).collect();
    assert_eq!(pa_ids, vec!["p_1"], "engram 项目库应只含其 L4 记忆 p_1");
    drop(pa_db);

    // beta 项目库应只含 p_2。
    let pb_db = store::open(&pb_path).expect("应能打开 beta 项目库");
    let pb_mems = store::all(&pb_db).expect("all 应成功");
    let pb_ids: Vec<String> = pb_mems.iter().map(|m| m.id.clone()).collect();
    assert_eq!(pb_ids, vec!["p_2"], "beta 项目库应只含其 L4 记忆 p_2");
    drop(pb_db);

    cleanup_file(&general_path);
    cleanup_file(&pa_path);
    cleanup_file(&pb_path);
    let _ = std::fs::remove_dir_all(&json_dir);
}

// 2. import 含 L4 但其 project 不在 --project-db 映射中 → 报错退出（非 0），不写入。
#[test]
fn import_l4_without_matching_project_db_fails() {
    let _guard = test_guard();
    let json_dir = unique_json_dir("import_no_pdb");
    let l4 = r#"{
        "id": "l4_x",
        "cue": "孤立的 L4",
        "pointer": { "kind": "none", "reference": null, "detail": null },
        "level": "L4.1",
        "project": "engram",
        "importance": 0.5,
        "pinned": false,
        "access_log": [1000000000.0],
        "status": "active",
        "superseded_by": null,
        "created_at": 1000000000.0,
        "tags": []
    }"#;
    std::fs::write(json_dir.join("l4_x.json"), l4).expect("写入 l4 失败");

    let general_path = unique_db_path("import_no_pdb_g");
    let other_path = unique_db_path("import_no_pdb_other");
    // 只挂载了 other 项目库，engram（l4_x 的归属项目）不在映射中 → 应报错。
    let output = run_import_raw(&general_path, &[("other", &other_path)], &json_dir);
    assert!(
        !output.status.success(),
        "L4 的 project 不在 --project-db 映射时 import 应非 0 退出"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("engram"),
        "stderr 应说明因项目 engram 无映射报错，实得：{stderr}"
    );

    cleanup_file(&general_path);
    cleanup_file(&other_path);
    let _ = std::fs::remove_dir_all(&json_dir);
}

// 3. render 合并公共库 + 两个项目库：通用 L1-3 正常、非 active 被过滤、
//    两个项目的 L4 都分组展示且小节标题带项目名、按项目名升序。
#[test]
fn render_merges_general_and_two_projects() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    // 公共库：一条 active 通用 + 一条 cold 通用。
    let gmems = vec![
        make("g", Level::L1, None, Status::Active, 0.5, now, vec![now]),
        make("g_cold", Level::L1, None, Status::Cold, 0.9, now, vec![now]),
    ];
    // 项目库 a（alpha）：一条 active L4.1。
    let pa_mems = vec![make(
        "pa1",
        Level::L4_1,
        Some("alpha"),
        Status::Active,
        0.5,
        now,
        vec![now],
    )];
    // 项目库 b（beta）：一条 active L4.1。
    let pb_mems = vec![make(
        "pb1",
        Level::L4_1,
        Some("beta"),
        Status::Active,
        0.5,
        now,
        vec![now],
    )];
    let general_path = seed_db("render_g", &gmems);
    let pa_path = seed_db("render_pa", &pa_mems);
    let pb_path = seed_db("render_pb", &pb_mems);

    // 同时挂两个项目库。
    let out = run_render(
        &general_path,
        &[("alpha", &pa_path), ("beta", &pb_path)],
        now,
    );
    assert!(out.contains("cue-g"), "通用 active 记忆应含");
    assert!(!out.contains("cue-g_cold"), "cold 记忆应被过滤");
    assert!(out.contains("cue-pa1"), "alpha 项目 L4 应展示");
    assert!(out.contains("cue-pb1"), "beta 项目 L4 应展示");
    // 小节标题应带项目名。
    assert!(
        out.contains("[alpha] L4.1 项目潜意识层"),
        "应有 alpha 的 L4.1 分组标题，实得：\n{out}"
    );
    assert!(
        out.contains("[beta] L4.1 项目潜意识层"),
        "应有 beta 的 L4.1 分组标题，实得：\n{out}"
    );
    // 项目应按名升序：alpha 节在 beta 节之前。
    let pos_alpha = out.find("[alpha] L4.1").expect("应含 alpha 节");
    let pos_beta = out.find("[beta] L4.1").expect("应含 beta 节");
    assert!(pos_alpha < pos_beta, "项目应按名升序，alpha 应在 beta 前");

    // 仅给公共库、不挂项目库：只剩通用 active，无任何 L4。
    let out_general_only = run_render(&general_path, &[], now);
    assert!(out_general_only.contains("cue-g"), "通用记忆应含");
    assert!(
        !out_general_only.contains("cue-pa1") && !out_general_only.contains("cue-pb1"),
        "未挂项目库时不应出现任何 L4"
    );

    cleanup_file(&general_path);
    cleanup_file(&pa_path);
    cleanup_file(&pb_path);
}

// 4. consolidate 降级写回路由到公共库：L2 通用记忆降到 L3，写回公共库。
//
// 参数选取同前：访问 300 天前，L2 effective 被 floor 托在 1.0 < DEMOTE_L2(1.5)
// 触发降级；降到 L3 后 raw≈-2.85 > EVICT_THRESHOLD(-3) 不被淘汰，隔离出降级写回。
#[test]
fn consolidate_demotion_routes_to_general() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    let created = now - 300.0 * 86400.0;
    let gmems = vec![make(
        "mem_demote",
        Level::L2,
        None,
        Status::Active,
        0.0,
        created,
        vec![created],
    )];
    let general_path = seed_db("demote_g", &gmems);

    // 不挂任何项目库；非 dry-run。
    let out = run_consolidate(&general_path, &[], now, false);
    assert!(out.contains("mem_demote"), "摘要应含被降级记忆 id");
    assert!(out.contains("降级"), "摘要应标注降级");

    // 公共库读回：level 应变 L3、状态仍 Active。
    let gdb = store::open(&general_path).expect("应能重新打开公共库");
    let got = store::get(&gdb, "mem_demote")
        .expect("get 应成功")
        .expect("应能读到该记忆");
    assert_eq!(got.level, Level::L3, "降级后 level 应为 L3");
    assert_eq!(got.status, Status::Active, "纯降级不应改 active 状态");
    drop(gdb);

    cleanup_file(&general_path);
}

// 5. consolidate 多项目 L4 降级各自写回其项目库，互不串库（且不误写公共库）。
//
// 两个项目 alpha / beta 各放一条会降级的 L4.2：L4.2 floor=0.5，300 天前单次访问下
// effective 被托在 0.5 < DEMOTE_L2(1.5) 触发降级到 L4.3。公共库另放一条不变的 L1。
#[test]
fn consolidate_multi_project_l4_demotion_routes_per_project() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    let created = now - 300.0 * 86400.0;

    // 公共库一条稳定 L1（高 importance，不会被动）。
    let gmems = vec![make(
        "g_stable",
        Level::L1,
        None,
        Status::Active,
        10.0,
        now - 86400.0,
        vec![now - 43200.0],
    )];
    // alpha 项目库一条会降级的 L4.2。
    let pa_mems = vec![make(
        "a_demote",
        Level::L4_2,
        Some("alpha"),
        Status::Active,
        0.0,
        created,
        vec![created],
    )];
    // beta 项目库一条会降级的 L4.2。
    let pb_mems = vec![make(
        "b_demote",
        Level::L4_2,
        Some("beta"),
        Status::Active,
        0.0,
        created,
        vec![created],
    )];
    let general_path = seed_db("l4dem_g", &gmems);
    let pa_path = seed_db("l4dem_pa", &pa_mems);
    let pb_path = seed_db("l4dem_pb", &pb_mems);

    let out = run_consolidate(
        &general_path,
        &[("alpha", &pa_path), ("beta", &pb_path)],
        now,
        false,
    );
    assert!(out.contains("a_demote"), "摘要应含 alpha 被降级记忆 id");
    assert!(out.contains("b_demote"), "摘要应含 beta 被降级记忆 id");
    assert!(out.contains("降级"), "摘要应标注降级");

    // alpha 库读回：a_demote 降到 L4.3，且 alpha 库不应混入 beta 的记忆。
    let pa_db = store::open(&pa_path).expect("应能重新打开 alpha 项目库");
    let got_a = store::get(&pa_db, "a_demote")
        .expect("get 应成功")
        .expect("应能在 alpha 库读到 a_demote");
    assert_eq!(
        got_a.level,
        Level::L4_3,
        "alpha 的 L4.2 应降到 L4.3 落 alpha 库"
    );
    assert!(
        store::get(&pa_db, "b_demote")
            .expect("get 应成功")
            .is_none(),
        "beta 的记忆不应被误写进 alpha 库"
    );
    drop(pa_db);

    // beta 库读回：b_demote 降到 L4.3，且 beta 库不应混入 alpha 的记忆。
    let pb_db = store::open(&pb_path).expect("应能重新打开 beta 项目库");
    let got_b = store::get(&pb_db, "b_demote")
        .expect("get 应成功")
        .expect("应能在 beta 库读到 b_demote");
    assert_eq!(
        got_b.level,
        Level::L4_3,
        "beta 的 L4.2 应降到 L4.3 落 beta 库"
    );
    assert!(
        store::get(&pb_db, "a_demote")
            .expect("get 应成功")
            .is_none(),
        "alpha 的记忆不应被误写进 beta 库"
    );
    drop(pb_db);

    // 公共库不应混入任何 L4，且 g_stable 不变。
    let gdb = store::open(&general_path).expect("应能重新打开公共库");
    assert!(
        store::get(&gdb, "a_demote").expect("get 应成功").is_none()
            && store::get(&gdb, "b_demote").expect("get 应成功").is_none(),
        "L4 记忆不应被误写进公共库"
    );
    let got_g = store::get(&gdb, "g_stable")
        .expect("get 应成功")
        .expect("g_stable 应在");
    assert_eq!(got_g.level, Level::L1, "稳定 L1 不应被动");
    drop(gdb);

    cleanup_file(&general_path);
    cleanup_file(&pa_path);
    cleanup_file(&pb_path);
}

// 6. consolidate 淘汰写回路由到正确的库：通用 L3 淘汰落共享库、项目 L4.3 淘汰落项目库。
#[test]
fn consolidate_eviction_routes_correctly() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    let created = now - 1000.0 * 86400.0; // 久到 effective 跌破淘汰阈值。

    let gmems = vec![make(
        "g_evict",
        Level::L3,
        None,
        Status::Active,
        0.0,
        created,
        vec![created],
    )];
    let pmems = vec![make(
        "p_evict",
        Level::L4_3,
        Some("engram"),
        Status::Active,
        0.0,
        created,
        vec![created],
    )];
    let general_path = seed_db("evict_g", &gmems);
    let project_path = seed_db("evict_p", &pmems);

    let out = run_consolidate(&general_path, &[("engram", &project_path)], now, false);
    assert!(out.contains("g_evict"), "摘要应含通用淘汰记忆");
    assert!(out.contains("p_evict"), "摘要应含项目淘汰记忆");
    assert!(out.contains("淘汰"), "摘要应标注淘汰");

    // 通用淘汰落公共库。
    let gdb = store::open(&general_path).expect("应能重新打开公共库");
    let got_g = store::get(&gdb, "g_evict")
        .expect("get 应成功")
        .expect("应能读到 g_evict");
    assert_eq!(
        got_g.status,
        Status::Cold,
        "通用 L3 淘汰应转 Cold（公共库）"
    );
    drop(gdb);

    // 项目淘汰落项目库。
    let pdb = store::open(&project_path).expect("应能重新打开项目库");
    let got_p = store::get(&pdb, "p_evict")
        .expect("get 应成功")
        .expect("应能读到 p_evict");
    assert_eq!(
        got_p.status,
        Status::Cold,
        "项目 L4.3 淘汰应转 Cold（项目库）"
    );
    drop(pdb);

    cleanup_file(&general_path);
    cleanup_file(&project_path);
}

// 7. consolidate dry-run 不写回任何库：仍打印摘要，但各库记忆不变。
#[test]
fn consolidate_dry_run_does_not_write() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    let created = now - 1000.0 * 86400.0;

    let gmems = vec![make(
        "g_dry",
        Level::L2,
        None,
        Status::Active,
        0.0,
        created,
        vec![created],
    )];
    let pmems = vec![make(
        "p_dry",
        Level::L4_2,
        Some("engram"),
        Status::Active,
        0.0,
        created,
        vec![created],
    )];
    let general_path = seed_db("dry_g", &gmems);
    let project_path = seed_db("dry_p", &pmems);

    let out = run_consolidate(&general_path, &[("engram", &project_path)], now, true);
    assert!(out.contains("dry-run"), "应标注 dry-run");
    assert!(out.contains("降级"), "dry-run 仍应算出降级");

    // 两库都应保持原 level。
    let gdb = store::open(&general_path).expect("应能重新打开公共库");
    let got_g = store::get(&gdb, "g_dry")
        .expect("get 应成功")
        .expect("应能读到 g_dry");
    assert_eq!(got_g.level, Level::L2, "dry-run 不应改写公共库 level");
    drop(gdb);

    let pdb = store::open(&project_path).expect("应能重新打开项目库");
    let got_p = store::get(&pdb, "p_dry")
        .expect("get 应成功")
        .expect("应能读到 p_dry");
    assert_eq!(got_p.level, Level::L4_2, "dry-run 不应改写项目库 level");
    drop(pdb);

    cleanup_file(&general_path);
    cleanup_file(&project_path);
}

// 8. import 后 render 端到端：导入双库后渲染，能看到通用与项目记忆。
#[test]
fn import_then_render_two_dbs() {
    let _guard = test_guard();
    let json_dir = unique_json_dir("imp_render");
    let g = r#"{
        "id": "g_r",
        "cue": "通用并渲染",
        "pointer": { "kind": "file", "reference": "src/r.rs:1", "detail": null },
        "level": "L1",
        "project": null,
        "importance": 0.8,
        "pinned": false,
        "access_log": [1000000000.0],
        "status": "active",
        "superseded_by": null,
        "created_at": 1000000000.0,
        "tags": []
    }"#;
    let p = r#"{
        "id": "p_r",
        "cue": "项目并渲染",
        "pointer": { "kind": "file", "reference": "src/pr.rs:1", "detail": null },
        "level": "L4.1",
        "project": "engram",
        "importance": 0.8,
        "pinned": false,
        "access_log": [1000000000.0],
        "status": "active",
        "superseded_by": null,
        "created_at": 1000000000.0,
        "tags": []
    }"#;
    std::fs::write(json_dir.join("g_r.json"), g).expect("写入 g_r 失败");
    std::fs::write(json_dir.join("p_r.json"), p).expect("写入 p_r 失败");

    let general_path = unique_db_path("imp_render_g");
    let project_path = unique_db_path("imp_render_p");
    let _ = run_import(&general_path, &[("engram", &project_path)], &json_dir);

    // 挂上项目库渲染：通用与项目记忆都应出现。
    let out = run_render(&general_path, &[("engram", &project_path)], 1_000_086_400.0);
    assert!(out.contains("通用并渲染"), "应渲染出通用记忆 cue");
    assert!(out.contains("项目并渲染"), "应渲染出项目记忆 cue");
    assert!(out.contains("src/r.rs:1"), "L1 全文层应显示通用 reference");
    assert!(
        out.contains("src/pr.rs:1"),
        "L4.1 全文层应显示项目 reference"
    );
    assert!(
        out.contains("[engram] L4.1 项目潜意识层"),
        "应有 engram 的 L4.1 分组标题，实得：\n{out}"
    );

    cleanup_file(&general_path);
    cleanup_file(&project_path);
    let _ = std::fs::remove_dir_all(&json_dir);
}

// ============================================================================
// 切片 4a：write / recall / list 三命令的集成测试
// ============================================================================

/// 调用 `engram write ...`，返回完整 Output（调用方自行断言成功/失败）。
fn run_write_raw(args: &[&str]) -> std::process::Output {
    let exe = env!("CARGO_BIN_EXE_engram");
    let mut cmd = Command::new(exe);
    cmd.arg("write");
    for a in args {
        cmd.arg(a);
    }
    cmd.output().expect("运行 engram 二进制失败")
}

/// 调用 `engram write ...` 并断言成功，返回 stdout（去尾换行，即新建的 id）。
fn run_write(args: &[&str]) -> String {
    let output = run_write_raw(args);
    assert!(
        output.status.success(),
        "write 退出码非 0，stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("stdout 非 UTF-8")
        .trim_end()
        .to_string()
}

/// 调用 `engram recall ...`，返回完整 Output。
fn run_recall_raw(args: &[&str]) -> std::process::Output {
    let exe = env!("CARGO_BIN_EXE_engram");
    let mut cmd = Command::new(exe);
    cmd.arg("recall");
    for a in args {
        cmd.arg(a);
    }
    cmd.output().expect("运行 engram 二进制失败")
}

/// 调用 `engram recall ...` 并断言成功，返回 stdout。
fn run_recall(args: &[&str]) -> String {
    let output = run_recall_raw(args);
    assert!(
        output.status.success(),
        "recall 退出码非 0，stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("stdout 非 UTF-8")
}

/// 调用 `engram list ...` 并断言成功，返回 stdout。
fn run_list(args: &[&str]) -> String {
    let exe = env!("CARGO_BIN_EXE_engram");
    let mut cmd = Command::new(exe);
    cmd.arg("list");
    for a in args {
        cmd.arg(a);
    }
    let output = cmd.output().expect("运行 engram 二进制失败");
    assert!(
        output.status.success(),
        "list 退出码非 0，stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("stdout 非 UTF-8")
}

// 9. write L3 落公共库、L4.2 落项目库，且字段（pinned/importance/tags/created_at）正确。
#[test]
fn write_routes_l3_to_general_and_l4_to_project() {
    let _guard = test_guard();
    let general_path = unique_db_path("write_route_g");
    let project_path = unique_db_path("write_route_p");
    let g = general_path.to_string_lossy().to_string();
    // 项目库以 name=path 形式给出。
    let p_kv = format!("engram={}", project_path.display());

    // 写一条 L3 通用记忆到公共库（指定 id 便于读回）。
    let l3_id = run_write(&[
        "--general-db",
        &g,
        "--level",
        "L3",
        "--cue",
        "redb 文件需独占访问",
        "--importance",
        "0.4",
        "--tags",
        "intent,redb",
        "--id",
        "w_l3",
        "--now",
        "1000000000",
    ]);
    assert_eq!(l3_id, "w_l3", "应回显指定的 id");

    // 写一条 L4.2 项目记忆到 engram 项目库（pinned）。
    let l4_id = run_write(&[
        "--general-db",
        &g,
        "--project-db",
        &p_kv,
        "--level",
        "L4.2",
        "--cue",
        "项目记忆一条",
        "--project",
        "engram",
        "--pinned",
        "--pointer-kind",
        "file",
        "--pointer-ref",
        "src/x.rs:10",
        "--id",
        "w_l4",
        "--now",
        "1000000000",
    ]);
    assert_eq!(l4_id, "w_l4");

    // 公共库应含 w_l3、字段正确，且不含 w_l4。
    let gdb = store::open(&general_path).expect("应能打开公共库");
    let got_l3 = store::get(&gdb, "w_l3")
        .expect("get 应成功")
        .expect("应读到 w_l3");
    assert_eq!(got_l3.level, Level::L3);
    assert_eq!(got_l3.project, None, "L3 应无 project");
    assert_eq!(got_l3.status, Status::Active, "新建应为 active");
    assert!(got_l3.access_log.is_empty(), "新建 access_log 应为空");
    assert!(
        (got_l3.created_at - 1_000_000_000.0).abs() < 1e-6,
        "created_at 应等于 now"
    );
    assert!(
        (got_l3.importance - 0.4).abs() < 1e-9,
        "importance 应落库为 0.4"
    );
    assert_eq!(
        got_l3.tags,
        vec!["intent".to_string(), "redb".to_string()],
        "tags 应按逗号解析"
    );
    assert!(
        store::get(&gdb, "w_l4").expect("get 应成功").is_none(),
        "L4 不应被误写进公共库"
    );
    drop(gdb);

    // 项目库应含 w_l4、pinned=true、project=engram、指针正确。
    let pdb = store::open(&project_path).expect("应能打开项目库");
    let got_l4 = store::get(&pdb, "w_l4")
        .expect("get 应成功")
        .expect("应读到 w_l4");
    assert_eq!(got_l4.level, Level::L4_2);
    assert_eq!(got_l4.project, Some("engram".to_string()));
    assert!(got_l4.pinned, "pinned 应落库为 true");
    assert_eq!(got_l4.pointer.kind, "file");
    assert_eq!(got_l4.pointer.reference, Some("src/x.rs:10".to_string()));
    drop(pdb);

    cleanup_file(&general_path);
    cleanup_file(&project_path);
}

// 10. write 校验失败：L4 无任何 --project-db / L4 缺 --project / L4 项目名不在映射 /
//     importance 越界 / L1-3 带 --project，均非 0 退出且不写入任何库。
#[test]
fn write_validation_errors_do_not_write() {
    let _guard = test_guard();
    let general_path = unique_db_path("write_err_g");
    let project_path = unique_db_path("write_err_p");
    let g = general_path.to_string_lossy().to_string();
    let p_kv = format!("engram={}", project_path.display());

    // (a) L4 给了 --project 但完全没给 --project-db（空映射）→ 项目不在映射，失败。
    let out_a = run_write_raw(&[
        "--general-db",
        &g,
        "--level",
        "L4.1",
        "--cue",
        "缺项目库",
        "--project",
        "engram",
        "--id",
        "bad_a",
        "--now",
        "1000000000",
    ]);
    assert!(!out_a.status.success(), "L4 无 --project-db 映射应失败");

    // (b) L4 给了 --project-db 但缺 --project → 失败。
    let out_b = run_write_raw(&[
        "--general-db",
        &g,
        "--project-db",
        &p_kv,
        "--level",
        "L4.1",
        "--cue",
        "缺项目名",
        "--id",
        "bad_b",
        "--now",
        "1000000000",
    ]);
    assert!(!out_b.status.success(), "L4 缺 --project 应失败");

    // (c) importance 越界（>1）→ 失败。
    let out_c = run_write_raw(&[
        "--general-db",
        &g,
        "--level",
        "L3",
        "--cue",
        "越界重要度",
        "--importance",
        "1.5",
        "--id",
        "bad_c",
        "--now",
        "1000000000",
    ]);
    assert!(!out_c.status.success(), "importance 越界应失败");

    // (d) L1-3 却给了 --project → 失败。
    let out_d = run_write_raw(&[
        "--general-db",
        &g,
        "--level",
        "L2",
        "--cue",
        "通用却带项目",
        "--project",
        "engram",
        "--id",
        "bad_d",
        "--now",
        "1000000000",
    ]);
    assert!(!out_d.status.success(), "L1-3 带 --project 应失败");

    // (e) L4 的 --project NAME 不在 --project-db 映射中（挂了 engram 却写 other）→ 失败。
    let out_e = run_write_raw(&[
        "--general-db",
        &g,
        "--project-db",
        &p_kv,
        "--level",
        "L4.1",
        "--cue",
        "项目名不在映射",
        "--project",
        "other",
        "--id",
        "bad_e",
        "--now",
        "1000000000",
    ]);
    assert!(
        !out_e.status.success(),
        "L4 项目名不在 --project-db 映射应失败"
    );

    // 任何 bad_* 都不应落库。公共库可能因 open 创建了空文件，但不应有这些 id。
    let gdb = store::open(&general_path).expect("应能打开公共库");
    for id in ["bad_a", "bad_b", "bad_c", "bad_d", "bad_e"] {
        assert!(
            store::get(&gdb, id).expect("get 应成功").is_none(),
            "校验失败的记忆 {id} 不应落公共库"
        );
    }
    drop(gdb);

    // 项目库也不应含任何 bad_* 记忆。
    let pdb = store::open(&project_path).expect("应能打开项目库");
    for id in ["bad_a", "bad_b", "bad_c", "bad_d", "bad_e"] {
        assert!(
            store::get(&pdb, id).expect("get 应成功").is_none(),
            "校验失败的记忆 {id} 不应落项目库"
        );
    }
    drop(pdb);

    cleanup_file(&general_path);
    cleanup_file(&project_path);
}

// 11. write 自动 id 唯一：连续两次写（不指定 id）应得到两个不同 id，且都落库。
#[test]
fn write_auto_id_is_unique() {
    let _guard = test_guard();
    let general_path = unique_db_path("write_autoid_g");
    let g = general_path.to_string_lossy().to_string();

    let id1 = run_write(&["--general-db", &g, "--level", "L3", "--cue", "自动 id 甲"]);
    let id2 = run_write(&["--general-db", &g, "--level", "L3", "--cue", "自动 id 乙"]);
    assert_ne!(id1, id2, "两次自动生成的 id 应不同");
    assert!(id1.starts_with("mem-"), "自动 id 应以 mem- 前缀开头");
    assert!(id2.starts_with("mem-"));

    let gdb = store::open(&general_path).expect("应能打开公共库");
    assert!(
        store::get(&gdb, &id1).expect("get 应成功").is_some(),
        "id1 应落库"
    );
    assert!(
        store::get(&gdb, &id2).expect("get 应成功").is_some(),
        "id2 应落库"
    );
    drop(gdb);

    cleanup_file(&general_path);
}

// 12. recall 词法检索（跨公共库 + 两个项目库）：命中词多者排前、--limit 截断、
//     默认搜到 cold、--active-only 排除 cold、superseded 不返回、--json 可解析、
//     且不写任何库；项目库里的 L4 命中也应被检索到。
#[test]
fn recall_search_ranking_and_filters() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    // 公共库：
    //  - hit2：cue+tag 命中 "redb" 与 "lock" 两词。
    //  - hit1：cue 仅命中 "redb"。
    //  - cold：cold 状态，命中 "redb"。
    //  - sup：superseded，命中 "redb"，应永不返回。
    let mut hit2 = make("hit2", Level::L3, None, Status::Active, 0.5, now, vec![now]);
    hit2.cue = "redb 文件锁冲突".to_string();
    hit2.tags = vec!["lock".to_string()];
    let mut hit1 = make("hit1", Level::L3, None, Status::Active, 0.5, now, vec![now]);
    hit1.cue = "redb 入门笔记".to_string();
    let mut cold = make("coldm", Level::L3, None, Status::Cold, 0.5, now, vec![now]);
    cold.cue = "redb 冷藏经验".to_string();
    let mut sup = make(
        "supm",
        Level::L3,
        None,
        Status::Superseded,
        0.5,
        now,
        vec![now],
    );
    sup.cue = "redb 旧结论已被取代".to_string();

    // alpha 项目库：一条命中 "redb" 的 L4。
    let mut pa_hit = make(
        "pa_hit",
        Level::L4_3,
        Some("alpha"),
        Status::Active,
        0.5,
        now,
        vec![now],
    );
    pa_hit.cue = "redb 项目笔记 alpha".to_string();

    let general_path = seed_db("recall_g", &[hit2, hit1, cold, sup]);
    let pa_path = seed_db("recall_pa", &[pa_hit]);
    let g = general_path.to_string_lossy().to_string();
    let pa_kv = format!("alpha={}", pa_path.display());

    // 默认 recall（搜 active+cold，挂上 alpha 项目库）：query "redb lock"。
    let out = run_recall(&[
        "--general-db",
        &g,
        "--project-db",
        &pa_kv,
        "--query",
        "redb lock",
        "--now",
        "1000000000",
    ]);
    // hit2 命中 2 词应在 hit1（1 词）之前。
    let pos_hit2 = out.find("hit2").expect("应含 hit2");
    let pos_hit1 = out.find("hit1").expect("应含 hit1");
    assert!(pos_hit2 < pos_hit1, "命中词多的 hit2 应排在 hit1 之前");
    assert!(out.contains("coldm"), "默认应搜到 cold 记忆");
    assert!(
        out.contains("pa_hit"),
        "应跨项目库检索到 alpha 的 L4 命中，实得：{out}"
    );
    assert!(
        !out.contains("supm"),
        "superseded 记忆永不返回，实得：{out}"
    );

    // --active-only：cold 应被排除。
    let out_active = run_recall(&[
        "--general-db",
        &g,
        "--project-db",
        &pa_kv,
        "--query",
        "redb",
        "--active-only",
        "--now",
        "1000000000",
    ]);
    assert!(out_active.contains("hit1"), "active 记忆应在");
    assert!(
        !out_active.contains("coldm"),
        "--active-only 应排除 cold，实得：{out_active}"
    );

    // --limit 1：只返回 1 条候选。
    let out_limit = run_recall(&[
        "--general-db",
        &g,
        "--project-db",
        &pa_kv,
        "--query",
        "redb",
        "--limit",
        "1",
        "--now",
        "1000000000",
    ]);
    assert!(out_limit.contains("共 1 条候选"), "limit=1 应只 1 条候选");

    // --json 可被 serde_json 解析为数组，且含 score 字段。
    let out_json = run_recall(&[
        "--general-db",
        &g,
        "--project-db",
        &pa_kv,
        "--query",
        "redb lock",
        "--json",
        "--now",
        "1000000000",
    ]);
    let parsed: serde_json::Value =
        serde_json::from_str(out_json.trim()).expect("recall --json 应可解析");
    let arr = parsed.as_array().expect("应为 JSON 数组");
    assert!(!arr.is_empty(), "候选数组不应为空");
    assert!(arr[0].get("score").is_some(), "每条候选应含 score 字段");
    // 第一条应是命中 2 词的 hit2。
    assert_eq!(
        arr[0].get("id").and_then(|v| v.as_str()),
        Some("hit2"),
        "JSON 首条应为命中词最多的 hit2"
    );

    // recall 不写库：公共库四种记忆状态/数量应原样不变。
    let gdb = store::open(&general_path).expect("应能重新打开公共库");
    let all_after = store::all(&gdb).expect("all 应成功");
    assert_eq!(all_after.len(), 4, "recall 不应增删公共库记忆");
    let sup_after = store::get(&gdb, "supm")
        .expect("get 应成功")
        .expect("supm 应在");
    assert_eq!(sup_after.status, Status::Superseded, "recall 不应改写状态");
    drop(gdb);

    // 项目库也不应被改动：pa_hit 仍在、仍为 L4.3/active。
    let pa_db = store::open(&pa_path).expect("应能重新打开 alpha 项目库");
    let pa_after = store::all(&pa_db).expect("all 应成功");
    assert_eq!(pa_after.len(), 1, "recall 不应增删项目库记忆");
    drop(pa_db);

    cleanup_file(&general_path);
    cleanup_file(&pa_path);
}

// 13. list 跨多库过滤与 JSON：挂两个项目库（engram + other），按
//     status/level/project 过滤正确，--json 含 effective 字段。
#[test]
fn list_filters_and_json_has_effective() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    let gmems = vec![
        make("la", Level::L1, None, Status::Active, 0.5, now, vec![now]),
        make("lb", Level::L3, None, Status::Cold, 0.5, now, vec![now]),
    ];
    // lc 属 engram 项目库，ld 属 other 项目库（分置两库）。
    let pa_mems = vec![make(
        "lc",
        Level::L4_2,
        Some("engram"),
        Status::Active,
        0.5,
        now,
        vec![now],
    )];
    let pb_mems = vec![make(
        "ld",
        Level::L4_2,
        Some("other"),
        Status::Active,
        0.5,
        now,
        vec![now],
    )];
    let general_path = seed_db("list_g", &gmems);
    let pa_path = seed_db("list_pa", &pa_mems);
    let pb_path = seed_db("list_pb", &pb_mems);
    let g = general_path.to_string_lossy().to_string();
    let pa_kv = format!("engram={}", pa_path.display());
    let pb_kv = format!("other={}", pb_path.display());

    // 按 status=cold：只 lb。
    let out_cold = run_list(&[
        "--general-db",
        &g,
        "--project-db",
        &pa_kv,
        "--project-db",
        &pb_kv,
        "--status",
        "cold",
        "--now",
        "1000000000",
    ]);
    assert!(out_cold.contains("cue-lb"), "cold 过滤应含 lb");
    assert!(!out_cold.contains("cue-la"), "cold 过滤不应含 active la");
    assert!(!out_cold.contains("cue-lc"), "cold 过滤不应含 active lc");

    // 按 level=L4.2：lc、ld（来自两个不同项目库）。
    let out_level = run_list(&[
        "--general-db",
        &g,
        "--project-db",
        &pa_kv,
        "--project-db",
        &pb_kv,
        "--level",
        "L4.2",
        "--now",
        "1000000000",
    ]);
    assert!(
        out_level.contains("cue-lc") && out_level.contains("cue-ld"),
        "L4.2 过滤应跨两个项目库都含，实得：{out_level}"
    );
    assert!(!out_level.contains("cue-la"), "L4.2 过滤不应含 L1 la");

    // 按 project=engram：只 lc（other 的 ld 与通用 la/lb 都被排除）。
    let out_proj = run_list(&[
        "--general-db",
        &g,
        "--project-db",
        &pa_kv,
        "--project-db",
        &pb_kv,
        "--project",
        "engram",
        "--now",
        "1000000000",
    ]);
    assert!(out_proj.contains("cue-lc"), "engram 过滤应含 lc");
    assert!(
        !out_proj.contains("cue-ld"),
        "engram 过滤不应含 other 的 ld"
    );
    assert!(!out_proj.contains("cue-la"), "engram 过滤不应含通用 la");

    // --json：解析为数组（默认 status=all 跨三库共 4 条），每条含 effective 字段。
    let out_json = run_list(&[
        "--general-db",
        &g,
        "--project-db",
        &pa_kv,
        "--project-db",
        &pb_kv,
        "--json",
        "--now",
        "1000000000",
    ]);
    let parsed: serde_json::Value =
        serde_json::from_str(out_json.trim()).expect("list --json 应可解析");
    let arr = parsed.as_array().expect("应为 JSON 数组");
    assert_eq!(
        arr.len(),
        4,
        "默认 status=all 应含全部 4 条（公共 2 + 两项目各 1）"
    );
    for item in arr {
        assert!(
            item.get("effective").is_some(),
            "每条 JSON 记忆应附带 effective 字段"
        );
        // 同时应保留 Memory 本体字段（如 id、level）。
        assert!(item.get("id").is_some(), "应保留 id 字段");
        assert!(item.get("level").is_some(), "应保留 level 字段");
    }

    cleanup_file(&general_path);
    cleanup_file(&pa_path);
    cleanup_file(&pb_path);
}

// 14. --project-db 端到端解析校验：malformed（无 '='）或重复 name 应非 0 退出。
#[test]
fn project_db_parse_errors_exit_nonzero() {
    let _guard = test_guard();
    let general_path = unique_db_path("parse_err_g");
    let g = general_path.to_string_lossy().to_string();
    let exe = env!("CARGO_BIN_EXE_engram");

    // (a) malformed：缺少 '='。借 render 命令触发解析。
    let out_malformed = Command::new(exe)
        .arg("render")
        .arg("--general-db")
        .arg(&g)
        .arg("--project-db")
        .arg("noeq")
        .arg("--now")
        .arg("1000000000")
        .output()
        .expect("运行 engram 二进制失败");
    assert!(
        !out_malformed.status.success(),
        "malformed --project-db（无 '='）应非 0 退出"
    );

    // (b) 重复 name：同名 a 给两次。
    let out_dup = Command::new(exe)
        .arg("render")
        .arg("--general-db")
        .arg(&g)
        .arg("--project-db")
        .arg("a=/tmp/x.redb")
        .arg("--project-db")
        .arg("a=/tmp/y.redb")
        .arg("--now")
        .arg("1000000000")
        .output()
        .expect("运行 engram 二进制失败");
    assert!(
        !out_dup.status.success(),
        "重复 name 的 --project-db 应非 0 退出"
    );
    let stderr = String::from_utf8_lossy(&out_dup.stderr);
    assert!(
        stderr.contains("重复"),
        "stderr 应说明因重复 name 报错，实得：{stderr}"
    );

    cleanup_file(&general_path);
}

// ============================================================================
// 切片 4b：confirm-use / supersede / merge / gc 四个维护命令的集成测试
// ============================================================================

/// 调用一个 engram 子命令并断言成功，返回完整 Output（调用方自行取 stdout/stderr）。
fn run_subcommand_raw(subcommand: &str, args: &[&str]) -> std::process::Output {
    let exe = env!("CARGO_BIN_EXE_engram");
    let mut cmd = Command::new(exe);
    cmd.arg(subcommand);
    for a in args {
        cmd.arg(a);
    }
    cmd.output().expect("运行 engram 二进制失败")
}

// 15. confirm-use：给 active 记忆追加使用使 effective 上升；id 不存在不崩。
#[test]
fn confirm_use_appends_access_and_raises_effective() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    let created = now - 5.0 * 86400.0;
    // 一条 L3，仅 5 天前单次访问（effective 偏低）。
    let gmems = vec![make(
        "cu_act",
        Level::L3,
        None,
        Status::Active,
        0.0,
        created,
        vec![created],
    )];
    let general_path = seed_db("cu_act_g", &gmems);
    let g = general_path.to_string_lossy().to_string();

    // 先读 effective（用 list --json，now=now）。
    let before_json = run_list(&["--general-db", &g, "--json", "--now", "1000000000"]);
    let before: serde_json::Value = serde_json::from_str(before_json.trim()).expect("应可解析");
    let eff_before = before.as_array().expect("数组")[0]
        .get("effective")
        .and_then(|v| v.as_f64())
        .expect("应有 effective");

    // confirm-use 追加一次 now 的真使用，并附带一个不存在的 id（不应崩）。
    let out = run_subcommand_raw(
        "confirm-use",
        &[
            "--general-db",
            &g,
            "--ids",
            "cu_act,does_not_exist",
            "--now",
            "1000000000",
        ],
    );
    assert!(
        out.status.success(),
        "confirm-use 应成功退出，stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("cu_act"), "应打印 cu_act 的处理结果");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("does_not_exist"),
        "不存在的 id 应记 stderr 而非崩溃，实得：{stderr}"
    );

    // 读库验证 access_log 多了一条 now。
    let gdb = store::open(&general_path).expect("应能重新打开公共库");
    let got = store::get(&gdb, "cu_act")
        .expect("get 应成功")
        .expect("应读到 cu_act");
    assert_eq!(got.access_log.len(), 2, "应追加一次访问（共 2 次）");
    assert!(
        got.access_log.iter().any(|&t| (t - now).abs() < 1e-6),
        "新追加的访问时间应为 now"
    );
    drop(gdb);

    // effective 应上升（多一次近期访问）。
    let after_json = run_list(&["--general-db", &g, "--json", "--now", "1000000000"]);
    let after: serde_json::Value = serde_json::from_str(after_json.trim()).expect("应可解析");
    let eff_after = after.as_array().expect("数组")[0]
        .get("effective")
        .and_then(|v| v.as_f64())
        .expect("应有 effective");
    assert!(
        eff_after > eff_before,
        "追加真使用后 effective 应上升：before={eff_before} after={eff_after}"
    );

    cleanup_file(&general_path);
}

// 16. confirm-use 复活：Cold 通用记忆复活为 Active 且 level=L3；项目 Cold 复活 level=L4.3；
//     且各自写回正确的库。
#[test]
fn confirm_use_revives_cold_to_correct_level_and_db() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    // 公共库一条 Cold 通用记忆（原 level 不重要，复活应回 L3）。
    let gmems = vec![make(
        "cu_cold_g",
        Level::L3,
        None,
        Status::Cold,
        0.0,
        now - 10.0 * 86400.0,
        vec![now - 10.0 * 86400.0],
    )];
    // 项目库一条 Cold L4.3。
    let pmems = vec![make(
        "cu_cold_p",
        Level::L4_3,
        Some("engram"),
        Status::Cold,
        0.0,
        now - 10.0 * 86400.0,
        vec![now - 10.0 * 86400.0],
    )];
    let general_path = seed_db("cu_rev_g", &gmems);
    let project_path = seed_db("cu_rev_p", &pmems);
    let g = general_path.to_string_lossy().to_string();
    let p_kv = format!("engram={}", project_path.display());

    let out = run_subcommand_raw(
        "confirm-use",
        &[
            "--general-db",
            &g,
            "--project-db",
            &p_kv,
            "--ids",
            "cu_cold_g,cu_cold_p",
            "--now",
            "1000000000",
        ],
    );
    assert!(
        out.status.success(),
        "confirm-use 应成功，stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 通用 Cold → Active、level=L3，落公共库。
    let gdb = store::open(&general_path).expect("应能重新打开公共库");
    let got_g = store::get(&gdb, "cu_cold_g")
        .expect("get 应成功")
        .expect("应读到");
    assert_eq!(got_g.status, Status::Active, "通用 Cold 应复活为 Active");
    assert_eq!(got_g.level, Level::L3, "通用复活应回 L3");
    drop(gdb);

    // 项目 Cold → Active、level=L4.3，落项目库（不串公共库）。
    let pdb = store::open(&project_path).expect("应能重新打开项目库");
    let got_p = store::get(&pdb, "cu_cold_p")
        .expect("get 应成功")
        .expect("应读到");
    assert_eq!(got_p.status, Status::Active, "项目 Cold 应复活为 Active");
    assert_eq!(got_p.level, Level::L4_3, "项目复活应回 L4.3");
    drop(pdb);
    let gdb2 = store::open(&general_path).expect("应能重新打开公共库");
    assert!(
        store::get(&gdb2, "cu_cold_p")
            .expect("get 应成功")
            .is_none(),
        "项目记忆不应被误写进公共库"
    );
    drop(gdb2);

    cleanup_file(&general_path);
    cleanup_file(&project_path);
}

// 17. supersede：OLD → Superseded、superseded_by 设置；之后 render/list active 不再出现。
#[test]
fn supersede_marks_and_hides_from_render() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    let gmems = vec![
        make("old", Level::L1, None, Status::Active, 0.8, now, vec![now]),
        make(
            "newer",
            Level::L1,
            None,
            Status::Active,
            0.8,
            now,
            vec![now],
        ),
    ];
    let general_path = seed_db("sup_g", &gmems);
    let g = general_path.to_string_lossy().to_string();

    // 先确认 render 中 old 可见。
    let before = run_render(&general_path, &[], now);
    assert!(before.contains("cue-old"), "标记前 render 应含 old");

    let out = run_subcommand_raw(
        "supersede",
        &[
            "--general-db",
            &g,
            "--id",
            "old",
            "--by",
            "newer",
            "--now",
            "1000000000",
        ],
    );
    assert!(
        out.status.success(),
        "supersede 应成功，stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 读库验证状态与 superseded_by。
    let gdb = store::open(&general_path).expect("应能重新打开公共库");
    let got = store::get(&gdb, "old")
        .expect("get 应成功")
        .expect("应读到 old");
    assert_eq!(got.status, Status::Superseded, "old 应转 Superseded");
    assert_eq!(
        got.superseded_by,
        Some("newer".to_string()),
        "superseded_by 应指向 newer"
    );
    drop(gdb);

    // render 不再展示 old（render 只显示 active），但 newer 仍在。
    let after = run_render(&general_path, &[], now);
    assert!(
        !after.contains("cue-old"),
        "标记后 render 不应再含 old，实得：\n{after}"
    );
    assert!(after.contains("cue-newer"), "newer 仍应在 render 中");

    cleanup_file(&general_path);
}

// 18. supersede：NEW 不存在仅警告、仍照常标记 OLD。
#[test]
fn supersede_with_missing_new_still_marks() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    let gmems = vec![make(
        "old2",
        Level::L2,
        None,
        Status::Active,
        0.5,
        now,
        vec![now],
    )];
    let general_path = seed_db("sup_missing_g", &gmems);
    let g = general_path.to_string_lossy().to_string();

    let out = run_subcommand_raw(
        "supersede",
        &[
            "--general-db",
            &g,
            "--id",
            "old2",
            "--by",
            "ghost_id",
            "--now",
            "1000000000",
        ],
    );
    assert!(out.status.success(), "NEW 不存在仍应成功标记 OLD");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ghost_id"),
        "NEW 不存在应 stderr 警告，实得：{stderr}"
    );

    let gdb = store::open(&general_path).expect("应能重新打开公共库");
    let got = store::get(&gdb, "old2")
        .expect("get 应成功")
        .expect("应读到 old2");
    assert_eq!(got.status, Status::Superseded, "仍应标记为 Superseded");
    assert_eq!(got.superseded_by, Some("ghost_id".to_string()));
    drop(gdb);

    cleanup_file(&general_path);
}

// 19. merge：新记忆字段正确（access_log 并集、importance 取最大、level 取最高、tags 含 merged）；
//     源全部变 Tombstone 且 superseded_by=新 id。
#[test]
fn merge_creates_new_and_tombstones_sources() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    // 两条通用源：s1 L3/imp0.2、s2 L2/imp0.7；access_log 各不同。
    let s1 = make(
        "ms1",
        Level::L3,
        None,
        Status::Active,
        0.2,
        now,
        vec![10.0, 20.0],
    );
    let s2 = make(
        "ms2",
        Level::L2,
        None,
        Status::Active,
        0.7,
        now,
        vec![20.0, 30.0],
    );
    let general_path = seed_db("merge_g", &[s1, s2]);
    let g = general_path.to_string_lossy().to_string();

    let out = run_subcommand_raw(
        "merge",
        &[
            "--general-db",
            &g,
            "--from",
            "ms1,ms2",
            "--cue",
            "合并后的通用记忆",
            "--id",
            "merged_id",
            "--now",
            "1000000000",
        ],
    );
    assert!(
        out.status.success(),
        "merge 应成功，stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("merged_id"), "应打印新 id");

    let gdb = store::open(&general_path).expect("应能重新打开公共库");
    // 新记忆字段。
    let merged = store::get(&gdb, "merged_id")
        .expect("get 应成功")
        .expect("应读到新合并记忆");
    assert_eq!(merged.cue, "合并后的通用记忆");
    assert_eq!(merged.level, Level::L2, "应取源中最高层 L2");
    assert!(
        (merged.importance - 0.7).abs() < 1e-9,
        "importance 应取最大 0.7"
    );
    assert_eq!(
        merged.access_log,
        vec![10.0, 20.0, 30.0],
        "access_log 应并集去重升序"
    );
    assert_eq!(merged.project, None, "通用合并 project 应为 None");
    assert_eq!(merged.status, Status::Active);
    assert!(
        merged.tags.contains(&"merged".to_string()),
        "tags 应含 merged"
    );

    // 源全部 Tombstone、superseded_by=新 id。
    for sid in ["ms1", "ms2"] {
        let s = store::get(&gdb, sid)
            .expect("get 应成功")
            .unwrap_or_else(|| panic!("应读到源 {sid}"));
        assert_eq!(s.status, Status::Tombstone, "源 {sid} 应转 Tombstone");
        assert_eq!(
            s.superseded_by,
            Some("merged_id".to_string()),
            "源 {sid} 的 superseded_by 应指向新 id"
        );
    }
    drop(gdb);

    cleanup_file(&general_path);
}

// 20. merge 混作用域报错：一条通用 + 一条项目 → 非 0 退出，不写任何库。
#[test]
fn merge_mixed_scope_fails() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    let gmems = vec![make(
        "mg",
        Level::L3,
        None,
        Status::Active,
        0.5,
        now,
        vec![now],
    )];
    let pmems = vec![make(
        "mp",
        Level::L4_3,
        Some("engram"),
        Status::Active,
        0.5,
        now,
        vec![now],
    )];
    let general_path = seed_db("merge_mix_g", &gmems);
    let project_path = seed_db("merge_mix_p", &pmems);
    let g = general_path.to_string_lossy().to_string();
    let p_kv = format!("engram={}", project_path.display());

    let out = run_subcommand_raw(
        "merge",
        &[
            "--general-db",
            &g,
            "--project-db",
            &p_kv,
            "--from",
            "mg,mp",
            "--cue",
            "混作用域应失败",
            "--id",
            "should_not_exist",
            "--now",
            "1000000000",
        ],
    );
    assert!(!out.status.success(), "混作用域 merge 应非 0 退出");

    // 不应产生新记忆，源也不应被改动。
    let gdb = store::open(&general_path).expect("应能重新打开公共库");
    assert!(
        store::get(&gdb, "should_not_exist")
            .expect("get 应成功")
            .is_none(),
        "失败的 merge 不应产生新记忆"
    );
    let mg = store::get(&gdb, "mg")
        .expect("get 应成功")
        .expect("mg 应在");
    assert_eq!(mg.status, Status::Active, "失败的 merge 不应改动源状态");
    drop(gdb);

    cleanup_file(&general_path);
    cleanup_file(&project_path);
}

// 21. gc：cold 超 ttl 被删；cold 未超 ttl 保留；cold 但最近 confirm-use 过不被删；
//     tombstone 未超长 ttl 保留；active/superseded 永不删。
#[test]
fn gc_deletes_only_expired_cold_and_tombstone() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    let day = 86400.0;
    let gmems = vec![
        // cold 超 ttl（200 天）→ 删。
        make(
            "gc_cold_old",
            Level::L3,
            None,
            Status::Cold,
            0.0,
            now - 200.0 * day,
            vec![now - 200.0 * day],
        ),
        // cold 未超 ttl（100 天）→ 留。
        make(
            "gc_cold_fresh",
            Level::L3,
            None,
            Status::Cold,
            0.0,
            now - 100.0 * day,
            vec![now - 100.0 * day],
        ),
        // cold 但 last_touch 仅 1 天前（创建很久）→ 留。
        make(
            "gc_cold_touched",
            Level::L3,
            None,
            Status::Cold,
            0.0,
            now - 999.0 * day,
            vec![now - 999.0 * day, now - 1.0 * day],
        ),
        // tombstone 未超长 ttl（1000 天 < 3650）→ 留。
        make(
            "gc_tomb_fresh",
            Level::L3,
            None,
            Status::Tombstone,
            0.0,
            now - 1000.0 * day,
            vec![now - 1000.0 * day],
        ),
        // active 极久 → 永不删。
        make(
            "gc_active",
            Level::L3,
            None,
            Status::Active,
            0.0,
            now - 9999.0 * day,
            vec![now - 9999.0 * day],
        ),
        // superseded 极久 → 永不删。
        make(
            "gc_sup",
            Level::L3,
            None,
            Status::Superseded,
            0.0,
            now - 9999.0 * day,
            vec![now - 9999.0 * day],
        ),
    ];
    let general_path = seed_db("gc_g", &gmems);
    let g = general_path.to_string_lossy().to_string();

    // 先 dry-run：报告将删 gc_cold_old，但不真删。
    let dry = run_subcommand_raw(
        "gc",
        &["--general-db", &g, "--dry-run", "--now", "1000000000"],
    );
    assert!(dry.status.success(), "gc --dry-run 应成功");
    let dry_out = String::from_utf8_lossy(&dry.stdout);
    assert!(
        dry_out.contains("gc_cold_old"),
        "dry-run 应报告将删 cold_old"
    );
    assert!(dry_out.contains("dry-run"), "应标注 dry-run");
    // dry-run 不删：所有记忆仍在。
    let gdb = store::open(&general_path).expect("应能重新打开公共库");
    assert_eq!(
        store::all(&gdb).expect("all 应成功").len(),
        6,
        "dry-run 不应删除任何记忆"
    );
    drop(gdb);

    // 真删。
    let out = run_subcommand_raw("gc", &["--general-db", &g, "--now", "1000000000"]);
    assert!(
        out.status.success(),
        "gc 应成功，stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let gdb = store::open(&general_path).expect("应能重新打开公共库");
    // 仅 gc_cold_old 被删。
    assert!(
        store::get(&gdb, "gc_cold_old")
            .expect("get 应成功")
            .is_none(),
        "超 ttl 的 cold 应被删"
    );
    for keep in [
        "gc_cold_fresh",
        "gc_cold_touched",
        "gc_tomb_fresh",
        "gc_active",
        "gc_sup",
    ] {
        assert!(
            store::get(&gdb, keep).expect("get 应成功").is_some(),
            "{keep} 不应被删"
        );
    }
    drop(gdb);

    cleanup_file(&general_path);
}

// 22. gc tombstone 超长 ttl：超过 tombstone_ttl_days 的墓碑被删；冷条目走各自项目库删除路由。
#[test]
fn gc_deletes_old_tombstone_and_routes_per_db() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    let day = 86400.0;
    // 公共库一条超长 ttl 的 tombstone（4000 天）。
    let gmems = vec![make(
        "gc_tomb_old",
        Level::L3,
        None,
        Status::Tombstone,
        0.0,
        now - 4000.0 * day,
        vec![now - 4000.0 * day],
    )];
    // 项目库一条超 ttl 的 cold（应从项目库删，不影响公共库）。
    let pmems = vec![make(
        "gc_p_cold",
        Level::L4_3,
        Some("engram"),
        Status::Cold,
        0.0,
        now - 300.0 * day,
        vec![now - 300.0 * day],
    )];
    let general_path = seed_db("gc_route_g", &gmems);
    let project_path = seed_db("gc_route_p", &pmems);
    let g = general_path.to_string_lossy().to_string();
    let p_kv = format!("engram={}", project_path.display());

    let out = run_subcommand_raw(
        "gc",
        &[
            "--general-db",
            &g,
            "--project-db",
            &p_kv,
            "--now",
            "1000000000",
        ],
    );
    assert!(
        out.status.success(),
        "gc 应成功，stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 公共库的老墓碑被删。
    let gdb = store::open(&general_path).expect("应能重新打开公共库");
    assert!(
        store::get(&gdb, "gc_tomb_old")
            .expect("get 应成功")
            .is_none(),
        "超长 ttl 的 tombstone 应被删"
    );
    drop(gdb);

    // 项目库的老 cold 从项目库删。
    let pdb = store::open(&project_path).expect("应能重新打开项目库");
    assert!(
        store::get(&pdb, "gc_p_cold").expect("get 应成功").is_none(),
        "项目库超 ttl 的 cold 应从项目库删"
    );
    drop(pdb);

    cleanup_file(&general_path);
    cleanup_file(&project_path);
}

// ============================================================================
// hook 辅助命令：resolve / session-start 的集成测试
// ============================================================================

/// 在系统临时目录下构造一个进程内唯一的**空**项目目录，返回其路径（已创建）。
fn unique_project_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let pid = std::process::id();
    dir.push(format!("engram_it_proj_{tag}_{pid}_{}", unique_suffix()));
    std::fs::create_dir_all(&dir).expect("应能创建临时项目目录");
    dir
}

// 23. resolve --format env：输出三行 ENGRAM_* 且路径正确（project_db 为
//     <project_dir>/.claude/engram.redb、general_db 为 --general-db 给定值、
//     project_name 为项目目录最后一段名）。
#[test]
fn resolve_env_format_outputs_three_lines() {
    let _guard = test_guard();
    let project_dir = unique_project_dir("resolve_env");
    let general_path = unique_db_path("resolve_env_g");
    let p = project_dir.to_string_lossy().to_string();
    let g = general_path.to_string_lossy().to_string();

    let out = run_subcommand_raw(
        "resolve",
        &["--project-dir", &p, "--general-db", &g, "--format", "env"],
    );
    assert!(
        out.status.success(),
        "resolve 应成功，stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout 非 UTF-8");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3, "env 格式应恰好三行，实得：\n{stdout}");

    // general_db = 给定 override。
    let expected_general = format!("ENGRAM_GENERAL_DB={}", general_path.display());
    assert!(
        lines.contains(&expected_general.as_str()),
        "应含 general_db 行，实得：\n{stdout}"
    );
    // project_db = <project_dir>/.claude/engram.redb。
    let expected_pdb = project_dir.join(".claude").join("engram.redb");
    let expected_pdb_line = format!("ENGRAM_PROJECT_DB={}", expected_pdb.display());
    assert!(
        lines.contains(&expected_pdb_line.as_str()),
        "应含 project_db 行 {expected_pdb_line}，实得：\n{stdout}"
    );
    // project_name = 项目目录最后一段名。
    let name = project_dir
        .file_name()
        .and_then(|s| s.to_str())
        .expect("项目目录应有名字");
    let expected_name_line = format!("ENGRAM_PROJECT_NAME={name}");
    assert!(
        lines.contains(&expected_name_line.as_str()),
        "应含 project_name 行 {expected_name_line}，实得：\n{stdout}"
    );

    // resolve 应已 create_dir_all 项目库父目录 <project_dir>/.claude。
    assert!(
        project_dir.join(".claude").is_dir(),
        "resolve 应创建出 <project_dir>/.claude 目录"
    );

    cleanup_file(&general_path);
    let _ = std::fs::remove_dir_all(&project_dir);
}

// 24. resolve --format json：输出可被 serde_json 解析，三字段齐全且值正确。
#[test]
fn resolve_json_format_is_parseable() {
    let _guard = test_guard();
    let project_dir = unique_project_dir("resolve_json");
    let general_path = unique_db_path("resolve_json_g");
    let p = project_dir.to_string_lossy().to_string();
    let g = general_path.to_string_lossy().to_string();

    let out = run_subcommand_raw(
        "resolve",
        &["--project-dir", &p, "--general-db", &g, "--format", "json"],
    );
    assert!(
        out.status.success(),
        "resolve --format json 应成功，stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout 非 UTF-8");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("resolve --format json 应可解析");

    // general_db 字段 = 给定 override。
    assert_eq!(
        parsed.get("general_db").and_then(|v| v.as_str()),
        Some(general_path.to_string_lossy().as_ref()),
        "json general_db 应等于 --general-db"
    );
    // project_db 字段 = <project_dir>/.claude/engram.redb。
    let expected_pdb = project_dir.join(".claude").join("engram.redb");
    assert_eq!(
        parsed.get("project_db").and_then(|v| v.as_str()),
        Some(expected_pdb.to_string_lossy().as_ref()),
        "json project_db 应为 <project_dir>/.claude/engram.redb"
    );
    // project_name 字段 = 目录最后一段名。
    let name = project_dir
        .file_name()
        .and_then(|s| s.to_str())
        .expect("项目目录应有名字");
    assert_eq!(
        parsed.get("project_name").and_then(|v| v.as_str()),
        Some(name),
        "json project_name 应为目录最后一段名"
    );

    cleanup_file(&general_path);
    let _ = std::fs::remove_dir_all(&project_dir);
}

// 25. session-start 空临时目录：exit 0、stdout 含前言与“== Engram 热索引”，
//     且会创建出 <project_dir>/.claude/ 目录（即便项目库本不存在）。
#[test]
fn session_start_on_empty_dir_prints_index_and_creates_claude_dir() {
    let _guard = test_guard();
    let project_dir = unique_project_dir("ss_empty");
    let general_path = unique_db_path("ss_empty_g");
    let p = project_dir.to_string_lossy().to_string();
    let g = general_path.to_string_lossy().to_string();

    let out = run_subcommand_raw(
        "session-start",
        &[
            "--project-dir",
            &p,
            "--general-db",
            &g,
            "--now",
            "1000000000",
        ],
    );
    assert!(
        out.status.success(),
        "session-start 在空目录上应 exit 0，stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout 非 UTF-8");
    // 前言。
    assert!(
        stdout.contains("以下是你的 engram 长期记忆热索引"),
        "stdout 应含前言，实得：\n{stdout}"
    );
    // 热索引表头。
    assert!(
        stdout.contains("== Engram 热索引"),
        "stdout 应含热索引表头，实得：\n{stdout}"
    );

    // 会创建出 <project_dir>/.claude/（项目库父目录）。
    assert!(
        project_dir.join(".claude").is_dir(),
        "session-start 应创建出 <project_dir>/.claude 目录"
    );

    cleanup_file(&general_path);
    // session-start 会在 .claude 下创建 engram.redb 空库，连目录一并清理。
    let _ = std::fs::remove_dir_all(&project_dir);
}

// 26. session-start --emit json：stdout 是一行可被 serde_json 解析的对象，
//     hookSpecificOutput.hookEventName == "SessionStart"，additionalContext
//     非空且含「Engram 热索引」字样（说明整段渲染文本确被塞进去）。
#[test]
fn session_start_emit_json_wraps_context() {
    let _guard = test_guard();
    let project_dir = unique_project_dir("ss_json");
    let general_path = unique_db_path("ss_json_g");
    let p = project_dir.to_string_lossy().to_string();
    let g = general_path.to_string_lossy().to_string();

    let out = run_subcommand_raw(
        "session-start",
        &[
            "--project-dir",
            &p,
            "--general-db",
            &g,
            "--now",
            "1000000000",
            "--emit",
            "json",
        ],
    );
    assert!(
        out.status.success(),
        "session-start --emit json 应 exit 0，stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout 非 UTF-8");
    // 应恰好一行 JSON。
    assert_eq!(
        stdout.lines().count(),
        1,
        "--emit json 应只打印一行 JSON，实得：\n{stdout}"
    );
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("session-start --emit json 应可被解析");

    let hook = parsed
        .get("hookSpecificOutput")
        .expect("应含 hookSpecificOutput");
    assert_eq!(
        hook.get("hookEventName").and_then(|v| v.as_str()),
        Some("SessionStart"),
        "hookEventName 应为 SessionStart"
    );
    let ctx = hook
        .get("additionalContext")
        .and_then(|v| v.as_str())
        .expect("additionalContext 应为字符串");
    assert!(!ctx.is_empty(), "additionalContext 非空");
    assert!(
        ctx.contains("Engram 热索引"),
        "additionalContext 应含整段渲染文本（Engram 热索引），实得：\n{ctx}"
    );
    assert!(
        ctx.contains("以下是你的 engram 长期记忆热索引"),
        "additionalContext 应含前言行，实得：\n{ctx}"
    );

    cleanup_file(&general_path);
    let _ = std::fs::remove_dir_all(&project_dir);
}

// 27. session-start --log <临时文件>：运行后该文件存在，且含一行带
//     "session-start" 与 project 名的记录；日志父目录不存在时能自动创建。
#[test]
fn session_start_log_appends_record_and_creates_parent() {
    let _guard = test_guard();
    let project_dir = unique_project_dir("ss_log");
    let general_path = unique_db_path("ss_log_g");
    let p = project_dir.to_string_lossy().to_string();
    let g = general_path.to_string_lossy().to_string();

    // 日志放在一个**尚不存在**的子目录下，验证父目录会被自动创建。
    let log_path = project_dir.join("logs").join("nested").join("ss.log");
    let lp = log_path.to_string_lossy().to_string();
    assert!(
        !log_path.parent().expect("应有父目录").exists(),
        "前置条件：日志父目录此时应不存在"
    );

    let out = run_subcommand_raw(
        "session-start",
        &[
            "--project-dir",
            &p,
            "--general-db",
            &g,
            "--now",
            "1700000000",
            "--log",
            &lp,
        ],
    );
    assert!(
        out.status.success(),
        "session-start --log 应 exit 0，stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    // 缺省 emit=text：stdout 仍应是热索引（不传 --emit 行为不变）。
    let stdout = String::from_utf8(out.stdout).expect("stdout 非 UTF-8");
    assert!(
        stdout.contains("== Engram 热索引"),
        "默认 emit=text 时 stdout 应仍是热索引，实得：\n{stdout}"
    );

    // 日志文件应存在。
    assert!(
        log_path.is_file(),
        "运行后日志文件应存在：{}",
        log_path.display()
    );
    let content = std::fs::read_to_string(&log_path).expect("应能读日志");
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 1, "应恰好追加一行，实得：\n{content}");
    let name = project_dir
        .file_name()
        .and_then(|s| s.to_str())
        .expect("项目目录应有名字");
    assert!(
        lines[0].contains("session-start"),
        "日志行应含 session-start，实得：{}",
        lines[0]
    );
    assert!(
        lines[0].contains(&format!("project={name}")),
        "日志行应含 project={name}，实得：{}",
        lines[0]
    );
    assert!(
        lines[0].starts_with("1700000000 "),
        "日志行应以给定 unix 秒开头，实得：{}",
        lines[0]
    );
    assert!(
        lines[0].contains("emit=text"),
        "日志行应记录 emit=text，实得：{}",
        lines[0]
    );

    // 再跑一次（同一日志文件）：应**追加**第二行（append 模式）。
    let out2 = run_subcommand_raw(
        "session-start",
        &[
            "--project-dir",
            &p,
            "--general-db",
            &g,
            "--now",
            "1700000001",
            "--log",
            &lp,
            "--emit",
            "json",
        ],
    );
    assert!(out2.status.success(), "第二次运行应 exit 0");
    let content2 = std::fs::read_to_string(&log_path).expect("应能读日志");
    assert_eq!(
        content2.lines().count(),
        2,
        "第二次运行应追加第二行（append），实得：\n{content2}"
    );
    assert!(
        content2
            .lines()
            .nth(1)
            .is_some_and(|l| l.contains("emit=json")),
        "第二行应记录 emit=json，实得：\n{content2}"
    );

    cleanup_file(&general_path);
    let _ = std::fs::remove_dir_all(&project_dir);
}

// 28. 不传 --emit / --log 时行为与之前完全一致：stdout = 前言行 + 热索引，
//     退出 0（回归保护：默认路径不受新选项影响）。
#[test]
fn session_start_default_unchanged() {
    let _guard = test_guard();
    let project_dir = unique_project_dir("ss_default");
    let general_path = unique_db_path("ss_default_g");
    let p = project_dir.to_string_lossy().to_string();
    let g = general_path.to_string_lossy().to_string();

    let out = run_subcommand_raw(
        "session-start",
        &[
            "--project-dir",
            &p,
            "--general-db",
            &g,
            "--now",
            "1000000000",
        ],
    );
    assert!(
        out.status.success(),
        "默认 session-start 应 exit 0，stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout 非 UTF-8");
    // 第一行恰为前言，随后是热索引表头。
    let mut lines = stdout.lines();
    assert_eq!(
        lines.next(),
        Some("「以下是你的 engram 长期记忆热索引。需要细节时按每条的指针去查 ground truth；不要凭印象。」"),
        "首行应为前言，实得：\n{stdout}"
    );
    assert!(
        stdout.contains("== Engram 热索引"),
        "应含热索引表头，实得：\n{stdout}"
    );
    // 不应误把 JSON 包裹打出来。
    assert!(
        !stdout.contains("hookSpecificOutput"),
        "默认路径不应输出 hook JSON，实得：\n{stdout}"
    );

    cleanup_file(&general_path);
    let _ = std::fs::remove_dir_all(&project_dir);
}

// ============================================================================
// hot-index：动态按需挂载子项目 L4 的集成测试
// ============================================================================

/// 在系统临时目录下构造一个进程内唯一的工作区根目录（已创建）。
fn unique_workspace_root(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let pid = std::process::id();
    dir.push(format!("engram_it_ws_{tag}_{pid}_{}", unique_suffix()));
    std::fs::create_dir_all(&dir).expect("应能创建临时工作区根目录");
    dir
}

/// 在 `<base>/.claude/engram.redb` 处建一个 db 并灌入 `mems`，关闭后返回该 db 路径。
fn seed_claude_db(base: &Path, mems: &[Memory]) -> PathBuf {
    let claude = base.join(".claude");
    std::fs::create_dir_all(&claude).expect("应能创建 .claude 目录");
    let db_path = claude.join("engram.redb");
    let db = store::open(&db_path).expect("应能创建数据库");
    store::put_many(&db, mems).expect("put_many 应成功");
    drop(db);
    db_path
}

/// 调用 `engram hot-index ...`，返回完整 Output（调用方自行断言）。
fn run_hot_index_raw(args: &[&str]) -> std::process::Output {
    run_subcommand_raw("hot-index", args)
}

/// 取一个路径的最后一段目录名（测试断言根名用）。
fn last_segment(p: &Path) -> String {
    p.file_name()
        .and_then(|s| s.to_str())
        .expect("路径应有最后一段")
        .to_string()
}

// 29. transcript 判定取最后触碰的子项目（正斜杠分隔符）；挂载集含根 L4 + 活跃子项目 L4。
#[test]
fn hot_index_transcript_picks_last_and_mounts_active_l4() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    let ws = unique_workspace_root("hi_transcript");
    let ws_str = ws.to_string_lossy().replace('\\', "/");

    // 两个真实子项目目录 engram、ai-2d-engine。
    let engram_dir = ws.join("engram");
    let ai_dir = ws.join("ai-2d-engine");
    std::fs::create_dir_all(&engram_dir).expect("建 engram 子目录");
    std::fs::create_dir_all(&ai_dir).expect("建 ai-2d-engine 子目录");

    // 根项目库一条 L4.1（root 名作 project）。根名 = ws 最后一段。
    let root_name = last_segment(&ws);
    let root_l4 = make(
        "root_l4",
        Level::L4_1,
        Some(&root_name),
        Status::Active,
        0.5,
        now,
        vec![now],
    );
    let _root_db = seed_claude_db(&ws, &[root_l4]);

    // engram 子项目库一条 L4.1。
    let eng_l4 = make(
        "eng_l4",
        Level::L4_1,
        Some("engram"),
        Status::Active,
        0.5,
        now,
        vec![now],
    );
    let _eng_db = seed_claude_db(&engram_dir, &[eng_l4]);

    // ai-2d-engine 子项目库一条 L4.1。
    let ai_l4 = make(
        "ai_l4",
        Level::L4_1,
        Some("ai-2d-engine"),
        Status::Active,
        0.5,
        now,
        vec![now],
    );
    let _ai_db = seed_claude_db(&ai_dir, &[ai_l4]);

    // 公共库一条 L1。
    let general_path = seed_db(
        "hi_transcript_g",
        &[make(
            "gen1",
            Level::L1,
            None,
            Status::Active,
            0.5,
            now,
            vec![now],
        )],
    );
    let g = general_path.to_string_lossy().to_string();

    // transcript：先触碰 engram，再触碰 ai-2d-engine → active 应为 ai-2d-engine。
    let transcript = ws.join("session.jsonl");
    let content = format!("{ws_str}/engram/src/x.rs 然后 {ws_str}/ai-2d-engine/y.rs\n");
    std::fs::write(&transcript, content).expect("写 transcript");
    let tp = transcript.to_string_lossy().to_string();

    let out = run_hot_index_raw(&[
        "--general-db",
        &g,
        "--workspace-root",
        ws.to_string_lossy().as_ref(),
        "--transcript",
        &tp,
        "--now",
        "1000000000",
    ]);
    assert!(
        out.status.success(),
        "hot-index 应成功，stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout 非 UTF-8");
    // 应含公共 L1、根 L4、活跃子项目 ai-2d-engine 的 L4；不含 engram 的 L4。
    assert!(stdout.contains("cue-gen1"), "应含公共 L1");
    assert!(stdout.contains("cue-root_l4"), "应含根 L4");
    assert!(
        stdout.contains("cue-ai_l4"),
        "应含活跃子项目 ai-2d-engine 的 L4，实得：\n{stdout}"
    );
    assert!(
        !stdout.contains("cue-eng_l4"),
        "不应含未激活的 engram 子项目 L4，实得：\n{stdout}"
    );

    cleanup_file(&general_path);
    let _ = std::fs::remove_dir_all(&ws);
}

// 30. transcript 判定支持 JSON 转义的双反斜杠分隔符变体。
#[test]
fn hot_index_transcript_handles_double_backslash() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    let ws = unique_workspace_root("hi_dbs");
    let engram_dir = ws.join("engram");
    std::fs::create_dir_all(&engram_dir).expect("建 engram 子目录");

    let eng_l4 = make(
        "eng_dbs",
        Level::L4_1,
        Some("engram"),
        Status::Active,
        0.5,
        now,
        vec![now],
    );
    let _eng_db = seed_claude_db(&engram_dir, &[eng_l4]);
    // 根库可为空（hot-index 会 open 创建）。
    let _root_db = seed_claude_db(&ws, &[]);

    let general_path = seed_db("hi_dbs_g", &[]);
    let g = general_path.to_string_lossy().to_string();

    // transcript 用双反斜杠（JSON 转义形态）写工作区路径。
    let ws_bs = ws.to_string_lossy().replace('\\', "\\\\");
    let transcript = ws.join("session.jsonl");
    let content = format!("{{\"cwd\":\"{ws_bs}\\\\engram\"}}");
    std::fs::write(&transcript, content).expect("写 transcript");
    let tp = transcript.to_string_lossy().to_string();

    let out = run_hot_index_raw(&[
        "--general-db",
        &g,
        "--workspace-root",
        ws.to_string_lossy().as_ref(),
        "--transcript",
        &tp,
        "--now",
        "1000000000",
    ]);
    assert!(
        out.status.success(),
        "hot-index 应成功，stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout 非 UTF-8");
    assert!(
        stdout.contains("cue-eng_dbs"),
        "双反斜杠分隔符也应判定出 engram，实得：\n{stdout}"
    );

    cleanup_file(&general_path);
    let _ = std::fs::remove_dir_all(&ws);
}

// 31. prompt 信号优先于 transcript：prompt 提 engram、transcript 指 ai-2d-engine → 取 engram。
#[test]
fn hot_index_prompt_takes_precedence_over_transcript() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    let ws = unique_workspace_root("hi_prompt");
    let ws_str = ws.to_string_lossy().replace('\\', "/");
    let engram_dir = ws.join("engram");
    let ai_dir = ws.join("ai-2d-engine");
    std::fs::create_dir_all(&engram_dir).expect("建 engram");
    std::fs::create_dir_all(&ai_dir).expect("建 ai-2d-engine");

    let _eng_db = seed_claude_db(
        &engram_dir,
        &[make(
            "eng_p",
            Level::L4_1,
            Some("engram"),
            Status::Active,
            0.5,
            now,
            vec![now],
        )],
    );
    let _ai_db = seed_claude_db(
        &ai_dir,
        &[make(
            "ai_p",
            Level::L4_1,
            Some("ai-2d-engine"),
            Status::Active,
            0.5,
            now,
            vec![now],
        )],
    );
    let _root_db = seed_claude_db(&ws, &[]);
    let general_path = seed_db("hi_prompt_g", &[]);
    let g = general_path.to_string_lossy().to_string();

    // transcript 指向 ai-2d-engine。
    let transcript = ws.join("session.jsonl");
    std::fs::write(&transcript, format!("{ws_str}/ai-2d-engine/y.rs")).expect("写 transcript");
    let tp = transcript.to_string_lossy().to_string();

    let out = run_hot_index_raw(&[
        "--general-db",
        &g,
        "--workspace-root",
        ws.to_string_lossy().as_ref(),
        "--transcript",
        &tp,
        "--prompt",
        "我现在要做 engram 这个项目",
        "--now",
        "1000000000",
    ]);
    assert!(out.status.success(), "hot-index 应成功");
    let stdout = String::from_utf8(out.stdout).expect("stdout 非 UTF-8");
    assert!(
        stdout.contains("cue-eng_p"),
        "prompt 信号应优先选 engram，实得：\n{stdout}"
    );
    assert!(
        !stdout.contains("cue-ai_p"),
        "不应选 transcript 指向的 ai-2d-engine，实得：\n{stdout}"
    );

    cleanup_file(&general_path);
    let _ = std::fs::remove_dir_all(&ws);
}

// 32. active 必须是真实子目录：prompt/transcript 提到的伪造名不算 → 只挂根 L4 + 公共。
#[test]
fn hot_index_active_must_be_real_subdir() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    let ws = unique_workspace_root("hi_fake");
    // 只建一个真实子目录 engram；prompt 故意提一个不存在的 nonexistent。
    let engram_dir = ws.join("engram");
    std::fs::create_dir_all(&engram_dir).expect("建 engram");
    let _eng_db = seed_claude_db(
        &engram_dir,
        &[make(
            "eng_real",
            Level::L4_1,
            Some("engram"),
            Status::Active,
            0.5,
            now,
            vec![now],
        )],
    );
    let root_name = last_segment(&ws);
    let _root_db = seed_claude_db(
        &ws,
        &[make(
            "root_only",
            Level::L4_1,
            Some(&root_name),
            Status::Active,
            0.5,
            now,
            vec![now],
        )],
    );
    let general_path = seed_db("hi_fake_g", &[]);
    let g = general_path.to_string_lossy().to_string();

    let out = run_hot_index_raw(&[
        "--general-db",
        &g,
        "--workspace-root",
        ws.to_string_lossy().as_ref(),
        "--prompt",
        "去 nonexistent 那个伪造项目",
        "--now",
        "1000000000",
    ]);
    assert!(out.status.success(), "hot-index 应成功");
    let stdout = String::from_utf8(out.stdout).expect("stdout 非 UTF-8");
    // 伪造名不算 active → 不挂任何子项目 L4，只剩根 L4。
    assert!(stdout.contains("cue-root_only"), "应含根 L4");
    assert!(
        !stdout.contains("cue-eng_real"),
        "未被激活的 engram 不应出现，实得：\n{stdout}"
    );

    cleanup_file(&general_path);
    let _ = std::fs::remove_dir_all(&ws);
}

// 33. 状态门控：同一 active 连续两次（带 --state）第二次输出空；active 变化则有输出。
#[test]
fn hot_index_state_gates_unchanged_active() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    let ws = unique_workspace_root("hi_state");
    let engram_dir = ws.join("engram");
    let ai_dir = ws.join("ai-2d-engine");
    std::fs::create_dir_all(&engram_dir).expect("建 engram");
    std::fs::create_dir_all(&ai_dir).expect("建 ai-2d-engine");
    let _eng_db = seed_claude_db(
        &engram_dir,
        &[make(
            "s_eng",
            Level::L4_1,
            Some("engram"),
            Status::Active,
            0.5,
            now,
            vec![now],
        )],
    );
    let _ai_db = seed_claude_db(
        &ai_dir,
        &[make(
            "s_ai",
            Level::L4_1,
            Some("ai-2d-engine"),
            Status::Active,
            0.5,
            now,
            vec![now],
        )],
    );
    let _root_db = seed_claude_db(&ws, &[]);
    let general_path = seed_db("hi_state_g", &[]);
    let g = general_path.to_string_lossy().to_string();
    let state_path = ws.join("hot-index-state.txt");
    let sp = state_path.to_string_lossy().to_string();
    let ws_arg = ws.to_string_lossy().to_string();

    // 第一次：active=engram，状态文件无 → 应有输出，并写回状态。
    let out1 = run_hot_index_raw(&[
        "--general-db",
        &g,
        "--workspace-root",
        &ws_arg,
        "--prompt",
        "做 engram",
        "--state",
        &sp,
        "--now",
        "1000000000",
    ]);
    assert!(out1.status.success(), "首次 hot-index 应成功");
    let s1 = String::from_utf8(out1.stdout).expect("stdout 非 UTF-8");
    assert!(s1.contains("cue-s_eng"), "首次应渲染 engram 的 L4");
    assert!(s1.contains("== Engram 热索引"), "首次应有热索引");
    // 状态文件应记 engram。
    let recorded = std::fs::read_to_string(&state_path).expect("应能读状态");
    assert_eq!(recorded.trim(), "engram", "状态应记 engram");

    // 第二次：同样 active=engram，状态门控应输出空。
    let out2 = run_hot_index_raw(&[
        "--general-db",
        &g,
        "--workspace-root",
        &ws_arg,
        "--prompt",
        "继续 engram",
        "--state",
        &sp,
        "--now",
        "1000000000",
    ]);
    assert!(out2.status.success(), "第二次 hot-index 应 exit 0");
    let s2 = String::from_utf8(out2.stdout).expect("stdout 非 UTF-8");
    assert!(
        s2.is_empty(),
        "active 未变时应输出空（不重注入），实得：\n{s2}"
    );

    // 第三次：active 变为 ai-2d-engine → 应重新有输出，并更新状态。
    let out3 = run_hot_index_raw(&[
        "--general-db",
        &g,
        "--workspace-root",
        &ws_arg,
        "--prompt",
        "切到 ai-2d-engine",
        "--state",
        &sp,
        "--now",
        "1000000000",
    ]);
    assert!(out3.status.success(), "第三次 hot-index 应成功");
    let s3 = String::from_utf8(out3.stdout).expect("stdout 非 UTF-8");
    assert!(
        s3.contains("cue-s_ai"),
        "active 变化后应渲染 ai-2d-engine 的 L4，实得：\n{s3}"
    );
    let recorded2 = std::fs::read_to_string(&state_path).expect("应能读状态");
    assert_eq!(
        recorded2.trim(),
        "ai-2d-engine",
        "状态应更新为 ai-2d-engine"
    );

    cleanup_file(&general_path);
    let _ = std::fs::remove_dir_all(&ws);
}

// 34. 无 active 时挂载集 = 公共 + 根 L4（不含任何子项目 L4）。
#[test]
fn hot_index_no_active_mounts_only_root_and_general() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    let ws = unique_workspace_root("hi_noactive");
    let engram_dir = ws.join("engram");
    std::fs::create_dir_all(&engram_dir).expect("建 engram");
    let _eng_db = seed_claude_db(
        &engram_dir,
        &[make(
            "na_eng",
            Level::L4_1,
            Some("engram"),
            Status::Active,
            0.5,
            now,
            vec![now],
        )],
    );
    let root_name = last_segment(&ws);
    let _root_db = seed_claude_db(
        &ws,
        &[make(
            "na_root",
            Level::L4_1,
            Some(&root_name),
            Status::Active,
            0.5,
            now,
            vec![now],
        )],
    );
    let general_path = seed_db(
        "hi_noactive_g",
        &[make(
            "na_gen",
            Level::L1,
            None,
            Status::Active,
            0.5,
            now,
            vec![now],
        )],
    );
    let g = general_path.to_string_lossy().to_string();

    // 不给 prompt / transcript → 无信号 → 无 active。
    let out = run_hot_index_raw(&[
        "--general-db",
        &g,
        "--workspace-root",
        ws.to_string_lossy().as_ref(),
        "--now",
        "1000000000",
    ]);
    assert!(out.status.success(), "hot-index 应成功");
    let stdout = String::from_utf8(out.stdout).expect("stdout 非 UTF-8");
    assert!(stdout.contains("cue-na_gen"), "应含公共 L1");
    assert!(stdout.contains("cue-na_root"), "应含根 L4");
    assert!(
        !stdout.contains("cue-na_eng"),
        "无 active 时不应含任何子项目 L4，实得：\n{stdout}"
    );

    cleanup_file(&general_path);
    let _ = std::fs::remove_dir_all(&ws);
}

// 35. --emit json：可被 serde_json 解析、hookEventName 取自 --hook-event、additionalContext 非空。
#[test]
fn hot_index_emit_json_wraps_context_with_hook_event() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    let ws = unique_workspace_root("hi_json");
    let engram_dir = ws.join("engram");
    std::fs::create_dir_all(&engram_dir).expect("建 engram");
    let _eng_db = seed_claude_db(
        &engram_dir,
        &[make(
            "j_eng",
            Level::L4_1,
            Some("engram"),
            Status::Active,
            0.5,
            now,
            vec![now],
        )],
    );
    let _root_db = seed_claude_db(&ws, &[]);
    let general_path = seed_db("hi_json_g", &[]);
    let g = general_path.to_string_lossy().to_string();

    let out = run_hot_index_raw(&[
        "--general-db",
        &g,
        "--workspace-root",
        ws.to_string_lossy().as_ref(),
        "--prompt",
        "做 engram",
        "--emit",
        "json",
        "--hook-event",
        "UserPromptSubmit",
        "--now",
        "1000000000",
    ]);
    assert!(out.status.success(), "hot-index --emit json 应成功");
    let stdout = String::from_utf8(out.stdout).expect("stdout 非 UTF-8");
    assert_eq!(
        stdout.lines().count(),
        1,
        "--emit json 应只打印一行 JSON，实得：\n{stdout}"
    );
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("hot-index --emit json 应可解析");
    let hook = parsed
        .get("hookSpecificOutput")
        .expect("应含 hookSpecificOutput");
    assert_eq!(
        hook.get("hookEventName").and_then(|v| v.as_str()),
        Some("UserPromptSubmit"),
        "hookEventName 应取自 --hook-event"
    );
    let ctx = hook
        .get("additionalContext")
        .and_then(|v| v.as_str())
        .expect("additionalContext 应为字符串");
    assert!(!ctx.is_empty(), "additionalContext 非空");
    assert!(
        ctx.contains("Engram 热索引"),
        "additionalContext 应含整段渲染文本，实得：\n{ctx}"
    );
    assert!(
        ctx.contains("cue-j_eng"),
        "additionalContext 应含活跃子项目 L4，实得：\n{ctx}"
    );

    cleanup_file(&general_path);
    let _ = std::fs::remove_dir_all(&ws);
}

// 36. --from-hook-stdin：从 stdin 读 cwd / prompt 作兜底；空/非法 stdin 不崩。
#[test]
fn hot_index_from_hook_stdin_fallbacks() {
    let _guard = test_guard();
    use std::io::Write as _;
    use std::process::Stdio;

    let now = 1_000_000_000.0;
    let ws = unique_workspace_root("hi_stdin");
    let engram_dir = ws.join("engram");
    std::fs::create_dir_all(&engram_dir).expect("建 engram");
    let _eng_db = seed_claude_db(
        &engram_dir,
        &[make(
            "stdin_eng",
            Level::L4_1,
            Some("engram"),
            Status::Active,
            0.5,
            now,
            vec![now],
        )],
    );
    let _root_db = seed_claude_db(&ws, &[]);
    let general_path = seed_db("hi_stdin_g", &[]);
    let g = general_path.to_string_lossy().to_string();

    // stdin 给 cwd（工作区根）+ prompt（提 engram），命令行不给 --workspace-root/--prompt。
    let ws_json = ws.to_string_lossy().replace('\\', "\\\\");
    let stdin_payload = format!("{{\"cwd\":\"{ws_json}\",\"prompt\":\"做 engram\"}}");

    let exe = env!("CARGO_BIN_EXE_engram");
    let mut child = Command::new(exe)
        .arg("hot-index")
        .arg("--general-db")
        .arg(&g)
        .arg("--from-hook-stdin")
        .arg("--now")
        .arg("1000000000")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("应能 spawn engram");
    child
        .stdin
        .as_mut()
        .expect("应有 stdin")
        .write_all(stdin_payload.as_bytes())
        .expect("写 stdin 失败");
    let out = child.wait_with_output().expect("等待子进程失败");
    assert!(
        out.status.success(),
        "hot-index --from-hook-stdin 应成功，stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout 非 UTF-8");
    assert!(
        stdout.contains("cue-stdin_eng"),
        "应从 stdin 的 cwd/prompt 兜底判出 engram，实得：\n{stdout}"
    );

    // 空 stdin 也不应 panic（无信号 → 无 active，仍正常输出根+公共）。
    let mut child2 = Command::new(exe)
        .arg("hot-index")
        .arg("--general-db")
        .arg(&g)
        .arg("--workspace-root")
        .arg(ws.to_string_lossy().as_ref())
        .arg("--from-hook-stdin")
        .arg("--now")
        .arg("1000000000")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("应能 spawn engram");
    // 不写任何内容直接关闭 stdin（空 stdin）。
    drop(child2.stdin.take());
    let out2 = child2.wait_with_output().expect("等待子进程失败");
    assert!(
        out2.status.success(),
        "空 stdin 应静默当作无、不崩，stderr={}",
        String::from_utf8_lossy(&out2.stderr)
    );

    cleanup_file(&general_path);
    let _ = std::fs::remove_dir_all(&ws);
}

// 37. --log：追加一行 hot-index 记录，含 event / active / root / ws。
#[test]
fn hot_index_log_appends_record() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    let ws = unique_workspace_root("hi_log");
    let engram_dir = ws.join("engram");
    std::fs::create_dir_all(&engram_dir).expect("建 engram");
    let _eng_db = seed_claude_db(
        &engram_dir,
        &[make(
            "log_eng",
            Level::L4_1,
            Some("engram"),
            Status::Active,
            0.5,
            now,
            vec![now],
        )],
    );
    let _root_db = seed_claude_db(&ws, &[]);
    let general_path = seed_db("hi_log_g", &[]);
    let g = general_path.to_string_lossy().to_string();

    let log_path = ws.join("logs").join("hi.log");
    let lp = log_path.to_string_lossy().to_string();

    let out = run_hot_index_raw(&[
        "--general-db",
        &g,
        "--workspace-root",
        ws.to_string_lossy().as_ref(),
        "--prompt",
        "做 engram",
        "--hook-event",
        "UserPromptSubmit",
        "--log",
        &lp,
        "--now",
        "1700000000",
    ]);
    assert!(out.status.success(), "hot-index --log 应成功");

    assert!(log_path.is_file(), "日志文件应存在：{}", log_path.display());
    let content = std::fs::read_to_string(&log_path).expect("应能读日志");
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 1, "应恰好追加一行，实得：\n{content}");
    assert!(lines[0].contains("hot-index"), "应含 hot-index");
    assert!(
        lines[0].contains("event=UserPromptSubmit"),
        "应记 event，实得：{}",
        lines[0]
    );
    assert!(
        lines[0].contains("active=engram"),
        "应记 active=engram，实得：{}",
        lines[0]
    );
    assert!(lines[0].starts_with("1700000000 "), "应以 now 开头");

    cleanup_file(&general_path);
    let _ = std::fs::remove_dir_all(&ws);
}

// ============================================================================
// status 子命令 + hot-index --status-file 的集成测试
// ============================================================================

/// 调用 `engram status ...`，返回完整 Output（调用方自行断言）。
fn run_status_raw(args: &[&str]) -> std::process::Output {
    run_subcommand_raw("status", args)
}

// 38. status --format oneline：跨公共库 + 两个项目库统计 active 分布，
//     输出一行 `● Engram | L1:.. L2:.. L3:..`，并按名升序追加各项目段；非 active 不计。
#[test]
fn status_oneline_counts_across_dbs() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    // 公共库：L1×2、L3×1（active）+ 一条 cold L1（不计）。
    let gmems = vec![
        make("o_g1", Level::L1, None, Status::Active, 0.5, now, vec![now]),
        make("o_g2", Level::L1, None, Status::Active, 0.5, now, vec![now]),
        make("o_g3", Level::L3, None, Status::Active, 0.5, now, vec![now]),
        make("o_gc", Level::L1, None, Status::Cold, 0.5, now, vec![now]),
    ];
    // alpha 项目库：一条 active L4。
    let pa_mems = vec![make(
        "o_a1",
        Level::L4_1,
        Some("alpha"),
        Status::Active,
        0.5,
        now,
        vec![now],
    )];
    // beta 项目库：两条 active L4（不同子层合并计数）。
    let pb_mems = vec![
        make(
            "o_b1",
            Level::L4_2,
            Some("beta"),
            Status::Active,
            0.5,
            now,
            vec![now],
        ),
        make(
            "o_b2",
            Level::L4_3,
            Some("beta"),
            Status::Active,
            0.5,
            now,
            vec![now],
        ),
    ];
    let general_path = seed_db("status_one_g", &gmems);
    let pa_path = seed_db("status_one_pa", &pa_mems);
    let pb_path = seed_db("status_one_pb", &pb_mems);
    let g = general_path.to_string_lossy().to_string();
    // 给一个不存在的 workspace-root，使根 L4 库不会被加载（避免污染计数）。
    let bogus_ws = unique_db_path("status_one_ws_nonexist");
    let bogus_ws_str = bogus_ws.to_string_lossy().to_string();
    let pa_kv = format!("alpha={}", pa_path.display());
    let pb_kv = format!("beta={}", pb_path.display());

    let out = run_status_raw(&[
        "--general-db",
        &g,
        "--workspace-root",
        &bogus_ws_str,
        "--project-db",
        &pa_kv,
        "--project-db",
        &pb_kv,
        "--format",
        "oneline",
        "--now",
        "1000000000",
    ]);
    assert!(
        out.status.success(),
        "status --format oneline 应成功，stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout 非 UTF-8");
    // 恰好一行（println! 的尾换行不算多余空行）。
    assert_eq!(
        stdout.lines().count(),
        1,
        "oneline 应只一行，实得：\n{stdout}"
    );
    assert_eq!(
        stdout.trim_end(),
        "● Engram | L1:2 L2:0 L3:1 | alpha:1 | beta:2",
        "oneline 串格式/计数/项目升序应正确"
    );

    cleanup_file(&general_path);
    cleanup_file(&pa_path);
    cleanup_file(&pb_path);
}

// 39. status --format full（缺省）：可读多行概况，含 active 总数、通用各层、各项目各 L4 子层、
//     cold/superseded/tombstone 计数。
#[test]
fn status_full_shows_breakdown_fields() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    let gmems = vec![
        make("f_g1", Level::L1, None, Status::Active, 0.5, now, vec![now]),
        make("f_g2", Level::L3, None, Status::Active, 0.5, now, vec![now]),
        make("f_gc", Level::L3, None, Status::Cold, 0.5, now, vec![now]),
        make(
            "f_gs",
            Level::L2,
            None,
            Status::Superseded,
            0.5,
            now,
            vec![now],
        ),
        make(
            "f_gt",
            Level::L3,
            None,
            Status::Tombstone,
            0.5,
            now,
            vec![now],
        ),
    ];
    // engram 项目库：L4.1×1、L4.2×1（active）。
    let p_mems = vec![
        make(
            "f_p1",
            Level::L4_1,
            Some("engram"),
            Status::Active,
            0.5,
            now,
            vec![now],
        ),
        make(
            "f_p2",
            Level::L4_2,
            Some("engram"),
            Status::Active,
            0.5,
            now,
            vec![now],
        ),
    ];
    let general_path = seed_db("status_full_g", &gmems);
    let p_path = seed_db("status_full_p", &p_mems);
    let g = general_path.to_string_lossy().to_string();
    let bogus_ws = unique_db_path("status_full_ws_nonexist");
    let bogus_ws_str = bogus_ws.to_string_lossy().to_string();
    let p_kv = format!("engram={}", p_path.display());

    // 不传 --format → 缺省 full。
    let out = run_status_raw(&[
        "--general-db",
        &g,
        "--workspace-root",
        &bogus_ws_str,
        "--project-db",
        &p_kv,
        "--now",
        "1000000000",
    ]);
    assert!(
        out.status.success(),
        "status（缺省 full）应成功，stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout 非 UTF-8");
    // active 总数 = 通用 2（f_g1,f_g2）+ engram 2（f_p1,f_p2）= 4。
    assert!(
        stdout.contains("active 总数：4"),
        "应含 active 总数 4，实得：\n{stdout}"
    );
    // 通用各层。
    assert!(
        stdout.contains("通用层：L1:1 L2:0 L3:1"),
        "应含通用层分布，实得：\n{stdout}"
    );
    // 项目各 L4 子层。
    assert!(
        stdout.contains("[engram] L4.1:1 L4.2:1 L4.3:0"),
        "应含 engram 各 L4 子层分布，实得：\n{stdout}"
    );
    // 其它状态计数。
    assert!(
        stdout.contains("cold:1 superseded:1 tombstone:1"),
        "应含 cold/superseded/tombstone 计数，实得：\n{stdout}"
    );

    cleanup_file(&general_path);
    cleanup_file(&p_path);
}

// 40. status 把工作区根 L4 库一并计入挂载集（复用 hot-index 那套加载）。
#[test]
fn status_includes_workspace_root_l4() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    let ws = unique_workspace_root("status_ws");
    let root_name = last_segment(&ws);
    // 根 L4 库一条 active L4.1（project = 根名）。
    let _root_db = seed_claude_db(
        &ws,
        &[make(
            "ws_root_l4",
            Level::L4_1,
            Some(&root_name),
            Status::Active,
            0.5,
            now,
            vec![now],
        )],
    );
    let general_path = seed_db(
        "status_ws_g",
        &[make(
            "ws_gen",
            Level::L1,
            None,
            Status::Active,
            0.5,
            now,
            vec![now],
        )],
    );
    let g = general_path.to_string_lossy().to_string();

    let out = run_status_raw(&[
        "--general-db",
        &g,
        "--workspace-root",
        ws.to_string_lossy().as_ref(),
        "--format",
        "oneline",
        "--now",
        "1000000000",
    ]);
    assert!(
        out.status.success(),
        "status 应成功，stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout 非 UTF-8");
    // 通用 L1:1；根 L4 计为 <root_name>:1。
    assert!(
        stdout.trim_end() == format!("● Engram | L1:1 L2:0 L3:0 | {root_name}:1"),
        "应把工作区根 L4 计入挂载集（{root_name}:1），实得：\n{stdout}"
    );

    cleanup_file(&general_path);
    let _ = std::fs::remove_dir_all(&ws);
}

// 41. hot-index --status-file：运行后该文件存在，内容等于 oneline_status（覆盖写）；
//     父目录不存在能自动创建。
#[test]
fn hot_index_status_file_written_equals_oneline() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    let ws = unique_workspace_root("hi_sf");
    let engram_dir = ws.join("engram");
    std::fs::create_dir_all(&engram_dir).expect("建 engram");
    // engram 子项目库一条 L4.1（被 prompt 激活后挂载）。
    let _eng_db = seed_claude_db(
        &engram_dir,
        &[make(
            "sf_eng",
            Level::L4_1,
            Some("engram"),
            Status::Active,
            0.5,
            now,
            vec![now],
        )],
    );
    // 根 L4 库一条 L4.1（root 名作 project）。
    let root_name = last_segment(&ws);
    let _root_db = seed_claude_db(
        &ws,
        &[make(
            "sf_root",
            Level::L4_1,
            Some(&root_name),
            Status::Active,
            0.5,
            now,
            vec![now],
        )],
    );
    // 公共库：L1×1、L3×1。
    let general_path = seed_db(
        "hi_sf_g",
        &[
            make(
                "sf_g1",
                Level::L1,
                None,
                Status::Active,
                0.5,
                now,
                vec![now],
            ),
            make(
                "sf_g2",
                Level::L3,
                None,
                Status::Active,
                0.5,
                now,
                vec![now],
            ),
        ],
    );
    let g = general_path.to_string_lossy().to_string();

    // status 文件放在尚不存在的子目录下，验证父目录会被自动创建。
    let status_file = ws.join("statusbar").join("engram.status");
    let sf = status_file.to_string_lossy().to_string();
    assert!(
        !status_file.parent().expect("应有父目录").exists(),
        "前置条件：状态文件父目录此时应不存在"
    );

    let out = run_hot_index_raw(&[
        "--general-db",
        &g,
        "--workspace-root",
        ws.to_string_lossy().as_ref(),
        "--prompt",
        "做 engram",
        "--status-file",
        &sf,
        "--now",
        "1000000000",
    ]);
    assert!(
        out.status.success(),
        "hot-index --status-file 应成功，stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 状态文件应存在且非空。
    assert!(
        status_file.is_file(),
        "运行后状态文件应存在：{}",
        status_file.display()
    );
    let content = std::fs::read_to_string(&status_file).expect("应能读状态文件");
    // 内容应等于挂载集（公共 L1:1 L3:1 + 根 L4 + 激活的 engram L4）的 oneline_status。
    // 用库函数算出期望值，保证与实现一致（项目按名升序：engram 在 <root_name> 关系视名字而定）。
    use engram::commands::oneline_status;
    let expected_mems = vec![
        make(
            "sf_g1",
            Level::L1,
            None,
            Status::Active,
            0.5,
            now,
            vec![now],
        ),
        make(
            "sf_g2",
            Level::L3,
            None,
            Status::Active,
            0.5,
            now,
            vec![now],
        ),
        make(
            "sf_root",
            Level::L4_1,
            Some(&root_name),
            Status::Active,
            0.5,
            now,
            vec![now],
        ),
        make(
            "sf_eng",
            Level::L4_1,
            Some("engram"),
            Status::Active,
            0.5,
            now,
            vec![now],
        ),
    ];
    let expected = oneline_status(&expected_mems, now);
    assert_eq!(
        content, expected,
        "状态文件内容应等于挂载集的 oneline_status"
    );

    cleanup_file(&general_path);
    let _ = std::fs::remove_dir_all(&ws);
}

// 42. hot-index --status-file 在状态门控判空（active 未变）时仍写状态文件。
#[test]
fn hot_index_status_file_written_even_when_gated_empty() {
    let _guard = test_guard();
    let now = 1_000_000_000.0;
    let ws = unique_workspace_root("hi_sf_gate");
    let engram_dir = ws.join("engram");
    std::fs::create_dir_all(&engram_dir).expect("建 engram");
    let _eng_db = seed_claude_db(
        &engram_dir,
        &[make(
            "g_eng",
            Level::L4_1,
            Some("engram"),
            Status::Active,
            0.5,
            now,
            vec![now],
        )],
    );
    let _root_db = seed_claude_db(&ws, &[]);
    let general_path = seed_db(
        "hi_sf_gate_g",
        &[make(
            "g_gen",
            Level::L1,
            None,
            Status::Active,
            0.5,
            now,
            vec![now],
        )],
    );
    let g = general_path.to_string_lossy().to_string();
    let state_path = ws.join("state.txt");
    let sp = state_path.to_string_lossy().to_string();
    let status_file = ws.join("engram.status");
    let sf = status_file.to_string_lossy().to_string();
    let ws_arg = ws.to_string_lossy().to_string();

    // 第一次：active=engram，状态文件无 → 有输出，写回状态与状态栏文件。
    let out1 = run_hot_index_raw(&[
        "--general-db",
        &g,
        "--workspace-root",
        &ws_arg,
        "--prompt",
        "做 engram",
        "--state",
        &sp,
        "--status-file",
        &sf,
        "--now",
        "1000000000",
    ]);
    assert!(out1.status.success(), "首次 hot-index 应成功");
    assert!(
        !String::from_utf8_lossy(&out1.stdout).is_empty(),
        "首次应有注入输出"
    );
    assert!(status_file.is_file(), "首次应写出状态栏文件");

    // 篡改状态栏文件内容，验证第二次（门控判空）仍会覆盖刷新它。
    std::fs::write(&status_file, "STALE").expect("应能改写状态文件");

    // 第二次：同样 active=engram → 状态门控判空、输出空，但状态栏文件仍应被覆盖刷新。
    let out2 = run_hot_index_raw(&[
        "--general-db",
        &g,
        "--workspace-root",
        &ws_arg,
        "--prompt",
        "继续 engram",
        "--state",
        &sp,
        "--status-file",
        &sf,
        "--now",
        "1000000000",
    ]);
    assert!(out2.status.success(), "第二次 hot-index 应 exit 0");
    let s2 = String::from_utf8(out2.stdout).expect("stdout 非 UTF-8");
    assert!(
        s2.is_empty(),
        "门控判空时应输出空（不重注入），实得：\n{s2}"
    );

    // 关键：即便门控判空、不注入，状态栏文件也应被刷新（不再是 STALE）。
    let content = std::fs::read_to_string(&status_file).expect("应能读状态文件");
    assert_ne!(content, "STALE", "门控判空时状态栏文件也应被覆盖刷新");
    assert!(
        content.starts_with("● Engram | "),
        "刷新后内容应为合法 oneline 串，实得：{content}"
    );

    cleanup_file(&general_path);
    let _ = std::fs::remove_dir_all(&ws);
}
