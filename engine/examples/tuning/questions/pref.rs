//! 偏好题 PREF-01..05（相对偏好 regret，进优化目标；方案 §9.2）。
//!
//! v0.9 预期更新：**PREF-02 / PREF-05 在重要度门（importance_gate=0.8）下
//! 应转绿（Loss=0）**——两题从「暴露缺口」转为「守护重要度门」的回归题，
//! 断言不变、预期结论由 Loss>0 改为 Loss=0。
//!
//! 打分口径（对样板 P 的 reach 三档比较做了一处**文档化的细化**）：
//! - 违反偏序（reach(A) < reach(B)）→ loss = 1.0；
//! - A 档位严格更高 → loss = 0.0；
//! - 同档时改用引擎 `effective_with`（lib 本体，非复刻公式）做严格区分：
//!   eff(A) > eff(B) → 0.0，否则 → 0.5「区分度不足」。
//!   细化理由：PREF-03/04 的题面时间尺度（10/90 天）下，弱者按引擎语义
//!   不可能已跌破淘汰线（-3 需约 500 天），reach 三档在该尺度天然同档；
//!   同档内的 effective 排序正是题目要考的「谁更该留」，故以此为严格区分。

use engram::activation::effective_with;
use engram::commands::level_rank;
use engram::consolidate::consolidate_with;
use engram::model::{EngramConfig, Level, Memory, Status};

use super::{days_ago, T};
use crate::events::{make_memory, sort_events, Event, EventKind, Reach, Replayer};

/// 一道偏好题的结果。
pub struct PrefOutcome {
    /// 题号（如 `PREF-01`）。
    pub id: &'static str,
    /// 一句话标题。
    pub title: &'static str,
    /// regret（0 = 完全满足且严格区分）。
    pub loss: f64,
    /// 量化细节。
    pub detail: String,
}

/// 跑全部 5 道偏好题。
pub fn run_all(cfg: &EngramConfig) -> Vec<PrefOutcome> {
    vec![
        pref01(cfg),
        pref02(cfg),
        pref03(cfg),
        pref04(cfg),
        pref05(cfg),
    ]
}

/// 记忆的可达档（与回放器口径一致：Active=Hot / Cold=Cold / 其它或不存在=Gone）。
fn reach_of(mems: &[Memory], id: &str) -> Reach {
    match mems.iter().find(|m| m.id == id) {
        None => Reach::Gone,
        Some(m) => match m.status {
            Status::Active => Reach::Hot,
            Status::Cold => Reach::Cold,
            Status::Superseded | Status::Tombstone => Reach::Gone,
        },
    }
}

/// reach 偏序 + effective 严格区分的统一打分（见模块头口径说明）。
fn reach_loss(
    cfg: &EngramConfig,
    mems: &[Memory],
    id_a: &str,
    id_b: &str,
    now: f64,
) -> (f64, String) {
    let (ra, rb) = (reach_of(mems, id_a), reach_of(mems, id_b));
    let eff = |id: &str| {
        mems.iter()
            .find(|m| m.id == id)
            .map(|m| effective_with(m, now, cfg))
            .unwrap_or(f64::NEG_INFINITY)
    };
    let (ea, eb) = (eff(id_a), eff(id_b));
    // 违反偏序 → 1.0；A 档位严格更高，或同档但 effective 严格更高 → 0.0；
    // 同档且 effective 也分不出 → 0.5「区分度不足」。
    let loss = if ra.rank() < rb.rank() {
        1.0
    } else if ra.rank() > rb.rank() || ea > eb {
        0.0
    } else {
        0.5
    };
    let detail = format!(
        "A={}/eff{ea:.3} B={}/eff{eb:.3} → loss={loss}",
        ra.name(),
        rb.name()
    );
    (loss, detail)
}

/// PREF-01 纠错胜噪声（样板 P）：被纠正 3 次的老约定应比一次性噪声更可达。
///
/// 用事件回放器构造（Correct 建模 = +1 真用 + importance+0.2 封顶，见 events.rs）。
fn pref01(cfg: &EngramConfig) -> PrefOutcome {
    let mut events = vec![
        Event {
            t: days_ago(600.0),
            id: "A".to_string(),
            kind: EventKind::Write {
                level: Level::L3,
                project: None,
                importance: 0.3,
            },
        },
        Event {
            t: days_ago(600.0),
            id: "B".to_string(),
            kind: EventKind::Write {
                level: Level::L3,
                project: None,
                importance: 0.1,
            },
        },
        Event {
            t: days_ago(598.0),
            id: "B".to_string(),
            kind: EventKind::Use,
        },
    ];
    for d in [10.0, 7.0, 4.0] {
        events.push(Event {
            t: days_ago(d),
            id: "A".to_string(),
            kind: EventKind::Correct,
        });
    }
    events.push(Event {
        t: T,
        id: String::new(),
        kind: EventKind::Tick,
    });
    sort_events(&mut events);
    let mut rp = Replayer::new(cfg);
    rp.run(&events);
    let imp_a = rp.find("A").map(|m| m.importance).unwrap_or(0.0);
    let (loss, detail) = reach_loss(cfg, &rp.mems, "A", "B", T);
    PrefOutcome {
        id: "PREF-01",
        title: "纠错胜噪声",
        loss,
        detail: format!("{detail}（A 纠正后 imp={imp_a:.2}）"),
    }
}

/// PREF-02 低频要命不被高频琐碎顶掉（核心张力）：
/// 高重要度低频的 A 层级不应低于低重要度高频的 B。
///
/// v0.9 预期转绿：重要度门下 A（imp 0.8）直升中层；升级视界下 B 的
/// 近因堆分也不再能靠「真实 now 采样」越阈。
fn pref02(cfg: &EngramConfig) -> PrefOutcome {
    let a = make_memory(
        "A",
        Level::L3,
        None,
        0.8,
        days_ago(40.0),
        vec![days_ago(30.0)],
    );
    let b = make_memory(
        "B",
        Level::L3,
        None,
        0.1,
        days_ago(7.0),
        vec![
            days_ago(1.0),
            days_ago(0.5),
            days_ago(0.3),
            days_ago(0.2),
            days_ago(0.1),
            days_ago(0.05),
        ],
    );
    let mut mems = vec![a, b];
    consolidate_with(&mut mems, T, cfg);
    let lv = |id: &str| mems.iter().find(|m| m.id == id).map(|m| m.level);
    let (la, lb) = (lv("A"), lv("B"));
    // 契约：level(A) 不低于 level(B)。level_rank 越小层级越高。
    let loss = match (la, lb) {
        (Some(la), Some(lb)) => (level_rank(la) as f64 - level_rank(lb) as f64).max(0.0),
        _ => 1.0,
    };
    PrefOutcome {
        id: "PREF-02",
        title: "低频要命不被高频琐碎顶掉",
        loss,
        detail: format!("level(A)={la:?} level(B)={lb:?}（v0.9 门下 A 应升 L2）"),
    }
}

/// PREF-03 grace 期内护新、期满让位：试用期里被用过的 A 应比一次没用的 B 更可达。
fn pref03(cfg: &EngramConfig) -> PrefOutcome {
    let a = make_memory(
        "A",
        Level::L3,
        None,
        0.1,
        days_ago(10.0),
        vec![days_ago(9.0), days_ago(2.0)],
    );
    let b = make_memory("B", Level::L3, None, 0.1, days_ago(10.0), vec![]);
    let mut mems = vec![a, b];
    consolidate_with(&mut mems, T, cfg);
    let (loss, detail) = reach_loss(cfg, &mems, "A", "B", T);
    PrefOutcome {
        id: "PREF-03",
        title: "grace 期内护新、期满让位",
        loss,
        detail,
    }
}

/// PREF-04 近期活跃胜久未问津：同龄记忆，最近一周还在用的 A 应比早停用的 B 更可达。
fn pref04(cfg: &EngramConfig) -> PrefOutcome {
    let a = make_memory(
        "A",
        Level::L3,
        None,
        0.0,
        days_ago(90.0),
        vec![days_ago(7.0), days_ago(3.0), days_ago(1.0)],
    );
    let b = make_memory(
        "B",
        Level::L3,
        None,
        0.0,
        days_ago(90.0),
        vec![days_ago(89.0)],
    );
    let mut mems = vec![a, b];
    consolidate_with(&mut mems, T, cfg);
    let (loss, detail) = reach_loss(cfg, &mems, "A", "B", T);
    PrefOutcome {
        id: "PREF-04",
        title: "近期活跃胜久未问津",
        loss,
        detail,
    }
}

/// PREF-05 重要度门空降：刚写入的高重要度记忆（imp 0.95）应直接进 L2 或更高。
///
/// v0.9 预期转绿：importance_gate 落地后 H 由门直升 L2，不再等频率攒分。
fn pref05(cfg: &EngramConfig) -> PrefOutcome {
    let mut mems = vec![make_memory("H", Level::L3, None, 0.95, T, vec![])];
    for i in 0..10 {
        mems.push(make_memory(
            &format!("n{i}"),
            Level::L3,
            None,
            0.3,
            days_ago(20.0),
            vec![days_ago(15.0)],
        ));
    }
    consolidate_with(&mut mems, T, cfg);
    let h_level = mems.iter().find(|m| m.id == "H").map(|m| m.level);
    let loss = match h_level {
        Some(Level::L1 | Level::L2) => 0.0,
        _ => 1.0,
    };
    PrefOutcome {
        id: "PREF-05",
        title: "重要度门空降",
        loss,
        detail: format!("H 结局层级={h_level:?}（应 L2 或更高）"),
    }
}
