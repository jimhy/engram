//! 质量题 QUAL-01..07（连续指标，扫参主力；方案 §9.3 + 批1审查新增观测）。
//!
//! 共用一份混合负载：[T−180d, T] 区间、N=300 条记忆事件流，按原型比例
//! （Evergreen 10% / HotBug 30% / Seasonal 5% / OneShot 45% / Refuted 10%）
//! 生成隐藏节律，每天一次 consolidate（+gc），种子固定、全程确定性。
//!
//! QUAL-06（满容乒乓率）与 QUAL-07（节奏公平性）为**只观测**指标：
//! 报告数值、不进优化目标（`observed_only = true`）。

use std::collections::BTreeMap;

use engram::activation::effective_with;
use engram::model::{EngramConfig, Level, Status};
use engram::render::HOT_INDEX_CHAR_BUDGET;

use super::{DAY, T};
use crate::events::{sort_events, Event, EventKind, Replayer};
use crate::protos::{expected_end_ok, sample_proto, schedule, ProtoInstance};
use crate::rng::XorShift64Star;

/// 混合负载天数。
const LOAD_DAYS: f64 = 180.0;
/// 混合负载记忆条数。
const LOAD_N: usize = 300;

/// 一道质量题的结果。
pub struct QualOutcome {
    /// 题号（如 `QUAL-01`）。
    pub id: &'static str,
    /// 一句话标题。
    pub title: &'static str,
    /// 原始量测值（各题量纲不同，见 detail）。
    pub value: f64,
    /// 归入优化目标的 loss（observed_only 题恒为 0）。
    pub loss: f64,
    /// 是否只观测不进优化目标。
    pub observed_only: bool,
    /// 量化细节。
    pub detail: String,
}

/// 跑全部 7 道质量题（含混合负载主仿真与 QUAL-07 的独立节奏仿真）。
pub fn run_all(cfg: &EngramConfig, seed: u64) -> Vec<QualOutcome> {
    let t0 = T - LOAD_DAYS * DAY;
    let mut rng = XorShift64Star::new(seed);

    // ---- 数据生成：原型实例（oracle 侧持有隐藏节律） ----
    let mut instances: Vec<ProtoInstance> = Vec::new();
    for i in 0..LOAD_N {
        let birth = rng.range_f64(t0, T - 5.0 * DAY);
        let proto = sample_proto(&mut rng);
        instances.push(schedule(&format!("q{i}"), proto, birth, T, &mut rng));
    }

    // ---- 数据输入：铺成事件流（Write/Use/Correct + 每日 Tick） ----
    let mut events: Vec<Event> = Vec::new();
    for inst in &instances {
        events.push(Event {
            t: inst.birth,
            id: inst.id.clone(),
            kind: EventKind::Write {
                level: Level::L3,
                project: None,
                importance: inst.importance,
            },
        });
        for &u in &inst.uses {
            events.push(Event {
                t: u,
                id: inst.id.clone(),
                kind: EventKind::Use,
            });
        }
        for &c in &inst.corrects {
            events.push(Event {
                t: c,
                id: inst.id.clone(),
                kind: EventKind::Correct,
            });
        }
    }
    for k in 0..=(LOAD_DAYS as usize) {
        events.push(Event {
            t: t0 + k as f64 * DAY,
            id: String::new(),
            kind: EventKind::Tick,
        });
    }
    sort_events(&mut events);

    let mut rp = Replayer::new(cfg);
    rp.record_render = true;
    rp.run(&events);

    // ---- 结果输出：量化指标 ----
    vec![
        qual01(&rp),
        qual02(&rp, &instances),
        qual03(&rp, &instances),
        qual04(&rp, &instances),
        qual05(&rp),
        qual06(&rp),
        qual07(cfg),
    ]
}

/// QUAL-01 token 预算恒定性：每日热索引渲染字符数序列的
/// loss = 均值超额（相对预算 P）+ 0.5·(std/P) + 0.5·max(0, 全程正斜率/P)。
///
/// 容量 token 化后为**双层结构**：存量治理（`TierParams::char_budget`，
/// consolidate 把各层常驻条目的累计渲染字符钳在预算内，状态真的改变）+
/// 渲染保险丝（[`HOT_INDEX_CHAR_BUDGET`]=24000，只截显示不动状态）。
/// 本题回放器用**不带预算的全量 render** 量测，量的是**治理后的实际渲染量**
/// ——若改用带预算渲染，保险丝会把膨胀截断藏起来、指标失真。
/// 预期方向：各层预算收紧 → 常驻规模被治理压缩 → 均值/斜率下降。
fn qual01(rp: &Replayer) -> QualOutcome {
    let xs = &rp.stats.render_chars;
    let n = xs.len().max(1) as f64;
    let mean = xs.iter().sum::<f64>() / n;
    let var = xs.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / n;
    let std = var.sqrt();
    // 线性斜率（最小二乘，x 取天序号）：正斜率 = 越用越臃肿。
    let mx = (n - 1.0) / 2.0;
    let mut sxy = 0.0;
    let mut sxx = 0.0;
    for (i, y) in xs.iter().enumerate() {
        let dx = i as f64 - mx;
        sxy += dx * (y - mean);
        sxx += dx * dx;
    }
    let slope_per_day = if sxx > 0.0 { sxy / sxx } else { 0.0 };
    let budget = HOT_INDEX_CHAR_BUDGET as f64;
    let loss = (mean - budget).max(0.0) / budget
        + 0.5 * (std / budget)
        + 0.5 * (slope_per_day * LOAD_DAYS / budget).max(0.0);
    QualOutcome {
        id: "QUAL-01",
        title: "token 预算恒定性",
        value: mean,
        loss,
        observed_only: false,
        detail: format!(
            "均值={mean:.0}字符 std={std:.0} 斜率={slope_per_day:+.1}/天（预算P={budget:.0}）"
        ),
    }
}

/// QUAL-02 效用加权命中率：未来真用前一刻可达度（Hot=1/Cold=0.5/Gone=0），
/// 按 importance（写入值 + 纠正加成）加权。loss = 1 − 加权命中率。
fn qual02(rp: &Replayer, instances: &[ProtoInstance]) -> QualOutcome {
    // importance 权重表：写入值 + 0.2×纠正次数（封顶 1.0，与 Correct 建模一致）。
    let weight: BTreeMap<&str, f64> = instances
        .iter()
        .map(|i| {
            (
                i.id.as_str(),
                (i.importance + 0.2 * i.corrects.len() as f64).min(1.0),
            )
        })
        .collect();
    let mut num = 0.0;
    let mut den = 0.0;
    for (id, score) in &rp.stats.pre_use {
        let w = weight.get(id.as_str()).copied().unwrap_or(0.0);
        num += w * score;
        den += w;
    }
    let hit = if den > 0.0 { num / den } else { 1.0 };
    QualOutcome {
        id: "QUAL-02",
        title: "效用加权命中率",
        value: hit,
        loss: 1.0 - hit,
        observed_only: false,
        detail: format!(
            "加权命中率={hit:.4}（{} 次真用采样）",
            rp.stats.pre_use.len()
        ),
    }
}

/// QUAL-03 噪声驻留时长：OneShotNoise 从最后真用到转 Cold 的平均天数
/// （负载末仍 Active 者按存活期截尾计入）。loss = 平均天数。
fn qual03(rp: &Replayer, instances: &[ProtoInstance]) -> QualOutcome {
    let mut dwell = Vec::new();
    for inst in instances {
        if inst.proto != crate::protos::Proto::OneShotNoise {
            continue;
        }
        let last_use = inst.uses.last().copied().unwrap_or(inst.birth);
        match rp.stats.first_cold.get(&inst.id) {
            Some(&fc) if fc >= last_use => dwell.push((fc - last_use) / DAY),
            Some(_) => {}
            None => {
                // 从未转 Cold：若仍 Active，按截尾（至少已驻留这么久）计入。
                if rp
                    .find(&inst.id)
                    .is_some_and(|m| m.status == Status::Active)
                {
                    dwell.push((T - last_use) / DAY);
                }
            }
        }
    }
    let mean = if dwell.is_empty() {
        0.0
    } else {
        dwell.iter().sum::<f64>() / dwell.len() as f64
    };
    QualOutcome {
        id: "QUAL-03",
        title: "噪声驻留时长",
        value: mean,
        loss: mean,
        observed_only: false,
        detail: format!("平均驻留={mean:.1}天（样本 {} 条噪声）", dwell.len()),
    }
}

/// QUAL-04 层级恰当性：原型「应然可达档」vs 实际档的错配率（oracle 判据见
/// `protos::expected_end_ok`）。loss = 错配率。
fn qual04(rp: &Replayer, instances: &[ProtoInstance]) -> QualOutcome {
    let mut mismatch = 0usize;
    let mut per_proto: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for inst in instances {
        let reach = rp.reach_of(&inst.id);
        let ok = expected_end_ok(inst, reach, T);
        let entry = per_proto.entry(inst.proto.name()).or_insert((0, 0));
        entry.1 += 1;
        if !ok {
            entry.0 += 1;
            mismatch += 1;
        }
    }
    let rate = mismatch as f64 / instances.len().max(1) as f64;
    let breakdown: Vec<String> = per_proto
        .iter()
        .map(|(k, (bad, total))| format!("{k}:{bad}/{total}"))
        .collect();
    QualOutcome {
        id: "QUAL-04",
        title: "层级恰当性",
        value: rate,
        loss: rate,
        observed_only: false,
        detail: format!("错配率={rate:.4}（{}）", breakdown.join(" ")),
    }
}

/// QUAL-05 稳定性（thrash）：每次 consolidate 平均 transition 数。loss = 平均数。
fn qual05(rp: &Replayer) -> QualOutcome {
    let avg = rp.stats.transitions_total as f64 / rp.stats.consolidations.max(1) as f64;
    QualOutcome {
        id: "QUAL-05",
        title: "稳定性 thrash",
        value: avg,
        loss: avg,
        observed_only: false,
        detail: format!(
            "平均 transition={avg:.3}/次（共 {} 次巩固 {} 次变迁）",
            rp.stats.consolidations, rp.stats.transitions_total
        ),
    }
}

/// QUAL-06 满容乒乓率（批1审查者点名观测）：importance≥gate 条目被溢出
/// 挤下后、又被门抬回的次数 / 会话数。只观测，暂不设阈值。
/// 计数口径为「任意后续场抬回」（与「下一场抬回」量级等价，差异与理由
/// 见 `events.rs` `apply_tick` 内注释）。
fn qual06(rp: &Replayer) -> QualOutcome {
    let rate = rp.stats.pingpong as f64 / rp.stats.consolidations.max(1) as f64;
    QualOutcome {
        id: "QUAL-06",
        title: "满容乒乓率（观测）",
        value: rate,
        loss: 0.0,
        observed_only: true,
        detail: format!(
            "乒乓={}次/{}会话（率={rate:.4}）",
            rp.stats.pingpong, rp.stats.consolidations
        ),
    }
}

/// QUAL-07 节奏公平性（观测）：同等「每 2 次机会用 1 次」密度下，
/// 周节奏（每 14 天用 1 次）vs 日节奏（每 2 天用 1 次）记忆在 180 天末的
/// 可达度差距——量化墙钟时基对会话节奏的失配。只观测不判死。
fn qual07(cfg: &EngramConfig) -> QualOutcome {
    let t0 = T - LOAD_DAYS * DAY;
    let cohort = 20usize;
    let mut events: Vec<Event> = Vec::new();
    for i in 0..cohort {
        for (prefix, period_days) in [("wk", 14.0), ("dy", 2.0)] {
            let id = format!("{prefix}{i}");
            events.push(Event {
                t: t0,
                id: id.clone(),
                kind: EventKind::Write {
                    level: Level::L3,
                    project: None,
                    importance: 0.5,
                },
            });
            let mut t = t0 + period_days * DAY;
            while t < T {
                events.push(Event {
                    t,
                    id: id.clone(),
                    kind: EventKind::Use,
                });
                t += period_days * DAY;
            }
        }
    }
    for k in 0..=(LOAD_DAYS as usize) {
        events.push(Event {
            t: t0 + k as f64 * DAY,
            id: String::new(),
            kind: EventKind::Tick,
        });
    }
    sort_events(&mut events);
    let mut rp = Replayer::new(cfg);
    rp.run(&events);

    // 两队列的平均可达度与平均 effective（effective 调 lib 本体）。
    let cohort_stats = |prefix: &str| {
        let mut reach_sum = 0.0;
        let mut eff_sum = 0.0;
        for i in 0..cohort {
            let id = format!("{prefix}{i}");
            reach_sum += rp.reach_of(&id).score();
            eff_sum += rp
                .find(&id)
                .map(|m| effective_with(m, T, cfg))
                .unwrap_or(f64::NEG_INFINITY);
        }
        (reach_sum / cohort as f64, eff_sum / cohort as f64)
    };
    let (wk_reach, wk_eff) = cohort_stats("wk");
    let (dy_reach, dy_eff) = cohort_stats("dy");
    let gap = dy_reach - wk_reach;
    QualOutcome {
        id: "QUAL-07",
        title: "节奏公平性（观测）",
        value: gap,
        loss: 0.0,
        observed_only: true,
        detail: format!(
            "日节奏 reach={dy_reach:.2}/eff={dy_eff:.2} 周节奏 reach={wk_reach:.2}/eff={wk_eff:.2} 差距={gap:+.2}"
        ),
    }
}
