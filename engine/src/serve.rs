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
        let kind = item.get("kind").and_then(Value::as_str).unwrap_or("project");
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
            b'%' if i + 2 < bytes.len() => {
                match (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push(h * 16 + l);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
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
    let bin = kb_binary().ok_or_else(|| "未找到 engram-kb 二进制（知识库组件未安装）".to_string())?;
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

    let mut db_vals: Vec<Value> = Vec::with_capacity(entries.len());
    let mut mem_vals: Vec<Value> = Vec::new();

    for e in &entries {
        let db = store::open(&e.path)
            .map_err(|err| format!("打开库 {} 失败：{err}", e.path.display()))?;
        let mems = store::all(&db)
            .map_err(|err| format!("读取库 {} 失败：{err}", e.path.display()))?;
        drop(db);

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
                map.insert("effective".to_string(), f64_json(activation::effective(m, now)));
                map.insert(
                    "promotion_score".to_string(),
                    f64_json(activation::promotion_score(m, now)),
                );
            }
            mem_vals.push(v);
        }
    }

    let payload = json!({
        "generated_at": now,
        "now": now,
        "scan_roots": cfg.scan_roots.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>(),
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

/// 处理单个连接：解析请求行 → 路由 → 写响应。
///
/// `/shutdown` 会在回完确认后 `process::exit(0)`——这是后台服务进程，直接退出即停服。
fn handle(mut stream: TcpStream, cfg: &ServeConfig) -> std::io::Result<()> {
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

    // 读掉剩余请求头（直到空行），不然某些客户端不认响应。
    loop {
        let mut header = String::new();
        let n = reader.read_line(&mut header)?;
        if n == 0 || header == "\r\n" || header == "\n" {
            break;
        }
    }

    match (method, path) {
        (_, "/") | (_, "/index.html") => {
            write_response(&mut stream, "200 OK", "text/html; charset=utf-8", VIEWER_HTML.as_bytes())
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
    let url = format!("http://{}:{}/", display_host(&cfg.host), cfg.port);

    let listener = match TcpListener::bind(&bind_addr) {
        Ok(l) => l,
        Err(e) if e.kind() == ErrorKind::AddrInUse => {
            eprintln!("端口 {} 已被占用，视作看板已在运行。", cfg.port);
            if cfg.open {
                open_browser(&url);
            }
            println!("{url}");
            return Ok(());
        }
        Err(e) => return Err(format!("绑定 {bind_addr} 失败：{e}")),
    };

    println!("Engram 记忆看板已启动：{url}");
    println!("（在页面上点「停止服务」，或直接结束本进程，即可关闭）");
    if cfg.open {
        open_browser(&url);
    }

    let cfg = Arc::new(cfg);
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let c = Arc::clone(&cfg);
                std::thread::spawn(move || {
                    if let Err(e) = handle(s, &c) {
                        eprintln!("处理连接出错：{e}");
                    }
                });
            }
            Err(e) => eprintln!("接受连接失败：{e}"),
        }
    }
    Ok(())
}
