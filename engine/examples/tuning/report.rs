//! 量化报告：JSON（`--out`）+ 终端对齐表格 + 基线对照（`--baseline`）。
//!
//! 综合可比分（composite，越小越好）的口径**固定且文档化**：
//! `composite = 1000×护栏失败数 + 10×总regret + Σ(质量题加权 loss)`；
//! 质量题权重（按各题量纲归一）：QUAL-01×1.0、QUAL-02×1.0、QUAL-03×0.01、
//! QUAL-04×1.0、QUAL-05×0.1；QUAL-06/07 只观测、不进加权。

use std::path::Path;

use engram::model::EngramConfig;

use crate::questions::inv::Guardrail;
use crate::questions::pref::PrefOutcome;
use crate::questions::qual::QualOutcome;
use crate::questions::replay::ReplayOutcome;

/// 报告里的一条护栏结果。
#[derive(serde::Serialize)]
pub struct ReportGuardrail {
    /// 题号。
    pub id: String,
    /// 标题。
    pub title: String,
    /// 是否通过。
    pub pass: bool,
    /// 量化细节。
    pub detail: String,
}

/// 报告里的一条偏好结果。
#[derive(serde::Serialize)]
pub struct ReportPref {
    /// 题号。
    pub id: String,
    /// 标题。
    pub title: String,
    /// regret。
    pub loss: f64,
    /// 量化细节。
    pub detail: String,
}

/// 报告里的一条质量结果。
#[derive(serde::Serialize)]
pub struct ReportQual {
    /// 题号。
    pub id: String,
    /// 标题。
    pub title: String,
    /// 原始量测值。
    pub value: f64,
    /// loss（观测题恒 0）。
    pub loss: f64,
    /// 是否只观测。
    pub observed_only: bool,
    /// 量化细节。
    pub detail: String,
}

/// 汇总段。
#[derive(serde::Serialize)]
pub struct Summary {
    /// 护栏通过数。
    pub guardrails_passed: usize,
    /// 护栏总数。
    pub guardrails_total: usize,
    /// 偏好题总 regret。
    pub total_regret: f64,
    /// 质量题加权 loss（见模块头权重口径）。
    pub qual_weighted: f64,
    /// 综合可比分（越小越好）。
    pub composite: f64,
}

/// 一份完整量化报告。
#[derive(serde::Serialize)]
pub struct Report {
    /// 被评估配置的全参数回显。
    pub config: serde_json::Value,
    /// 题库指纹（FNV-1a 64，十六进制）。
    pub fingerprint: String,
    /// 护栏题结果。
    pub guardrails: Vec<ReportGuardrail>,
    /// 偏好题结果。
    pub prefs: Vec<ReportPref>,
    /// 质量题结果。
    pub quals: Vec<ReportQual>,
    /// 可选的真实回放段（只验不调）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replay: Option<ReplayOutcome>,
    /// 汇总。
    pub summary: Summary,
}

/// 质量题进综合分的权重（观测题不在表内）。
fn qual_weight(id: &str) -> f64 {
    match id {
        "QUAL-01" => 1.0,
        "QUAL-02" => 1.0,
        "QUAL-03" => 0.01,
        "QUAL-04" => 1.0,
        "QUAL-05" => 0.1,
        _ => 0.0,
    }
}

/// 把 [`EngramConfig`] 的全部字段回显成 JSON（手工展开，字段全 pub、零依赖变更）。
pub fn config_json(cfg: &EngramConfig) -> serde_json::Value {
    let tier = |t: &engram::model::TierParams| {
        serde_json::json!({
            "capacity": t.capacity,
            "char_budget": t.char_budget,
            "d": t.d,
            "floor": t.floor,
            "load_full": t.load_full,
            "resident": t.resident,
        })
    };
    serde_json::json!({
        "l1": tier(&cfg.l1),
        "l2": tier(&cfg.l2),
        "l3": tier(&cfg.l3),
        "l4_1": tier(&cfg.l4_1),
        "l4_2": tier(&cfg.l4_2),
        "l4_3": tier(&cfg.l4_3),
        "promote_l2": cfg.promote_l2,
        "promote_l1": cfg.promote_l1,
        "demote_l2": cfg.demote_l2,
        "demote_l1": cfg.demote_l1,
        "evict_threshold": cfg.evict_threshold,
        "grace_b0": cfg.grace_b0,
        "grace_tau_days": cfg.grace_tau_days,
        "ttl_days": cfg.ttl_days,
        "tombstone_ttl_days": cfg.tombstone_ttl_days,
        "min_uses": cfg.min_uses,
        "promotion_horizon_days": cfg.promotion_horizon_days,
        "importance_gate": cfg.importance_gate,
    })
}

/// 由三类题目结果组装完整报告（含汇总与综合分）。
pub fn build(
    cfg: &EngramConfig,
    guardrails: Vec<Guardrail>,
    prefs: Vec<PrefOutcome>,
    quals: Vec<QualOutcome>,
    replay: Option<ReplayOutcome>,
) -> Report {
    let passed = guardrails.iter().filter(|g| g.pass).count();
    let total = guardrails.len();
    let total_regret: f64 = prefs.iter().map(|p| p.loss).sum();
    let qual_weighted: f64 = quals
        .iter()
        .filter(|q| !q.observed_only)
        .map(|q| qual_weight(q.id) * q.loss)
        .sum();
    let composite = 1000.0 * (total - passed) as f64 + 10.0 * total_regret + qual_weighted;

    Report {
        config: config_json(cfg),
        fingerprint: format!("{:016x}", crate::questions::fingerprint()),
        guardrails: guardrails
            .into_iter()
            .map(|g| ReportGuardrail {
                id: g.id.to_string(),
                title: g.title.to_string(),
                pass: g.pass,
                detail: g.detail,
            })
            .collect(),
        prefs: prefs
            .into_iter()
            .map(|p| ReportPref {
                id: p.id.to_string(),
                title: p.title.to_string(),
                loss: p.loss,
                detail: p.detail,
            })
            .collect(),
        quals: quals
            .into_iter()
            .map(|q| ReportQual {
                id: q.id.to_string(),
                title: q.title.to_string(),
                value: q.value,
                loss: q.loss,
                observed_only: q.observed_only,
                detail: q.detail,
            })
            .collect(),
        replay,
        summary: Summary {
            guardrails_passed: passed,
            guardrails_total: total,
            total_regret,
            qual_weighted,
            composite,
        },
    }
}

/// 终端对齐表格输出。
pub fn print_table(rep: &Report) {
    println!(
        "== engram 参数整定量化报告（题库指纹 {}） ==",
        rep.fingerprint
    );
    println!("\n-- 护栏题（pass/fail，违反即判废） --");
    for g in &rep.guardrails {
        println!(
            "{:<8} {:<4} {:<26} {}",
            g.id,
            if g.pass { "PASS" } else { "FAIL" },
            g.title,
            g.detail
        );
    }
    println!("\n-- 偏好题（regret，0 为满分） --");
    for p in &rep.prefs {
        println!(
            "{:<8} loss={:<6.2} {:<26} {}",
            p.id, p.loss, p.title, p.detail
        );
    }
    println!("\n-- 质量题（连续指标） --");
    for q in &rep.quals {
        let tag = if q.observed_only { "观测" } else { "打分" };
        println!(
            "{:<8} [{}] value={:<10.3} loss={:<8.4} {:<20} {}",
            q.id, tag, q.value, q.loss, q.title, q.detail
        );
    }
    if let Some(r) = &rep.replay {
        println!("\n-- 回放题（只验不调） --");
        crate::questions::replay::print(r);
    }
    let s = &rep.summary;
    println!(
        "\n== 汇总：护栏 {}/{} 通过 | 总regret={:.2} | 质量加权loss={:.4} | 综合分={:.4}（越小越好） ==",
        s.guardrails_passed, s.guardrails_total, s.total_regret, s.qual_weighted, s.composite
    );
}

/// 报告写 JSON 文件。
pub fn write_json(rep: &Report, path: &Path) -> Result<(), String> {
    let json = serde_json::to_string_pretty(rep).map_err(|e| format!("序列化报告失败：{e}"))?;
    std::fs::write(path, json).map_err(|e| format!("写报告 {} 失败：{e}", path.display()))
}

/// 与基线报告（旧 JSON）逐题对照：pass 翻转与 loss 变化高亮输出。
pub fn diff_baseline(rep: &Report, baseline_path: &Path) -> Result<(), String> {
    let content = std::fs::read_to_string(baseline_path)
        .map_err(|e| format!("读基线 {} 失败：{e}", baseline_path.display()))?;
    let old: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| format!("解析基线 JSON 失败：{e}"))?;

    println!("\n== 与基线 {} 的逐题对照 ==", baseline_path.display());
    let mut changed = 0usize;

    // 护栏 pass 翻转。
    if let Some(olds) = old.get("guardrails").and_then(|v| v.as_array()) {
        for g in &rep.guardrails {
            let old_pass = olds
                .iter()
                .find(|o| o.get("id").and_then(|i| i.as_str()) == Some(g.id.as_str()))
                .and_then(|o| o.get("pass"))
                .and_then(|p| p.as_bool());
            if let Some(op) = old_pass {
                if op != g.pass {
                    changed += 1;
                    println!(
                        "[翻转] {} {} → {}",
                        g.id,
                        if op { "PASS" } else { "FAIL" },
                        if g.pass { "PASS" } else { "FAIL" }
                    );
                }
            }
        }
    }
    // 偏好/质量 loss 变化。
    for (section, news) in [
        (
            "prefs",
            rep.prefs
                .iter()
                .map(|p| (p.id.clone(), p.loss))
                .collect::<Vec<_>>(),
        ),
        (
            "quals",
            rep.quals
                .iter()
                .map(|q| (q.id.clone(), q.loss))
                .collect::<Vec<_>>(),
        ),
    ] {
        if let Some(olds) = old.get(section).and_then(|v| v.as_array()) {
            for (id, new_loss) in news {
                let old_loss = olds
                    .iter()
                    .find(|o| o.get("id").and_then(|i| i.as_str()) == Some(id.as_str()))
                    .and_then(|o| o.get("loss"))
                    .and_then(|l| l.as_f64());
                if let Some(ol) = old_loss {
                    if (ol - new_loss).abs() > 1e-9 {
                        changed += 1;
                        println!(
                            "[loss变化] {id} {ol:.4} → {new_loss:.4}（Δ{:+.4}）",
                            new_loss - ol
                        );
                    }
                }
            }
        }
    }
    // 综合分对照。
    if let Some(old_comp) = old
        .get("summary")
        .and_then(|s| s.get("composite"))
        .and_then(|c| c.as_f64())
    {
        println!(
            "综合分：{old_comp:.4} → {:.4}（Δ{:+.4}）",
            rep.summary.composite,
            rep.summary.composite - old_comp
        );
    }
    if changed == 0 {
        println!("（逐题无差异）");
    }
    Ok(())
}
