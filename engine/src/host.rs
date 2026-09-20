//! 宿主接管协议（`ENGRAM_HOST_*`）：让 engram 在**被宿主客户端拉起的子进程**里让路。
//!
//! 背景：engram 原本一个大模型 CLI 配一份适配器（claude / codex / kimi / opencode），
//! 每接一家都要重写 hook 脚本。把 engram 嵌进 iTools 的 AIChat 这类「一个客户端里跑
//! 多条引擎路」的宿主后，四条引擎路应当**共用一个记忆入口**：热索引注入与会话末复盘
//! 由宿主统一做，各 CLI 自带的 engram 插件在这些子进程里让路。
//!
//! # 硬约束
//! 让路**只认环境变量**，绝不读写任何全局配置 / hooks.json / 清单文件。宿主起子进程时
//! 注入这几个变量，Windows Terminal 里直跑的 claude / codex 天然没有它们，行为一个字
//! 都不变——这是「宿主客户端与直跑 CLI 必须能同时使用」的落点。
//!
//! # 环境变量
//! | 变量 | 含义 |
//! |---|---|
//! | `ENGRAM_HOST` | 让路总开关（非空即生效，如 `aichat`）。hook 类子命令静默 no-op。 |
//! | `ENGRAM_HOST_DIR` | 文件桥目录（`<沙盒>/engram/rpc/<会话id>`）。数据类子命令转发到此。 |
//! | `ENGRAM_HOST_SESSION` | 宿主会话 id，记账用，原样透传进请求行的 `session` 字段。 |
//! | `ENGRAM_HOST_BYPASS` | 宿主自己调引擎干活时置 `1`：**压过上面全部**，一切照直连库执行（防自噬）。 |
//! | `ENGRAM_HOST_TIMEOUT_MS` | 转发等待回执的超时，缺省 [`DEFAULT_TIMEOUT_MS`]，上限 [`MAX_TIMEOUT_MS`]。 |
//! | `ENGRAM_HOST_DEBUG` | 置位时把「为何退回直连」打到 stderr；缺省全程静默。 |
//!
//! # 两类子命令
//! - **hook 类**（[`HOOK_COMMANDS`]）：`ENGRAM_HOST` 非空 → 静默 no-op、退出 0，
//!   不输出、不写 state/status/pending/watermark。热索引与复盘改由宿主做。
//! - **数据类**（[`FORWARD_COMMANDS`]）：`ENGRAM_HOST_DIR` 非空 → 走文件桥转发，
//!   由宿主代为执行并回执。
//!
//! # 文件桥
//! 用文件而非 HTTP：宿主是插件页，监听不了端口。与宿主既有的 MCP 桥同构。
//! - 请求：往 `<ENGRAM_HOST_DIR>/req.jsonl` **追加**一行 JSON（见 [`build_request`]）；
//! - 回执：轮询 `<ENGRAM_HOST_DIR>/resp.jsonl`，找 `id` 匹配的行，
//!   取 `stdout` / `stderr` / `exit_code`。
//!
//! # 死规矩：桥不通必须退回直连
//! 目录不存在、写失败、超时、回执畸形——任一发生都**立刻退回直连库正常执行**。
//! 绝不能因为宿主没开或桥卡住，就让记忆命令失败或挂死。这条比「接管」本身优先。
//!
//! 由此带来一个副作用：`write` 这类**有副作用**的命令若「转发超时 → 退回直连执行」，
//! 而宿主稍后才把那条陈旧请求捞起来执行，就会写重。请求行因此带上 `ts` 与 `timeout_ms`
//! 两个字段——**宿主必须跳过 `now > ts + timeout_ms` 的过期请求**（客户端此时已经放弃并
//! 自行直连执行过了）。

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 让路总开关环境变量名。
pub const ENV_HOST: &str = "ENGRAM_HOST";
/// 宿主会话 id 环境变量名（记账用，原样透传）。
pub const ENV_HOST_SESSION: &str = "ENGRAM_HOST_SESSION";
/// 文件桥目录环境变量名。
pub const ENV_HOST_DIR: &str = "ENGRAM_HOST_DIR";
/// 防自噬旁路环境变量名（宿主自己调引擎时置位）。
pub const ENV_HOST_BYPASS: &str = "ENGRAM_HOST_BYPASS";
/// 转发超时覆盖环境变量名（毫秒）。
pub const ENV_HOST_TIMEOUT_MS: &str = "ENGRAM_HOST_TIMEOUT_MS";
/// 转发诊断开关环境变量名（置位时把退回直连的原因打到 stderr）。
pub const ENV_HOST_DEBUG: &str = "ENGRAM_HOST_DEBUG";

/// 文件桥的请求文件名（追加写）。
pub const REQ_FILE: &str = "req.jsonl";
/// 文件桥的回执文件名（轮询读）。
pub const RESP_FILE: &str = "resp.jsonl";

/// 协议版本号，写进每行请求的 `protocol` 字段，供宿主分辨客户端能力。
pub const PROTOCOL_VERSION: u32 = 1;

/// 等待回执的缺省超时（毫秒）。
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;
/// 等待回执的超时上限（毫秒）：再怎么配也不许让记忆命令长时间挂住。
pub const MAX_TIMEOUT_MS: u64 = 60_000;
/// 轮询回执的间隔（毫秒）。
pub const POLL_INTERVAL_MS: u64 = 20;

/// hook 类子命令：宿主接管时静默 no-op。
///
/// `session-start` 是 `hot-index` 的历史同类（同为会话开始注入），一并纳入，
/// 免得老适配器绕过让路又注一遍。
pub const HOOK_COMMANDS: &[&str] = &[
    "hot-index",
    // prompt-recall 与 hot-index 同属 UserPromptSubmit 路径：宿主接管时同样让路，
    // 由宿主统一决定注入什么。漏登记的后果是**双份注入**——宿主注一份、
    // 子进程再注一份，而且两份可能来自不同版本的引擎。
    "prompt-recall",
    "catchup-scan",
    "review-prepare",
    "session-start",
];

/// 数据类子命令：宿主接管时转发给宿主执行。
///
/// 刻意**不含** `resolve`——宿主自己要靠它问库路径，转发就成了自噬；也不含
/// `forget` / `gc` / `merge` / `supersede` 等不可逆操作与 `serve` / `doctor`
/// 等本机运维命令，它们一律直连库。
pub const FORWARD_COMMANDS: &[&str] = &[
    "recall",
    "write",
    "confirm-use",
    "list",
    "status",
    "render",
];

/// 本次调用该走哪条路。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostAction {
    /// 照常直连库执行（没有宿主、或被 `ENGRAM_HOST_BYPASS` 压过、或该子命令不在两张表里）。
    Direct,
    /// hook 类让路：什么都不做、不输出、退出 0。
    Noop,
    /// 数据类转发：往 `dir` 的文件桥投递请求，最多等 `timeout_ms`。
    Forward {
        /// 文件桥目录（`ENGRAM_HOST_DIR`）。
        dir: PathBuf,
        /// 等待回执的超时（毫秒）。
        timeout_ms: u64,
        /// 宿主会话 id（`ENGRAM_HOST_SESSION`），原样透传。
        session: Option<String>,
    },
}

/// 宿主回执。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostResponse {
    /// 原样打印到 stdout 的内容。
    pub stdout: String,
    /// 原样打印到 stderr 的内容。
    pub stderr: String,
    /// 进程退出码。
    pub exit_code: i64,
}

/// 转发失败的原因；**任一种都意味着调用方应退回直连库执行**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardError {
    /// `ENGRAM_HOST_DIR` 指向的目录不存在（宿主没开 / 会话已收摊）。
    NoDir,
    /// 写请求或读回执时的 IO 失败。
    Io(String),
    /// 等到超时也没等来 id 匹配的回执（没人消费 `req.jsonl`）。
    Timeout,
}

impl std::fmt::Display for ForwardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDir => write!(f, "文件桥目录不存在"),
            Self::Io(e) => write!(f, "文件桥 IO 失败：{e}"),
            Self::Timeout => write!(f, "等待宿主回执超时"),
        }
    }
}

/// 判定环境变量是否「置位」：非空，且不是 `0` / `false`（大小写不敏感）。
///
/// 协议里说的「非空即生效」在这里放宽了一点：显式写 `ENGRAM_HOST=0` 的人意思
/// 显然是关掉，不该被当成开启。
fn flag_on(raw: Option<String>) -> bool {
    match raw {
        Some(v) => {
            let t = v.trim();
            !t.is_empty() && !t.eq_ignore_ascii_case("0") && !t.eq_ignore_ascii_case("false")
        }
        None => false,
    }
}

/// 取环境变量的非空 trim 值；空串与全空白视作未设置。
fn non_empty(raw: Option<String>) -> Option<String> {
    let v = raw?;
    let t = v.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// 从 `ENGRAM_HOST_TIMEOUT_MS` 解析超时；无效值回落 [`DEFAULT_TIMEOUT_MS`]，并封顶 [`MAX_TIMEOUT_MS`]。
fn timeout_ms<F>(env: &F) -> u64
where
    F: Fn(&str) -> Option<String>,
{
    let parsed = non_empty(env(ENV_HOST_TIMEOUT_MS))
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    parsed.min(MAX_TIMEOUT_MS)
}

/// 从原始命令行参数（**已去掉 argv\[0\]**）里取子命令名。
///
/// 只认第一个参数：它以 `-` 开头（`--help` / `--version`）就没有子命令，
/// 返回 `None` 让 clap 照常处理。刻意不做完整解析——让路判定必须发生在
/// `Cli::parse()` **之前**，这样 `engram recall --query x`（缺 `--general-db`）
/// 也能整条转发给宿主，由宿主补全库路径后执行。
#[must_use]
pub fn subcommand_of(args: &[String]) -> Option<&str> {
    let first = args.first()?;
    if first.starts_with('-') {
        return None;
    }
    Some(first.as_str())
}

/// 按子命令与环境变量判定走哪条路。`env` 为环境变量查找闭包（生产传
/// `|k| std::env::var(k).ok()`，测试传内存映射）。
///
/// 判定顺序：
/// 1. `ENGRAM_HOST_BYPASS` 置位 → [`HostAction::Direct`]（**压过一切**，防自噬）；
/// 2. hook 类子命令 + `ENGRAM_HOST` 非空 → [`HostAction::Noop`]；
/// 3. 数据类子命令 + `ENGRAM_HOST_DIR` 非空 → [`HostAction::Forward`]；
/// 4. 其余 → [`HostAction::Direct`]。
#[must_use]
pub fn decide<F>(subcommand: Option<&str>, env: F) -> HostAction
where
    F: Fn(&str) -> Option<String>,
{
    // 防自噬：宿主收到转发后要真去调这个二进制干活，它调用时置本位。
    // 这一位同时关掉 no-op 与转发——否则宿主自己跑 hot-index / review-prepare
    // 也会被让路挡掉，接管反而落空。
    if flag_on(env(ENV_HOST_BYPASS)) {
        return HostAction::Direct;
    }
    let Some(sub) = subcommand else {
        return HostAction::Direct;
    };
    if HOOK_COMMANDS.contains(&sub) && flag_on(env(ENV_HOST)) {
        return HostAction::Noop;
    }
    if FORWARD_COMMANDS.contains(&sub) {
        if let Some(dir) = non_empty(env(ENV_HOST_DIR)) {
            return HostAction::Forward {
                dir: PathBuf::from(dir),
                timeout_ms: timeout_ms(&env),
                session: non_empty(env(ENV_HOST_SESSION)),
            };
        }
    }
    HostAction::Direct
}

/// 进程内自增序号，与纳秒时间戳、pid 一起拼出全局唯一的请求 id。
static REQ_SEQ: AtomicU64 = AtomicU64::new(0);

/// 当前 unix 毫秒。
fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// 生成一个不会撞的请求 id：`eg-<纳秒>-<pid>-<自增序号>`。
///
/// 同一台机器上可能有多个引擎子进程同时往一个桥投递（多引擎路并发），
/// 纳秒 + pid + 进程内自增三者叠加即可保证唯一。
fn new_request_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let seq = REQ_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("eg-{nanos}-{pid}-{seq}")
}

/// 拼一行请求 JSON。
///
/// 必备字段 `id` / `cmd` / `args` 是协议基线；其余为**附加**字段，宿主不认识
/// 可以直接忽略：
/// - `cwd`：客户端的工作目录——转发的命令行常常没带 `--general-db`（模型直接敲
///   `engram recall --query x`），宿主要靠它锚定作用域；
/// - `session`：`ENGRAM_HOST_SESSION` 原样透传，记账用；
/// - `ts` / `timeout_ms`：客户端的放弃时刻。**超过即视为已被放弃，宿主必须跳过**，
///   否则「超时退回直连」与宿主的迟到执行会把 `write` 做成两遍；
/// - `protocol`：[`PROTOCOL_VERSION`]。
#[must_use]
pub fn build_request(
    id: &str,
    cmd: &str,
    args: &[String],
    cwd: Option<&str>,
    session: Option<&str>,
    ts: u64,
    timeout_ms: u64,
) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "cmd": cmd,
        "args": args,
        "cwd": cwd,
        "session": session,
        "ts": ts,
        "timeout_ms": timeout_ms,
        "protocol": PROTOCOL_VERSION,
    })
}

/// 从一行回执 JSON 里抽出 [`HostResponse`]；`id` 不匹配或不是对象则返回 `None`。
///
/// 容错取向：`stdout` / `stderr` 缺失当空串，`exit_code` 缺失当 0，
/// 并额外认 `exitCode` 这个驼峰别名——宿主是 TypeScript 写的，两种写法都可能出现。
fn parse_response_line(line: &[u8], id: &str) -> Option<HostResponse> {
    let v: serde_json::Value = serde_json::from_slice(line).ok()?;
    if v.get("id")?.as_str()? != id {
        return None;
    }
    let text = |k: &str| {
        v.get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let exit_code = v
        .get("exit_code")
        .or_else(|| v.get("exitCode"))
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    Some(HostResponse {
        stdout: text("stdout"),
        stderr: text("stderr"),
        exit_code,
    })
}

/// 增量扫一遍回执文件的新内容，找 id 匹配的行。
///
/// `pos` 是已读到的字节偏移、`carry` 是上次读到的**半行**残留——回执是被别的进程
/// 追加写的，任何一次读都可能截在半行上，必须留着与下次读到的内容拼起来再解析。
fn scan_response(
    path: &Path,
    id: &str,
    pos: &mut u64,
    carry: &mut Vec<u8>,
) -> Result<Option<HostResponse>, ForwardError> {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        // 宿主还没建回执文件：不是错，接着等。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ForwardError::Io(e.to_string())),
    };
    f.seek(SeekFrom::Start(*pos))
        .map_err(|e| ForwardError::Io(e.to_string()))?;
    let mut fresh = Vec::new();
    let n = f
        .read_to_end(&mut fresh)
        .map_err(|e| ForwardError::Io(e.to_string()))?;
    if n == 0 {
        return Ok(None);
    }
    *pos += n as u64;
    carry.extend_from_slice(&fresh);

    let mut start = 0usize;
    let mut hit = None;
    while let Some(off) = carry[start..].iter().position(|b| *b == b'\n') {
        let end = start + off;
        let line = &carry[start..end];
        if hit.is_none() {
            if let Some(r) = parse_response_line(line, id) {
                hit = Some(r);
            }
        }
        start = end + 1;
    }
    // 只保留最后那截没换行的半行，已消费的整行丢掉。
    carry.drain(..start);
    Ok(hit)
}

/// 把一行请求追加进 `req.jsonl`。
///
/// 整行（含结尾换行）一次 `write_all` 写出，尽量让并发追加不互相插花。
fn append_request(path: &Path, line: &str) -> Result<(), ForwardError> {
    let mut buf = Vec::with_capacity(line.len() + 1);
    buf.extend_from_slice(line.as_bytes());
    buf.push(b'\n');
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| ForwardError::Io(e.to_string()))?;
    f.write_all(&buf)
        .map_err(|e| ForwardError::Io(e.to_string()))?;
    f.flush().map_err(|e| ForwardError::Io(e.to_string()))
}

/// 走文件桥把一次调用转发给宿主，阻塞等回执。
///
/// # Errors
/// 目录不存在（[`ForwardError::NoDir`]）、IO 失败（[`ForwardError::Io`]）、
/// 等不到回执（[`ForwardError::Timeout`]）。**三者调用方都应退回直连库执行**，
/// 绝不可把错误抛给用户。
pub fn forward(
    dir: &Path,
    cmd: &str,
    args: &[String],
    session: Option<&str>,
    timeout_ms: u64,
) -> Result<HostResponse, ForwardError> {
    // 目录不存在就别建：建了请求也没人消费，白等一个超时。直接退回直连。
    if !dir.is_dir() {
        return Err(ForwardError::NoDir);
    }
    let id = new_request_id();
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string());
    let req = build_request(
        &id,
        cmd,
        args,
        cwd.as_deref(),
        session,
        unix_millis(),
        timeout_ms,
    );
    let line = serde_json::to_string(&req).map_err(|e| ForwardError::Io(e.to_string()))?;
    append_request(&dir.join(REQ_FILE), &line)?;

    let resp_path = dir.join(RESP_FILE);
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut pos = 0u64;
    let mut carry = Vec::new();
    loop {
        // 先扫一遍再判超时：即便 timeout 配得极小，也至少给一次机会。
        if let Some(r) = scan_response(&resp_path, &id, &mut pos, &mut carry)? {
            return Ok(r);
        }
        if Instant::now() >= deadline {
            return Err(ForwardError::Timeout);
        }
        std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
    }
}

/// 是否打开了转发诊断（`ENGRAM_HOST_DEBUG`）。
#[must_use]
pub fn debug_on<F>(env: F) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    flag_on(env(ENV_HOST_DEBUG))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// 造一个内存环境变量查找闭包。
    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn 无宿主环境变量时一律直连() {
        let env = env_of(&[]);
        for sub in HOOK_COMMANDS.iter().chain(FORWARD_COMMANDS.iter()) {
            assert_eq!(decide(Some(sub), &env), HostAction::Direct, "子命令 {sub}");
        }
    }

    #[test]
    fn hook类命令在host置位时让路() {
        let env = env_of(&[(ENV_HOST, "aichat")]);
        for sub in HOOK_COMMANDS {
            assert_eq!(decide(Some(sub), &env), HostAction::Noop, "子命令 {sub}");
        }
        // 数据类不受 ENGRAM_HOST 单独影响（转发要看 ENGRAM_HOST_DIR）。
        for sub in FORWARD_COMMANDS {
            assert_eq!(decide(Some(sub), &env), HostAction::Direct, "子命令 {sub}");
        }
    }

    #[test]
    fn 数据类命令在hostdir置位时转发() {
        let env = env_of(&[
            (ENV_HOST, "aichat"),
            (ENV_HOST_DIR, "C:/tmp/rpc/s1"),
            (ENV_HOST_SESSION, "s_abc"),
        ]);
        for sub in FORWARD_COMMANDS {
            match decide(Some(sub), &env) {
                HostAction::Forward {
                    dir,
                    timeout_ms,
                    session,
                } => {
                    assert_eq!(dir, PathBuf::from("C:/tmp/rpc/s1"));
                    assert_eq!(timeout_ms, DEFAULT_TIMEOUT_MS);
                    assert_eq!(session.as_deref(), Some("s_abc"));
                }
                other => panic!("子命令 {sub} 应转发，实得 {other:?}"),
            }
        }
    }

    #[test]
    fn bypass压过一切() {
        let env = env_of(&[
            (ENV_HOST, "aichat"),
            (ENV_HOST_DIR, "C:/tmp/rpc/s1"),
            (ENV_HOST_BYPASS, "1"),
        ]);
        for sub in HOOK_COMMANDS.iter().chain(FORWARD_COMMANDS.iter()) {
            assert_eq!(decide(Some(sub), &env), HostAction::Direct, "子命令 {sub}");
        }
    }

    #[test]
    fn 显式关闭的写法不算置位() {
        for off in ["", "  ", "0", "false", "FALSE"] {
            let env = env_of(&[(ENV_HOST, off)]);
            assert_eq!(decide(Some("hot-index"), &env), HostAction::Direct, "{off:?}");
        }
    }

    #[test]
    fn resolve与不可逆命令不转发() {
        let env = env_of(&[(ENV_HOST, "aichat"), (ENV_HOST_DIR, "C:/tmp/rpc/s1")]);
        for sub in ["resolve", "forget", "gc", "merge", "supersede", "serve", "doctor"] {
            assert_eq!(decide(Some(sub), &env), HostAction::Direct, "子命令 {sub}");
        }
    }

    #[test]
    fn 超时可覆盖且封顶() {
        let cases = [
            ("500", 500),
            ("0", DEFAULT_TIMEOUT_MS),
            ("abc", DEFAULT_TIMEOUT_MS),
            ("999999999", MAX_TIMEOUT_MS),
        ];
        for (raw, want) in cases {
            let env = env_of(&[(ENV_HOST_DIR, "C:/tmp"), (ENV_HOST_TIMEOUT_MS, raw)]);
            match decide(Some("recall"), &env) {
                HostAction::Forward { timeout_ms, .. } => assert_eq!(timeout_ms, want, "{raw}"),
                other => panic!("应转发，实得 {other:?}"),
            }
        }
    }

    #[test]
    fn 取子命令只看第一个参数() {
        let owned = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(subcommand_of(&owned(&["recall", "--query", "x"])), Some("recall"));
        assert_eq!(subcommand_of(&owned(&["--version"])), None);
        assert_eq!(subcommand_of(&owned(&["--help"])), None);
        assert_eq!(subcommand_of(&owned(&[])), None);
        // 「help recall」这类不在两张表里，照常交给 clap。
        assert_eq!(subcommand_of(&owned(&["help", "recall"])), Some("help"));
    }

    #[test]
    fn 回执解析容错() {
        let ok = br#"{"id":"a1","stdout":"hi","stderr":"e","exit_code":3}"#;
        assert_eq!(
            parse_response_line(ok, "a1"),
            Some(HostResponse {
                stdout: "hi".into(),
                stderr: "e".into(),
                exit_code: 3,
            })
        );
        // id 不匹配 / 畸形行 / 非对象：一律 None，继续等。
        assert_eq!(parse_response_line(ok, "other"), None);
        assert_eq!(parse_response_line(b"{ not json", "a1"), None);
        assert_eq!(parse_response_line(b"[1,2]", "a1"), None);
        // 缺字段取缺省，驼峰别名也认。
        assert_eq!(
            parse_response_line(br#"{"id":"a1","exitCode":1}"#, "a1"),
            Some(HostResponse {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 1,
            })
        );
    }

    #[test]
    fn 目录不存在立刻退回不等超时() {
        let dir = std::env::temp_dir().join(format!("engram-host-missing-{}", new_request_id()));
        let began = Instant::now();
        let got = forward(&dir, "recall", &[], None, 5_000);
        assert_eq!(got, Err(ForwardError::NoDir));
        assert!(began.elapsed() < Duration::from_millis(500), "不该等满超时");
    }

    #[test]
    fn 没人消费请求时超时退回() {
        let dir = std::env::temp_dir().join(format!("engram-host-idle-{}", new_request_id()));
        std::fs::create_dir_all(&dir).expect("建临时桥目录失败");
        let got = forward(&dir, "recall", &["--query".into(), "x".into()], None, 120);
        assert_eq!(got, Err(ForwardError::Timeout));
        // 请求确实投递了，且字段齐全——宿主晚点捞起来能看懂。
        let req = std::fs::read_to_string(dir.join(REQ_FILE)).expect("读 req.jsonl 失败");
        let v: serde_json::Value = serde_json::from_str(req.trim()).expect("req 行不是 JSON");
        assert_eq!(v["cmd"], "recall");
        assert_eq!(v["args"][0], "--query");
        assert_eq!(v["timeout_ms"], 120);
        assert!(v["ts"].as_u64().is_some_and(|t| t > 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 半行回执不会漏读() {
        let dir = std::env::temp_dir().join(format!("engram-host-partial-{}", new_request_id()));
        std::fs::create_dir_all(&dir).expect("建临时桥目录失败");
        let resp = dir.join(RESP_FILE);
        // 先写半行，再补齐——模拟宿主写到一半被读到。
        std::fs::write(&resp, br#"{"id":"zz","stdo"#).expect("写半行失败");
        let mut pos = 0u64;
        let mut carry = Vec::new();
        assert_eq!(scan_response(&resp, "zz", &mut pos, &mut carry), Ok(None));
        let mut f = OpenOptions::new().append(true).open(&resp).expect("追加失败");
        f.write_all(br#"ut":"done","exit_code":0}"#).expect("补齐失败");
        f.write_all(b"\n").expect("补换行失败");
        drop(f);
        let got = scan_response(&resp, "zz", &mut pos, &mut carry).expect("扫描失败");
        assert_eq!(got.map(|r| r.stdout), Some("done".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
