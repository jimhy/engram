//! 本地网页看板服务（`serve` 子命令的内核）。
//!
//! 起一个只监听本机回环地址的极简 HTTP 服务，把**公共库 + 本机所有项目库**的
//! 全部记忆汇成 JSON 供网页看板浏览。设计约束与整个引擎一致：**只用标准库 +
//! 既有依赖（serde_json / redb），不引入任何网络框架**，保持"单二进制、零外部
//! 依赖"的交付形态。
//!
//! 三条路由：
//! - `GET /`（或 `/index.html`）→ 返回自包含的看板 HTML（[`VIEWER_HTML`]，编译期内嵌）；
//! - `GET /api/memories` → **每次请求都重新发现库并读全量**，返回带派生量的 JSON；
//! - `POST /shutdown`（或 `GET /shutdown`）→ 回一个确认后 `process::exit(0)` 优雅停服。
//!
//! **库发现**（"扫描本机全部项目库"）：engram 没有全局项目注册表，故本模块用
//! 三路并集来发现库并**自我累积**一张注册表：
//! 1. 公共库（`general_db`）；
//! 2. 注册表 `~/.engram/projects.json`（历次发现累积的 name→path，跨 workspace 覆盖）；
//! 3. 扫描传入的 `scan_roots`（项目管理目录）：其自身管理库 + 其**直接子目录**里
//!    带 `.engram/engram.redb` 的项目库；
//! 4. 外加 `--project-db` 显式传入的库。
//!
//! 全部按**规范化路径去重**，读时只对 `is_file` 的库调 `store::open`（绝不凭空建库），
//! 读完即 `drop` 释放跨进程文件锁。
//!
//! **并发**：每个连接开一个线程处理；服务只读、开-读-即关，与会话侧 hook 的写入
//! 靠 redb 的跨进程文件锁 + `store::open` 的重试退避协调。

use std::collections::HashSet;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::activation;
use crate::commands::{ENGRAM_DB_FILE, ENGRAM_DIR, WORKSPACE_MARKER};
use crate::model::{EngramConfig, SECS_PER_DAY};
use crate::store;

/// 编译期内嵌的看板页面（单文件、无外链资源，离线可用）。
const VIEWER_HTML: &str = include_str!("web/viewer.html");

/// 单个连接的读超时，避免半开连接长期占着线程。
const READ_TIMEOUT: Duration = Duration::from_secs(15);

/// 知识库子目录名（锚点 `.engram/` 之下），与 kb crate 的 `scope::KB_DIR` 约定一致。
const KB_DIR_NAME: &str = "kb";
/// 共享知识库数据子目录名，与 kb crate 的 `scope::SHARED_KB_SUBDIR` 约定一致。
const KB_SHARED_SUBDIR: &str = "store";

/// 库的种类，决定看板上的分组标签。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DbKind {
    /// 公共库（用户级共享 L1-L3）。
    General,
    /// 项目管理目录的管理库。
    Workspace,
    /// 具体项目库（L4）。
    Project,
}

impl DbKind {
    /// 稳定字符串表示（用于 JSON 与注册表）。
    fn as_str(self) -> &'static str {
        match self {
            DbKind::General => "general",
            DbKind::Workspace => "workspace",
            DbKind::Project => "project",
        }
    }

    /// 从注册表里的字符串解析回种类；未知值保守当作 `Project`。
    fn from_str(s: &str) -> DbKind {
        match s {
            "general" => DbKind::General,
            "workspace" => DbKind::Workspace,
            _ => DbKind::Project,
        }
    }
}

/// 一个待展示的库：种类 + 展示名 + 文件路径。
#[derive(Debug, Clone)]
struct DbEntry {
    kind: DbKind,
    name: String,
    path: PathBuf,
}

/// `serve` 服务的运行配置（由 `main.rs` 锚定作用域后组装）。
///
/// 本模块**不做**作用域锚定（那是 `main.rs` 的职责），只按此配置发现库、读数据、起服务。
#[derive(Debug, Clone)]
pub struct ServeConfig {
    /// 绑定主机（默认 `127.0.0.1`，只对本机开放）。
    pub host: String,
    /// 绑定端口（默认 8765）。
    pub port: u16,
    /// 公共库路径。
    pub general_db: PathBuf,
    /// 待扫描的项目管理目录：扫其管理库与直接子项目库。
    pub scan_roots: Vec<PathBuf>,
    /// `--project-db name=path` 显式传入的库（name, path）。
    pub extra_project_dbs: Vec<(String, PathBuf)>,
    /// 启动后是否自动用默认浏览器打开看板。
    pub open: bool,
}

/// 取当前 unix 秒（供 effective / promotion_score 的懒计算用）。
fn now_secs() -> Result<f64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .map_err(|e| format!("系统时钟早于 UNIX 纪元：{e}"))
}

/// 生成一次性会话 token（32 个 hex 字符），不引入新依赖。
///
/// 熵源：两个独立的 [`std::collections::hash_map::RandomState`]（其内部键来自
/// OS 熵源，且每个实例互不相同）对「纳秒时间戳 + 进程 id」求哈希后拼接。
/// 看板服务此前对本机任意进程 / DNS rebinding 页面完全裸奔（可读全部记忆、可调
/// /shutdown）；启动时生成一次性 token、URL 携带、所有敏感路由校验，即把访问面
/// 收敛到「看得到启动 URL 的人」。
fn generate_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::BuildHasher;

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    // 两个 RandomState 各自带独立随机键，哈希结果外部不可预测。
    let a = RandomState::new().hash_one((nanos, pid));
    let b = RandomState::new().hash_one((pid, nanos, a));
    format!("{a:016x}{b:016x}")
}

/// 常量时间比较两个 token 是否相等（防时序侧信道逐字节猜 token）。
///
/// 长度不等直接返回 false——token 定长 32 hex，长度信息不构成泄漏。
fn constant_time_token_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    // 逐字节异或累积，无早退：比较耗时与差异位置无关。
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 从 query 串里取 `token=` 参数值（百分号解码）；无则 `None`。
fn token_from_query(query: &str) -> Option<String> {
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == "token" {
            return Some(percent_decode(v));
        }
    }
    None
}

/// 校验一次请求携带的 token（query 的 `token=` 或 `X-Engram-Token` 头）。
///
/// 二者任一与 `expected` **常量时间**相等即通过；都缺失或都不匹配则拒绝。
fn request_authorized(expected: &str, query: &str, header_token: Option<&str>) -> bool {
    if let Some(t) = token_from_query(query) {
        if constant_time_token_eq(expected, &t) {
            return true;
        }
    }
    if let Some(t) = header_token {
        if constant_time_token_eq(expected, t.trim()) {
            return true;
        }
    }
    false
}

/// 去掉 Windows `std::fs::canonicalize` 可能带的 verbatim 前缀 `\\?\`（仅盘符形态）。
fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        if !rest.starts_with(r"UNC\") {
            return PathBuf::from(rest.to_string());
        }
    }
    p
}

/// 规范化路径用作去重键；失败（路径不存在等）时退回原路径。
fn canon_key(p: &Path) -> PathBuf {
    std::fs::canonicalize(p)
        .map(strip_verbatim)
        .unwrap_or_else(|_| p.to_path_buf())
}

/// 取目录末段名；根盘符等取不到时回退 `"root"`。
fn dir_name(dir: &Path) -> String {
    dir.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".to_string())
}

/// 注册表文件路径：与公共库同目录下的 `projects.json`。
fn registry_path(cfg: &ServeConfig) -> Option<PathBuf> {
    cfg.general_db.parent().map(|d| d.join("projects.json"))
}

/// 读注册表（历次累积的 name/kind/path）。文件缺失或解析失败均**静默返回空**，
/// 注册表只是加速发现的缓存，坏了不该让服务失败。
fn read_registry(cfg: &ServeConfig) -> Vec<(DbKind, String, PathBuf)> {
    let Some(path) = registry_path(cfg) else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in arr {
        let name = item.get("name").and_then(Value::as_str).unwrap_or("");
        let kind = item
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("project");
        let p = item.get("path").and_then(Value::as_str).unwrap_or("");
        if !name.is_empty() && !p.is_empty() {
            out.push((DbKind::from_str(kind), name.to_string(), PathBuf::from(p)));
        }
    }
    out
}

/// 把发现到的项目/管理库写回注册表（不含公共库），以便下次从任意目录起服务也能覆盖。
/// 写失败静默忽略（注册表是尽力而为的缓存）。
fn write_registry(cfg: &ServeConfig, entries: &[DbEntry]) {
    let Some(path) = registry_path(cfg) else {
        return;
    };
    let arr: Vec<Value> = entries
        .iter()
        .filter(|e| e.kind != DbKind::General)
        .map(|e| {
            json!({
                "name": e.name,
                "kind": e.kind.as_str(),
                "path": e.path.to_string_lossy(),
            })
        })
        .collect();
    if let Ok(text) = serde_json::to_string_pretty(&Value::Array(arr)) {
        let _ = std::fs::write(&path, text);
    }
}

/// 若 `path` 是文件且规范化路径未见过，则登记一条 [`DbEntry`]。
fn add_unique(
    out: &mut Vec<DbEntry>,
    seen: &mut HashSet<PathBuf>,
    kind: DbKind,
    name: String,
    path: PathBuf,
) {
    if !path.is_file() {
        return;
    }
    if seen.insert(canon_key(&path)) {
        out.push(DbEntry { kind, name, path });
    }
}

/// 三路并集发现全部库：公共库 + 注册表 + 扫描项目管理目录 + 显式 `--project-db`。
/// 按规范化路径去重，公共库恒排第一。
fn discover(cfg: &ServeConfig) -> Vec<DbEntry> {
    let mut out: Vec<DbEntry> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    // 1. 公共库恒在最前。
    add_unique(
        &mut out,
        &mut seen,
        DbKind::General,
        "通用".to_string(),
        cfg.general_db.clone(),
    );

    // 2. 注册表累积项（跨 workspace 覆盖）。
    for (kind, name, path) in read_registry(cfg) {
        add_unique(&mut out, &mut seen, kind, name, path);
    }

    // 3. 扫描各项目管理目录：其自身管理库 + 直接子目录里的项目库。
    for root in &cfg.scan_roots {
        let is_ws = root.join(ENGRAM_DIR).join(WORKSPACE_MARKER).is_file();
        if is_ws {
            add_unique(
                &mut out,
                &mut seen,
                DbKind::Workspace,
                dir_name(root),
                root.join(ENGRAM_DIR).join(ENGRAM_DB_FILE),
            );
        }
        let Ok(rd) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in rd.flatten() {
            let child = entry.path();
            if !child.is_dir() {
                continue;
            }
            let child_ws = child.join(ENGRAM_DIR).join(WORKSPACE_MARKER).is_file();
            let kind = if child_ws {
                DbKind::Workspace
            } else {
                DbKind::Project
            };
            add_unique(
                &mut out,
                &mut seen,
                kind,
                dir_name(&child),
                child.join(ENGRAM_DIR).join(ENGRAM_DB_FILE),
            );
        }
    }

    // 4. 显式 --project-db。
    for (name, path) in &cfg.extra_project_dbs {
        add_unique(
            &mut out,
            &mut seen,
            DbKind::Project,
            name.clone(),
            path.clone(),
        );
    }

    out
}

/// 平台对应的 engram-kb sidecar 资产名（与 `scripts/ensure-kb.sh` 一致）。
fn kb_asset_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "engram-kb-windows-x86_64.exe"
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            "engram-kb-macos-aarch64"
        } else {
            "engram-kb-macos-x86_64"
        }
    } else {
        "engram-kb-linux-x86_64"
    }
}

/// 定位已安装的 engram-kb 二进制：`<HOME>/.engram/bin/<asset>`（与 ensure-kb.sh 同规则）。
///
/// 知识库 sidecar 独立于插件、按需下载，未装（用户从未用过知识库）时返回 `None`，
/// 调用方据此在看板上给出"知识库组件未安装"提示而非报错。
fn kb_binary() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    let p = PathBuf::from(home)
        .join(".engram")
        .join("bin")
        .join(kb_asset_name());
    p.is_file().then_some(p)
}

/// 由记忆库推导其同锚点下的知识库目录：
/// - 公共库 → 用户级共享知识库 `<HOME>/.engram/kb/store/`；
/// - 项目 / 管理库 `<X>/.engram/engram.redb` → `<X>/.engram/kb/`。
///
/// 依据是记忆库文件的父目录恒为锚点的 `.engram/`（公共库为 `<HOME>/.engram/`）。
fn kb_dir_for(entry: &DbEntry) -> Option<PathBuf> {
    let engram_dir = entry.path.parent()?;
    match entry.kind {
        DbKind::General => Some(engram_dir.join(KB_DIR_NAME).join(KB_SHARED_SUBDIR)),
        _ => Some(engram_dir.join(KB_DIR_NAME)),
    }
}

/// 调用 engram-kb 子命令并解析其 `--json` 单行输出。
///
/// 只读命令（list/dump）不加载嵌入模型，stdout 是干净单行 JSON；非 0 退出时取
/// stderr 的中文错误返回。engram-kb 参数不经 shell（[`Command`] 直接 exec），无注入风险。
fn run_kb_json(bin: &Path, args: &[&str]) -> Result<Value, String> {
    let out = Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| format!("调用 engram-kb 失败：{e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(stdout.trim()).map_err(|e| format!("解析 engram-kb 输出失败：{e}"))
}

/// 百分号解码（`%XX` → 字节，`+` → 空格）：解析 `/api/kb/doc` 的 query 参数用。
/// 非法转义原样保留，UTF-8 无效时 lossy 转换（不 panic）。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                }
                _ => {
                    out.push(bytes[i]);
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 单个十六进制字符 → 数值（0–15）；非 hex 返回 `None`。
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 构造 `/api/kb` 响应体：发现各库 → 推导 kb 目录 → 对已建库调 `engram-kb list` 汇总。
///
/// sidecar 未安装时返回 `{kb_bin_found:false, kbs:[]}`，让前端给出安装提示而非报错；
/// 只对存在 `manifest.json` 的目录起 list（避免对空目录凭空建 lancedb），坏库跳过。
fn build_kb_payload(cfg: &ServeConfig) -> Result<String, String> {
    let entries = discover(cfg);
    let Some(bin) = kb_binary() else {
        return Ok(json!({ "kb_bin_found": false, "kbs": [] }).to_string());
    };
    let mut kbs: Vec<Value> = Vec::new();
    for e in &entries {
        let Some(kb_dir) = kb_dir_for(e) else {
            continue;
        };
        if !kb_dir.join("manifest.json").is_file() {
            continue;
        }
        let dir_str = kb_dir.to_string_lossy().into_owned();
        let Ok(v) = run_kb_json(&bin, &["list", "--db", &dir_str, "--json"]) else {
            continue;
        };
        let docs = v.get("docs").cloned().unwrap_or_else(|| json!({}));
        kbs.push(json!({
            "scope": e.name,
            "kind": e.kind.as_str(),
            "db_dir": dir_str,
            "docs": docs,
        }));
    }
    serde_json::to_string(&json!({ "kb_bin_found": true, "kbs": kbs }))
        .map_err(|e| format!("序列化知识库响应失败：{e}"))
}

/// 构造 `/api/kb/doc` 响应体：解析 query（`db` + `doc`）→ 校验 db 属于已发现的 kb
/// 目录 → 调 `engram-kb dump` 透传该文档 chunk 原文。
///
/// db 必须命中 [`discover`] 推导出的某个 kb 目录，杜绝借 query 越权读任意目录。
fn build_kb_doc_payload(cfg: &ServeConfig, query: &str) -> Result<String, String> {
    let mut db: Option<String> = None;
    let mut doc: Option<String> = None;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "db" => db = Some(percent_decode(v)),
            "doc" => doc = Some(percent_decode(v)),
            _ => {}
        }
    }
    let db = db.ok_or_else(|| "缺少 db 参数".to_string())?;
    let doc = doc.ok_or_else(|| "缺少 doc 参数".to_string())?;

    let allowed = discover(cfg)
        .iter()
        .filter_map(kb_dir_for)
        .any(|d| d.to_string_lossy() == db);
    if !allowed {
        return Err("db 不在已发现的知识库范围内".to_string());
    }
    let bin =
        kb_binary().ok_or_else(|| "未找到 engram-kb 二进制（知识库组件未安装）".to_string())?;
    let v = run_kb_json(&bin, &["dump", "--db", &db, "--doc", &doc, "--json"])?;
    serde_json::to_string(&v).map_err(|e| format!("序列化文档响应失败：{e}"))
}

/// 把 `f64` 转成合法 JSON 值：有限值原样，无穷/NaN 用 `null`（看板将 `null` 视作 ∞）。
fn f64_json(v: f64) -> Value {
    if v.is_finite() {
        json!(v)
    } else {
        Value::Null
    }
}

/// 构造 `/api/memories` 的响应体：发现库 → 逐库读全量 → 附派生量 → 汇成 JSON 串。
///
/// 每次请求都重新发现 + 重读，故看板刷新即见最新（含新建项目）。读时逐库
/// 开-读-即 `drop`，尽量缩短持锁窗口。
fn build_payload(cfg: &ServeConfig) -> Result<String, String> {
    let now = now_secs()?;
    let entries = discover(cfg);
    // 顺手把发现结果写回注册表，长期累积覆盖面。
    write_registry(cfg, &entries);
    build_payload_from_entries(&entries, now, &cfg.scan_roots)
}

/// [`build_payload`] 的内核（按给定库列表构造响应），拆出以便单测。
///
/// **单库隔离**：某库 open / read 失败**不再让整个请求 500**——该库以
/// `{error: "..."}` 字段进 `dbs`（count=0、无记忆），其余库照常返回；前端据此
/// 在对应库位显示「该库暂不可读」。此前一个损坏/被占用的项目库会拖垮整个看板。
fn build_payload_from_entries(
    entries: &[DbEntry],
    now: f64,
    scan_roots: &[PathBuf],
) -> Result<String, String> {
    // 升级分的视界采样时刻：与 consolidate 的升级判定同语义（「明天它还够格吗」，
    // 见 EngramConfig::promotion_horizon_days）——看板展示的 promotion_score_horizon
    // 即升级状态机真正采样的那个值，避免「看板显示的分」与「实际升级用的分」两张皮。
    let horizon_now = now + EngramConfig::default().promotion_horizon_days * SECS_PER_DAY;

    let mut db_vals: Vec<Value> = Vec::with_capacity(entries.len());
    let mut mem_vals: Vec<Value> = Vec::new();

    for e in entries {
        // 单库隔离：open/read 失败只标记该库，不中断其余库。
        let mems = match store::open(&e.path).and_then(|db| store::all(&db)) {
            Ok(mems) => mems,
            Err(err) => {
                db_vals.push(json!({
                    "kind": e.kind.as_str(),
                    "name": e.name,
                    "path": e.path.to_string_lossy(),
                    "count": 0,
                    "error": format!("{err}"),
                }));
                continue;
            }
        };

        db_vals.push(json!({
            "kind": e.kind.as_str(),
            "name": e.name,
            "path": e.path.to_string_lossy(),
            "count": mems.len(),
        }));

        for m in &mems {
            let mut v = serde_json::to_value(m)
                .map_err(|err| format!("序列化记忆 {} 失败：{err}", m.id))?;
            if let Value::Object(map) = &mut v {
                map.insert(
                    "effective".to_string(),
                    f64_json(activation::effective(m, now)),
                );
                map.insert(
                    "promotion_score".to_string(),
                    f64_json(activation::promotion_score(m, now)),
                );
                // 与升级判定同语义的视界采样值（批1 引入升级视界后，即时
                // promotion_score 已不再是升级状态机实际用的分）。
                map.insert(
                    "promotion_score_horizon".to_string(),
                    f64_json(activation::promotion_score(m, horizon_now)),
                );
            }
            mem_vals.push(v);
        }
    }

    let payload = json!({
        "generated_at": now,
        "now": now,
        "scan_roots": scan_roots.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>(),
        "dbs": db_vals,
        "memories": mem_vals,
    });
    serde_json::to_string(&payload).map_err(|e| format!("序列化响应失败：{e}"))
}

/// URL 里对外展示的主机名：`0.0.0.0` 显示成 `localhost`，其余原样。
fn display_host(host: &str) -> String {
    if host == "0.0.0.0" {
        "localhost".to_string()
    } else {
        host.to_string()
    }
}

/// 尽力而为地用系统默认浏览器打开 `url`；失败只记 stderr、不影响服务。
fn open_browser(url: &str) {
    let result = if cfg!(target_os = "windows") {
        // 空串是 `start` 的窗口标题占位，避免把带引号的 url 当成标题。
        Command::new("cmd").args(["/C", "start", "", url]).spawn()
    } else if cfg!(target_os = "macos") {
        Command::new("open").arg(url).spawn()
    } else {
        Command::new("xdg-open").arg(url).spawn()
    };
    if let Err(e) = result {
        eprintln!("自动打开浏览器失败（请手动访问 {url}）：{e}");
    }
}

/// 写一段 HTTP 响应（含头 + 体），`Connection: close` 单发即断。
fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// token 缺失/不符时根路径返回的提示页（不泄漏任何记忆数据）。
const MISSING_TOKEN_HTML: &str = "<!doctype html><html lang=\"zh-CN\"><meta charset=\"utf-8\">\
<title>Engram 看板</title><body style=\"font-family:sans-serif;padding:40px\">\
<h2>缺 token，请从启动 URL 打开</h2>\
<p>看板访问需要一次性 token。请回到启动 <code>engram serve</code> 的终端，\
用它打印的完整 URL（含 <code>?token=…</code>）打开本页。</p></body></html>";

/// 处理单个连接：解析请求行 → token 鉴权 → 路由 → 写响应。
///
/// 鉴权：全部路由都要求启动时生成的一次性 token（query `token=` 或
/// `X-Engram-Token` 头，常量时间比较）。根路径缺 token 时回一页中文提示；
/// `/api/*` 与 `/shutdown` 缺 token 时回 401。这挡住「本机任意进程 / DNS
/// rebinding 页面读全部记忆、调 /shutdown」的裸奔面。
///
/// `/shutdown` 会在回完确认后 `process::exit(0)`——这是后台服务进程，直接退出即停服。
fn handle(mut stream: TcpStream, cfg: &ServeConfig, token: &str) -> std::io::Result<()> {
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    let peek = stream.try_clone()?;
    let mut reader = BufReader::new(peek);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(()); // 空连接
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let raw_path = parts.next().unwrap_or("/");
    let path = raw_path.split('?').next().unwrap_or("/");
    let query = raw_path.split_once('?').map(|(_, q)| q).unwrap_or("");

    // 读掉剩余请求头（直到空行），顺带捕获 X-Engram-Token（大小写不敏感）。
    let mut header_token: Option<String> = None;
    loop {
        let mut header = String::new();
        let n = reader.read_line(&mut header)?;
        if n == 0 || header == "\r\n" || header == "\n" {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.trim().eq_ignore_ascii_case("x-engram-token") {
                header_token = Some(value.trim().to_string());
            }
        }
    }

    let authorized = request_authorized(token, query, header_token.as_deref());

    match (method, path) {
        (_, "/") | (_, "/index.html") => {
            if !authorized {
                // 不带（或带错）token：只给提示页，不给看板本体。
                return write_response(
                    &mut stream,
                    "403 Forbidden",
                    "text/html; charset=utf-8",
                    MISSING_TOKEN_HTML.as_bytes(),
                );
            }
            write_response(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                VIEWER_HTML.as_bytes(),
            )
        }
        // /api/* 与 /shutdown：token 校验失败一律 401，不执行任何逻辑。
        ("GET", "/api/memories" | "/api/kb" | "/api/kb/doc") | (_, "/shutdown") if !authorized => {
            write_response(
                &mut stream,
                "401 Unauthorized",
                "text/plain; charset=utf-8",
                "token 校验失败：请从启动 URL 携带 ?token= 访问".as_bytes(),
            )
        }
        ("GET", "/api/memories") => match build_payload(cfg) {
            Ok(json) => write_response(
                &mut stream,
                "200 OK",
                "application/json; charset=utf-8",
                json.as_bytes(),
            ),
            Err(e) => write_response(
                &mut stream,
                "500 Internal Server Error",
                "text/plain; charset=utf-8",
                e.as_bytes(),
            ),
        },
        ("GET", "/api/kb") => match build_kb_payload(cfg) {
            Ok(json) => write_response(
                &mut stream,
                "200 OK",
                "application/json; charset=utf-8",
                json.as_bytes(),
            ),
            Err(e) => write_response(
                &mut stream,
                "500 Internal Server Error",
                "text/plain; charset=utf-8",
                e.as_bytes(),
            ),
        },
        ("GET", "/api/kb/doc") => {
            let query = raw_path.split_once('?').map(|(_, q)| q).unwrap_or("");
            match build_kb_doc_payload(cfg, query) {
                Ok(json) => write_response(
                    &mut stream,
                    "200 OK",
                    "application/json; charset=utf-8",
                    json.as_bytes(),
                ),
                Err(e) => write_response(
                    &mut stream,
                    "500 Internal Server Error",
                    "text/plain; charset=utf-8",
                    e.as_bytes(),
                ),
            }
        }
        (_, "/shutdown") => {
            let _ = write_response(
                &mut stream,
                "200 OK",
                "application/json; charset=utf-8",
                b"{\"ok\":true}",
            );
            let _ = stream.flush();
            std::process::exit(0);
        }
        _ => write_response(
            &mut stream,
            "404 Not Found",
            "text/plain; charset=utf-8",
            b"not found",
        ),
    }
}

/// 启动看板服务并阻塞在监听循环上（`serve` 子命令入口）。
///
/// 绑定成功后打印看板 URL；若 `open` 为真则自动开浏览器。**端口被占**时视作"服务
/// 已在运行"——直接（按需）开浏览器复用并成功返回，让重复触发也幂等无害。
///
/// # Errors
/// 绑定失败（非"端口被占"）时返回中文错误字符串。监听循环正常情况下不返回。
pub fn run(cfg: ServeConfig) -> Result<(), String> {
    let bind_addr = format!("{}:{}", cfg.host, cfg.port);
    // 一次性 token：只在启动 URL 里出现；所有敏感路由都校验它（见 [`handle`]）。
    let token = generate_token();
    let url = format!(
        "http://{}:{}/?token={token}",
        display_host(&cfg.host),
        cfg.port
    );

    let listener = match TcpListener::bind(&bind_addr) {
        Ok(l) => l,
        Err(e) if e.kind() == ErrorKind::AddrInUse => {
            eprintln!("端口 {} 已被占用，视作看板已在运行。", cfg.port);
            eprintln!("（注意：已在运行的实例有自己的 token，请用它启动时打印的 URL 访问。）");
            if cfg.open {
                open_browser(&url);
            }
            println!("{url}");
            return Ok(());
        }
        Err(e) => return Err(format!("绑定 {bind_addr} 失败：{e}")),
    };

    println!("Engram 记忆看板已启动：{url}");
    println!(
        "（在页面上点「停止服务」，或直接结束本进程，即可关闭；URL 含一次性 token，请勿外传）"
    );
    if cfg.open {
        open_browser(&url);
    }

    let cfg = Arc::new(cfg);
    let token = Arc::new(token);
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let c = Arc::clone(&cfg);
                let t = Arc::clone(&token);
                std::thread::spawn(move || {
                    if let Err(e) = handle(s, &c, &t) {
                        eprintln!("处理连接出错：{e}");
                    }
                });
            }
            Err(e) => eprintln!("接受连接失败：{e}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Level, Memory, Pointer, Status, MEMORY_SCHEMA_VERSION};

    // ---- token 鉴权（纯函数层，不起 TCP）----

    #[test]
    fn constant_time_eq_matches_and_rejects() {
        assert!(constant_time_token_eq("abcd1234", "abcd1234"));
        assert!(
            !constant_time_token_eq("abcd1234", "abcd1235"),
            "末位不同应拒绝"
        );
        assert!(
            !constant_time_token_eq("abcd", "abcd1234"),
            "长度不同应拒绝"
        );
        assert!(!constant_time_token_eq("", "x"), "空串对非空应拒绝");
        assert!(constant_time_token_eq("", ""), "两个空串相等");
    }

    #[test]
    fn generate_token_is_32_hex_and_unique() {
        let a = generate_token();
        let b = generate_token();
        assert_eq!(a.len(), 32, "token 应为 32 个 hex 字符");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "应全为 hex：{a}");
        assert_ne!(a, b, "两次生成应不同（RandomState 熵源）");
    }

    #[test]
    fn request_authorized_accepts_query_or_header() {
        let tok = "deadbeefdeadbeefdeadbeefdeadbeef";
        // query 携带。
        assert!(request_authorized(tok, &format!("token={tok}"), None));
        // query 里混其它参数也能取到。
        assert!(request_authorized(
            tok,
            &format!("db=x&token={tok}&doc=y"),
            None
        ));
        // 头携带（含空白修剪）。
        assert!(request_authorized(tok, "", Some(&format!(" {tok} "))));
        // 都缺失 / 都错 → 拒绝。
        assert!(!request_authorized(tok, "", None));
        assert!(!request_authorized(tok, "token=wrong", Some("alsowrong")));
    }

    // ---- 单库隔离（build_payload_from_entries；文件 IO 但不起 TCP）----

    /// 进程内唯一的临时目录。
    fn unique_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut dir = std::env::temp_dir();
        dir.push(format!("engram_serve_{tag}_{}_{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("应能创建临时目录");
        dir
    }

    #[test]
    fn payload_isolates_broken_db_and_keeps_good_one() {
        let dir = unique_dir("isolate");
        // 好库：写入一条记忆。
        let good_path = dir.join("good.redb");
        let db = store::open(&good_path).expect("应能建好库");
        let m = Memory {
            id: "mem_ok".to_string(),
            cue: "好库里的记忆".to_string(),
            pointer: Pointer {
                kind: "none".to_string(),
                reference: None,
                detail: None,
            },
            level: Level::L2,
            project: None,
            importance: 0.5,
            pinned: false,
            access_log: vec![1_000_000_000.0],
            status: Status::Active,
            superseded_by: None,
            created_at: 1_000_000_000.0,
            tags: vec![],
            schema_version: MEMORY_SCHEMA_VERSION,
        };
        store::put(&db, &m).expect("写入应成功");
        drop(db);
        // 坏库：写一段垃圾字节冒充 redb 文件（open 必失败）。
        let bad_path = dir.join("bad.redb");
        std::fs::write(&bad_path, b"this is not a redb database").expect("写坏库应成功");

        let entries = vec![
            DbEntry {
                kind: DbKind::General,
                name: "通用".to_string(),
                path: good_path,
            },
            DbEntry {
                kind: DbKind::Project,
                name: "坏项目".to_string(),
                path: bad_path,
            },
        ];
        let json_str = build_payload_from_entries(&entries, 1_000_000_000.0, &[])
            .expect("坏库不应让整个响应失败");
        let v: Value = serde_json::from_str(&json_str).expect("应为合法 JSON");

        let dbs = v.get("dbs").and_then(Value::as_array).expect("应有 dbs");
        assert_eq!(dbs.len(), 2, "两个库都应出现在 dbs 里");
        assert!(dbs[0].get("error").is_none(), "好库不应带 error");
        let bad_err = dbs[1]
            .get("error")
            .and_then(Value::as_str)
            .expect("坏库应带 error 字段");
        assert!(!bad_err.is_empty(), "error 说明不应为空");

        let mems = v
            .get("memories")
            .and_then(Value::as_array)
            .expect("应有 memories");
        assert_eq!(mems.len(), 1, "好库的记忆应照常返回");
        assert_eq!(mems[0].get("id").and_then(Value::as_str), Some("mem_ok"));
        // 派生量齐全：effective / promotion_score / 与升级判定同语义的视界采样值。
        assert!(mems[0].get("effective").is_some());
        assert!(mems[0].get("promotion_score").is_some());
        assert!(
            mems[0].get("promotion_score_horizon").is_some(),
            "应附带升级视界采样值 promotion_score_horizon"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- percent_decode 边界（纯函数）----

    #[test]
    fn percent_decode_handles_invalid_and_utf8_boundaries() {
        // UTF-8 多字节（中文）逐字节转义应正确还原。
        assert_eq!(percent_decode("%E4%B8%AD%E6%96%87"), "中文");
        // 原生多字节字符与转义混排：原生字节原样透传、转义各自解码。
        assert_eq!(percent_decode("中%E6%96%87"), "中文");
        // '+' 与 %20 都是空格。
        assert_eq!(percent_decode("a+b%20c"), "a b c");
        // 大小写 hex 均可。
        assert_eq!(percent_decode("%41%62"), "Ab");
        // 非法 hex 转义（%zz）原样保留，不 panic、不吞字符。
        assert_eq!(percent_decode("%zz"), "%zz");
        // 截断转义：'%' 后不足两位，原样保留。
        assert_eq!(percent_decode("%4"), "%4");
        assert_eq!(percent_decode("abc%"), "abc%");
        // 解码出无效 UTF-8 字节时 lossy 替换（U+FFFD），绝不 panic。
        assert_eq!(percent_decode("%FF"), "\u{FFFD}");
        // 空串恒等。
        assert_eq!(percent_decode(""), "");
    }

    // ---- discover 库发现（去重 + 扫描；文件 IO 但不起 TCP、不开真库）----

    /// 构造一个只填必要字段的 ServeConfig（host/port/open 与发现逻辑无关）。
    fn cfg_with(
        general_db: PathBuf,
        scan_roots: Vec<PathBuf>,
        extra_project_dbs: Vec<(String, PathBuf)>,
    ) -> ServeConfig {
        ServeConfig {
            host: "127.0.0.1".to_string(),
            port: 0,
            general_db,
            scan_roots,
            extra_project_dbs,
            open: false,
        }
    }

    // discover 应按规范化路径把「注册表 / 显式 --project-db / 不同拼法」的同一文件
    // 去重成一条；不存在的注册表路径（is_file 为假）不得进入结果。
    #[test]
    fn discover_dedups_across_registry_and_explicit_sources() {
        let dir = unique_dir("disc_dedup");
        // 占位“库文件”：discover 只查 is_file、不开库，任意字节即可。
        let general = dir.join("general.redb");
        std::fs::write(&general, b"stub").expect("写 general 占位");
        let proj = dir.join("proj.redb");
        std::fs::write(&proj, b"stub").expect("写 proj 占位");

        // 注册表（与公共库同目录的 projects.json）：登记 proj、一条不存在路径、
        // 一条与公共库重复的路径。
        let registry = json!([
            { "name": "reg-name", "kind": "project", "path": proj.to_string_lossy() },
            { "name": "ghost", "kind": "project",
              "path": dir.join("missing.redb").to_string_lossy() },
            { "name": "dup-general", "kind": "project", "path": general.to_string_lossy() },
        ]);
        std::fs::write(dir.join("projects.json"), registry.to_string()).expect("写注册表");

        let cfg = cfg_with(
            general.clone(),
            vec![],
            vec![
                // 与注册表登记的是同一文件 → 应去重。
                ("extra-name".to_string(), proj.clone()),
                // 同一文件的另一种拼法（多一个 `.` 中间段）→ canon_key 应归一去重。
                ("dotted".to_string(), dir.join(".").join("proj.redb")),
                // 与公共库重复 → 应去重。
                ("general-again".to_string(), general.clone()),
            ],
        );
        let entries = discover(&cfg);
        assert_eq!(
            entries.len(),
            2,
            "同一文件的多来源/多拼法应去重成一条，实得：{entries:?}"
        );
        assert_eq!(entries[0].kind, DbKind::General, "公共库应恒排第一");
        assert_eq!(entries[0].name, "通用");
        assert_eq!(
            entries[1].name, "reg-name",
            "注册表先于显式 --project-db，首见名应保留"
        );
        assert_eq!(entries[1].kind, DbKind::Project);
        assert!(
            entries
                .iter()
                .all(|e| e.path.file_name().is_some_and(|f| f != "missing.redb")),
            "不存在的注册表路径不得进入结果"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // discover 对 scan_roots 的扫描：管理目录自身的管理库 + 直接子目录里的项目库；
    // 无 .engram 的子目录与根下普通文件应被忽略。
    #[test]
    fn discover_scans_workspace_root_and_direct_children() {
        let dir = unique_dir("disc_scan");
        // 公共库放独立子目录，其旁不存在 projects.json（注册表为空）。
        let home = dir.join("home");
        std::fs::create_dir_all(&home).expect("建 home");
        let general = home.join("general.redb");
        std::fs::write(&general, b"stub").expect("写 general 占位");

        // 管理目录 ws：带 workspace 标记 + 自身管理库。
        let root = dir.join("ws");
        let root_engram = root.join(ENGRAM_DIR);
        std::fs::create_dir_all(&root_engram).expect("建 ws/.engram");
        std::fs::write(root_engram.join(WORKSPACE_MARKER), b"").expect("写 workspace 标记");
        std::fs::write(root_engram.join(ENGRAM_DB_FILE), b"stub").expect("写管理库占位");

        // 直接子项目 projA：有 .engram/engram.redb → 应被发现为 Project。
        let proj_a_engram = root.join("projA").join(ENGRAM_DIR);
        std::fs::create_dir_all(&proj_a_engram).expect("建 projA/.engram");
        std::fs::write(proj_a_engram.join(ENGRAM_DB_FILE), b"stub").expect("写 projA 库占位");

        // 干扰项：无 .engram 的子目录与根下普通文件都应被忽略。
        std::fs::create_dir_all(root.join("plain")).expect("建 plain");
        std::fs::write(root.join("note.txt"), b"x").expect("写 note");

        let cfg = cfg_with(general.clone(), vec![root.clone()], vec![]);
        let entries = discover(&cfg);
        assert_eq!(
            entries.len(),
            3,
            "应发现 公共库 + 管理库 + projA 恰三条，实得：{entries:?}"
        );
        assert_eq!(entries[0].kind, DbKind::General, "公共库应恒排第一");
        assert!(
            entries
                .iter()
                .any(|e| e.kind == DbKind::Workspace && e.name == "ws"),
            "管理目录自身的管理库应以 Workspace 身份被发现"
        );
        assert!(
            entries
                .iter()
                .any(|e| e.kind == DbKind::Project && e.name == "projA"),
            "直接子目录的项目库应以 Project 身份被发现"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- /api/kb/doc 白名单等值校验（安全边界；不起 TCP）----

    // db 参数必须与 discover 推导出的某个 kb 目录**逐字节等值**才放行：
    // 父目录、尾分隔符、大小写变体、路径穿越统统拒绝——杜绝借 query 越权读任意目录。
    #[test]
    fn kb_doc_whitelist_requires_exact_db_match() {
        const WHITELIST_ERR: &str = "db 不在已发现的知识库范围内";
        let dir = unique_dir("kb_doc_wl");
        let general = dir.join("general.redb");
        std::fs::write(&general, b"stub").expect("写 general 占位");
        let cfg = cfg_with(general.clone(), vec![], vec![]);

        // 缺参数：各给出明确错误（db 先于 doc 校验）。
        assert_eq!(
            build_kb_doc_payload(&cfg, "doc=x").expect_err("缺 db 应拒绝"),
            "缺少 db 参数"
        );
        assert_eq!(
            build_kb_doc_payload(&cfg, "db=whatever").expect_err("缺 doc 应拒绝"),
            "缺少 doc 参数"
        );

        // 白名单里唯一的放行值：公共库锚点推导出的共享 kb 目录串。
        let kb_dir = kb_dir_for(&DbEntry {
            kind: DbKind::General,
            name: "通用".to_string(),
            path: general.clone(),
        })
        .expect("应能推导公共库的 kb 目录");
        let kb_str = kb_dir.to_string_lossy().into_owned();
        let sep = std::path::MAIN_SEPARATOR;

        // 大小写变体：末段 store → STORE（Windows 文件系统视作同目录，但校验是
        // 字节等值，必须拒绝）。构造自 kb_dir 的父目录，确保仅大小写不同。
        let case_variant = kb_dir
            .parent()
            .expect("kb 目录应有父目录")
            .join("STORE")
            .to_string_lossy()
            .into_owned();
        let attempts = [
            // 锚点目录本身（白名单值的祖先）。
            dir.to_string_lossy().into_owned(),
            // 合法值补一个尾分隔符。
            format!("{kb_str}{sep}"),
            // 大小写变体。
            case_variant,
            // 路径穿越。
            format!("{kb_str}{sep}..{sep}..{sep}evil"),
        ];
        for attempt in &attempts {
            assert_eq!(
                build_kb_doc_payload(&cfg, &format!("db={attempt}&doc=x"))
                    .expect_err("越权 db 应被拒绝"),
                WHITELIST_ERR,
                "非等值 db（{attempt}）必须被白名单拦下"
            );
        }

        // 等值命中：必须通过白名单闸门——其后的失败只会是「sidecar 未装」或
        // engram-kb 对不存在 kb 目录的调用失败，决不能再是白名单拒绝
        // （若机器装了 sidecar 且 dump 意外成功返回 Ok，同样证明已通过白名单）。
        if let Err(e) = build_kb_doc_payload(&cfg, &format!("db={kb_str}&doc=x")) {
            assert_ne!(e, WHITELIST_ERR, "等值 db 不应被白名单拦下");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
