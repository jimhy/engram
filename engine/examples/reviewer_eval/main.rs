//! engram 复盘者评测 harness（reviewer_eval）。
//!
//! 定位：对「会话末复盘者」这一 **LLM 环节**做端到端金标评测——tuning harness
//! 量化的是确定性状态机（升降级/衰减/容量），本 harness 量化的是复盘者在真实
//! 转录上「该加固的加固了没、该写的写了没、不该写的写没写、密钥漏没漏」。
//!
//! 每个 case 的流程：
//! 1. 在 TMP 下建独立沙箱：用 engine lib（store）预置 general.redb 与项目库
//!    （preexisting 按 scope 分流）；
//! 2. 转录行写成切片 `.jsonl`；
//! 3. 生成**录制 shim**（Windows `.cmd` + POSIX `.sh` 双版）：完整 argv 以 JSONL
//!    追加到命令日志，再原样转发给真 engram 二进制（debug 构建）并透传退出码；
//! 4. 按 `scripts/launch-reviewer.*` 的同款逻辑填 `reviewer-prompt.md` 模板
//!    （`{{ENGRAM}}`→shim、`{{TRANSCRIPT}}`→切片、库路径→沙箱；另用 lib render
//!    对沙箱库生成真实热索引文本注入 prompt，保证 `#id` 短标记可用）；
//! 5. 起复盘者：`ENGRAM_EVAL_CLI`（缺省 `claude`）`-p < prompt`，`--allowedTools`
//!    只放行 Read 与 shim；同步等待，超时可配（缺省 300s），stdout/err 落沙箱；
//! 6. 判分（纯函数，见 `score.rs`）：初始/结束库快照 + shim 命令日志 + expect；
//! 7. `--mock`：跳过第 5 步，把 case 的 `mock_actions.argv` 逐条喂给 shim
//!    （等效回放），用于无 LLM 冒烟与 CI。
//!
//! 用法（在 `plugin/engine` 目录下，先 `cargo build` 出 debug 引擎二进制）：
//!
//! ```text
//! cargo run --example reviewer_eval -- run --mock                       # 无 LLM 冒烟（CI）
//! cargo run --example reviewer_eval -- run [--cli claude] [--timeout-secs 300]
//!     [--cases-dir <dir>] [--case <name>] [--out report.json] [--baseline old.json]
//!     [--engine <engram二进制>] [--clean]
//! ```
//!
//! - 真实复盘者 CLI 由 `--cli` 或环境变量 `ENGRAM_EVAL_CLI` 指定（缺省 `claude`）。
//! - 退出码：harness 自身出错（case 解析失败、沙箱搭建失败等）才非零；case 判级
//!   （pass/fail/hard_fail）只进报告——自带冒烟对里有一例故意 hard_fail，CI 以
//!   「harness 跑完 + 报告断言」为门，不以判级为门。
//! - `--mock` 下没有 `mock_actions` 的 case 跳过并提示（金标 case 可只服务 live 评测）。
//! - 零新依赖；金标 case JSON 契约见 `cases.rs`，判分规则见 `score.rs`。
//!
//! 另有内部子命令 `shim-log`（录制 shim 借它把 argv 追加成 JSONL），用户无需直接使用。

mod cases;
mod report;
mod runner;
mod sandbox;
mod score;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// 打印用法说明。
fn usage() {
    eprintln!(
        "用法：cargo run --example reviewer_eval -- <run|shim-log> [选项]\n\
         run       跑金标 case 评测：\n\
           --mock              不起 LLM，回放 case.mock_actions（无 LLM 冒烟 / CI）\n\
           --cases-dir <dir>   金标 case 目录（缺省 examples/reviewer_eval/cases）\n\
           --case <name>       只跑指定名字的 case\n\
           --cli <cmd>         复盘者 CLI（缺省 env ENGRAM_EVAL_CLI，再缺省 claude）\n\
           --timeout-secs <n>  每 case 复盘者超时秒数（缺省 300）\n\
           --engine <path>     engram 二进制（缺省 target/debug/engram，env ENGRAM_EVAL_ENGINE）\n\
           --out <file>        JSON 报告输出\n\
           --baseline <file>   与旧 JSON 报告对照\n\
           --clean             判分后删除各 case 沙箱目录（缺省保留供取证）\n\
         shim-log  （内部）录制 shim 用：--log <文件> -- <argv...>\n\
         \n\
         真实评测（不带 --mock）需要环境变量 ENGRAM_EVAL_CLI（或 --cli）指向一个\n\
         可用的 headless CLI；起跑前会先做一次可行性探测（<cli> --version），\n\
         探测失败立即报错并给出排查提示（例如 claude 凭据对无头模式受限返回 403\n\
         的场景），不会带病跑完全部 case 再产出空报告。"
    );
}

/// 从参数列表里取 `--key value` 形式的值。
fn opt_val(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// 取 `--key` 布尔开关是否出现。
fn opt_flag(args: &[String], key: &str) -> bool {
    args.iter().any(|a| a == key)
}

/// 解析必为整数的选项；缺省用 `default`。
fn opt_u64(args: &[String], key: &str, default: u64) -> Result<u64, String> {
    match opt_val(args, key) {
        None => Ok(default),
        Some(s) => s
            .parse::<u64>()
            .map_err(|e| format!("选项 {key} 的值 {s} 不是整数：{e}")),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().map(String::as_str) else {
        usage();
        return ExitCode::FAILURE;
    };
    let rest = &args[1..];

    let result = match cmd {
        "run" => cmd_run(rest),
        "shim-log" => cmd_shim_log(rest),
        other => {
            usage();
            Err(format!("未知子命令 {other}"))
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("reviewer_eval 失败：{e}");
            ExitCode::FAILURE
        }
    }
}

/// （内部）录制 shim 的日志臂：把 `--` 之后的完整 argv 以 `{"argv":[...]}`
/// 追加到 `--log` 指定的 JSONL 文件。JSON 转义交给 serde——批处理 / sh 自身
/// 不做任何转义，天然扛住 cue 里的引号、反斜杠与中文。
fn cmd_shim_log(args: &[String]) -> Result<(), String> {
    let log = opt_val(args, "--log").ok_or("shim-log 需要 --log <文件>")?;
    let sep = args
        .iter()
        .position(|a| a == "--")
        .ok_or("shim-log 需要 `--` 分隔转发 argv")?;
    let argv: Vec<&String> = args[sep + 1..].iter().collect();
    let line = serde_json::to_string(&serde_json::json!({ "argv": argv }))
        .map_err(|e| format!("序列化 argv 失败：{e}"))?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|e| format!("打开日志 {log} 失败：{e}"))?;
    writeln!(f, "{line}").map_err(|e| format!("写日志失败：{e}"))?;
    Ok(())
}

/// 定位真 engram 二进制：`--engine` > env `ENGRAM_EVAL_ENGINE` > 本 harness
/// 可执行文件旁的 `target/debug/engram` > manifest 下的 `target/debug/engram`。
fn find_engine(args: &[String], manifest: &Path) -> Result<PathBuf, String> {
    let bin_name = if cfg!(windows) {
        "engram.exe"
    } else {
        "engram"
    };
    let explicit = opt_val(args, "--engine").or_else(|| {
        std::env::var("ENGRAM_EVAL_ENGINE")
            .ok()
            .filter(|s| !s.trim().is_empty())
    });
    if let Some(p) = explicit {
        let p = PathBuf::from(p);
        return if p.exists() {
            Ok(p)
        } else {
            Err(format!("指定的 engram 二进制不存在：{}", p.display()))
        };
    }
    // examples 的可执行文件位于 target/debug/examples/，上一级即 target/debug/。
    if let Ok(exe) = std::env::current_exe() {
        if let Some(debug_dir) = exe.parent().and_then(Path::parent) {
            let cand = debug_dir.join(bin_name);
            if cand.exists() {
                return Ok(cand);
            }
        }
    }
    let cand = manifest.join("target").join("debug").join(bin_name);
    if cand.exists() {
        return Ok(cand);
    }
    Err(
        "未找到 engram 调试二进制：请先在 plugin/engine 下执行 `cargo build`，\
         或用 --engine / 环境变量 ENGRAM_EVAL_ENGINE 显式指定"
            .to_string(),
    )
}

/// `run` 子命令：加载金标 case → 逐 case 沙箱 + 执行（live/mock）+ 判分 → 报告。
fn cmd_run(args: &[String]) -> Result<(), String> {
    let mock = opt_flag(args, "--mock");
    let clean = opt_flag(args, "--clean");
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let plugin_root = manifest
        .parent()
        .ok_or("engine manifest 目录没有父目录（找不到 plugin 根）")?
        .to_path_buf();
    let cases_dir = opt_val(args, "--cases-dir")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            manifest
                .join("examples")
                .join("reviewer_eval")
                .join("cases")
        });
    let only = opt_val(args, "--case");
    let timeout_secs = opt_u64(args, "--timeout-secs", 300)?;
    let cli = opt_val(args, "--cli")
        .or_else(|| std::env::var("ENGRAM_EVAL_CLI").ok())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "claude".to_string());
    let engine = find_engine(args, &manifest)?;
    let self_exe = std::env::current_exe().map_err(|e| format!("取自身可执行文件路径失败：{e}"))?;

    // live 模式前置：明确提示 CLI 来源，并做最小可行性探测——起不来就立即报错
    // （含 403 场景排查提示），不带病跑完全部 case 再产出空报告。
    if !mock {
        println!(
            "live 模式：复盘者 CLI = {cli}（可用 --cli 或环境变量 ENGRAM_EVAL_CLI 覆盖；\
             真实评测需其为可用的 headless CLI）"
        );
        runner::probe_cli(&cli)?;
    }

    let mut all_cases = cases::load_dir(&cases_dir)?;
    if let Some(name) = &only {
        all_cases.retain(|c| &c.name == name);
        if all_cases.is_empty() {
            return Err(format!(
                "--case {name} 在 {} 下没有匹配",
                cases_dir.display()
            ));
        }
    }
    if all_cases.is_empty() {
        return Err(format!(
            "case 目录 {} 下没有任何金标 case",
            cases_dir.display()
        ));
    }

    // 本次运行的沙箱根：TMP 下带 pid+纳秒戳，互不相扰。
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let run_root = std::env::temp_dir().join(format!(
        "engram-reviewer-eval-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&run_root)
        .map_err(|e| format!("建评测根目录 {} 失败：{e}", run_root.display()))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);

    let mut rows: Vec<report::CaseRow> = Vec::new();
    for case_ in &all_cases {
        // --mock 下无 mock_actions 的 case 跳过并提示（它们只服务 live 评测）。
        if mock && case_.mock_actions.is_empty() {
            eprintln!(
                "跳过 {}：--mock 模式但该 case 无 mock_actions（仅可 live 评测）",
                case_.name
            );
            rows.push(report::CaseRow {
                name: case_.name.clone(),
                description: case_.description.clone(),
                grade: "skipped".to_string(),
                skipped_reason: Some("--mock 模式下无 mock_actions".to_string()),
                sandbox: None,
                run: None,
                score: None,
            });
            continue;
        }

        println!("[{}] 搭沙箱…", case_.name);
        let sb = sandbox::build(case_, &run_root, &engine, &self_exe, &plugin_root, now)?;
        let before =
            sandbox::snapshot(&sb).map_err(|e| format!("case {} 初始快照失败：{e}", case_.name))?;

        let run_outcome = if mock {
            println!(
                "[{}] mock 回放 {} 个动作…",
                case_.name,
                case_.mock_actions.len()
            );
            runner::run_mock(&sb, case_)?
        } else {
            println!(
                "[{}] 起复盘者（cli={cli}, timeout={timeout_secs}s）…",
                case_.name
            );
            runner::run_live(&sb, &cli, timeout_secs)?
        };

        let after =
            sandbox::snapshot(&sb).map_err(|e| format!("case {} 结束快照失败：{e}", case_.name))?;
        let log_lines: Vec<String> = std::fs::read_to_string(&sb.cmd_log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        let log = score::parse_cmd_log(&log_lines);
        let sc = score::score(&before, &after, &log, &case_.expect, &case_.project_name);

        let sandbox_path = sb.dir.display().to_string();
        if clean {
            let _ = std::fs::remove_dir_all(&sb.dir);
        }
        rows.push(report::CaseRow {
            name: case_.name.clone(),
            description: case_.description.clone(),
            grade: match sc.grade {
                score::Grade::Pass => "pass",
                score::Grade::Fail => "fail",
                score::Grade::HardFail => "hard_fail",
            }
            .to_string(),
            skipped_reason: None,
            sandbox: if clean { None } else { Some(sandbox_path) },
            run: Some(run_outcome),
            score: Some(sc),
        });
    }

    let summary = report::summarize(&rows);
    let rep = report::EvalReport {
        mode: if mock { "mock" } else { "live" }.to_string(),
        cli,
        engine: engine.display().to_string(),
        cases_dir: cases_dir.display().to_string(),
        cases: rows,
        summary,
    };
    report::print_table(&rep);

    if let Some(baseline) = opt_val(args, "--baseline") {
        report::diff_baseline(&rep, Path::new(&baseline))?;
    }
    if let Some(out) = opt_val(args, "--out") {
        report::write_json(&rep, Path::new(&out))?;
        println!("报告已写入 {out}");
    }
    if clean {
        let _ = std::fs::remove_dir_all(&run_root);
    }
    Ok(())
}
