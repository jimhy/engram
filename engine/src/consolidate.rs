//! 升降级状态机（consolidate）。
//!
//! 本模块是 Engram 的“会话末巩固”核心算法：把一批 active 记忆按
//! **阈值升降级（带迟滞）→ 容量溢出级联下推 → 淘汰** 三步推进，
//! 产出一组状态变迁 [`Transition`]。函数为**纯算法、不做任何 IO**，
//! `now` 由外部注入便于测试。
//!
//! 关键设计（见设计文档 §6、§7）：
//! - **爬升用 [`crate::activation::promotion_score`]**（无 grace/floor，
//!   是“挣来的分”）；**下跌与淘汰用 [`crate::activation::effective`]**
//!   （含 grace/floor）。于是 grace/floor 只防错杀、不助上位。
//! - **升级视界**：升级判定在虚拟时刻 `now + promotion_horizon_days` 采样
//!   `promotion_score`（“明天它还够格吗”），抹平“刚用完/刚写完”的分钟级
//!   近因尖峰；**降级/淘汰仍用真实 `now`**（升降不对称是刻意设计）。
//!   见 [`EngramConfig::promotion_horizon_days`]。
//! - **重要度门**：底层 `importance >= importance_gate` 的条目直接空降中层，
//!   不经频率门；对应地中层同等条目豁免阈值降回底层，避免每场乒乓。
//!   见 [`EngramConfig::importance_gate`]。
//! - **迟滞**：升级阈值明显高于降级阈值，中间留死区，杜绝边界乒乓。
//! - **容量双约束**：溢出下推同时受「条数 ≤ capacity」与「累计渲染字符 ≤
//!   char_budget」约束，先触发者生效（token 治理管根源，渲染预算只是保险丝）。
//!   计量与热索引渲染行同源（[`crate::render::memory_render_cost`]）。
//! - **逐级**：一次最多移动一层。
//! - **只处理 `status == Active` 的记忆**；Cold/Superseded/Tombstone 一律不动。

use crate::activation::{effective_with, promotion_score_with};
use crate::model::{EngramConfig, Level, Memory, Status, SECS_PER_DAY};
use crate::render::memory_render_cost;
// 以下符号重构后仅测试用到（生产路径改走 `cfg` 字段与 `*_with`）；
// 标 cfg(test) 免生产侧 unused 警告。
#[cfg(test)]
use crate::activation::{effective, promotion_score};
#[cfg(test)]
use crate::model::{params, DEMOTE_L2, EVICT_THRESHOLD, IMPORTANCE_GATE, PROMOTE_L2};

/// 一次状态变迁的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionKind {
    /// 阈值升级（爬升一层）。
    Promote,
    /// 阈值降级（下跌一层）。
    Demote,
    /// 淘汰（底层 effective 跌破阈值 → 转 Cold）。
    Evict,
    /// 容量溢出下推（被挤出本层 → 下推一层；从底层挤出则转 Cold）。
    Overflow,
}

/// 一条记忆经 consolidate 产生的状态变迁记录。
#[derive(Debug, Clone, PartialEq)]
pub struct Transition {
    /// 发生变迁的记忆 id。
    pub id: String,
    /// 变迁种类。
    pub kind: TransitionKind,
    /// 变迁前所在层级。
    pub from: Level,
    /// 变迁后的目标层级；若本次变迁不改变层级（如纯转 Cold）则为 `None`。
    pub to_level: Option<Level>,
    /// 变迁后的目标状态；若本次变迁不改变状态（如纯升降级）则为 `None`。
    pub to_status: Option<Status>,
}

/// 返回某层级在其轨道内“上一层”（更高层）的层级。
///
/// 通用轨道 L3→L2→L1，项目轨道 L4.3→L4.2→L4.1。顶层（L1 / L4.1）无更高层，返回 `None`。
fn level_up(level: Level) -> Option<Level> {
    match level {
        Level::L3 => Some(Level::L2),
        Level::L2 => Some(Level::L1),
        Level::L1 => None,
        Level::L4_3 => Some(Level::L4_2),
        Level::L4_2 => Some(Level::L4_1),
        Level::L4_1 => None,
    }
}

/// 返回某层级在其轨道内“下一层”（更低层）的层级。
///
/// 通用轨道 L1→L2→L3，项目轨道 L4.1→L4.2→L4.3。底层（L3 / L4.3）无更低层，返回 `None`。
fn level_down(level: Level) -> Option<Level> {
    match level {
        Level::L1 => Some(Level::L2),
        Level::L2 => Some(Level::L3),
        Level::L3 => None,
        Level::L4_1 => Some(Level::L4_2),
        Level::L4_2 => Some(Level::L4_3),
        Level::L4_3 => None,
    }
}

/// 层级在其轨道内的位置：顶 / 中 / 底。
enum TierRole {
    /// 顶层：L1 / L4.1。
    Top,
    /// 中层：L2 / L4.2。
    Middle,
    /// 底层：L3 / L4.3。
    Bottom,
}

/// 判定某层级在轨道内的位置。
fn tier_role(level: Level) -> TierRole {
    match level {
        Level::L1 | Level::L4_1 => TierRole::Top,
        Level::L2 | Level::L4_2 => TierRole::Middle,
        Level::L3 | Level::L4_3 => TierRole::Bottom,
    }
}

/// 第一步：阈值升降级（逐级、带迟滞）。
///
/// 对每条 active 记忆，依其**当前层级**判定目标层级（一次最多移动一层）：
/// - 底层（L3/L4.3）：未 pinned 且 `importance ≥ importance_gate` → 重要度门
///   直接升一层（显式空降，不经频率门）；否则
///   `promotion_score(虚拟时刻) ≥ PROMOTE_L2` → 升一层；否则不动。
/// - 中层（L2/L4.2）：`promotion_score(虚拟时刻) ≥ PROMOTE_L1` → 升一层；
///   否则若 `effective < DEMOTE_L2` 且 `importance < importance_gate`
///   → 降一层；否则不动。
/// - 顶层（L1/L4.1）：`effective < DEMOTE_L1` → 降一层；否则不动。
///
/// **升级视界**：所有升级判定的 `promotion_score` 在虚拟时刻
/// `now + cfg.promotion_horizon_days`（天）采样——“明天它还够格吗”。
/// 刚发生的访问在 1 天视界下 Δt 从分钟级抬到 ≥1 天、瞬态贡献归零，
/// 升级必须靠跨日的真实使用历史攒出来，而非“刚用完/刚写完”的近因尖峰。
/// **降级判定仍用真实 `now`**（升降不对称是刻意设计）。
///
/// **重要度门与降级的豁免**：中层条目若 `importance ≥ importance_gate`
/// 则不因 `effective` 跌破阈值降回底层——否则重要度门每场把它抬上中层、
/// 阈值又把它打回底层，形成永动乒乓；只要 importance 不被下调，
/// 它就该稳定留在中层。
///
/// 升级判据用 `promotion_score`、降级判据用 `effective`，二者阈值间留死区，构成迟滞。
fn step_threshold(
    memories: &mut [Memory],
    now: f64,
    cfg: &EngramConfig,
    transitions: &mut Vec<Transition>,
) {
    // 升级判定的虚拟采样时刻：now + 视界（天→秒）。
    let promote_at = now + cfg.promotion_horizon_days * SECS_PER_DAY;
    for m in memories.iter_mut() {
        if m.status != Status::Active {
            continue;
        }
        let from = m.level;
        let target = match tier_role(from) {
            TierRole::Bottom => {
                if !m.pinned && m.importance >= cfg.importance_gate {
                    // 重要度门（设计文档 §6 第二扇门）：显式标了高重要度的底层
                    // 条目直接空降中层。不查频率门，也不送顶层（L1 入口仍只有
                    // pin / 手动指定）。
                    level_up(from)
                } else if promotion_score_with(m, promote_at, cfg) >= cfg.promote_l2 {
                    level_up(from)
                } else {
                    None
                }
            }
            TierRole::Middle => {
                if promotion_score_with(m, promote_at, cfg) >= cfg.promote_l1 {
                    level_up(from)
                } else if effective_with(m, now, cfg) < cfg.demote_l2
                    && m.importance < cfg.importance_gate
                {
                    // importance ≥ gate 的条目豁免降回底层（与重要度门配对的
                    // 迟滞规则，防止“门抬上去→阈值打下来”的每场乒乓）。
                    level_down(from)
                } else {
                    None
                }
            }
            TierRole::Top => {
                if effective_with(m, now, cfg) < cfg.demote_l1 {
                    level_down(from)
                } else {
                    None
                }
            }
        };
        if let Some(to) = target {
            // 目标层比当前层更高即为升级，否则为降级。
            let kind = if is_higher(to, from) {
                TransitionKind::Promote
            } else {
                TransitionKind::Demote
            };
            m.level = to;
            transitions.push(Transition {
                id: m.id.clone(),
                kind,
                from,
                to_level: Some(to),
                to_status: None,
            });
        }
    }
}

/// 判断 `a` 是否比 `b` 更高（更靠近 L1 / L4.1）。仅用于区分升/降。
fn is_higher(a: Level, b: Level) -> bool {
    // 同轨道内用一个“深度”表示，深度越小越高。
    fn depth(l: Level) -> u8 {
        match l {
            Level::L1 | Level::L4_1 => 0,
            Level::L2 | Level::L4_2 => 1,
            Level::L3 | Level::L4_3 => 2,
        }
    }
    depth(a) < depth(b)
}

/// 一个作用域分组键：通用记忆用 `None`，项目记忆用其项目名。
///
/// 容量统计以此分组——通用层（L1/L2/L3）跨所有 `project==None` 的记忆，
/// L4 层按各自 `project` 分组，互不影响。
#[derive(PartialEq, Eq, Hash, Clone)]
enum ScopeKey {
    /// 通用作用域（L1/L2/L3）。
    General,
    /// 某项目作用域（L4.x）。
    Project(String),
}

/// 取一条记忆的作用域分组键。
fn scope_key(m: &Memory) -> ScopeKey {
    match &m.project {
        None => ScopeKey::General,
        Some(p) => ScopeKey::Project(p.clone()),
    }
}

/// 第二步：容量溢出级联下推（条数与字符预算**双约束**）。
///
/// 按作用域分组、自上而下（顶→中→底）逐层处理：组内条目按保留优先序排序后
/// 取保留前缀，直到「条数 ≤ `capacity` **且** 累计渲染字符 ≤ `char_budget`」
/// 双满足，**先触发者生效**；其余**下推一层**。下推可能令下一层再超约束，
/// 故自上而下处理使其传播。从底层（L3/L4.3）挤出的记忆转 `status = Cold`。
///
/// **字符预算**（token 治理，见 [`crate::model::TierParams::char_budget`]）：
/// 每条的成本用 [`memory_render_cost`] 计量——与热索引渲染行逐字节同源，
/// 全系统唯一计数。`char_budget = 0` 表示不设预算（仅条数约束）。
/// **每层至少保留 1 条**：保留优先序首条即便单独超预算也保留，防止长 cue
/// 把整层饿死。pinned 恒排最前（effective=INF），自然优先占预算——这是正确
/// 行为：用户显式置顶的记忆理应最后才被字符预算挤出。
///
/// **保留优先序（文档化的平局规则）**：显式比较链
/// 1. `effective` 降序（主判据）；
/// 2. `promotion_score` 降序（长期闲置的条目 effective 会被 tier floor
///    大面积钳平——如 L1 floor=4.5——此时看未被 floor 托底的“挣来的分”）；
/// 3. `importance` 降序（使用痕迹也打平时，显式标注的重要度说话）；
/// 4. `created_at` 升序（**更早创建者优先保留**——资历更老、被留存越久
///    的记忆语义价值更高）。
///
/// 引入显式比较链之前，floor 平局的挤出顺序由 redb 键序（id 纳秒前缀）
/// 隐式决定；现在平局规则是文档化、可测试的确定行为。
///
/// pinned 记忆 effective 与 promotion_score 均为 INF，恒在前列，不会被下推。
fn step_overflow(
    memories: &mut [Memory],
    now: f64,
    cfg: &EngramConfig,
    transitions: &mut Vec<Transition>,
) {
    // 自上而下的层级处理顺序：顶层在前，底层在后，保证下推可级联传播。
    // 两条轨道相互独立，合在一个序列里逐层处理即可。
    const ORDER: [Level; 6] = [
        Level::L1,
        Level::L2,
        Level::L4_1,
        Level::L4_2,
        Level::L3,
        Level::L4_3,
    ];

    for &level in ORDER.iter() {
        let tier = cfg.tier(level);
        let capacity = tier.capacity;
        let char_budget = tier.char_budget;

        // 收集本层、active 记忆的索引，按作用域分组。
        // 用 BTreeMap 保证遍历顺序确定（便于测试稳定）。
        let mut groups: std::collections::BTreeMap<String, Vec<usize>> =
            std::collections::BTreeMap::new();
        for (i, m) in memories.iter().enumerate() {
            if m.status != Status::Active || m.level != level {
                continue;
            }
            // BTreeMap 的键用字符串：General 用空串前缀区分，Project 用 "p:" 前缀。
            let key = match scope_key(m) {
                ScopeKey::General => "g:".to_string(),
                ScopeKey::Project(p) => format!("p:{p}"),
            };
            groups.entry(key).or_default().push(i);
        }

        for (_key, mut idxs) in groups {
            // 双约束早退：条数与累计渲染字符都未超时，本组无事可做（免排序）。
            let total_cost: usize = idxs
                .iter()
                .map(|&i| memory_render_cost(&memories[i], tier.load_full))
                .sum();
            if idxs.len() <= capacity && (char_budget == 0 || total_cost <= char_budget) {
                continue;
            }
            // 按保留优先序排序（见本函数 rustdoc 的显式比较链）；
            // f64 的 NaN 在 partial_cmp 失败时视作相等、落入下一级判据。
            idxs.sort_by(|&a, &b| {
                let ma = &memories[a];
                let mb = &memories[b];
                // 1. effective 降序。
                let ea = effective_with(ma, now, cfg);
                let eb = effective_with(mb, now, cfg);
                eb.partial_cmp(&ea)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| {
                        // 2. promotion_score 降序（不含 grace/floor，破 floor 平局）。
                        let pa = promotion_score_with(ma, now, cfg);
                        let pb = promotion_score_with(mb, now, cfg);
                        pb.partial_cmp(&pa).unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .then_with(|| {
                        // 3. importance 降序。
                        mb.importance
                            .partial_cmp(&ma.importance)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .then_with(|| {
                        // 4. created_at 升序：更早创建者优先保留。
                        ma.created_at
                            .partial_cmp(&mb.created_at)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
            });
            // 取保留前缀：直到「条数 ≤ capacity 且 累计渲染字符 ≤ char_budget」
            // 双满足，先触发者生效。首条无条件保留（每层至少 1 条，防饿死）；
            // pinned 经排序恒在最前，自然优先占预算（见本函数 rustdoc）。
            let mut kept = 0usize;
            let mut cost_acc = 0usize;
            for &i in idxs.iter() {
                if kept >= capacity {
                    break;
                }
                let cost = memory_render_cost(&memories[i], tier.load_full);
                if kept > 0 && char_budget > 0 && cost_acc + cost > char_budget {
                    break;
                }
                cost_acc += cost;
                kept += 1;
            }
            // 前 kept 条保留，其余下推。
            for &i in idxs.iter().skip(kept) {
                let from = memories[i].level;
                match level_down(from) {
                    Some(to) => {
                        // 下推一层。
                        memories[i].level = to;
                        transitions.push(Transition {
                            id: memories[i].id.clone(),
                            kind: TransitionKind::Overflow,
                            from,
                            to_level: Some(to),
                            to_status: None,
                        });
                    }
                    None => {
                        // 底层挤出 → 转 Cold。
                        memories[i].status = Status::Cold;
                        transitions.push(Transition {
                            id: memories[i].id.clone(),
                            kind: TransitionKind::Overflow,
                            from,
                            to_level: None,
                            to_status: Some(Status::Cold),
                        });
                    }
                }
            }
        }
    }
}

/// 第三步：淘汰。
///
/// 任何仍为 Active 的底层（L3/L4.3）记忆，若 `effective < EVICT_THRESHOLD`
/// 则 `status = Cold`。
fn step_evict(
    memories: &mut [Memory],
    now: f64,
    cfg: &EngramConfig,
    transitions: &mut Vec<Transition>,
) {
    for m in memories.iter_mut() {
        if m.status != Status::Active {
            continue;
        }
        if !matches!(tier_role(m.level), TierRole::Bottom) {
            continue;
        }
        if effective_with(m, now, cfg) < cfg.evict_threshold {
            let from = m.level;
            m.status = Status::Cold;
            transitions.push(Transition {
                id: m.id.clone(),
                kind: TransitionKind::Evict,
                from,
                to_level: None,
                to_status: Some(Status::Cold),
            });
        }
    }
}

/// 对一批记忆执行 consolidate 升降级状态机，原地修改并返回所有状态变迁。
///
/// **只处理 `status == Active` 的记忆**；Cold/Superseded/Tombstone 一律不动。
/// 算法分三步顺序执行（见各 `step_*` 私有函数）：
/// 1. 阈值升降级（逐级、带迟滞）；
/// 2. 容量溢出级联下推（条数与字符预算**双约束**，按作用域分组、自上而下）；
/// 3. 淘汰（底层 effective 跌破阈值转 Cold）。
///
/// 本函数纯算法、不做 IO。不强求对同一输入幂等，但**保证终止**
/// （三步均为有界遍历，无循环重试）。
///
/// # 参数
/// - `memories`：待巩固的记忆集合切片（原地修改其 `level` / `status`，
///   不增删元素，故取 `&mut [Memory]`；`&mut Vec<Memory>` 可经解引用直接传入）。
/// - `now`：当前时间（unix 秒）。
///
/// # 返回
/// 本次产生的所有 [`Transition`]，按发生顺序排列（先升降级、后溢出、再淘汰）。
pub fn consolidate(memories: &mut [Memory], now: f64) -> Vec<Transition> {
    consolidate_with(memories, now, &EngramConfig::default())
}

/// 同 [`consolidate`]，但用给定 `cfg` 扫参（整定 harness 入口）。三步均把 `cfg`
/// 透传到升降阈值、容量与淘汰判定。
pub fn consolidate_with(memories: &mut [Memory], now: f64, cfg: &EngramConfig) -> Vec<Transition> {
    let mut transitions = Vec::new();
    step_threshold(memories, now, cfg, &mut transitions);
    step_overflow(memories, now, cfg, &mut transitions);
    step_evict(memories, now, cfg, &mut transitions);
    transitions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Pointer;

    /// 一天的秒数（与 model::SECS_PER_DAY 一致，测试本地常量）。
    const DAY: f64 = 86400.0;

    /// 构造一条测试记忆。
    fn make(
        id: &str,
        level: Level,
        importance: f64,
        created_at: f64,
        access_log: Vec<f64>,
    ) -> Memory {
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
            access_log,
            status: Status::Active,
            superseded_by: None,
            created_at,
            tags: vec![],
            schema_version: crate::model::MEMORY_SCHEMA_VERSION,
        }
    }

    /// 在变迁集合里查找某 id、某种类的变迁。
    fn find<'a>(ts: &'a [Transition], id: &str, kind: TransitionKind) -> Option<&'a Transition> {
        ts.iter().find(|t| t.id == id && t.kind == kind)
    }

    // 1. 降级：L2、低 effective(<1.5) → 降 L3。
    #[test]
    fn demote_l2_to_l3() {
        let now = 1_000_000_000.0;
        // importance 0、单次访问在 100 天前、grace 早退；L2 floor=1.0。
        // effective = max(1.0, 0 + activation + ~0) 应 < 1.5。
        let m = make(
            "a",
            Level::L2,
            0.0,
            now - 100.0 * DAY,
            vec![now - 100.0 * DAY],
        );
        assert!(effective(&m, now) < DEMOTE_L2, "前置：effective 应 < 1.5");
        let mut mems = vec![m];
        let ts = consolidate(&mut mems, now);
        assert_eq!(mems[0].level, Level::L3, "应降到 L3");
        let t = find(&ts, "a", TransitionKind::Demote).expect("应有 Demote 变迁");
        assert_eq!(t.from, Level::L2);
        assert_eq!(t.to_level, Some(Level::L3));
    }

    // 2. 升级：L3、跨日持续真使用攒出 promotion_score（虚拟视界采样）≥ 2.5 → 升 L2。
    //
    // 升级判定在虚拟时刻 now + promotion_horizon_days(=1 天) 采样，故必须用
    // **跨日**的使用历史（此处连续 7 天、每天 3 次真使用）攒分；旧版测试用
    // “2 天内 4 次近期访问 + importance 1.0”在 1 天视界下 promotion≈2.22 < 2.5
    // 已不够格——这是接受的语义收紧（升级需更持续的跨日使用）。
    // importance 取 0.5（< importance_gate=0.8），确保走的是频率门而非重要度门。
    #[test]
    fn promote_l3_to_l2() {
        let now = 1_000_000_000.0;
        // 连续 7 天、每天 3 次真使用：虚拟时刻下 Δt=1..7 天，
        // sum = 3·Σ j^-0.5 ≈ 12.05，activation = ln(12.05) ≈ 2.49，
        // promotion_score ≈ 0.5 + 2.49 ≈ 2.99 ≥ 2.5。
        let mut log = Vec::new();
        for k in 0..7 {
            for _ in 0..3 {
                log.push(now - k as f64 * DAY);
            }
        }
        let m = make("b", Level::L3, 0.5, now - 8.0 * DAY, log);
        assert!(
            promotion_score(&m, now + 1.0 * DAY) >= PROMOTE_L2,
            "前置：虚拟时刻的 promotion_score 应 ≥ 2.5，实得 {}",
            promotion_score(&m, now + 1.0 * DAY)
        );
        let mut mems = vec![m];
        let ts = consolidate(&mut mems, now);
        assert_eq!(mems[0].level, Level::L2, "应升到 L2");
        let t = find(&ts, "b", TransitionKind::Promote).expect("应有 Promote 变迁");
        assert_eq!(t.from, Level::L3);
        assert_eq!(t.to_level, Some(Level::L2));
    }

    // 3. L1 守住：L1、importance 0.3、400 天没访问 → 仍 L1（floor 保护）。
    #[test]
    fn l1_holds_by_floor() {
        let now = 1_000_000_000.0;
        let m = make(
            "c",
            Level::L1,
            0.3,
            now - 400.0 * DAY,
            vec![now - 400.0 * DAY],
        );
        // effective 被 floor=4.5 托住，不低于 DEMOTE_L1=4.0，故不降级。
        assert!(effective(&m, now) >= 4.5);
        let mut mems = vec![m];
        let ts = consolidate(&mut mems, now);
        assert_eq!(mems[0].level, Level::L1, "L1 应被 floor 守住不降级");
        assert!(ts.is_empty(), "不应有任何变迁");
    }

    // 4. 淘汰：L3、effective < -3 → status 变 Cold。
    #[test]
    fn evict_l3_when_below_threshold() {
        let now = 1_000_000_000.0;
        // importance 0、1000 天前单次访问、grace 已退、L3 floor=-10 不截断。
        let m = make(
            "d",
            Level::L3,
            0.0,
            now - 1000.0 * DAY,
            vec![now - 1000.0 * DAY],
        );
        assert!(
            effective(&m, now) < EVICT_THRESHOLD,
            "前置：effective 应 < -3，实得 {}",
            effective(&m, now)
        );
        let mut mems = vec![m];
        let ts = consolidate(&mut mems, now);
        assert_eq!(mems[0].status, Status::Cold, "应转 Cold");
        assert_eq!(mems[0].level, Level::L3, "层级不变");
        let t = find(&ts, "d", TransitionKind::Evict).expect("应有 Evict 变迁");
        assert_eq!(t.to_status, Some(Status::Cold));
    }

    // 5. 容量溢出级联：8 条 active L1（cap 7）→ 恰好 7 条留 L1，
    //    effective 最低的 1 条被下推到 L2，且产生 Overflow transition。
    #[test]
    fn overflow_cascade_l1() {
        let now = 1_000_000_000.0;
        let mut mems = Vec::new();
        // 8 条 L1。为避免阈值降级干扰，给足够高的 importance（effective 高），
        // 但让其中一条 effective 明显最低（importance 较低）以便被挤出。
        // importance 全部 ≥ 4.0 保证 effective ≥ DEMOTE_L1，threshold 步不降级。
        for i in 0..8 {
            // m7 的 importance 最低 → effective 最低 → 被下推。
            let importance = if i == 7 { 4.0 } else { 6.0 + i as f64 };
            mems.push(make(
                &format!("m{i}"),
                Level::L1,
                importance,
                now - 1.0 * DAY,
                vec![now - 0.5 * DAY],
            ));
        }
        let ts = consolidate(&mut mems, now);
        let l1_count = mems.iter().filter(|m| m.level == Level::L1).count();
        let l2_count = mems.iter().filter(|m| m.level == Level::L2).count();
        assert_eq!(l1_count, 7, "应恰好留 7 条在 L1");
        assert_eq!(l2_count, 1, "应有 1 条被下推到 L2");
        // 被下推的应是 m7（effective 最低）。
        let pushed = mems
            .iter()
            .find(|m| m.level == Level::L2)
            .expect("应有下推记忆");
        assert_eq!(pushed.id, "m7", "effective 最低的 m7 应被下推");
        let t = find(&ts, "m7", TransitionKind::Overflow).expect("应有 Overflow 变迁");
        assert_eq!(t.from, Level::L1);
        assert_eq!(t.to_level, Some(Level::L2));
    }

    // 6. grace 防错杀但不助上位 + 升级视界抹平瞬时尖峰：新 L3 记忆
    //    （created_at=now、单次访问=now、importance 低）→ 不被淘汰（grace 抬
    //    effective 保 Active）；也不被升级——升级判定在虚拟时刻
    //    now + promotion_horizon_days(=1 天) 采样 promotion_score，access=now
    //    的访问在视界下 Δt≈1 天、activation≈0，promotion_score≈importance。
    //
    // 历史背景：引入视界前，access=now 时 Δt 被钳到 1 分钟（MIN_DT_DAYS），
    // activation 出现 (1/1440)^-d 的尖峰（L3 下约 3.6）、单次访问即越过升级
    // 阈值，本用例曾被迫把访问挪到 1 天前躲开尖峰。视界机制已把尖峰从升级
    // 判定中消除，此处按用例本意用 access=now 直接验证。
    #[test]
    fn grace_protects_but_does_not_promote() {
        let now = 1_000_000_000.0;
        let m = make("e", Level::L3, 0.1, now, vec![now]);
        // 真实 now 采样确有近因尖峰（≈ 0.1 + 3.6），这正是旧缺陷的来源。
        assert!(
            promotion_score(&m, now) > PROMOTE_L2,
            "前置：真实 now 采样的瞬时尖峰应越过 2.5，实得 {}",
            promotion_score(&m, now)
        );
        // 虚拟视界采样：Δt≈1 天 → activation≈0 → promotion_score≈importance≈0.1。
        assert!(
            promotion_score(&m, now + 1.0 * DAY) < PROMOTE_L2,
            "虚拟时刻的 promotion_score 应 < 2.5，实得 {}",
            promotion_score(&m, now + 1.0 * DAY)
        );
        // effective 含约 2.0 的 grace，远高于淘汰阈值。
        assert!(effective(&m, now) > EVICT_THRESHOLD);
        let mut mems = vec![m];
        let ts = consolidate(&mut mems, now);
        assert_eq!(mems[0].status, Status::Active, "grace 应防止被淘汰");
        assert_eq!(
            mems[0].level,
            Level::L3,
            "不应被升级（grace 不助上位、尖峰被视界抹平）"
        );
        assert!(ts.is_empty(), "不应产生任何变迁");
    }

    // 7. pinned 豁免：pinned 的 L1 记忆无访问 → 不降级、不淘汰；
    //    容量溢出时不被下推。
    #[test]
    fn pinned_exempt_from_demote_and_overflow() {
        let now = 1_000_000_000.0;
        // 先验：单独一条 pinned L1 无访问，不降级不淘汰。
        let mut pinned = make("p", Level::L1, 0.0, now - 9999.0 * DAY, vec![]);
        pinned.pinned = true;
        let mut solo = vec![pinned.clone()];
        let ts1 = consolidate(&mut solo, now);
        assert_eq!(solo[0].level, Level::L1);
        assert_eq!(solo[0].status, Status::Active);
        assert!(ts1.is_empty(), "pinned 单条不应有变迁");

        // 容量溢出场景：8 条 L1，其中 1 条 pinned 且 importance 低。
        // pinned 的 effective=INF 恒在前列，被挤出的应是某条普通记忆，而非 pinned。
        let mut mems = Vec::new();
        mems.push(pinned); // p：pinned、importance 0、无访问。
        for i in 0..7 {
            mems.push(make(
                &format!("n{i}"),
                Level::L1,
                6.0 + i as f64,
                now - 1.0 * DAY,
                vec![now - 0.5 * DAY],
            ));
        }
        let ts2 = consolidate(&mut mems, now);
        // pinned 仍在 L1、仍 Active。
        let p = mems.iter().find(|m| m.id == "p").expect("p 应在");
        assert_eq!(p.level, Level::L1, "pinned 不应被下推");
        assert_eq!(p.status, Status::Active);
        // pinned 不应出现在任何 Overflow 变迁里。
        assert!(
            find(&ts2, "p", TransitionKind::Overflow).is_none(),
            "pinned 不应被溢出下推"
        );
        // 被下推的是 effective 最低的普通记忆（n0，importance 6.0）。
        let pushed: Vec<_> = mems.iter().filter(|m| m.level == Level::L2).collect();
        assert_eq!(pushed.len(), 1, "应恰有 1 条被下推");
        assert_eq!(pushed[0].id, "n0");
    }

    // 8. 迟滞死区：同一条 effective≈2.1（落在死区 (1.5, 2.5) 内）的记忆，
    //    当前在 L3 时不升（虚拟时刻 promotion_score < 2.5），
    //    当前在 L2 时不降（effective ≥ 1.5）。
    //
    // 构造说明：importance 取 0.5（< importance_gate=0.8，不触发重要度门），
    // 死区分数靠 activation 攒——昨天 5 次真使用，真实 now 下 Δt=1 天、
    // (1)^-d=1 与 d 无关，activation = ln(5) ≈ 1.609，effective ≈ 2.109。
    // 升级判定在虚拟时刻 now+1 天采样（Δt=2 天），分数只会更低，仍在死区下方。
    #[test]
    fn hysteresis_dead_zone() {
        let now = 1_000_000_000.0;
        let make_at = |level: Level| {
            // created_at 在 30 天前，grace=2·exp(-10)≈9e-5 已近归零。
            make("z", level, 0.5, now - 30.0 * DAY, vec![now - 1.0 * DAY; 5])
        };

        // 当前 L3：effective 在死区内、虚拟时刻 promotion_score < 2.5 → 不升。
        let mut as_l3 = vec![make_at(Level::L3)];
        assert!(
            (effective(&as_l3[0], now) - 2.109).abs() < 1e-2,
            "L3 effective 应≈2.11（死区内），实得 {}",
            effective(&as_l3[0], now)
        );
        assert!(
            promotion_score(&as_l3[0], now + 1.0 * DAY) < PROMOTE_L2,
            "虚拟时刻 promotion_score 应 < 2.5，实得 {}",
            promotion_score(&as_l3[0], now + 1.0 * DAY)
        );
        let ts3 = consolidate(&mut as_l3, now);
        assert_eq!(as_l3[0].level, Level::L3, "死区内不应升级");
        assert!(ts3.is_empty());

        // 当前 L2：effective≈2.1 ≥ 1.5 → 不降；虚拟时刻 promotion_score < 5.0 → 不升。
        let mut as_l2 = vec![make_at(Level::L2)];
        assert!(
            effective(&as_l2[0], now) >= DEMOTE_L2,
            "L2 effective 应 ≥ 1.5（不触降级），实得 {}",
            effective(&as_l2[0], now)
        );
        let ts2 = consolidate(&mut as_l2, now);
        assert_eq!(as_l2[0].level, Level::L2, "死区内不应降级");
        assert!(ts2.is_empty());
    }

    // 9. 非 Active 不动：Superseded/Cold 记忆经 consolidate 后层级与状态不变。
    #[test]
    fn non_active_untouched() {
        let now = 1_000_000_000.0;
        // 一条本会被淘汰的 L3（effective 极低），但状态是 Cold → 不动。
        let mut cold = make(
            "cold",
            Level::L3,
            0.0,
            now - 1000.0 * DAY,
            vec![now - 1000.0 * DAY],
        );
        cold.status = Status::Cold;
        // 一条本会升级的 L3（promotion_score 高），但状态是 Superseded → 不动。
        let mut sup = make(
            "sup",
            Level::L3,
            5.0,
            now - 1.0 * DAY,
            vec![now - 0.1 * DAY, now - 0.2 * DAY],
        );
        sup.status = Status::Superseded;

        let mut mems = vec![cold, sup];
        let ts = consolidate(&mut mems, now);
        assert_eq!(mems[0].level, Level::L3);
        assert_eq!(mems[0].status, Status::Cold, "Cold 不应被动");
        assert_eq!(mems[1].level, Level::L3);
        assert_eq!(mems[1].status, Status::Superseded, "Superseded 不应被动");
        assert!(ts.is_empty(), "非 Active 记忆不应产生变迁");
    }

    // 10. L4 按项目分组容量：两个不同 project 各自的 L4 层容量独立计算。
    #[test]
    fn l4_capacity_grouped_by_project() {
        let now = 1_000_000_000.0;
        let cap = params(Level::L4_1).capacity; // 10
        let mut mems = Vec::new();
        // 项目 alpha：cap+1 = 11 条 L4.1（超容 1 条）。
        for i in 0..(cap + 1) {
            let mut m = make(
                &format!("a{i}"),
                Level::L4_1,
                10.0 + i as f64,
                now - 1.0 * DAY,
                vec![now - 0.5 * DAY],
            );
            m.project = Some("alpha".to_string());
            mems.push(m);
        }
        // 项目 beta：cap-1 = 9 条 L4.1（不超容）。
        for i in 0..(cap - 1) {
            let mut m = make(
                &format!("b{i}"),
                Level::L4_1,
                10.0 + i as f64,
                now - 1.0 * DAY,
                vec![now - 0.5 * DAY],
            );
            m.project = Some("beta".to_string());
            mems.push(m);
        }

        let _ts = consolidate(&mut mems, now);

        // alpha 的 L4.1 应恰好 cap 条，1 条被下推到 L4.2。
        let alpha_l41 = mems
            .iter()
            .filter(|m| m.project.as_deref() == Some("alpha") && m.level == Level::L4_1)
            .count();
        let alpha_l42 = mems
            .iter()
            .filter(|m| m.project.as_deref() == Some("alpha") && m.level == Level::L4_2)
            .count();
        assert_eq!(alpha_l41, cap, "alpha 应留 cap 条在 L4.1");
        assert_eq!(alpha_l42, 1, "alpha 应有 1 条下推到 L4.2");

        // beta 的 L4.1 应全部保留（未超容），无下推。
        let beta_l41 = mems
            .iter()
            .filter(|m| m.project.as_deref() == Some("beta") && m.level == Level::L4_1)
            .count();
        let beta_l42 = mems
            .iter()
            .filter(|m| m.project.as_deref() == Some("beta") && m.level == Level::L4_2)
            .count();
        assert_eq!(beta_l41, cap - 1, "beta 未超容应全部保留在 L4.1");
        assert_eq!(beta_l42, 0, "beta 不应有下推");
    }

    // 11. 升级视界红线（分钟级）：L3、importance 0.3、单次访问在 now−5 分钟——
    //     真实 now 采样有 ≈3.1 的瞬时近因尖峰（旧语义下必升级），
    //     虚拟视界（1 天）采样下不得升级。
    #[test]
    fn minute_old_access_does_not_promote() {
        let now = 1_000_000_000.0;
        let five_min_ago = now - 5.0 * 60.0;
        let m = make("m5", Level::L3, 0.3, five_min_ago, vec![five_min_ago]);
        // 前置：真实 now 采样的分数确实越阈——证明本用例踩在旧缺陷的红线上，
        // 是升级视界（而非分数本身不够）挡下了它。
        assert!(
            promotion_score(&m, now) >= PROMOTE_L2,
            "前置：真实 now 采样应有越阈尖峰，实得 {}",
            promotion_score(&m, now)
        );
        let mut mems = vec![m];
        let ts = consolidate(&mut mems, now);
        assert_eq!(mems[0].level, Level::L3, "5 分钟前的单次访问不得触发升级");
        assert_eq!(mems[0].status, Status::Active, "grace 应保 Active");
        assert!(ts.is_empty(), "不应产生任何变迁");
    }

    // 12. 升级视界红线（新写条目）：access_log 空、created_at = now−3 分钟——
    //     隐式首次访问=created_at，真实 now 采样同样有尖峰；刚写完的条目
    //     不得在收尾 consolidate 里升级。
    #[test]
    fn fresh_write_does_not_promote() {
        let now = 1_000_000_000.0;
        let m = make("fresh", Level::L3, 0.5, now - 3.0 * 60.0, vec![]);
        assert!(
            promotion_score(&m, now) >= PROMOTE_L2,
            "前置：真实 now 采样应有越阈尖峰，实得 {}",
            promotion_score(&m, now)
        );
        let mut mems = vec![m];
        let ts = consolidate(&mut mems, now);
        assert_eq!(mems[0].level, Level::L3, "刚写完的条目不得触发升级");
        assert_eq!(mems[0].status, Status::Active, "grace 应保 Active");
        assert!(ts.is_empty(), "不应产生任何变迁");
    }

    // 13. 重要度门：底层 importance ≥ gate(0.8) 的条目零访问也直接空降中层
    //     （L3→L2、L4.3→L4.2 同构）；门不送顶层（L1 入口仍只有 pin/手动指定）。
    #[test]
    fn importance_gate_promotes_bottom_to_middle() {
        let now = 1_000_000_000.0;
        let m = make("g1", Level::L3, 0.85, now - 10.0 * DAY, vec![]);
        assert!(m.importance >= IMPORTANCE_GATE, "前置：0.85 应过门");
        let mut m4 = make("g2", Level::L4_3, 0.9, now - 10.0 * DAY, vec![]);
        m4.project = Some("alpha".to_string());
        let mut mems = vec![m, m4];
        let ts = consolidate(&mut mems, now);
        assert_eq!(mems[0].level, Level::L2, "L3 高重要度应空降 L2");
        assert_eq!(mems[1].level, Level::L4_2, "L4.3 高重要度应空降 L4.2");
        let t = find(&ts, "g1", TransitionKind::Promote).expect("g1 应有 Promote 变迁");
        assert_eq!(t.from, Level::L3);
        assert_eq!(t.to_level, Some(Level::L2));
        assert!(
            find(&ts, "g2", TransitionKind::Promote).is_some(),
            "g2 应有 Promote 变迁"
        );
    }

    // 14. 重要度门阈下不动：L3 importance 0.75（< gate）零访问 → 不升级。
    #[test]
    fn importance_below_gate_does_not_promote() {
        let now = 1_000_000_000.0;
        let m = make("ng", Level::L3, 0.75, now - 10.0 * DAY, vec![]);
        assert!(m.importance < IMPORTANCE_GATE, "前置：0.75 应在门下");
        let mut mems = vec![m];
        let ts = consolidate(&mut mems, now);
        assert_eq!(mems[0].level, Level::L3, "importance < gate 不应空降");
        assert!(ts.is_empty(), "不应产生任何变迁");
    }

    // 15. 门与迟滞不乒乓：L2、importance ≥ gate、300 天未用（effective 被 L2
    //     floor 托在 1.0 < DEMOTE_L2=1.5）→ 豁免降级留在 L2——否则重要度门
    //     每场把它抬上 L2、阈值又打回 L3，形成永动乒乓。
    //     对照组 importance < gate 同条件应正常降 L3。
    #[test]
    fn importance_gate_exempts_middle_from_demotion() {
        let now = 1_000_000_000.0;
        let old = now - 300.0 * DAY;
        let hi = make("hi", Level::L2, 0.85, old, vec![old]);
        let lo = make("lo", Level::L2, 0.75, old, vec![old]);
        // 前置：两条的 effective 都已跌破降级阈值。
        assert!(effective(&hi, now) < DEMOTE_L2, "前置：hi 应跌破降级阈值");
        assert!(effective(&lo, now) < DEMOTE_L2, "前置：lo 应跌破降级阈值");
        let mut mems = vec![hi, lo];
        let ts = consolidate(&mut mems, now);
        assert_eq!(
            mems[0].level,
            Level::L2,
            "importance ≥ gate 应豁免降回底层（防每场乒乓）"
        );
        assert!(
            find(&ts, "hi", TransitionKind::Demote).is_none(),
            "hi 不应有 Demote 变迁"
        );
        assert_eq!(mems[1].level, Level::L3, "importance < gate 应正常降级");
        assert!(
            find(&ts, "lo", TransitionKind::Demote).is_some(),
            "lo 应有 Demote 变迁"
        );
    }

    // 16. 溢出平局显式 tie-break（第 2/3 层：promotion_score / importance）：
    //     8 条 L1 的 effective 全被 floor=4.5 钳平，被挤出的必须是
    //     promotion_score / importance 最低且 created_at 最新者。
    #[test]
    fn overflow_tie_break_by_promotion_score() {
        let now = 1_000_000_000.0;
        let old = now - 400.0 * DAY;
        let mut mems = Vec::new();
        // loser 放在切片最前：若比较链失效、平局退化为稳定排序的插入序，
        // 它反而会被保留——以此证明是显式 tie-break（而非插入序/redb 键序）
        // 在决定挤出顺序。loser 的 importance 最低（0.2）且 created_at 最新。
        mems.push(make("loser", Level::L1, 0.2, old + 1.0 * DAY, vec![old]));
        for i in 0..7 {
            mems.push(make(&format!("k{i}"), Level::L1, 0.5, old, vec![old]));
        }
        // 前置：全部 effective 恰被 L1 floor 钳成 4.5（大面积平局）。
        for m in &mems {
            assert_eq!(
                effective(m, now),
                4.5,
                "{} 的 effective 应被钳在 floor",
                m.id
            );
        }
        let ts = consolidate(&mut mems, now);
        let pushed: Vec<_> = mems.iter().filter(|m| m.level == Level::L2).collect();
        assert_eq!(pushed.len(), 1, "应恰有 1 条被挤出 L1");
        assert_eq!(
            pushed[0].id, "loser",
            "被挤出的应是 promotion_score/importance 最低、created_at 最新者"
        );
        assert!(
            find(&ts, "loser", TransitionKind::Overflow).is_some(),
            "loser 应有 Overflow 变迁"
        );
    }

    // 17. 溢出平局显式 tie-break（第 4 层：created_at 升序）：effective、
    //     promotion_score、importance 全平时，更早创建者优先保留，
    //     created_at 最新者被挤出。
    #[test]
    fn overflow_tie_break_by_created_at() {
        let now = 1_000_000_000.0;
        let old = now - 400.0 * DAY;
        let mut mems = Vec::new();
        // newest 放最前（同上，排除稳定排序插入序的干扰）；access_log 与
        // importance 与其余 7 条完全相同 → 前三层判据全平，仅 created_at 更晚。
        mems.push(make("newest", Level::L1, 0.5, old + 5.0 * DAY, vec![old]));
        for i in 0..7 {
            mems.push(make(&format!("e{i}"), Level::L1, 0.5, old, vec![old]));
        }
        for m in &mems {
            assert_eq!(
                effective(m, now),
                4.5,
                "{} 的 effective 应被钳在 floor",
                m.id
            );
        }
        let ts = consolidate(&mut mems, now);
        let pushed: Vec<_> = mems.iter().filter(|m| m.level == Level::L2).collect();
        assert_eq!(pushed.len(), 1, "应恰有 1 条被挤出 L1");
        assert_eq!(
            pushed[0].id, "newest",
            "全平局时应挤出 created_at 最新者（更早创建者优先保留）"
        );
        assert!(
            find(&ts, "newest", TransitionKind::Overflow).is_some(),
            "newest 应有 Overflow 变迁"
        );
    }

    // 18. 字符预算触发下推：L2 仅 3 条（条数远未满 capacity=30），但每条 cue
    //     约 2500 字符，累计渲染字符超出 L2 char_budget=6000 → 保留优先序前
    //     2 条（effective 较高者）留下，最低 effective 者被挤到 L3。
    #[test]
    fn overflow_char_budget_pushes_down_despite_count_ok() {
        let now = 1_000_000_000.0;
        let p2 = params(Level::L2);
        let long_cue = "记".repeat(2500);
        let mut mems = Vec::new();
        // importance 3.0/2.9/2.8 拉开 effective 排序；近期访问保证不触发阈值
        // 降级，也不够格升 L1（虚拟视界下 promotion_score < 5.0）。
        for (i, imp) in [3.0, 2.9, 2.8].iter().enumerate() {
            let mut m = make(
                &format!("w{i}"),
                Level::L2,
                *imp,
                now - 1.0 * DAY,
                vec![now - 0.5 * DAY],
            );
            m.cue = long_cue.clone();
            mems.push(m);
        }
        // 前置：条数远未超容，但 3 条累计成本超预算、前 2 条在预算内。
        let cost3: usize = mems.iter().map(|m| memory_render_cost(m, true)).sum();
        let cost2: usize = mems
            .iter()
            .take(2)
            .map(|m| memory_render_cost(m, true))
            .sum();
        assert!(mems.len() < p2.capacity, "前置：条数应远未满容量");
        assert!(
            cost3 > p2.char_budget,
            "前置：3 条累计成本应超预算，实得 {cost3}"
        );
        assert!(
            cost2 <= p2.char_budget,
            "前置：2 条累计成本应在预算内，实得 {cost2}"
        );
        let ts = consolidate(&mut mems, now);
        assert_eq!(mems[0].level, Level::L2, "effective 最高者应留 L2");
        assert_eq!(mems[1].level, Level::L2, "effective 次高者应留 L2");
        assert_eq!(
            mems[2].level,
            Level::L3,
            "effective 最低者应被字符预算挤到 L3"
        );
        let t = find(&ts, "w2", TransitionKind::Overflow).expect("w2 应有 Overflow 变迁");
        assert_eq!(t.from, Level::L2);
        assert_eq!(t.to_level, Some(Level::L3));
    }

    // 19. 条数触发（预算未满）——既有行为回归：8 条短 cue L1（cap=7、累计
    //     字符远低于 char_budget=2000）→ 双约束里条数先触发，行为与纯条数
    //     约束时代完全一致：恰好挤出 effective 最低的 1 条。
    #[test]
    fn overflow_count_triggers_when_budget_unfilled() {
        let now = 1_000_000_000.0;
        let mut mems = Vec::new();
        for i in 0..8 {
            let importance = if i == 7 { 4.0 } else { 6.0 + i as f64 };
            mems.push(make(
                &format!("s{i}"),
                Level::L1,
                importance,
                now - 1.0 * DAY,
                vec![now - 0.5 * DAY],
            ));
        }
        let cost: usize = mems.iter().map(|m| memory_render_cost(m, true)).sum();
        assert!(
            cost <= params(Level::L1).char_budget,
            "前置：短 cue 累计成本应低于预算，实得 {cost}"
        );
        let ts = consolidate(&mut mems, now);
        assert_eq!(
            mems.iter().filter(|m| m.level == Level::L1).count(),
            7,
            "条数约束应先触发，恰留 7 条"
        );
        let pushed: Vec<_> = mems.iter().filter(|m| m.level == Level::L2).collect();
        assert_eq!(pushed.len(), 1, "应恰有 1 条被挤出");
        assert_eq!(
            pushed[0].id, "s7",
            "被挤出的仍应是 effective 最低者（回归）"
        );
        assert!(find(&ts, "s7", TransitionKind::Overflow).is_some());
    }

    // 20. 首条超预算仍保留（每层至少 1 条，防饿死）：单条 cue 7000 字符的 L2
    //     远超 char_budget=6000，但作为保留优先序首条必须留下；再加一条短 cue
    //     次条时，预算已被首条吃穿，次条被挤出。
    #[test]
    fn overflow_keeps_first_even_over_budget() {
        let now = 1_000_000_000.0;
        let huge = "忆".repeat(7000);
        // 场景 A：单条超预算 → 仍保留，不产生任何变迁。
        let mut solo_m = make("h0", Level::L2, 3.0, now - 1.0 * DAY, vec![now - 0.5 * DAY]);
        solo_m.cue = huge.clone();
        assert!(
            memory_render_cost(&solo_m, true) > params(Level::L2).char_budget,
            "前置：单条成本应超预算"
        );
        let mut solo = vec![solo_m];
        let ts_a = consolidate(&mut solo, now);
        assert_eq!(
            solo[0].level,
            Level::L2,
            "首条即超预算也应保留（至少 1 条）"
        );
        assert!(ts_a.is_empty(), "单条超预算不应有任何变迁");

        // 场景 B：超预算首条 + 短 cue 次条 → 首条留、次条被挤到 L3。
        let mut a = make("h1", Level::L2, 3.0, now - 1.0 * DAY, vec![now - 0.5 * DAY]);
        a.cue = huge;
        let b = make("h2", Level::L2, 2.0, now - 1.0 * DAY, vec![now - 0.5 * DAY]);
        let mut mems = vec![a, b];
        let ts_b = consolidate(&mut mems, now);
        assert_eq!(
            mems[0].level,
            Level::L2,
            "超预算首条（effective 更高）应保留"
        );
        assert_eq!(mems[1].level, Level::L3, "预算被首条吃穿后次条应被挤出");
        assert!(find(&ts_b, "h2", TransitionKind::Overflow).is_some());
    }

    // 21. pinned 优先占预算：L1 里 pinned 长 cue（≈1985 字符，接近吃满
    //     char_budget=2000）+ 一条高 importance 的普通短 cue → pinned 恒排
    //     最前（effective=INF）自然先占预算，普通条目被字符预算挤到 L2。
    //     这是正确行为：用户显式置顶的记忆理应最后才被预算挤出。
    #[test]
    fn overflow_char_budget_pinned_takes_priority() {
        let now = 1_000_000_000.0;
        let mut p = make("pin", Level::L1, 0.0, now - 100.0 * DAY, vec![]);
        p.pinned = true;
        p.cue = "钉".repeat(1950);
        let n = make(
            "norm",
            Level::L1,
            8.0,
            now - 1.0 * DAY,
            vec![now - 0.5 * DAY],
        );
        // 前置：pinned 单条在预算内，加上普通条即超。
        let budget = params(Level::L1).char_budget;
        let c_p = memory_render_cost(&p, true);
        let c_n = memory_render_cost(&n, true);
        assert!(
            c_p <= budget && c_p + c_n > budget,
            "前置：pinned 应在预算内、合计超预算，实得 {c_p}+{c_n}（预算 {budget}）"
        );
        let mut mems = vec![n, p]; // 普通条放前，排除插入序干扰。
        let ts = consolidate(&mut mems, now);
        let pin = mems.iter().find(|m| m.id == "pin").expect("pin 应在");
        let norm = mems.iter().find(|m| m.id == "norm").expect("norm 应在");
        assert_eq!(pin.level, Level::L1, "pinned 应留 L1（优先占预算）");
        assert_eq!(pin.status, Status::Active);
        assert_eq!(norm.level, Level::L2, "非 pinned 应被字符预算挤到 L2");
        assert!(
            find(&ts, "pin", TransitionKind::Overflow).is_none(),
            "pinned 不应有 Overflow 变迁"
        );
        assert!(
            find(&ts, "norm", TransitionKind::Overflow).is_some(),
            "norm 应有 Overflow 变迁"
        );
    }
}
