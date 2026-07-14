//! engram 参数整定量化 harness（docs/参数整定方案.md §9 题库的可执行落地）。
//!
//! 定位：对「数据生成 → 数据输入 → 结果输出」全链路给出**量化**测试结果，
//! 为参数优化提供锚定，不靠猜。纯内存 `Vec<Memory>` 回放，零磁盘零库；
//! 引擎逻辑一律调用 lib 本体（`consolidate_with` / `gc_should_delete` /
//! activation 系 / `EngramConfig` / render / recall），**严禁在 harness 里
//! 复刻任何公式**（零公式漂移）。
//!
//! 用法（在 `plugin/engine` 目录下）：
//!
//! ```text
//! cargo run --example tuning -- run [--seed 42] [--out report.json] [--baseline old.json]
//!                                   [--replay-dir <export目录> --cutoff <unix秒>] [--fingerprint]
//! cargo run --example tuning -- sweep --param importance_gate=0.5:1.0:6 [--seed 42] [--out sweep.json]
//! cargo run --example tuning -- search --n 50 --seed 42 [--out search.json]
//! cargo run --example tuning -- replay --dir <export目录> --cutoff <unix秒>
//! cargo run --example tuning -- fingerprint
//! ```
//!
//! 防自欺五铁律（方案 §5）在本 harness 的落点：
//! 1. oracle 与机制正交 —— 见 `protos.rs` 文件头声明（节律只用周期+抖动+泊松式稀疏）；
//! 2. 出题先于调参、题库冻结进 git —— `fingerprint` 子命令输出题库骨架的规范化哈希，
//!    冻结后 CI 可校验未被回头改题；
//! 3. train/val/test 不同种子 —— `--seed` 显式注入，全程确定性（内联 xorshift64*，
//!    禁用系统时钟，时间全部用相对锚点 T 的常量）；
//! 4. 真实回放只验不调 —— `replay` 输出里硬编码打印幸存者偏差声明；
//! 5. 报告平坦度 —— `search` 对 top 配置输出邻域 loss 方差。

mod events;
mod protos;
mod questions;
mod report;
mod rng;
mod sweep;

use std::process::ExitCode;

/// 打印用法说明。
fn usage() {
    eprintln!(
        "用法：cargo run --example tuning -- <run|sweep|search|replay|fingerprint> [选项]\n\
         run         全量跑题库并输出量化报告（--seed/--out/--baseline/--replay-dir/--cutoff/--fingerprint）\n\
         sweep       单参数扫描（--param 名=lo:hi:步数 [--seed] [--out]）\n\
         search      随机搜索（--n 采样数 --seed 种子 [--out]）\n\
         replay      真实回放（--dir export目录 --cutoff unix秒；只验不调）\n\
         fingerprint 输出题库定义的规范化哈希（冻结校验用）"
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

/// 解析必为数值的选项；缺省用 `default`，解析失败报中文错误。
fn opt_f64(args: &[String], key: &str, default: f64) -> Result<f64, String> {
    match opt_val(args, key) {
        None => Ok(default),
        Some(s) => s
            .parse::<f64>()
            .map_err(|e| format!("选项 {key} 的值 {s} 不是数字：{e}")),
    }
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
        "sweep" => cmd_sweep(rest),
        "search" => cmd_search(rest),
        "replay" => cmd_replay(rest),
        "fingerprint" => {
            println!("题库指纹（FNV-1a 64）：{:016x}", questions::fingerprint());
            Ok(())
        }
        other => {
            usage();
            Err(format!("未知子命令 {other}"))
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tuning 失败：{e}");
            ExitCode::FAILURE
        }
    }
}

/// `run` 子命令：默认参数（或将来注入的配置）全量跑 20 护栏 + 5 偏好 + 7 质量题，
/// 可选追加真实回放段，输出终端表格与 JSON 报告。
fn cmd_run(args: &[String]) -> Result<(), String> {
    let seed = opt_u64(args, "--seed", 42)?;
    let cfg = engram::model::EngramConfig::default();

    if opt_flag(args, "--fingerprint") {
        println!("题库指纹（FNV-1a 64）：{:016x}", questions::fingerprint());
    }

    // 可选真实回放段（只验不调）。
    let replay = match opt_val(args, "--replay-dir") {
        None => None,
        Some(dir) => {
            let cutoff = opt_f64(args, "--cutoff", f64::NAN)?;
            if cutoff.is_nan() {
                return Err("--replay-dir 需要配套 --cutoff <unix秒>".to_string());
            }
            Some(questions::replay::run(
                std::path::Path::new(&dir),
                cutoff,
                &cfg,
            )?)
        }
    };

    let rep = sweep::evaluate(&cfg, seed, replay);
    report::print_table(&rep);

    if let Some(baseline) = opt_val(args, "--baseline") {
        report::diff_baseline(&rep, std::path::Path::new(&baseline))?;
    }
    if let Some(out) = opt_val(args, "--out") {
        report::write_json(&rep, std::path::Path::new(&out))?;
        println!("报告已写入 {out}");
    }
    Ok(())
}

/// `sweep` 子命令：白名单单参数扫描，输出护栏通过率 + loss 曲线（JSON+CSV）。
fn cmd_sweep(args: &[String]) -> Result<(), String> {
    let seed = opt_u64(args, "--seed", 42)?;
    let spec = opt_val(args, "--param").ok_or("sweep 需要 --param 名=lo:hi:步数")?;
    let out = opt_val(args, "--out");
    sweep::cmd_sweep(&spec, seed, out.as_deref())
}

/// `search` 子命令：随机搜索 + 全护栏过滤 + top-10 + 平坦度（铁律 5）。
fn cmd_search(args: &[String]) -> Result<(), String> {
    let n = opt_u64(args, "--n", 50)? as usize;
    let seed = opt_u64(args, "--seed", 42)?;
    let out = opt_val(args, "--out");
    sweep::cmd_search(n, seed, out.as_deref())
}

/// `replay` 子命令：读 engram export 目录做真实回放（只验不调，含幸存者偏差）。
fn cmd_replay(args: &[String]) -> Result<(), String> {
    let dir = opt_val(args, "--dir").ok_or("replay 需要 --dir <export目录>")?;
    let cutoff = opt_f64(args, "--cutoff", f64::NAN)?;
    if cutoff.is_nan() {
        return Err("replay 需要 --cutoff <unix秒>".to_string());
    }
    let cfg = engram::model::EngramConfig::default();
    let outcome = questions::replay::run(std::path::Path::new(&dir), cutoff, &cfg)?;
    questions::replay::print(&outcome);
    Ok(())
}
