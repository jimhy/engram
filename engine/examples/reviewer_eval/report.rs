//! 评测报告：逐 case 表 + 汇总指标，文本（终端）+ `--out` JSON + `--baseline`
//! 对照（与 examples/tuning 的报告同风格）。

use std::path::Path;

use crate::runner::RunOutcome;
use crate::score::{ratio, CaseScore, Grade};

/// 报告里的一行 case 结果。
#[derive(serde::Serialize)]
pub struct CaseRow {
    /// case 名。
    pub name: String,
    /// 一句话场景说明。
    pub description: String,
    /// 判级：`pass` / `fail` / `hard_fail` / `skipped`。
    pub grade: String,
    /// 跳过原因（仅 skipped 行有）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_reason: Option<String>,
    /// 沙箱目录（人工取证用）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    /// 执行概要（超时 / 动作失败等）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<RunOutcome>,
    /// 判分明细（skipped 行无）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<CaseScore>,
}

/// 汇总指标（分子/分母都带上，比率按「分母 0 → 1.0」口径）。
#[derive(serde::Serialize)]
pub struct Summary {
    /// case 总数（含跳过）。
    pub cases_total: usize,
    /// 实际判分的 case 数。
    pub cases_scored: usize,
    /// 跳过数（--mock 下无 mock_actions 等）。
    pub cases_skipped: usize,
    /// 判级计数。
    pub pass: usize,
    /// 判级计数。
    pub fail: usize,
    /// 判级计数（= 安全硬失败 case 数）。
    pub hard_fail: usize,
    /// 加固：实际∩期望 总数。
    pub confirm_hits: usize,
    /// 加固：实际总数。
    pub confirm_total: usize,
    /// 加固：期望总数。
    pub confirm_expected: usize,
    /// 加固查准（微平均）。
    pub confirm_precision: f64,
    /// 加固查全（微平均）。
    pub confirm_recall: f64,
    /// 违规加固总数（confirm_use_forbidden 被加固）。
    pub confirm_violations: usize,
    /// must_write 规格总数。
    pub must_write_total: usize,
    /// 漏掉的规格总数。
    pub must_write_missed: usize,
    /// 漏写率 = 漏 / 总。
    pub miss_rate: f64,
    /// 噪声违规总数。
    pub noise_violations: usize,
    /// supersede 期望总数。
    pub supersede_expected: usize,
    /// supersede 命中总数。
    pub supersede_hits: usize,
    /// supersede 命中率。
    pub supersede_rate: f64,
    /// 层级诊断分母（关键词命中的规格数）。
    pub level_checked: usize,
    /// 层级诊断分子。
    pub level_hits: usize,
    /// 层级命中率。
    pub level_rate: f64,
    /// importance 诊断分母。
    pub importance_checked: usize,
    /// importance 诊断分子。
    pub importance_hits: usize,
    /// importance 区间命中率。
    pub importance_rate: f64,
    /// 计划外写入总数（待人工复核）。
    pub unplanned_total: usize,
}

/// 一份完整评测报告。
#[derive(serde::Serialize)]
pub struct EvalReport {
    /// 运行模式：`mock` 或 `live`。
    pub mode: String,
    /// 复盘者 CLI（live 模式实际使用者；mock 模式仅回显配置）。
    pub cli: String,
    /// 真 engram 二进制路径。
    pub engine: String,
    /// case 目录。
    pub cases_dir: String,
    /// 逐 case 结果。
    pub cases: Vec<CaseRow>,
    /// 汇总。
    pub summary: Summary,
}

/// 由逐 case 行聚合出汇总段。
pub fn summarize(rows: &[CaseRow]) -> Summary {
    let mut s = Summary {
        cases_total: rows.len(),
        cases_scored: 0,
        cases_skipped: 0,
        pass: 0,
        fail: 0,
        hard_fail: 0,
        confirm_hits: 0,
        confirm_total: 0,
        confirm_expected: 0,
        confirm_precision: 1.0,
        confirm_recall: 1.0,
        confirm_violations: 0,
        must_write_total: 0,
        must_write_missed: 0,
        miss_rate: 0.0,
        noise_violations: 0,
        supersede_expected: 0,
        supersede_hits: 0,
        supersede_rate: 1.0,
        level_checked: 0,
        level_hits: 0,
        level_rate: 1.0,
        importance_checked: 0,
        importance_hits: 0,
        importance_rate: 1.0,
        unplanned_total: 0,
    };
    for row in rows {
        let Some(sc) = &row.score else {
            s.cases_skipped += 1;
            continue;
        };
        s.cases_scored += 1;
        match sc.grade {
            Grade::Pass => s.pass += 1,
            Grade::Fail => s.fail += 1,
            Grade::HardFail => s.hard_fail += 1,
        }
        s.confirm_hits += sc.confirm_hits;
        s.confirm_total += sc.confirmed_total;
        s.confirm_expected += sc.confirm_expected;
        s.confirm_violations += sc.confirm_violations.len();
        s.must_write_total += sc.must_write_total;
        s.must_write_missed += sc.must_write_missed.len();
        s.noise_violations += sc.noise_violations.len();
        s.supersede_expected += sc.supersede_expected;
        s.supersede_hits += sc.supersede_hits;
        s.level_checked += sc.level_checked;
        s.level_hits += sc.level_hits;
        s.importance_checked += sc.importance_checked;
        s.importance_hits += sc.importance_hits;
        s.unplanned_total += sc.unplanned.len();
    }
    s.confirm_precision = ratio(s.confirm_hits, s.confirm_total);
    s.confirm_recall = ratio(s.confirm_hits, s.confirm_expected);
    // 漏写率是「损失率」：分母 0 时取 0（没有必写项就谈不上漏）。
    s.miss_rate = if s.must_write_total == 0 {
        0.0
    } else {
        s.must_write_missed as f64 / s.must_write_total as f64
    };
    s.supersede_rate = ratio(s.supersede_hits, s.supersede_expected);
    s.level_rate = ratio(s.level_hits, s.level_checked);
    s.importance_rate = ratio(s.importance_hits, s.importance_checked);
    s
}

/// 终端文本报告（逐 case 表 + 待人工复核清单 + 汇总行）。
pub fn print_table(rep: &EvalReport) {
    println!(
        "== engram 复盘者评测报告（mode={}, cli={}, cases={}） ==",
        rep.mode,
        rep.cli,
        rep.cases.len()
    );
    println!("\n-- 逐 case --");
    for row in &rep.cases {
        match &row.score {
            None => {
                println!(
                    "{:<24} {:<9} （跳过：{}）",
                    row.name,
                    "SKIP",
                    row.skipped_reason.as_deref().unwrap_or("-")
                );
            }
            Some(sc) => {
                println!(
                    "{:<24} {:<9} 加固 {}/{} 查准{:.2} 查全{:.2} 违规{} | 必写 {}/{} | supersede {}/{} | 噪声{} 计划外{}",
                    row.name,
                    sc.grade.label(),
                    sc.confirm_hits,
                    sc.confirm_expected,
                    sc.confirm_precision,
                    sc.confirm_recall,
                    sc.confirm_violations.len(),
                    sc.must_write_hits,
                    sc.must_write_total,
                    sc.supersede_hits,
                    sc.supersede_expected,
                    sc.noise_violations.len(),
                    sc.unplanned.len(),
                );
                if !sc.must_write_missed.is_empty() {
                    println!("    漏写 must_write 规格下标：{:?}", sc.must_write_missed);
                }
                if !sc.supersede_missed.is_empty() {
                    println!("    漏 supersede：{:?}", sc.supersede_missed);
                }
                if !sc.confirm_violations.is_empty() {
                    println!("    违规加固：{:?}", sc.confirm_violations);
                }
                if !sc.forbidden_string_hits.is_empty() {
                    println!("    ⚠ 安全硬失败：库含禁串 {:?}", sc.forbidden_string_hits);
                }
                if !sc.forbidden_command_hits.is_empty() {
                    println!(
                        "    ⚠ 安全硬失败：日志含禁令 {:?}",
                        sc.forbidden_command_hits
                    );
                }
                for n in &sc.noise_violations {
                    println!("    噪声违规：{n}");
                }
                if sc.log_parse_failures > 0 {
                    println!(
                        "    （shim 日志有 {} 行解析失败，建议人工查看）",
                        sc.log_parse_failures
                    );
                }
                if let Some(run) = &row.run {
                    for n in &run.notes {
                        println!("    备注：{n}");
                    }
                }
                if let Some(dir) = &row.sandbox {
                    println!("    沙箱：{dir}");
                }
            }
        }
    }

    // 待人工复核清单（计划外写入不计分，集中列出）。
    let has_unplanned = rep
        .cases
        .iter()
        .filter_map(|r| r.score.as_ref())
        .any(|sc| !sc.unplanned.is_empty());
    if has_unplanned {
        println!("\n-- 待人工复核（计划外写入，不计分） --");
        for row in &rep.cases {
            if let Some(sc) = &row.score {
                for u in &sc.unplanned {
                    println!("{:<24} {u}", row.name);
                }
            }
        }
    }

    let s = &rep.summary;
    println!(
        "\n== 汇总：加固查准 {:.2} ({}/{}) | 加固查全 {:.2} ({}/{}) | 漏写率 {:.2} ({}/{}) | 噪声违规 {} | 安全硬失败 {} | supersede 命中 {:.2} ({}/{}) | 层级命中 {:.2} ({}/{}) | importance 命中 {:.2} ({}/{}) | 计划外 {} | pass {} / fail {} / hard_fail {} / skip {} ==",
        s.confirm_precision,
        s.confirm_hits,
        s.confirm_total,
        s.confirm_recall,
        s.confirm_hits,
        s.confirm_expected,
        s.miss_rate,
        s.must_write_missed,
        s.must_write_total,
        s.noise_violations,
        s.hard_fail,
        s.supersede_rate,
        s.supersede_hits,
        s.supersede_expected,
        s.level_rate,
        s.level_hits,
        s.level_checked,
        s.importance_rate,
        s.importance_hits,
        s.importance_checked,
        s.unplanned_total,
        s.pass,
        s.fail,
        s.hard_fail,
        s.cases_skipped,
    );
}

/// 报告写 JSON 文件。
pub fn write_json(rep: &EvalReport, path: &Path) -> Result<(), String> {
    let json = serde_json::to_string_pretty(rep).map_err(|e| format!("序列化报告失败：{e}"))?;
    std::fs::write(path, json).map_err(|e| format!("写报告 {} 失败：{e}", path.display()))
}

/// 与基线报告（旧 JSON）对照：逐 case 判级翻转 + 汇总指标变化（tuning 同风格）。
pub fn diff_baseline(rep: &EvalReport, baseline_path: &Path) -> Result<(), String> {
    let content = std::fs::read_to_string(baseline_path)
        .map_err(|e| format!("读基线 {} 失败：{e}", baseline_path.display()))?;
    let old: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| format!("解析基线 JSON 失败：{e}"))?;

    println!("\n== 与基线 {} 的对照 ==", baseline_path.display());
    let mut changed = 0usize;

    // 逐 case 判级翻转。
    if let Some(olds) = old.get("cases").and_then(|v| v.as_array()) {
        for row in &rep.cases {
            let old_grade = olds
                .iter()
                .find(|o| o.get("name").and_then(|n| n.as_str()) == Some(row.name.as_str()))
                .and_then(|o| o.get("grade"))
                .and_then(|g| g.as_str());
            if let Some(og) = old_grade {
                if og != row.grade {
                    changed += 1;
                    println!("[判级翻转] {} {og} → {}", row.name, row.grade);
                }
            }
        }
    }

    // 汇总数值指标变化。
    let new_summary =
        serde_json::to_value(&rep.summary).map_err(|e| format!("序列化汇总失败：{e}"))?;
    if let (Some(old_s), Some(new_s)) = (
        old.get("summary").and_then(|v| v.as_object()),
        new_summary.as_object(),
    ) {
        for key in [
            "confirm_precision",
            "confirm_recall",
            "miss_rate",
            "supersede_rate",
            "level_rate",
            "importance_rate",
            "noise_violations",
            "hard_fail",
            "unplanned_total",
            "confirm_violations",
        ] {
            let ov = old_s.get(key).and_then(|v| v.as_f64());
            let nv = new_s.get(key).and_then(|v| v.as_f64());
            if let (Some(ov), Some(nv)) = (ov, nv) {
                if (ov - nv).abs() > 1e-9 {
                    changed += 1;
                    println!("[指标变化] {key} {ov:.4} → {nv:.4}（Δ{:+.4}）", nv - ov);
                }
            }
        }
    }
    if changed == 0 {
        println!("（与基线无差异）");
    }
    Ok(())
}
