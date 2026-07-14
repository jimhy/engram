//! 起复盘者（真实 LLM headless）与 mock 回放两条执行路径。
//!
//! - **live**：复刻 `scripts/launch-reviewer.*` 的起法——生成一次性启动脚本
//!   （Windows `.cmd` / POSIX `.sh`），`<cli> -p --allowedTools "Read"
//!   "Bash(<shim>:*)" "Bash(\"<shim>\":*)" < prompt > out 2> err`，同步等待、
//!   超时可配（缺省 300 秒）。子进程环境带 `ENGRAM_REVIEWER=1`，防止复盘者
//!   自己的 SessionStart/SessionEnd hook 递归/污染真实用户库。
//! - **mock**：跳过 LLM，把 case 的 `mock_actions.argv` 逐条喂给录制 shim
//!   （等效回放），用于无 LLM 冒烟与 CI；对带库参数的子命令自动补挂沙箱库
//!   （`--general-db` / `--project-db`），让金标 case 无需硬编码沙箱路径。

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::cases::GoldCase;
use crate::sandbox::{fwd, Sandbox};

/// 一次执行（live 或 mock）的结果概要。
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunOutcome {
    /// live 模式：复盘者是否超时被杀。
    pub timed_out: bool,
    /// 退出码（被信号杀等无码情形为 None；mock 模式恒 None——逐动作各有退出码）。
    pub exit_code: Option<i32>,
    /// 运行备注（超时、动作失败等；进报告帮助人工排查）。
    pub notes: Vec<String>,
}

/// 接受 `--general-db` / `--project-db` 挂载参数的引擎子命令白名单
/// （mock 回放自动补挂沙箱库只对它们生效；`consolidate-done` 等不吃库参数）。
const DB_SUBCOMMANDS: &[&str] = &[
    "render",
    "recall",
    "write",
    "export",
    "list",
    "consolidate",
    "import",
    "confirm-use",
    "supersede",
    "merge",
    "forget",
    "gc",
    "graduate",
];

/// live 前置探测：验证复盘者 CLI 至少能启动（`<cli> --version`）。
///
/// 探测只覆盖「起得来」这一层——`claude -p` 的凭据 403 这类**运行期**拒绝要到
/// 真正跑 case 时才触发（见 [`run_live`] 对 stderr 的摘要上报），但「命令不存在 /
/// PATH 不对 / 版本命令即报错」这类硬伤在这里就拦下，错误信息原样透出不吞。
pub fn probe_cli(cli: &str) -> Result<(), String> {
    let output = if cfg!(windows) {
        // 经 cmd 解析，兼容 `claude` 实为 .cmd/.ps1 shim 的常见安装形态。
        Command::new("cmd")
            .args(["/C", cli, "--version"])
            .stdin(Stdio::null())
            .output()
    } else {
        Command::new("sh")
            .arg("-c")
            .arg(format!("\"{cli}\" --version"))
            .stdin(Stdio::null())
            .output()
    };
    let hint = "排查提示：真实评测需 ENGRAM_EVAL_CLI（或 --cli）指向可用的 headless CLI；\
                若本机 `claude -p` 报 403（凭据对无头模式受限），需先修复凭据或改用其它\
                兼容 CLI——mock 冒烟请用 `run --mock`（不依赖任何 LLM）。";
    match output {
        Err(e) => Err(format!("复盘者 CLI「{cli}」无法启动：{e}\n{hint}")),
        Ok(out) if !out.status.success() => Err(format!(
            "复盘者 CLI 探测失败：`{cli} --version` 退出码 {:?}\nstderr：{}\nstdout：{}\n{hint}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim(),
            String::from_utf8_lossy(&out.stdout).trim(),
        )),
        Ok(_) => Ok(()),
    }
}

/// 代理派生（堵 403）：与 `launch-reviewer.*` 同款逻辑的 Rust 版。headless `claude -p`
/// 复盘子进程若缺 HTTPS_PROXY 会走直连、被 Anthropic 以「403 Request not allowed」按地区
/// 限制拒绝。返回应显式设给子进程的代理值（同时用于 HTTPS_PROXY / HTTP_PROXY），无需设时返回
/// `None`。优先级：① `ENGRAM_REVIEWER_PROXY` 显式指定；② 已有 `HTTPS_PROXY`/`https_proxy`
/// → 返回 None（harness 环境经 `Command` 默认继承直接透传，无需覆盖）；③ `ALL_PROXY`/
/// `all_proxy` → 据以派生；④ 都没有 → None。值可能是 host:port（无 scheme），统一补 `http://`。
fn derive_reviewer_proxy() -> Option<String> {
    let norm = |v: &str| -> String {
        let t = v.trim();
        if t.contains("://") {
            t.to_string()
        } else {
            format!("http://{t}")
        }
    };
    let nonempty = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    if let Some(v) = nonempty("ENGRAM_REVIEWER_PROXY") {
        return Some(norm(&v));
    }
    // 已有 HTTPS_PROXY/https_proxy：保持不动（默认继承已透传，不覆盖）。
    if nonempty("HTTPS_PROXY").is_some() || nonempty("https_proxy").is_some() {
        return None;
    }
    if let Some(v) = nonempty("ALL_PROXY").or_else(|| nonempty("all_proxy")) {
        return Some(norm(&v));
    }
    None
}

/// live：起真实复盘者并同步等待（超时杀）。
///
/// 一次性启动脚本落在沙箱内（`run-reviewer.cmd` / `.sh`），allowedTools 的写法
/// 与 launch-reviewer 一致：只放行 `Read` 与录制 shim 的 Bash 调用（含带引号
/// 变体，覆盖模型给路径加引号的写法）。
pub fn run_live(sb: &Sandbox, cli: &str, timeout_secs: u64) -> Result<RunOutcome, String> {
    let shim_fwd = fwd(&sb.shim);
    let mut notes = Vec::new();

    let mut cmd = if cfg!(windows) {
        let script = sb.dir.join("run-reviewer.cmd");
        let prompt = sb.prompt_file.display().to_string();
        let out = sb.out_file.display().to_string();
        let err = sb.err_file.display().to_string();
        // cmd 批处理里引号按 "" 转义（与 launch-reviewer.ps1 同款）。
        let text = format!(
            "@echo off\r\n\
             call \"{cli}\" -p --allowedTools \"Read\" \"Bash({shim_fwd}:*)\" \"Bash(\"\"{shim_fwd}\"\":*)\" < \"{prompt}\" > \"{out}\" 2> \"{err}\"\r\n\
             exit /b %ERRORLEVEL%\r\n"
        );
        std::fs::write(&script, text).map_err(|e| format!("写启动脚本失败：{e}"))?;
        Command::new(&script)
    } else {
        let script = sb.dir.join("run-reviewer.sh");
        let prompt = fwd(&sb.prompt_file);
        let out = fwd(&sb.out_file);
        let err = fwd(&sb.err_file);
        let text = format!(
            "#!/usr/bin/env sh\n\
             \"{cli}\" -p --allowedTools \"Read\" \"Bash({shim_fwd}:*)\" \"Bash(\\\"{shim_fwd}\\\":*)\" < \"{prompt}\" > \"{out}\" 2> \"{err}\"\n"
        );
        std::fs::write(&script, text).map_err(|e| format!("写启动脚本失败：{e}"))?;
        let mut c = Command::new("sh");
        c.arg(&script);
        c
    };
    cmd.env("ENGRAM_REVIEWER", "1")
        .current_dir(&sb.dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // harness 自身的 HTTPS_PROXY/HTTP_PROXY/ALL_PROXY 经 Command 默认继承透传给子进程；
    // 这里再补一层派生：仅设了 ALL_PROXY（或用 ENGRAM_REVIEWER_PROXY 指定）时据以补
    // HTTPS_PROXY/HTTP_PROXY，避免与 launch-reviewer.* 不一致而在无头模式触发 403。
    if let Some(proxy) = derive_reviewer_proxy() {
        cmd.env("HTTPS_PROXY", &proxy).env("HTTP_PROXY", &proxy);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("起复盘者失败（cli={cli}）：{e}"))?;
    let deadline = Instant::now() + Duration::from_secs(timeout_secs.max(1));
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let code = status.code();
                if !status.success() {
                    notes.push(format!(
                        "复盘者退出码 {code:?}（stderr 见 {}）",
                        sb.err_file.display()
                    ));
                    // 不吞错：把 stderr 摘要直接抬进报告备注，403 场景专门点名。
                    let err_txt = std::fs::read_to_string(&sb.err_file).unwrap_or_default();
                    let err_trim = err_txt.trim();
                    if !err_trim.is_empty() {
                        let excerpt: String = err_trim.chars().take(400).collect();
                        notes.push(format!("stderr 摘要：{excerpt}"));
                    }
                    if err_trim.contains("403") {
                        notes.push(
                            "检测到 403：当前凭据对无头模式（-p）受限——请修复凭据，\
                             或用 ENGRAM_EVAL_CLI 指向其它可用 CLI 后重跑真实评测"
                                .to_string(),
                        );
                    }
                }
                return Ok(RunOutcome {
                    timed_out: false,
                    exit_code: code,
                    notes,
                });
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    notes.push(format!(
                        "复盘者超时（>{timeout_secs}s）被杀；按当前库状态照常判分"
                    ));
                    return Ok(RunOutcome {
                        timed_out: true,
                        exit_code: None,
                        notes,
                    });
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(format!("等待复盘者失败：{e}")),
        }
    }
}

/// mock：把 `mock_actions` 逐条喂给录制 shim（等效回放）。
///
/// 单个动作失败（非零退出码 / 起进程失败）记入 notes 并继续——判分看的是
/// 库快照与命令日志的最终事实，不以动作退出码定成败。
pub fn run_mock(sb: &Sandbox, case_: &GoldCase) -> Result<RunOutcome, String> {
    let mut notes = Vec::new();
    for (i, action) in case_.mock_actions.iter().enumerate() {
        let mut argv = action.argv.clone();
        let sub = argv.first().cloned().unwrap_or_default();
        if DB_SUBCOMMANDS.contains(&sub.as_str()) {
            if !argv.iter().any(|a| a == "--general-db") {
                argv.push("--general-db".to_string());
                argv.push(fwd(&sb.general_db));
            }
            if !argv.iter().any(|a| a == "--project-db") {
                argv.push("--project-db".to_string());
                argv.push(format!("{}={}", case_.project_name, fwd(&sb.project_db)));
            }
        }

        // Windows 直接 spawn .cmd（std 对 .bat/.cmd 有专门的安全引用处理）；
        // POSIX 直接执行带 shebang 的 shim.sh。
        let status = Command::new(&sb.shim)
            .args(&argv)
            .current_dir(&sb.dir)
            .stdin(Stdio::null())
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => notes.push(format!("mock 动作 #{i}（{sub}）退出码 {:?}", s.code())),
            Err(e) => notes.push(format!("mock 动作 #{i}（{sub}）起进程失败：{e}")),
        }
    }
    Ok(RunOutcome {
        timed_out: false,
        exit_code: None,
        notes,
    })
}
