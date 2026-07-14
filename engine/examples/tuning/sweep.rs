//! 扫参与随机搜索：run（全量评估）/ sweep（单参数曲线）/ search（随机采样
//! + 全护栏过滤 + top-10 + 平坦度，铁律 5）。

use engram::model::EngramConfig;

use crate::questions::{inv, pref, qual};
use crate::report::{self, Report};
use crate::rng::XorShift64Star;

/// 对一个配置全量评估（全部护栏 + 偏好 + 质量题，数量以题库为准），组装成报告。
pub fn evaluate(
    cfg: &EngramConfig,
    seed: u64,
    replay: Option<crate::questions::replay::ReplayOutcome>,
) -> Report {
    let guardrails = inv::run_all(cfg);
    let prefs = pref::run_all(cfg);
    let quals = qual::run_all(cfg, seed);
    report::build(cfg, guardrails, prefs, quals, replay)
}

/// 扫参白名单：`--param 名=lo:hi:步数` 允许的参数名 → EngramConfig 字段。
///
/// 容量、字符预算与 min_uses 为整数字段，传入值四舍五入；
/// `*_char_budget` 传 0 表示关闭该层字符预算（仅条数约束，INV-20 会翻红）。
pub fn apply_param(cfg: &mut EngramConfig, name: &str, v: f64) -> Result<(), String> {
    match name {
        "promotion_horizon_days" => cfg.promotion_horizon_days = v,
        "importance_gate" => cfg.importance_gate = v,
        "grace_b0" => cfg.grace_b0 = v,
        "grace_tau_days" => cfg.grace_tau_days = v,
        "ttl_days" => cfg.ttl_days = v,
        "tombstone_ttl_days" => cfg.tombstone_ttl_days = v,
        "min_uses" => cfg.min_uses = v.round().max(0.0) as usize,
        "promote_l2" => cfg.promote_l2 = v,
        "promote_l1" => cfg.promote_l1 = v,
        "demote_l2" => cfg.demote_l2 = v,
        "demote_l1" => cfg.demote_l1 = v,
        "evict_threshold" => cfg.evict_threshold = v,
        "l1_capacity" => cfg.l1.capacity = v.round().max(1.0) as usize,
        "l2_capacity" => cfg.l2.capacity = v.round().max(1.0) as usize,
        "l3_capacity" => cfg.l3.capacity = v.round().max(1.0) as usize,
        "l4_1_capacity" => cfg.l4_1.capacity = v.round().max(1.0) as usize,
        "l4_2_capacity" => cfg.l4_2.capacity = v.round().max(1.0) as usize,
        "l4_3_capacity" => cfg.l4_3.capacity = v.round().max(1.0) as usize,
        "l1_char_budget" => cfg.l1.char_budget = v.round().max(0.0) as usize,
        "l2_char_budget" => cfg.l2.char_budget = v.round().max(0.0) as usize,
        "l3_char_budget" => cfg.l3.char_budget = v.round().max(0.0) as usize,
        "l4_1_char_budget" => cfg.l4_1.char_budget = v.round().max(0.0) as usize,
        "l4_2_char_budget" => cfg.l4_2.char_budget = v.round().max(0.0) as usize,
        "l4_3_char_budget" => cfg.l4_3.char_budget = v.round().max(0.0) as usize,
        "l1_d" => cfg.l1.d = v,
        "l2_d" => cfg.l2.d = v,
        "l3_d" => cfg.l3.d = v,
        "l4_1_d" => cfg.l4_1.d = v,
        "l4_2_d" => cfg.l4_2.d = v,
        "l4_3_d" => cfg.l4_3.d = v,
        "l1_floor" => cfg.l1.floor = v,
        "l2_floor" => cfg.l2.floor = v,
        "l3_floor" => cfg.l3.floor = v,
        "l4_1_floor" => cfg.l4_1.floor = v,
        "l4_2_floor" => cfg.l4_2.floor = v,
        "l4_3_floor" => cfg.l4_3.floor = v,
        other => return Err(format!("参数 {other} 不在扫参白名单内")),
    }
    Ok(())
}

/// 解析 `名=lo:hi:步数`。
fn parse_sweep_spec(spec: &str) -> Result<(String, f64, f64, usize), String> {
    let (name, rest) = spec
        .split_once('=')
        .ok_or_else(|| format!("--param {spec} 格式应为 名=lo:hi:步数"))?;
    let parts: Vec<&str> = rest.split(':').collect();
    if parts.len() != 3 {
        return Err(format!("--param {spec} 格式应为 名=lo:hi:步数"));
    }
    let lo: f64 = parts[0]
        .parse()
        .map_err(|e| format!("lo={} 解析失败：{e}", parts[0]))?;
    let hi: f64 = parts[1]
        .parse()
        .map_err(|e| format!("hi={} 解析失败：{e}", parts[1]))?;
    let steps: usize = parts[2]
        .parse()
        .map_err(|e| format!("步数={} 解析失败：{e}", parts[2]))?;
    if steps < 2 {
        return Err("步数至少为 2".to_string());
    }
    Ok((name.to_string(), lo, hi, steps))
}

/// 单点扫描结果行。
#[derive(serde::Serialize)]
struct SweepRow {
    /// 本点参数值。
    value: f64,
    /// 护栏通过数。
    guardrails_passed: usize,
    /// 护栏总数。
    guardrails_total: usize,
    /// 总 regret。
    total_regret: f64,
    /// 质量加权 loss。
    qual_weighted: f64,
    /// 综合分。
    composite: f64,
    /// 各质量题 loss（观测题记 value）。
    quals: serde_json::Value,
}

/// `sweep` 子命令：白名单单参数扫描，输出护栏通过率 + loss 曲线（JSON + CSV）。
pub fn cmd_sweep(spec: &str, seed: u64, out: Option<&str>) -> Result<(), String> {
    let (name, lo, hi, steps) = parse_sweep_spec(spec)?;
    // 先验证参数名合法。
    apply_param(&mut EngramConfig::default(), &name, lo)?;

    let mut rows: Vec<SweepRow> = Vec::new();
    let mut csv = String::from(
        "value,guardrails_passed,guardrails_total,total_regret,qual_weighted,composite\n",
    );
    println!("== 扫参 {name}：{lo}..{hi}（{steps} 点，seed={seed}） ==");
    println!(
        "{:<12} {:<10} {:<12} {:<12} {:<12}",
        "值", "护栏", "总regret", "质量加权", "综合分"
    );
    for i in 0..steps {
        let v = lo + (hi - lo) * i as f64 / (steps - 1) as f64;
        let mut cfg = EngramConfig::default();
        apply_param(&mut cfg, &name, v)?;
        let rep = evaluate(&cfg, seed, None);
        let s = &rep.summary;
        let pass_str = format!("{}/{}", s.guardrails_passed, s.guardrails_total);
        println!(
            "{:<12.4} {:<10} {:<12.2} {:<12.4} {:<12.4}",
            v, pass_str, s.total_regret, s.qual_weighted, s.composite
        );
        csv.push_str(&format!(
            "{v},{},{},{},{},{}\n",
            s.guardrails_passed, s.guardrails_total, s.total_regret, s.qual_weighted, s.composite
        ));
        let quals: serde_json::Value = serde_json::Value::Object(
            rep.quals
                .iter()
                .map(|q| {
                    (
                        q.id.clone(),
                        serde_json::json!(if q.observed_only { q.value } else { q.loss }),
                    )
                })
                .collect(),
        );
        rows.push(SweepRow {
            value: v,
            guardrails_passed: s.guardrails_passed,
            guardrails_total: s.guardrails_total,
            total_regret: s.total_regret,
            qual_weighted: s.qual_weighted,
            composite: s.composite,
            quals,
        });
    }

    match out {
        Some(path) => {
            let json = serde_json::to_string_pretty(&serde_json::json!({
                "param": name, "seed": seed, "rows": rows,
            }))
            .map_err(|e| format!("序列化扫参结果失败：{e}"))?;
            std::fs::write(path, json).map_err(|e| format!("写 {path} 失败：{e}"))?;
            let csv_path = format!("{path}.csv");
            std::fs::write(&csv_path, csv).map_err(|e| format!("写 {csv_path} 失败：{e}"))?;
            println!("曲线已写入 {path} 与 {csv_path}");
        }
        None => {
            println!("\n-- CSV --\n{csv}");
        }
    }
    Ok(())
}

/// 随机搜索的参数边界（「合理边界」：以题库冻结语义为前提的可行域）。
const SEARCH_BOUNDS: &[(&str, f64, f64)] = &[
    ("promotion_horizon_days", 0.5, 2.0),
    ("importance_gate", 0.76, 0.85),
    ("grace_b0", 1.0, 3.0),
    ("grace_tau_days", 2.0, 6.0),
    ("ttl_days", 90.0, 240.0),
    ("min_uses", 1.0, 2.0),
    ("promote_l2", 1.8, 2.7),
    ("promote_l1", 4.5, 6.5),
    ("demote_l2", 0.8, 1.8),
    ("demote_l1", 3.2, 4.2),
    ("evict_threshold", -4.5, -2.0),
    ("l1_capacity", 5.0, 10.0),
    ("l2_capacity", 20.0, 45.0),
    ("l3_capacity", 100.0, 250.0),
    // 各层字符预算（容量 token 化）：默认值 ×0.25 ~ ×3 的合理区间；
    // 不含 0（关预算会翻红 INV-20，0 只留给显式 sweep 做敏感性验证）。
    ("l1_char_budget", 500.0, 6000.0),
    ("l2_char_budget", 1500.0, 18000.0),
    ("l3_char_budget", 1000.0, 12000.0),
    ("l4_1_char_budget", 500.0, 6000.0),
    ("l4_2_char_budget", 1250.0, 15000.0),
    ("l4_3_char_budget", 750.0, 9000.0),
    ("l1_d", 0.05, 0.15),
    ("l2_d", 0.15, 0.35),
    ("l3_d", 0.35, 0.65),
    ("l1_floor", 4.3, 6.0),
    ("l2_floor", 0.5, 1.4),
    ("l3_floor", -12.0, -6.0),
];

/// 采样约束：迟滞死区必须存在、淘汰线在降级线之下、L1 floor 高于其降级线。
fn sample_valid(cfg: &EngramConfig) -> bool {
    cfg.promote_l2 > cfg.demote_l2 + 0.3
        && cfg.promote_l1 > cfg.demote_l1 + 0.3
        && cfg.evict_threshold < cfg.demote_l2
        && cfg.l1.floor > cfg.demote_l1
}

/// 在边界内随机采样一个配置（拒绝采样保证约束，最多重试 50 次）。
fn sample_config(rng: &mut XorShift64Star) -> Option<(EngramConfig, Vec<(String, f64)>)> {
    for _ in 0..50 {
        let mut cfg = EngramConfig::default();
        let mut params = Vec::new();
        for (name, lo, hi) in SEARCH_BOUNDS {
            let v = rng.range_f64(*lo, *hi);
            // 白名单内的名字必然应用成功。
            let _ = apply_param(&mut cfg, name, v);
            params.push((name.to_string(), v));
        }
        if sample_valid(&cfg) {
            return Some((cfg, params));
        }
    }
    None
}

/// 搜索结果一行。
#[derive(serde::Serialize)]
struct SearchRow {
    /// 采样序号。
    index: usize,
    /// 采样到的参数值。
    params: serde_json::Value,
    /// 护栏通过数。
    guardrails_passed: usize,
    /// 总 regret。
    total_regret: f64,
    /// 质量加权 loss。
    qual_weighted: f64,
    /// 综合分。
    composite: f64,
    /// 邻域综合分标准差（平坦度，铁律 5：越小越稳；仅 top-10 计算）。
    #[serde(skip_serializing_if = "Option::is_none")]
    flatness: Option<f64>,
}

/// 一个搜索候选：结果行 + 采样到的参数表（邻域扰动用）。
struct SearchCand {
    /// 结果行（序列化输出）。
    row: SearchRow,
    /// 采样到的 (参数名, 值) 列表。
    params: Vec<(String, f64)>,
}

/// `search` 子命令：随机采样 N 个配置 → 先过全护栏 → 按（总regret, 质量加权）
/// 排序 → top-10 + 邻域平坦度。
pub fn cmd_search(n: usize, seed: u64, out: Option<&str>) -> Result<(), String> {
    let mut rng = XorShift64Star::new(seed);
    let mut cands: Vec<SearchCand> = Vec::new();
    let mut rejected = 0usize;
    // 护栏总数从评估报告派生（题库扩容时无需同步改这里）。
    let mut guardrail_total: Option<usize> = None;

    println!("== 随机搜索：n={n} seed={seed}（先过全护栏，再按 regret/质量加权排序） ==");
    for i in 0..n {
        let Some((cfg, params)) = sample_config(&mut rng) else {
            rejected += 1;
            continue;
        };
        let rep = evaluate(&cfg, seed, None);
        let s = &rep.summary;
        guardrail_total = Some(s.guardrails_total);
        let row = SearchRow {
            index: i,
            params: serde_json::Value::Object(
                params
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::json!(v)))
                    .collect(),
            ),
            guardrails_passed: s.guardrails_passed,
            total_regret: s.total_regret,
            qual_weighted: s.qual_weighted,
            composite: s.composite,
            flatness: None,
        };
        cands.push(SearchCand { row, params });
    }

    let total = cands.len();
    // 无任何有效采样时（cands 为空）取 0，下方过滤自然得到空集。
    let guardrail_total = guardrail_total.unwrap_or(0);
    let mut passing: Vec<&mut SearchCand> = cands
        .iter_mut()
        .filter(|c| c.row.guardrails_passed == guardrail_total)
        .collect();
    println!(
        "采样 {total} 个（拒绝 {rejected} 个），全护栏通过 {} 个",
        passing.len()
    );
    passing.sort_by(|a, b| {
        a.row
            .total_regret
            .partial_cmp(&b.row.total_regret)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                a.row
                    .qual_weighted
                    .partial_cmp(&b.row.qual_weighted)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });

    // top-10 平坦度（铁律 5）：每个 top 配置取 6 个 ±5% 扰动邻居，
    // 邻域综合分标准差过大 = 对题库微扰过敏 = 过拟合信号。
    let top_n = passing.len().min(10);
    println!(
        "\n{:<6} {:<10} {:<12} {:<12} {:<12} {:<10}",
        "排名", "采样号", "总regret", "质量加权", "综合分", "平坦度std"
    );
    for (rank, cand) in passing.iter_mut().take(top_n).enumerate() {
        let mut neighbor_comps = Vec::new();
        for _ in 0..6 {
            let mut ncfg = EngramConfig::default();
            for (name, v) in cand.params.iter() {
                let perturbed = v * (1.0 + rng.range_f64(-0.05, 0.05));
                let _ = apply_param(&mut ncfg, name, perturbed);
            }
            if !sample_valid(&ncfg) {
                continue;
            }
            neighbor_comps.push(evaluate(&ncfg, seed, None).summary.composite);
        }
        let flatness = if neighbor_comps.len() >= 2 {
            let m = neighbor_comps.iter().sum::<f64>() / neighbor_comps.len() as f64;
            let var = neighbor_comps
                .iter()
                .map(|x| (x - m) * (x - m))
                .sum::<f64>()
                / neighbor_comps.len() as f64;
            Some(var.sqrt())
        } else {
            None
        };
        cand.row.flatness = flatness;
        let flat_str = flatness.map_or("-".to_string(), |f| format!("{f:.4}"));
        println!(
            "{:<6} {:<10} {:<12.4} {:<12.4} {:<12.4} {}",
            rank + 1,
            cand.row.index,
            cand.row.total_regret,
            cand.row.qual_weighted,
            cand.row.composite,
            flat_str,
        );
    }
    if top_n == 0 {
        println!("（无配置通过全护栏；请扩大 n 或检查边界）");
    }

    if let Some(path) = out {
        let all: Vec<&SearchRow> = cands.iter().map(|c| &c.row).collect();
        let json = serde_json::to_string_pretty(&serde_json::json!({
            "n": n, "seed": seed, "rejected": rejected, "rows": all,
        }))
        .map_err(|e| format!("序列化搜索结果失败：{e}"))?;
        std::fs::write(path, json).map_err(|e| format!("写 {path} 失败：{e}"))?;
        println!("搜索结果已写入 {path}");
    }
    Ok(())
}
