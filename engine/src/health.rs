//! 记忆库健康体检 / 迁移诊断的**纯只读**扫描层。
//!
//! 本模块是 `doctor`（只读体检）与 `migrate`（版本触发的一次性迁移）**共享**的诊断
//! 内核。它把「一批记忆里有哪些健康问题」的判定收成一处纯函数 [`scan`]，
//! **只读不写**——所有入口都取 `&[Memory]` 不可变借用、只往报告结构里 `push`，
//! 物理上无法修改任何 `importance` / `status`，也不做任何删除。
//!
//! # 结构性问题 vs 语义性问题（迁移纪律的边界，务必分清）
//!
//! 迁移把问题分两类处理：
//! - **结构性**（可自动洗）：缺 `schema_version`、未来时间戳、`access_log` 乱序/重复、
//!   层级/容量分布违背新规则——这些由迁移的清洗步与 [`crate::consolidate`] 自动修正。
//! - **语义性**（**只标记进报告、绝不自动改**）：`importance` 偏离层锚、悬空
//!   `superseded_by`、疑似重复 cue、孤儿——这些牵涉「用户/复盘者的判断」，机器无权
//!   篡改。本模块产出的 [`HealthReport`] 中，[`HealthReport::off_anchor_importance`] /
//!   [`HealthReport::dangling_superseded`] / [`HealthReport::duplicate_cues`] 三项即迁移
//!   只上报、不动手的语义性问题。
//!
//! 因此本模块**没有任何 `&mut` 入口**：语义诊断永远不可能「顺手改一下」，这是
//! 「语义只标记」零泄漏进结构路径的编译期保证。

use std::collections::{BTreeMap, HashSet};

use crate::model::{importance_anchor_band, EngramConfig, Level, Memory, Status};
use crate::render::memory_render_cost;

/// 六个层级的规范顺序（通用轨道 L1-L3 在前、项目轨道 L4.1-L4.3 在后）。
///
/// 层级分布统计与「超预算层」诊断按此顺序输出，保证报告稳定可测。
pub const ALL_LEVELS: [Level; 6] = [
    Level::L1,
    Level::L2,
    Level::L3,
    Level::L4_1,
    Level::L4_2,
    Level::L4_3,
];

/// 未来时间戳判定的容忍窗口（秒）：仅 `> now + FUTURE_SKEW_SECS` 才算「未来」。
///
/// 与 `import` / 迁移清洗步的钳制阈值同为 60s（容忍正常机器间时钟偏差），保证
/// 「体检报的未来时间戳数」与「迁移会钳制的条数」判据一致。
pub const FUTURE_SKEW_SECS: f64 = 60.0;

/// `doctor` / `migrate` 用来标识「通用作用域」的作用域标签（区别于具体项目名）。
pub const GENERAL_SCOPE_LABEL: &str = "(general)";

/// 一层「常驻字符累计超出该层字符预算」的报告项（容量治理视角）。
#[derive(Debug, Clone, PartialEq)]
pub struct OverBudgetLayer {
    /// 作用域标签：[`GENERAL_SCOPE_LABEL`] 或项目名。
    pub scope: String,
    /// 层级。
    pub level: Level,
    /// 该层 Active 常驻条目的累计渲染字符（同 [`memory_render_cost`]）。
    pub used: usize,
    /// 该层的字符预算 `char_budget`。
    pub budget: usize,
    /// 该层 Active 条数。
    pub count: usize,
}

/// 一条「`superseded_by` 指向库中不存在 id」的悬空指针报告项。
#[derive(Debug, Clone, PartialEq)]
pub struct DanglingRef {
    /// 持有悬空指针的记忆 id。
    pub id: String,
    /// `superseded_by` 指向的、库中查无此 id 的目标。
    pub target: String,
}

/// 一条「`importance` 偏离所在层的 v5 层锚区间」的报告项（语义性，只标记不改）。
#[derive(Debug, Clone, PartialEq)]
pub struct OffAnchor {
    /// 记忆 id。
    pub id: String,
    /// 所在层级。
    pub level: Level,
    /// 该记忆的 `importance`。
    pub importance: f64,
    /// 所在层锚区间下界（见 [`importance_anchor_band`]）。
    pub band_lo: f64,
    /// 所在层锚区间上界。
    pub band_hi: f64,
}

/// 一组「疑似完全同 cue 的重复」报告项（语义性，只标记不改）。
#[derive(Debug, Clone, PartialEq)]
pub struct DuplicateGroup {
    /// 重复的 cue 文本。
    pub cue: String,
    /// 共享该 cue 的全部记忆 id（≥ 2 个，已按 id 升序）。
    pub ids: Vec<String>,
}

/// 一批记忆的健康体检结果。字段即 `doctor` 报告的各诊断维度。
///
/// 语义性子集（迁移只标记不改）：[`Self::off_anchor_importance`] /
/// [`Self::dangling_superseded`] / [`Self::duplicate_cues`]。其余为结构性/统计维度。
#[derive(Debug, Clone, PartialEq)]
pub struct HealthReport {
    /// 成功解析的记忆总数。
    pub total: usize,
    /// 无法反序列化的坏行数（本扫描只见已解析记忆，此值由调用方用
    /// `raw_len - total` 填入；[`scan`] 缺省置 0）。
    pub bad_rows: usize,
    /// 各层级记忆条数（按 [`ALL_LEVELS`] 顺序，含全部状态）。
    pub level_counts: [(Level, usize); 6],
    /// `schema_version` 值 → 条数分布。
    pub schema_versions: BTreeMap<u32, usize>,
    /// 含未来时间戳（`created_at` 或某 `access_log` 项 `> now + FUTURE_SKEW_SECS`）的
    /// 记忆 id 列表（已按 id 升序）。
    pub future_timestamps: Vec<String>,
    /// 超字符预算的层（Active 常驻累计 > `char_budget`）。
    pub over_budget_layers: Vec<OverBudgetLayer>,
    /// 悬空 `superseded_by`（指向库中不存在 id）。
    pub dangling_superseded: Vec<DanglingRef>,
    /// `importance` 偏离所在层锚区间的 Active 记忆（语义性，只标记）。
    pub off_anchor_importance: Vec<OffAnchor>,
    /// 疑似同 cue 重复的 Active 记忆分组（语义性，只标记）。
    pub duplicate_cues: Vec<DuplicateGroup>,
    /// 孤儿：状态为 `Superseded` 却无 `superseded_by` 后继指针的记忆 id
    /// （已按 id 升序）——「被取代却没记录被谁取代」的不一致。
    pub orphans: Vec<String>,
}

impl HealthReport {
    /// 语义性问题是否存在（三类：层锚偏离 / 悬空指针 / 疑似重复）。
    ///
    /// 迁移据此决定报告里是否要提示「以下为语义性问题，仅标记、未自动修改」。
    pub fn has_semantic_issues(&self) -> bool {
        !self.off_anchor_importance.is_empty()
            || !self.dangling_superseded.is_empty()
            || !self.duplicate_cues.is_empty()
    }
}

/// 取某层级在 [`ALL_LEVELS`] 中的下标（0..6），用于分组时的稳定排序键。
fn level_index(level: Level) -> usize {
    match level {
        Level::L1 => 0,
        Level::L2 => 1,
        Level::L3 => 2,
        Level::L4_1 => 3,
        Level::L4_2 => 4,
        Level::L4_3 => 5,
    }
}

/// 判定一条记忆是否含未来时间戳（`created_at` 或任一 `access_log` 超过容忍窗口）。
///
/// 阈值同 [`FUTURE_SKEW_SECS`]。供 [`scan`] 与调用方（迁移清洗前预判）共用。
pub fn has_future_timestamp(m: &Memory, now: f64) -> bool {
    let limit = now + FUTURE_SKEW_SECS;
    m.created_at > limit || m.access_log.iter().any(|&t| t > limit)
}

/// 对一批记忆做**只读**健康体检，产出 [`HealthReport`]。
///
/// 纯函数、不做任何 IO、不修改入参。`cfg` 提供各层字符预算/加载详略（超预算层诊断
/// 用），`now` 用于未来时间戳判定。作用域按 `Memory.project` 分组（`None` = 通用、
/// `Some(name)` = 该项目）。
///
/// 各语义性维度只扫 `Active` 记忆（层锚偏离 / 疑似重复 / 超预算层——只有活跃常驻的
/// 记忆其层级与预算才有治理意义）；结构性/一致性维度（未来时间戳、悬空指针、孤儿、
/// 分布统计）覆盖全部状态。
///
/// # 参数
/// - `mems`：待体检的记忆集合（合并集）。
/// - `cfg`：层级参数（字符预算 / 加载详略）。
/// - `now`：当前时间（unix 秒），未来时间戳判定用。
pub fn scan(mems: &[Memory], cfg: &EngramConfig, now: f64) -> HealthReport {
    // ---- 层级分布 ----
    let mut level_counts: [(Level, usize); 6] = [
        (Level::L1, 0),
        (Level::L2, 0),
        (Level::L3, 0),
        (Level::L4_1, 0),
        (Level::L4_2, 0),
        (Level::L4_3, 0),
    ];
    for m in mems {
        level_counts[level_index(m.level)].1 += 1;
    }

    // ---- schema_version 分布 ----
    let mut schema_versions: BTreeMap<u32, usize> = BTreeMap::new();
    for m in mems {
        *schema_versions.entry(m.schema_version).or_insert(0) += 1;
    }

    // ---- 未来时间戳（全状态）----
    let mut future_timestamps: Vec<String> = mems
        .iter()
        .filter(|m| has_future_timestamp(m, now))
        .map(|m| m.id.clone())
        .collect();
    future_timestamps.sort();

    // ---- 超字符预算的层（Active，按 (scope, level) 分组）----
    // 键：(作用域标签, 层级下标)；值：(累计渲染字符, 条数)。BTreeMap 保证输出稳定。
    let mut budget_acc: BTreeMap<(String, usize), (usize, usize)> = BTreeMap::new();
    for m in mems {
        if m.status != Status::Active {
            continue;
        }
        let scope = m
            .project
            .clone()
            .unwrap_or_else(|| GENERAL_SCOPE_LABEL.to_string());
        let tier = cfg.tier(m.level);
        let cost = memory_render_cost(m, tier.load_full);
        let e = budget_acc
            .entry((scope, level_index(m.level)))
            .or_insert((0, 0));
        e.0 += cost;
        e.1 += 1;
    }
    let mut over_budget_layers: Vec<OverBudgetLayer> = Vec::new();
    for ((scope, lvl_idx), (used, count)) in budget_acc {
        let level = ALL_LEVELS[lvl_idx];
        let budget = cfg.tier(level).char_budget;
        if budget > 0 && used > budget {
            over_budget_layers.push(OverBudgetLayer {
                scope,
                level,
                used,
                budget,
                count,
            });
        }
    }

    // ---- 悬空 superseded_by（全状态）----
    let id_set: HashSet<&str> = mems.iter().map(|m| m.id.as_str()).collect();
    let mut dangling_superseded: Vec<DanglingRef> = Vec::new();
    for m in mems {
        if let Some(target) = &m.superseded_by {
            if !id_set.contains(target.as_str()) {
                dangling_superseded.push(DanglingRef {
                    id: m.id.clone(),
                    target: target.clone(),
                });
            }
        }
    }
    dangling_superseded.sort_by(|a, b| a.id.cmp(&b.id));

    // ---- importance 偏离层锚（Active）----
    let mut off_anchor_importance: Vec<OffAnchor> = Vec::new();
    for m in mems {
        if m.status != Status::Active {
            continue;
        }
        let (lo, hi) = importance_anchor_band(m.level);
        if m.importance < lo || m.importance > hi {
            off_anchor_importance.push(OffAnchor {
                id: m.id.clone(),
                level: m.level,
                importance: m.importance,
                band_lo: lo,
                band_hi: hi,
            });
        }
    }
    off_anchor_importance.sort_by(|a, b| a.id.cmp(&b.id));

    // ---- 疑似同 cue 重复（Active）----
    let mut by_cue: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for m in mems {
        if m.status != Status::Active {
            continue;
        }
        by_cue.entry(m.cue.clone()).or_default().push(m.id.clone());
    }
    let mut duplicate_cues: Vec<DuplicateGroup> = Vec::new();
    for (cue, mut ids) in by_cue {
        if ids.len() >= 2 {
            ids.sort();
            duplicate_cues.push(DuplicateGroup { cue, ids });
        }
    }

    // ---- 孤儿：Superseded 但无 superseded_by 后继（全状态里挑 Superseded）----
    let mut orphans: Vec<String> = mems
        .iter()
        .filter(|m| m.status == Status::Superseded && m.superseded_by.is_none())
        .map(|m| m.id.clone())
        .collect();
    orphans.sort();

    HealthReport {
        total: mems.len(),
        bad_rows: 0,
        level_counts,
        schema_versions,
        future_timestamps,
        over_budget_layers,
        dangling_superseded,
        off_anchor_importance,
        duplicate_cues,
        orphans,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Pointer, MEMORY_SCHEMA_VERSION};

    const DAY: f64 = 86400.0;

    fn make(id: &str, level: Level, status: Status, importance: f64) -> Memory {
        Memory {
            id: id.to_string(),
            cue: format!("cue-{id}"),
            pointer: Pointer {
                kind: "none".to_string(),
                reference: None,
                detail: None,
            },
            level,
            project: None,
            importance,
            pinned: false,
            access_log: vec![1_000_000_000.0],
            status,
            superseded_by: None,
            created_at: 1_000_000_000.0,
            tags: vec![],
            schema_version: MEMORY_SCHEMA_VERSION,
        }
    }

    // 1. 层级分布与 schema_version 分布统计正确。
    #[test]
    fn counts_levels_and_schema_versions() {
        let now = 1_000_000_000.0;
        let mut a = make("a", Level::L1, Status::Active, 0.9);
        a.schema_version = 0;
        let b = make("b", Level::L1, Status::Active, 0.9);
        let c = make("c", Level::L3, Status::Active, 0.1);
        let rep = scan(&[a, b, c], &EngramConfig::default(), now);
        assert_eq!(rep.total, 3);
        assert_eq!(rep.level_counts[0], (Level::L1, 2), "L1 应 2 条");
        assert_eq!(rep.level_counts[2], (Level::L3, 1), "L3 应 1 条");
        assert_eq!(rep.schema_versions.get(&0), Some(&1));
        assert_eq!(rep.schema_versions.get(&1), Some(&2));
    }

    // 2. importance 偏离层锚只扫 Active，且不改任何值（只读）。
    #[test]
    fn off_anchor_flags_active_only() {
        let now = 1_000_000_000.0;
        // L1 importance 0.1 远低于层锚 [0.85,1.0] → 报。
        let bad = make("bad", Level::L1, Status::Active, 0.1);
        // 同样偏离但状态为 Cold → 不报（只扫 Active）。
        let cold = make("cold", Level::L1, Status::Cold, 0.1);
        // L1 importance 0.9 在层锚内 → 不报。
        let ok = make("ok", Level::L1, Status::Active, 0.9);
        let rep = scan(&[bad, cold, ok], &EngramConfig::default(), now);
        assert_eq!(rep.off_anchor_importance.len(), 1);
        assert_eq!(rep.off_anchor_importance[0].id, "bad");
        assert!(rep.has_semantic_issues());
    }

    // 3. 悬空 superseded_by（指向不存在 id）被识别，指向存在 id 的不报。
    #[test]
    fn detects_dangling_superseded() {
        let now = 1_000_000_000.0;
        let mut a = make("a", Level::L3, Status::Superseded, 0.1);
        a.superseded_by = Some("ghost".to_string());
        let mut b = make("b", Level::L3, Status::Superseded, 0.1);
        b.superseded_by = Some("c".to_string()); // c 存在
        let c = make("c", Level::L3, Status::Active, 0.1);
        let rep = scan(&[a, b, c], &EngramConfig::default(), now);
        assert_eq!(rep.dangling_superseded.len(), 1);
        assert_eq!(rep.dangling_superseded[0].id, "a");
        assert_eq!(rep.dangling_superseded[0].target, "ghost");
    }

    // 4. 孤儿：Superseded 无 superseded_by 后继被识别。
    #[test]
    fn detects_orphan_superseded_without_successor() {
        let now = 1_000_000_000.0;
        let orphan = make("orphan", Level::L3, Status::Superseded, 0.1);
        let mut linked = make("linked", Level::L3, Status::Superseded, 0.1);
        linked.superseded_by = Some("x".to_string());
        let rep = scan(&[orphan, linked], &EngramConfig::default(), now);
        assert_eq!(rep.orphans, vec!["orphan"]);
    }

    // 5. 疑似同 cue 重复分组（Active，≥2 条）。
    #[test]
    fn groups_duplicate_cues() {
        let now = 1_000_000_000.0;
        let mut a = make("a", Level::L3, Status::Active, 0.1);
        let mut b = make("b", Level::L3, Status::Active, 0.1);
        a.cue = "同一句".to_string();
        b.cue = "同一句".to_string();
        let c = make("c", Level::L3, Status::Active, 0.1); // cue 唯一
        let rep = scan(&[a, b, c], &EngramConfig::default(), now);
        assert_eq!(rep.duplicate_cues.len(), 1);
        assert_eq!(rep.duplicate_cues[0].cue, "同一句");
        assert_eq!(rep.duplicate_cues[0].ids, vec!["a", "b"]);
    }

    // 6. 未来时间戳（created_at 或 access_log 超窗口）被识别。
    #[test]
    fn detects_future_timestamps() {
        let now = 1_000_000_000.0;
        let mut fut = make("fut", Level::L3, Status::Active, 0.1);
        fut.access_log = vec![now + 10.0 * DAY];
        let ok = make("ok", Level::L3, Status::Active, 0.1);
        let rep = scan(&[fut, ok], &EngramConfig::default(), now);
        assert_eq!(rep.future_timestamps, vec!["fut"]);
    }

    // 7. 超字符预算的层：单条超长 cue 的 L2 累计 > char_budget → 报。
    #[test]
    fn detects_over_budget_layer() {
        let now = 1_000_000_000.0;
        let mut big = make("big", Level::L2, Status::Active, 0.6);
        big.cue = "字".repeat(8000); // 远超 L2 char_budget=6000
        let rep = scan(&[big], &EngramConfig::default(), now);
        assert_eq!(rep.over_budget_layers.len(), 1);
        assert_eq!(rep.over_budget_layers[0].level, Level::L2);
        assert_eq!(rep.over_budget_layers[0].scope, GENERAL_SCOPE_LABEL);
        assert!(rep.over_budget_layers[0].used > rep.over_budget_layers[0].budget);
    }

    // 8. 干净库无任何问题、无语义问题。
    #[test]
    fn clean_library_reports_nothing() {
        let now = 1_000_000_000.0;
        let a = make("a", Level::L1, Status::Active, 0.9);
        let b = make("b", Level::L3, Status::Active, 0.2);
        let rep = scan(&[a, b], &EngramConfig::default(), now);
        assert!(!rep.has_semantic_issues());
        assert!(rep.future_timestamps.is_empty());
        assert!(rep.over_budget_layers.is_empty());
        assert!(rep.dangling_superseded.is_empty());
        assert!(rep.orphans.is_empty());
        assert_eq!(rep.bad_rows, 0);
    }
}
