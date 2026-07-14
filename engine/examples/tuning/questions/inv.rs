//! 护栏题 INV-01..20（pass/fail，违反即判废；方案 §9.1 + v0.9 语义修订）。
//!
//! v0.9 语义修订点：
//! - INV-07 重构造为**跨日持续使用**（连续 7 天 × 3 次/天 + imp 0.5）——升级在
//!   `now + promotion_horizon_days` 虚拟视界采样，旧构造「2 天 4 次」已不够格；
//! - INV-06 死区用例的 importance 重设为 0.5（限 [0,1] 且 < importance_gate），
//!   死区分数靠「昨天 5 次真使用」的 activation 攒（参考 engine consolidate.rs
//!   现有单测 `hysteresis_dead_zone` 的构造）；
//! - 新增 INV-17（分钟级采样红线）/ INV-18（溢出平局确定性）/ INV-19（重要度门）。
//!
//! 批5（容量 token 化）新增：
//! - INV-20 字符预算溢出——条数远未满容也必须被 `char_budget` 钳住，
//!   同题子断言「首条超预算仍至少保留 1 条」（防长 cue 饿死整层）。

use engram::activation::{effective_with, promotion_score_with};
use engram::commands::gc_should_delete;
use engram::consolidate::{consolidate_with, TransitionKind};
use engram::model::{EngramConfig, Level, Memory, Status};
use engram::render::memory_render_cost;

use super::{days_ago, DAY, T};
use crate::events::make_memory;

/// 一道护栏题的判定结果。
pub struct Guardrail {
    /// 题号（如 `INV-01`）。
    pub id: &'static str,
    /// 一句话标题。
    pub title: &'static str,
    /// 是否通过。
    pub pass: bool,
    /// 量化细节（实测数值），供报告与排查。
    pub detail: String,
}

/// 跑全部 20 道护栏题。
pub fn run_all(cfg: &EngramConfig) -> Vec<Guardrail> {
    vec![
        inv01(cfg),
        inv02(cfg),
        inv03(cfg),
        inv04(cfg),
        inv05(cfg),
        inv06(cfg),
        inv07(cfg),
        inv08(cfg),
        inv09(cfg),
        inv10(cfg),
        inv11(cfg),
        inv12(cfg),
        inv13(cfg),
        inv14(cfg),
        inv15(cfg),
        inv16(cfg),
        inv17(cfg),
        inv18(cfg),
        inv19(cfg),
        inv20(cfg),
    ]
}

/// INV-01 高频胜稀疏：5 次真用的 effective 应高于 1 次。
fn inv01(cfg: &EngramConfig) -> Guardrail {
    let a = make_memory(
        "a",
        Level::L3,
        None,
        0.0,
        days_ago(5.0),
        vec![
            days_ago(5.0),
            days_ago(3.0),
            days_ago(2.0),
            days_ago(1.0),
            days_ago(0.5),
        ],
    );
    let b = make_memory(
        "b",
        Level::L3,
        None,
        0.0,
        days_ago(5.0),
        vec![days_ago(5.0)],
    );
    let (ea, eb) = (effective_with(&a, T, cfg), effective_with(&b, T, cfg));
    Guardrail {
        id: "INV-01",
        title: "高频胜稀疏",
        pass: ea > eb,
        detail: format!("eff(A)={ea:.3} eff(B)={eb:.3}"),
    }
}

/// INV-02 近因胜久远：半天前单次真用应高于 50 天前单次。
fn inv02(cfg: &EngramConfig) -> Guardrail {
    let a = make_memory(
        "a",
        Level::L3,
        None,
        0.0,
        days_ago(0.5),
        vec![days_ago(0.5)],
    );
    let b = make_memory(
        "b",
        Level::L3,
        None,
        0.0,
        days_ago(50.0),
        vec![days_ago(50.0)],
    );
    let (ea, eb) = (effective_with(&a, T, cfg), effective_with(&b, T, cfg));
    Guardrail {
        id: "INV-02",
        title: "近因胜久远",
        pass: ea > eb,
        detail: format!("eff(近)={ea:.3} eff(远)={eb:.3}"),
    }
}

/// INV-03 随时间单调衰减：同一条记忆在 T / T+30d / T+100d 的 effective 一路下降。
fn inv03(cfg: &EngramConfig) -> Guardrail {
    let m = make_memory(
        "m",
        Level::L3,
        None,
        0.0,
        days_ago(5.0),
        vec![days_ago(5.0), days_ago(3.0), days_ago(1.0)],
    );
    let e0 = effective_with(&m, T, cfg);
    let e30 = effective_with(&m, T + 30.0 * DAY, cfg);
    let e100 = effective_with(&m, T + 100.0 * DAY, cfg);
    Guardrail {
        id: "INV-03",
        title: "随时间单调衰减",
        pass: e0 > e30 && e30 > e100,
        detail: format!("eff(T)={e0:.3} eff(T+30d)={e30:.3} eff(T+100d)={e100:.3}"),
    }
}

/// INV-04 L1 floor 守得住：400 天零真用的 L1 身份记忆不降级不淘汰。
fn inv04(cfg: &EngramConfig) -> Guardrail {
    let mut mems = vec![make_memory(
        "c",
        Level::L1,
        None,
        0.3,
        days_ago(400.0),
        vec![days_ago(400.0)],
    )];
    let eff = effective_with(&mems[0], T, cfg);
    consolidate_with(&mut mems, T, cfg);
    let pass = mems[0].level == Level::L1 && mems[0].status == Status::Active;
    Guardrail {
        id: "INV-04",
        title: "L1 floor 守得住",
        pass,
        detail: format!("eff={eff:.3} 结局={:?}/{:?}", mems[0].level, mems[0].status),
    }
}

/// INV-05 L3 易忘：500 天没碰的临时记忆应淘汰进冷库。
fn inv05(cfg: &EngramConfig) -> Guardrail {
    let mut mems = vec![make_memory(
        "d",
        Level::L3,
        None,
        0.0,
        days_ago(500.0),
        vec![days_ago(500.0)],
    )];
    let eff = effective_with(&mems[0], T, cfg);
    consolidate_with(&mut mems, T, cfg);
    Guardrail {
        id: "INV-05",
        title: "L3 易忘（淘汰进冷库）",
        pass: mems[0].status == Status::Cold,
        detail: format!("eff={eff:.3} 结局={:?}", mems[0].status),
    }
}

/// INV-06 迟滞不抖（核心）：卡在死区（eff≈2.1）的记忆，起于 L3 连 5 次
/// consolidate 不升、起于 L2 连 5 次不降，合计 0 次翻层。
///
/// v0.9 修订构造：importance 0.5（限 [0,1] 且 < importance_gate=0.8，不触发
/// 重要度门），分数靠「昨天 5 次真使用」攒；5 次巩固间隔 2 小时（同日多会话）。
fn inv06(cfg: &EngramConfig) -> Guardrail {
    let mut total_flips = 0usize;
    let mut detail = String::new();
    for level in [Level::L3, Level::L2] {
        let mut mems = vec![make_memory(
            "z",
            level,
            None,
            0.5,
            days_ago(30.0),
            vec![days_ago(1.0); 5],
        )];
        for k in 0..5 {
            let now = T + k as f64 * 2.0 * 3600.0;
            total_flips += consolidate_with(&mut mems, now, cfg).len();
        }
        detail.push_str(&format!("起于{:?}终于{:?}; ", level, mems[0].level));
    }
    Guardrail {
        id: "INV-06",
        title: "迟滞不抖（死区零翻层）",
        pass: total_flips == 0,
        detail: format!("{detail}合计翻层={total_flips}"),
    }
}

/// INV-07 频率门正常升：跨日持续使用（连续 7 天 × 3 次/天 + imp 0.5）应升 L2。
///
/// v0.9 语义收紧：升级在 `now+promotion_horizon_days` 虚拟视界采样，
/// 旧构造「2 天 4 次 + imp 1.0」已不够格（promotion≈2.22 < 2.5）。
fn inv07(cfg: &EngramConfig) -> Guardrail {
    let mut log = Vec::new();
    for k in 0..7 {
        for _ in 0..3 {
            log.push(days_ago(k as f64));
        }
    }
    let m = make_memory("b", Level::L3, None, 0.5, days_ago(8.0), log);
    let promote_at = T + cfg.promotion_horizon_days * DAY;
    let ps = promotion_score_with(&m, promote_at, cfg);
    let mut mems = vec![m];
    consolidate_with(&mut mems, T, cfg);
    Guardrail {
        id: "INV-07",
        title: "频率门正常升（跨日持续使用）",
        pass: mems[0].level == Level::L2,
        detail: format!("虚拟视界 promotion={ps:.3} 结局={:?}", mems[0].level),
    }
}

/// INV-08 grace 容量竞争护新（样板 G）：L3 满员 + 1 条 2 天前的新记忆，
/// 溢出后新记忆应仍 Active（试用期防「因新而非因差」被秒杀）。
fn inv08(cfg: &EngramConfig) -> Guardrail {
    let cap = cfg.l3.capacity;
    let mut mems: Vec<Memory> = (0..cap)
        .map(|i| {
            make_memory(
                &format!("old{i}"),
                Level::L3,
                None,
                0.3,
                days_ago(30.0),
                vec![days_ago(10.0), days_ago(5.0)],
            )
        })
        .collect();
    mems.push(make_memory(
        "E",
        Level::L3,
        None,
        0.3,
        days_ago(2.0),
        vec![],
    ));
    let eff_e = effective_with(&mems[cap], T, cfg);
    let eff_old = effective_with(&mems[0], T, cfg);
    consolidate_with(&mut mems, T, cfg);
    let e = mems.iter().find(|m| m.id == "E").map(|m| m.status);
    Guardrail {
        id: "INV-08",
        title: "grace 容量竞争护新",
        pass: e == Some(Status::Active),
        detail: format!("eff(E)={eff_e:.3} eff(老)={eff_old:.3} E结局={e:?}（cap={cap}）"),
    }
}

/// INV-09 grace 不助上位：新鲜度加成抬高 effective，但不得触发升级。
fn inv09(cfg: &EngramConfig) -> Guardrail {
    let m = make_memory("e", Level::L3, None, 0.1, T, vec![days_ago(1.0)]);
    let eff = effective_with(&m, T, cfg);
    let ps = promotion_score_with(&m, T + cfg.promotion_horizon_days * DAY, cfg);
    let mut mems = vec![m];
    let ts = consolidate_with(&mut mems, T, cfg);
    let pass = mems[0].level == Level::L3 && mems[0].status == Status::Active && ts.is_empty();
    Guardrail {
        id: "INV-09",
        title: "grace 不助上位",
        pass,
        detail: format!(
            "eff={eff:.3}（含grace） promotion={ps:.3} 变迁数={}",
            ts.len()
        ),
    }
}

/// INV-10 容量溢出级联：cap+1 条 L1 → 恰留 cap 条，effective 最低者下推 L2。
fn inv10(cfg: &EngramConfig) -> Guardrail {
    let cap = cfg.l1.capacity;
    let loser = format!("m{cap}");
    let mut mems: Vec<Memory> = (0..=cap)
        .map(|i| {
            let importance = if i == cap { 4.0 } else { 6.0 + i as f64 };
            make_memory(
                &format!("m{i}"),
                Level::L1,
                None,
                importance,
                days_ago(1.0),
                vec![days_ago(0.5)],
            )
        })
        .collect();
    let ts = consolidate_with(&mut mems, T, cfg);
    let l1_count = mems.iter().filter(|m| m.level == Level::L1).count();
    let pushed: Vec<&str> = ts
        .iter()
        .filter(|t| t.kind == TransitionKind::Overflow && t.from == Level::L1)
        .map(|t| t.id.as_str())
        .collect();
    let pass = l1_count == cap && pushed == vec![loser.as_str()];
    Guardrail {
        id: "INV-10",
        title: "容量溢出级联",
        pass,
        detail: format!("留L1={l1_count}/{cap} 下推={pushed:?}（应为 {loser}）"),
    }
}

/// INV-11 L4 按项目独立计容量：α 超容 1 条下推，β 未超容零下推。
fn inv11(cfg: &EngramConfig) -> Guardrail {
    let cap = cfg.l4_1.capacity;
    let mut mems = Vec::new();
    for i in 0..=cap {
        mems.push(make_memory(
            &format!("a{i}"),
            Level::L4_1,
            Some("alpha"),
            10.0 + i as f64,
            days_ago(1.0),
            vec![days_ago(0.5)],
        ));
    }
    for i in 0..cap.saturating_sub(1) {
        mems.push(make_memory(
            &format!("b{i}"),
            Level::L4_1,
            Some("beta"),
            10.0 + i as f64,
            days_ago(1.0),
            vec![days_ago(0.5)],
        ));
    }
    consolidate_with(&mut mems, T, cfg);
    let count = |p: &str, lv: Level| {
        mems.iter()
            .filter(|m| m.project.as_deref() == Some(p) && m.level == lv)
            .count()
    };
    let (a1, a2) = (count("alpha", Level::L4_1), count("alpha", Level::L4_2));
    let (b1, b2) = (count("beta", Level::L4_1), count("beta", Level::L4_2));
    let pass = a1 == cap && a2 == 1 && b1 == cap - 1 && b2 == 0;
    Guardrail {
        id: "INV-11",
        title: "L4 按项目独立计容量",
        pass,
        detail: format!("α L4.1={a1} 下推={a2}；β L4.1={b1} 下推={b2}（cap={cap}）"),
    }
}

/// INV-12 pinned 永驻：置顶记忆零真用也不被降级/溢出下推。
fn inv12(cfg: &EngramConfig) -> Guardrail {
    let cap = cfg.l1.capacity;
    let mut pinned = make_memory("p", Level::L1, None, 0.0, days_ago(9999.0), vec![]);
    pinned.pinned = true;
    let mut mems = vec![pinned];
    for i in 0..cap {
        mems.push(make_memory(
            &format!("n{i}"),
            Level::L1,
            None,
            6.0 + i as f64,
            days_ago(1.0),
            vec![days_ago(0.5)],
        ));
    }
    let ts = consolidate_with(&mut mems, T, cfg);
    let p = mems.iter().find(|m| m.id == "p");
    let p_ok = p.is_some_and(|m| m.level == Level::L1 && m.status == Status::Active);
    let p_pushed = ts
        .iter()
        .any(|t| t.id == "p" && t.kind == TransitionKind::Overflow);
    Guardrail {
        id: "INV-12",
        title: "pinned 永驻",
        pass: p_ok && !p_pushed,
        detail: format!("pinned 留L1={p_ok} 被下推={p_pushed}"),
    }
}

/// INV-13 密钥轮换不被误删（与门第二臂）：真用 3 次的稀疏冷记忆 gc 不删。
fn inv13(cfg: &EngramConfig) -> Guardrail {
    let mut m = make_memory(
        "rot",
        Level::L3,
        None,
        0.4,
        days_ago(800.0),
        vec![days_ago(800.0), days_ago(500.0), days_ago(200.0)],
    );
    m.status = Status::Cold;
    let del = gc_should_delete(&m, T, cfg.ttl_days, cfg.tombstone_ttl_days, cfg.min_uses);
    Guardrail {
        id: "INV-13",
        title: "密钥轮换不被误删",
        pass: !del,
        detail: format!(
            "age=200d>ttl={} 但真用3次>min_uses={} → 删除={del}",
            cfg.ttl_days, cfg.min_uses
        ),
    }
}

/// INV-14 纯噪声被回收：只用过诞生那次、200 天没碰的冷条目应删。
fn inv14(cfg: &EngramConfig) -> Guardrail {
    let mut m = make_memory(
        "noise",
        Level::L3,
        None,
        0.1,
        days_ago(200.0),
        vec![days_ago(200.0)],
    );
    m.status = Status::Cold;
    let del = gc_should_delete(&m, T, cfg.ttl_days, cfg.tombstone_ttl_days, cfg.min_uses);
    Guardrail {
        id: "INV-14",
        title: "纯噪声被回收",
        pass: del,
        detail: format!("age=200d ttl={} 真用1次 → 删除={del}", cfg.ttl_days),
    }
}

/// INV-15 tombstone 长寿：1000 天的墓碑留、4000 天的墓碑删。
fn inv15(cfg: &EngramConfig) -> Guardrail {
    let mut a = make_memory(
        "ta",
        Level::L3,
        None,
        0.1,
        days_ago(1000.0),
        vec![days_ago(1000.0)],
    );
    a.status = Status::Tombstone;
    let mut b = make_memory(
        "tb",
        Level::L3,
        None,
        0.1,
        days_ago(4000.0),
        vec![days_ago(4000.0)],
    );
    b.status = Status::Tombstone;
    let da = gc_should_delete(&a, T, cfg.ttl_days, cfg.tombstone_ttl_days, cfg.min_uses);
    let db = gc_should_delete(&b, T, cfg.ttl_days, cfg.tombstone_ttl_days, cfg.min_uses);
    Guardrail {
        id: "INV-15",
        title: "tombstone 长寿",
        pass: !da && db,
        detail: format!(
            "1000d删={da}（应留） 4000d删={db}（应删，tombstone_ttl={}）",
            cfg.tombstone_ttl_days
        ),
    }
}

/// INV-16 recall 重置 TTL 计时：临删前的一次真用应保住冷记忆。
fn inv16(cfg: &EngramConfig) -> Guardrail {
    let mut m = make_memory(
        "rev",
        Level::L3,
        None,
        0.1,
        days_ago(800.0),
        vec![days_ago(800.0), days_ago(10.0)],
    );
    m.status = Status::Cold;
    let del = gc_should_delete(&m, T, cfg.ttl_days, cfg.tombstone_ttl_days, cfg.min_uses);
    Guardrail {
        id: "INV-16",
        title: "recall 重置 TTL 计时",
        pass: !del,
        detail: format!("last_touch=10d前 真用2次 → 删除={del}"),
    }
}

/// INV-17 分钟级采样红线（新增）：单次访问在 5 分钟前的 L3/imp0.3 条目，
/// consolidate 不得升级——旧「Δt 钳位 1 分钟 → activation 尖峰 → 单次即升级」
/// 缺陷的永久回归闸（升级视界应压平瞬时尖峰）。
fn inv17(cfg: &EngramConfig) -> Guardrail {
    let five_min_ago = T - 5.0 * 60.0;
    let m = make_memory("m5", Level::L3, None, 0.3, five_min_ago, vec![five_min_ago]);
    // 红线证明：真实 now 采样确有瞬时尖峰（若无视界机制它必越阈）。
    let spike = promotion_score_with(&m, T, cfg);
    let horizon = promotion_score_with(&m, T + cfg.promotion_horizon_days * DAY, cfg);
    let mut mems = vec![m];
    consolidate_with(&mut mems, T, cfg);
    let pass = mems[0].level == Level::L3 && mems[0].status == Status::Active;
    Guardrail {
        id: "INV-17",
        title: "分钟级采样红线（尖峰回归闸）",
        pass,
        detail: format!(
            "真实now采样={spike:.3}（尖峰） 视界采样={horizon:.3} 结局={:?}",
            mems[0].level
        ),
    }
}

/// INV-18 辅助：对给定构造跑一次 consolidate，收集从 L1 被溢出下推的 id（排序后返回）。
fn overflow_pushed(mut mems: Vec<Memory>, cfg: &EngramConfig) -> Vec<String> {
    let ts = consolidate_with(&mut mems, T, cfg);
    let mut pushed: Vec<String> = ts
        .iter()
        .filter(|t| t.kind == TransitionKind::Overflow && t.from == Level::L1)
        .map(|t| t.id.clone())
        .collect();
    pushed.sort();
    pushed
}

/// INV-18 溢出平局确定性（新增）：同 effective 的 L1 满员溢出，被挤出者
/// 必须由 tie-break 链（effective→promotion_score→importance→created_at）
/// 唯一决定，且与构造顺序无关。三组子构造分别把决胜点钉在链的第 2/3/4 环
/// （全员 effective 都被 L1 floor 钳平，第 1 环恒平）：
///
/// - (a) promotion_score 环：loser 的 importance 最低 → promotion_score
///   唯一最低 → 恒被挤出（原有构造，附三种构造顺序排除插入序干扰）；
/// - (b) importance 环：victim 的 importance 直接取 promotion_score(keeper)
///   ——victim 的唯一访问恰在整 1 天前，activation=ln(1^-d)=0 精确为零，故
///   promotion(victim)=imp(victim)+0 与 promotion(keeper) 是同一表达式同一
///   舍入、逐位相等——第 2 环打平，由第 3 环 importance 决胜
///   （imp(victim)<imp(keeper) → victim 被挤出）。victim 的 created_at 刻意
///   更早：若误落到第 4 环会反过来保 victim 挤 keeper、断言即红，反证决胜
///   确实发生在 importance 环；
/// - (c) created_at 环：全员 importance 与 access_log 完全相同 → 前三环逐位
///   打平（effective 差异被 floor 钳平且 grace 在 400 天量级已舍入归零，
///   promotion_score 不含 grace），由第 4 环 created_at 决胜：更早创建者
///   保留，最晚创建的 young 被挤出。
fn inv18(cfg: &EngramConfig) -> Guardrail {
    let cap = cfg.l1.capacity;
    let old = days_ago(400.0);

    // (a) 基准集合：loser 的 importance 最低（0.2）且 created_at 最新 → 按链应被挤出。
    //     三种构造顺序：原序（loser 在首）、逆序、旋转——排除稳定排序插入序的干扰。
    let mut base: Vec<Memory> = vec![make_memory(
        "loser",
        Level::L1,
        None,
        0.2,
        old + DAY,
        vec![old],
    )];
    for i in 0..cap {
        base.push(make_memory(
            &format!("k{i}"),
            Level::L1,
            None,
            0.5,
            old,
            vec![old],
        ));
    }
    let mut reversed = base.clone();
    reversed.reverse();
    let mut rotated = base.clone();
    let mid = 3 % rotated.len();
    rotated.rotate_left(mid);
    let a_out: Vec<Vec<String>> = [base, reversed, rotated]
        .into_iter()
        .map(|m| overflow_pushed(m, cfg))
        .collect();
    let a_ok = a_out.iter().all(|o| *o == ["loser".to_string()]);

    // (b) importance 决胜：keeper 单次访问 1.5 天前（activation<0，故
    //     imp(victim)=promotion(keeper)∈(0,0.5)，合法且严格小于 0.5）；
    //     victim 单次访问恰整 1 天前（activation=0）、created_at 更早（401 天）；
    //     填充条 imp=0.9 在第 2 环即胜出，把平局对决留给 keeper vs victim。
    let keeper = make_memory("keeper", Level::L1, None, 0.5, old, vec![days_ago(1.5)]);
    let victim_imp = promotion_score_with(&keeper, T, cfg);
    let victim = make_memory(
        "victim",
        Level::L1,
        None,
        victim_imp,
        old - DAY,
        vec![days_ago(1.0)],
    );
    let mut b_set = vec![keeper, victim];
    for i in 0..cap.saturating_sub(1) {
        b_set.push(make_memory(
            &format!("f{i}"),
            Level::L1,
            None,
            0.9,
            old,
            vec![days_ago(1.0)],
        ));
    }
    let mut b_rev = b_set.clone();
    b_rev.reverse();
    let b_out: Vec<Vec<String>> = [b_set, b_rev]
        .into_iter()
        .map(|m| overflow_pushed(m, cfg))
        .collect();
    let b_ok = b_out.iter().all(|o| *o == ["victim".to_string()]);

    // (c) created_at 决胜：全员 (imp 0.5, access [-400d]) 完全相同，唯 created_at
    //     不同——元老 401 天前创建，young 400 天前创建（最晚）→ 应被挤出。
    let mut c_set = vec![make_memory("young", Level::L1, None, 0.5, old, vec![old])];
    for i in 0..cap {
        c_set.push(make_memory(
            &format!("e{i}"),
            Level::L1,
            None,
            0.5,
            old - DAY,
            vec![old],
        ));
    }
    let mut c_rev = c_set.clone();
    c_rev.reverse();
    let c_out: Vec<Vec<String>> = [c_set, c_rev]
        .into_iter()
        .map(|m| overflow_pushed(m, cfg))
        .collect();
    let c_ok = c_out.iter().all(|o| *o == ["young".to_string()]);

    Guardrail {
        id: "INV-18",
        title: "溢出平局确定性",
        pass: a_ok && b_ok && c_ok,
        detail: format!(
            "(a)promotion环挤出={a_out:?}(应恒[loser]) (b)importance环挤出={b_out:?}(应恒[victim],imp={victim_imp:.4}) (c)created_at环挤出={c_out:?}(应恒[young])"
        ),
    }
}

/// INV-19 重要度门（新增）：imp0.85 的 L3 直升 L2 且绝不进 L1；imp0.75 不动；
/// 中层 imp0.85 即便 effective 跌破阈值也不被降回（防门/阈值乒乓）。
fn inv19(cfg: &EngramConfig) -> Guardrail {
    // (a) 底层高重要度 → 空降中层（且不越级进顶层）。
    let mut a = vec![make_memory(
        "g1",
        Level::L3,
        None,
        0.85,
        days_ago(10.0),
        vec![],
    )];
    consolidate_with(&mut a, T, cfg);
    let a_ok = a[0].level == Level::L2;

    // (b) 门下不动：imp 0.75 < gate。
    let mut b = vec![make_memory(
        "ng",
        Level::L3,
        None,
        0.75,
        days_ago(10.0),
        vec![],
    )];
    let ts_b = consolidate_with(&mut b, T, cfg);
    let b_ok = b[0].level == Level::L3 && ts_b.is_empty();

    // (c) 中层豁免降回：300 天未用、effective 跌破降级阈值，但 imp≥gate 不降。
    let mut c = vec![make_memory(
        "hi",
        Level::L2,
        None,
        0.85,
        days_ago(300.0),
        vec![days_ago(300.0)],
    )];
    let eff_c = effective_with(&c[0], T, cfg);
    consolidate_with(&mut c, T, cfg);
    let c_ok = c[0].level == Level::L2;

    Guardrail {
        id: "INV-19",
        title: "重要度门（空降/门下不动/豁免降回）",
        pass: a_ok && b_ok && c_ok,
        detail: format!(
            "0.85→{:?}(应L2) 0.75→{:?}(应L3) 中层0.85 eff={eff_c:.3}→{:?}(应留L2)",
            a[0].level, b[0].level, c[0].level
        ),
    }
}

/// INV-20 字符预算溢出（批5 容量 token 化）：条数远未满容也要被字符预算钳住。
///
/// - (a) 主断言：3 条长 cue 的 L2（条数 3 ≪ capacity），每条 cue 取
///   `B/2 − 100` 字符（B = `l2.char_budget`，构造保证 2 条累计 ≤ B、3 条
///   累计 > B）——溢出必须由**字符预算**（而非条数）触发，effective 最低者
///   被下推 L3，其余留 L2。计量用引擎 [`memory_render_cost`]（与热索引
///   渲染行同源，全系统唯一计数），前置断言验证构造未被参数漂移误伤。
/// - (b) 子断言：单条 cue 超预算（B + 500 字符）的 L2——保留优先序首条
///   即便单独吃穿预算也必须保留（每层至少 1 条，防长 cue 饿死整层），
///   且零变迁。
///
/// 敏感性：`l2_char_budget=0`（关预算）→ (a) 不再下推、翻红，题对坏参数敏感。
fn inv20(cfg: &EngramConfig) -> Guardrail {
    let budget = cfg.l2.char_budget;
    // (a) 3 条长 cue：importance 3.0/2.9/2.8 拉开 effective 排序；近期访问
    //     保证不触发阈值降级，虚拟视界下 promotion≈2.9 < promote_l1 不升 L1。
    let cue_len = (budget / 2).saturating_sub(100).max(50);
    let mut mems = Vec::new();
    for (i, imp) in [3.0, 2.9, 2.8].iter().enumerate() {
        let mut m = make_memory(
            &format!("w{i}"),
            Level::L2,
            None,
            *imp,
            days_ago(1.0),
            vec![days_ago(0.5)],
        );
        m.cue = "记".repeat(cue_len);
        mems.push(m);
    }
    let cost3: usize = mems.iter().map(|m| memory_render_cost(m, true)).sum();
    let cost2: usize = mems
        .iter()
        .take(2)
        .map(|m| memory_render_cost(m, true))
        .sum();
    // 前置：条数远未满容、3 条累计超预算、前 2 条在预算内（构造自检）。
    let pre_ok = mems.len() < cfg.l2.capacity && cost3 > budget && cost2 <= budget;
    consolidate_with(&mut mems, T, cfg);
    let a_ok = pre_ok
        && mems[0].level == Level::L2
        && mems[1].level == Level::L2
        && mems[2].level == Level::L3;

    // (b) 单条超预算仍保留（每层至少 1 条，防饿死）。
    let mut solo_m = make_memory(
        "h0",
        Level::L2,
        None,
        3.0,
        days_ago(1.0),
        vec![days_ago(0.5)],
    );
    solo_m.cue = "忆".repeat(budget + 500);
    let solo_cost = memory_render_cost(&solo_m, true);
    let mut solo = vec![solo_m];
    let ts_b = consolidate_with(&mut solo, T, cfg);
    let b_ok = solo_cost > budget
        && solo[0].level == Level::L2
        && solo[0].status == Status::Active
        && ts_b.is_empty();

    Guardrail {
        id: "INV-20",
        title: "字符预算溢出（token 治理）",
        pass: a_ok && b_ok,
        detail: format!(
            "(a)条数3<cap={} 累计={cost3}>B={budget} 前2条={cost2}≤B 结局={:?}/{:?}/{:?}(应L2/L2/L3) (b)单条={solo_cost}超B 结局={:?} 零变迁={}",
            cfg.l2.capacity,
            mems[0].level,
            mems[1].level,
            mems[2].level,
            solo[0].level,
            ts_b.is_empty()
        ),
    }
}
