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
//! - **迟滞**：升级阈值明显高于降级阈值，中间留死区，杜绝边界乒乓。
//! - **逐级**：一次最多移动一层。
//! - **只处理 `status == Active` 的记忆**；Cold/Superseded/Tombstone 一律不动。

use crate::activation::{effective, promotion_score};
use crate::model::{
    params, Level, Memory, Status, DEMOTE_L1, DEMOTE_L2, EVICT_THRESHOLD, PROMOTE_L1, PROMOTE_L2,
};

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
/// - 底层（L3/L4.3）：`promotion_score ≥ PROMOTE_L2` → 升一层；否则不动。
/// - 中层（L2/L4.2）：`promotion_score ≥ PROMOTE_L1` → 升一层；
///   否则若 `effective < DEMOTE_L2` → 降一层；否则不动。
/// - 顶层（L1/L4.1）：`effective < DEMOTE_L1` → 降一层；否则不动。
///
/// 升级判据用 `promotion_score`、降级判据用 `effective`，二者阈值间留死区，构成迟滞。
fn step_threshold(memories: &mut [Memory], now: f64, transitions: &mut Vec<Transition>) {
    for m in memories.iter_mut() {
        if m.status != Status::Active {
            continue;
        }
        let from = m.level;
        let target = match tier_role(from) {
            TierRole::Bottom => {
                if promotion_score(m, now) >= PROMOTE_L2 {
                    level_up(from)
                } else {
                    None
                }
            }
            TierRole::Middle => {
                if promotion_score(m, now) >= PROMOTE_L1 {
                    level_up(from)
                } else if effective(m, now) < DEMOTE_L2 {
                    level_down(from)
                } else {
                    None
                }
            }
            TierRole::Top => {
                if effective(m, now) < DEMOTE_L1 {
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

/// 第二步：容量溢出级联下推。
///
/// 按作用域分组、自上而下（顶→中→底）逐层处理：组内某层条数超过
/// `params(level).capacity` 时，按 `effective` 降序保留前 capacity 条，
/// 其余**下推一层**；下推可能令下一层再超容，故自上而下处理使其传播。
/// 从底层（L3/L4.3）挤出的记忆转 `status = Cold`。
///
/// pinned 记忆 effective 为 INF，恒在前列，不会被下推。
fn step_overflow(memories: &mut [Memory], now: f64, transitions: &mut Vec<Transition>) {
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
        let capacity = params(level).capacity;

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
            if idxs.len() <= capacity {
                continue;
            }
            // 按 effective 降序排序；NaN 视作最小沉底。
            idxs.sort_by(|&a, &b| {
                let ea = effective(&memories[a], now);
                let eb = effective(&memories[b], now);
                eb.partial_cmp(&ea).unwrap_or(std::cmp::Ordering::Equal)
            });
            // 前 capacity 条保留，其余下推。
            for &i in idxs.iter().skip(capacity) {
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
fn step_evict(memories: &mut [Memory], now: f64, transitions: &mut Vec<Transition>) {
    for m in memories.iter_mut() {
        if m.status != Status::Active {
            continue;
        }
        if !matches!(tier_role(m.level), TierRole::Bottom) {
            continue;
        }
        if effective(m, now) < EVICT_THRESHOLD {
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
/// 2. 容量溢出级联下推（按作用域分组、自上而下）；
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
    let mut transitions = Vec::new();
    step_threshold(memories, now, &mut transitions);
    step_overflow(memories, now, &mut transitions);
    step_evict(memories, now, &mut transitions);
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

    // 2. 升级：L3、promotion_score ≥ 2.5 → 升 L2。
    #[test]
    fn promote_l3_to_l2() {
        let now = 1_000_000_000.0;
        // importance 1.0 + 多次近期访问，promotion_score 拉到 ≥ 2.5。
        let m = make(
            "b",
            Level::L3,
            1.0,
            now - 2.0 * DAY,
            vec![
                now - 1.0 * DAY,
                now - 0.5 * DAY,
                now - 0.2 * DAY,
                now - 0.1 * DAY,
            ],
        );
        assert!(
            promotion_score(&m, now) >= PROMOTE_L2,
            "前置：promotion_score 应 ≥ 2.5，实得 {}",
            promotion_score(&m, now)
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

    // 6. grace 防错杀但不助上位：新 L3 记忆（created_at=now，grace 满额、
    //    importance 低、仅 1 次低频访问）→ 不被淘汰（保持 Active）；同时
    //    不会被升级（promotion_score 不含 grace）。
    //
    // 说明（偏离指令的取舍）：指令原文写“单次访问=now”。但在本模型里 Δt 被
    // 下限钳到 1 分钟（MIN_DT_DAYS），access=now 会让 activation 出现一个
    // (1/1440)^-d 的尖峰（L3 下约 3.6），从而 promotion_score 反而越过 2.5——
    // 这是“刚刚真用过”的合法近因得分，而非 grace 所致。为忠实表达本用例的
    // 本意（grace 抬 effective 防错杀、但不帮升级），此处把那次访问放在 1 天前，
    // 使 activation≈0、promotion_score≈importance，干净地隔离出 grace 的作用。
    #[test]
    fn grace_protects_but_does_not_promote() {
        let now = 1_000_000_000.0;
        let m = make("e", Level::L3, 0.1, now, vec![now - 1.0 * DAY]);
        // promotion_score 不含 grace，≈ importance ≈ 0.1，明显 < PROMOTE_L2。
        assert!(
            promotion_score(&m, now) < PROMOTE_L2,
            "promotion_score 应 < 2.5，实得 {}",
            promotion_score(&m, now)
        );
        // effective 含约 2.0 的 grace，远高于 promotion_score，也远高于淘汰阈值。
        assert!(effective(&m, now) > EVICT_THRESHOLD);
        assert!(
            effective(&m, now) > promotion_score(&m, now) + 1.5,
            "grace 应把 effective 明显抬高于 promotion_score"
        );
        let mut mems = vec![m];
        let ts = consolidate(&mut mems, now);
        assert_eq!(mems[0].status, Status::Active, "grace 应防止被淘汰");
        assert_eq!(mems[0].level, Level::L3, "不应被升级（grace 不助上位）");
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

    // 8. 迟滞死区：同一条 promotion_score=effective≈2.0 的记忆，
    //    当前在 L3 时不升（2.0<2.5），当前在 L2 时不降（2.0≥1.5）。
    #[test]
    fn hysteresis_dead_zone() {
        let now = 1_000_000_000.0;
        // 构造一条记忆，使得 promotion_score 与 effective 都 ≈ 2.0。
        // 取 created_at 久远（grace≈0），则 effective≈promotion_score（floor 不截断时）。
        // L3 floor=-10、L2 floor=1.0 均不会截断 2.0。
        // importance + activation ≈ 2.0：调 importance 与单次访问时间。
        // 单次访问 d=0.5（L3）下：activation = ln((Δt_day)^-0.5)。
        // 取 Δt=1 天 → activation=ln(1)=0；importance=2.0 → score=2.0。
        let make_at = |level: Level| {
            // 注意 d 随层级变，但 1 天 Δt 时 (1)^-d = 1，ln=0，与 d 无关，恰好稳定。
            make("z", level, 2.0, now - 30.0 * DAY, vec![now - 1.0 * DAY])
        };

        // 容差用 1e-3：created_at 在 30 天前，grace=2·exp(-10)≈9e-5 尚未严格归零，
        // 故 effective 比 promotion_score 高出这一极小残量；不影响死区结论。

        // 当前 L3：promotion_score≈2.0 < 2.5 → 不升。
        let mut as_l3 = vec![make_at(Level::L3)];
        assert!(
            (promotion_score(&as_l3[0], now) - 2.0).abs() < 1e-3,
            "L3 promotion_score 应≈2.0，实得 {}",
            promotion_score(&as_l3[0], now)
        );
        let ts3 = consolidate(&mut as_l3, now);
        assert_eq!(as_l3[0].level, Level::L3, "死区内不应升级");
        assert!(ts3.is_empty());

        // 当前 L2：effective≈2.0 ≥ 1.5 → 不降；promotion_score≈2.0 < 5.0 → 不升。
        let mut as_l2 = vec![make_at(Level::L2)];
        assert!(
            (effective(&as_l2[0], now) - 2.0).abs() < 1e-3,
            "L2 effective 应≈2.0，实得 {}",
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
}
