//! 衰减与加固模型（懒计算）。
//!
//! 实现 ACT-R base-level activation 方程、冷启动宽限期（grace boost）
//! 以及二者合成的有效权重 [`effective`]。所有函数都接受 `now: f64`
//! （unix 秒）参数以便测试注入，不在内部读系统时间。
//!
//! 设计文档参考：§5 衰减与加固模型、§6 grace boost。

use crate::model::{EngramConfig, Memory, MIN_DT_DAYS, SECS_PER_DAY};
// `params` 重构后仅测试用到（生产路径改走 `cfg.tier`）；标 cfg(test) 免生产侧 unused 警告。
#[cfg(test)]
use crate::model::params;

/// 计算 base-level activation：`ln( Σⱼ (Δtⱼ_day)^(−d) )`。
///
/// `Σ` 项一举统一了频率与近因：每多一次真使用就多一个加项（频率越高分越高），
/// 越近的访问 Δt 越小、贡献越大（近因 boost）。
///
/// 每个 Δt（天）都取下限 [`MIN_DT_DAYS`] 以避免除零/无穷。
///
/// # 参数
/// - `access_log`：真使用时间戳列表（unix 秒）。
/// - `now`：当前时间（unix 秒）。
/// - `d`：该层衰减率。
///
/// # 返回
/// activation 的对数值。当 `access_log` 为空、或求和结果非正时，
/// 返回 [`f64::NEG_INFINITY`]（视为不可计算 / 已彻底衰减）。
pub fn activation(access_log: &[f64], now: f64, d: f64) -> f64 {
    if access_log.is_empty() {
        return f64::NEG_INFINITY;
    }
    let mut sum = 0.0_f64;
    for &t in access_log {
        // Δt（天），并取下限避免除零。
        let dt_day = ((now - t) / SECS_PER_DAY).max(MIN_DT_DAYS);
        // (Δt)^(−d)：越近的访问该项越大。
        sum += dt_day.powf(-d);
    }
    if sum <= 0.0 {
        return f64::NEG_INFINITY;
    }
    sum.ln()
}

/// 计算冷启动宽限期奖励：`B₀ · exp(−age/τ)`。
///
/// 给新记忆一段试用期，在 `effective` 上叠加随时间衰减的临时奖励，
/// 防止新记忆纯粹因为"新"而被秒杀（见设计文档 §6）。
///
/// `age` 取 `max(0.0)`，防止 `created_at` 在未来时出现负年龄。
///
/// # 参数
/// - `created_at`：记忆创建时间（unix 秒）。
/// - `now`：当前时间（unix 秒）。
pub fn grace_boost(created_at: f64, now: f64) -> f64 {
    grace_boost_with(created_at, now, &EngramConfig::default())
}

/// 同 [`grace_boost`]，但用给定 `cfg` 的 B₀/τ（整定 harness 扫参入口）。
pub fn grace_boost_with(created_at: f64, now: f64, cfg: &EngramConfig) -> f64 {
    let age_day = ((now - created_at) / SECS_PER_DAY).max(0.0);
    cfg.grace_b0 * (-age_day / cfg.grace_tau_days).exp()
}

/// 计算一条记忆的有效权重 `effective`。
///
/// 合成规则：
/// - 若 `m.pinned` 为 `true`，直接返回 [`f64::INFINITY`]（置顶豁免衰减）。
/// - 否则 `raw = importance + activation + grace_boost`，再对该层 floor 取上限：
///   `effective = max(floor, raw)`。
///
/// 当 `access_log` 为空时，用 `[created_at]` 当作隐式的首次访问，
/// 保证新记忆也能算出有限的 activation。
///
/// # 参数
/// - `m`：待评估的记忆。
/// - `now`：当前时间（unix 秒）。
pub fn effective(m: &Memory, now: f64) -> f64 {
    effective_with(m, now, &EngramConfig::default())
}

/// 同 [`effective`]，但用给定 `cfg` 的层参数与 grace（整定 harness 扫参入口）。
pub fn effective_with(m: &Memory, now: f64, cfg: &EngramConfig) -> f64 {
    // 置顶记忆永远排在最前，豁免一切衰减。
    if m.pinned {
        return f64::INFINITY;
    }
    let p = cfg.tier(m.level);
    // access_log 为空时，用创建时间作隐式首次访问。
    let implicit = [m.created_at];
    let log: &[f64] = if m.access_log.is_empty() {
        &implicit
    } else {
        &m.access_log
    };
    let raw = m.importance + activation(log, now, p.d) + grace_boost_with(m.created_at, now, cfg);
    raw.max(p.floor)
}

/// 计算一条记忆的“升级分”`promotion_score`。
///
/// 与 [`effective`] 的关键区别：**不含 grace boost、不对 floor 取上限**。
/// 即 `promotion_score = importance + activation`，是记忆靠真使用“挣来的分”，
/// 只用于爬升（升级）判断。
///
/// 设计意图：**爬升用 `promotion_score`（无 grace/floor），下跌与淘汰用
/// [`effective`]（含 grace/floor）**。于是 grace 和 floor 只起“防错杀”作用
/// ——防止新记忆/重要记忆被误降误淘汰——而**不**帮助它们越级上位：
/// 一条新记忆即便靠 grace 把 `effective` 抬得很高，也不会因此被升级，
/// 必须靠真正的使用频率与近因攒出 activation 才够格爬升。
///
/// 计算细节：
/// - 若 `m.pinned` 为 `true`，直接返回 [`f64::INFINITY`]（置顶恒在最前）。
/// - `access_log` 为空时，用 `[m.created_at]` 当作隐式首次访问。
/// - 衰减率 `d` 取 `params(m.level).d`。
///
/// # 参数
/// - `m`：待评估的记忆。
/// - `now`：当前时间（unix 秒）。
pub fn promotion_score(m: &Memory, now: f64) -> f64 {
    promotion_score_with(m, now, &EngramConfig::default())
}

/// 同 [`promotion_score`]，但用给定 `cfg` 的层参数（整定 harness 扫参入口）。
pub fn promotion_score_with(m: &Memory, now: f64, cfg: &EngramConfig) -> f64 {
    // 置顶记忆永远具备最高升级资格。
    if m.pinned {
        return f64::INFINITY;
    }
    let p = cfg.tier(m.level);
    // access_log 为空时，用创建时间作隐式首次访问。
    let implicit = [m.created_at];
    let log: &[f64] = if m.access_log.is_empty() {
        &implicit
    } else {
        &m.access_log
    };
    // 注意：不叠加 grace boost、不对 floor 取上限。
    m.importance + activation(log, now, p.d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Level, Pointer, Status, EVICT_THRESHOLD};

    /// 构造一条用于测试的记忆，便于各用例按需覆写字段。
    fn make_memory(level: Level, importance: f64, created_at: f64, access_log: Vec<f64>) -> Memory {
        Memory {
            id: "mem_test".to_string(),
            cue: "测试记忆".to_string(),
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

    /// 把"天数"换算成相对 now 的 unix 秒时间戳。
    fn days_ago(now: f64, days: f64) -> f64 {
        now - days * SECS_PER_DAY
    }

    // 1. 高频胜稀疏：访问次数多的应当 effective 更高。
    #[test]
    fn high_frequency_beats_sparse() {
        let now = 1_000_000_000.0;
        // A：5 次访问。
        let a = make_memory(
            Level::L3,
            0.0,
            days_ago(now, 5.0),
            vec![
                days_ago(now, 5.0),
                days_ago(now, 3.0),
                days_ago(now, 2.0),
                days_ago(now, 1.0),
                days_ago(now, 0.5),
            ],
        );
        // B：仅 1 次访问（5 天前）。
        let b = make_memory(Level::L3, 0.0, days_ago(now, 5.0), vec![days_ago(now, 5.0)]);
        assert!(
            effective(&a, now) > effective(&b, now),
            "高频记忆 effective 应高于稀疏记忆"
        );
    }

    // 2. 近因胜久远：同样单次访问，近的应更高。
    #[test]
    fn recency_beats_old() {
        let now = 1_000_000_000.0;
        let recent = make_memory(Level::L3, 0.0, days_ago(now, 0.5), vec![days_ago(now, 0.5)]);
        let old = make_memory(
            Level::L3,
            0.0,
            days_ago(now, 50.0),
            vec![days_ago(now, 50.0)],
        );
        assert!(
            effective(&recent, now) > effective(&old, now),
            "近因记忆 effective 应高于久远记忆"
        );
    }

    // 3. L1 floor 守得住：即便长期不访问也不掉破 floor。
    #[test]
    fn l1_floor_holds() {
        let now = 1_000_000_000.0;
        let m = make_memory(
            Level::L1,
            0.3,
            days_ago(now, 400.0),
            vec![days_ago(now, 400.0)],
        );
        let eff = effective(&m, now);
        assert!(eff >= 4.5, "L1 effective 应 >= floor 4.5，实得 {eff}");
        assert!(
            eff > EVICT_THRESHOLD,
            "L1 effective 应高于淘汰阈值，实得 {eff}"
        );
    }

    // 4. 随时间单调衰减：固定 access_log，时间越往后 effective 越低。
    #[test]
    fn monotonic_decay_over_time() {
        let base = 1_000_000_000.0;
        // 固定 access_log（相对 base 的若干天前），避免 floor 截断影响单调性，
        // 故 L3 的 floor 极低（-10），此处衰减区间不触底。
        let log = vec![
            days_ago(base, 5.0),
            days_ago(base, 3.0),
            days_ago(base, 1.0),
        ];
        let m = make_memory(Level::L3, 0.0, days_ago(base, 5.0), log);

        let e0 = effective(&m, base);
        let e30 = effective(&m, base + 30.0 * SECS_PER_DAY);
        let e100 = effective(&m, base + 100.0 * SECS_PER_DAY);
        assert!(e0 > e30, "now+0 应高于 now+30 天：{e0} vs {e30}");
        assert!(e30 > e100, "now+30 应高于 now+100 天：{e30} vs {e100}");
    }

    // 5. grace 衰减且抬升新记忆。
    #[test]
    fn grace_boost_behaviour() {
        let now = 1_000_000_000.0;
        // grace_boost(now, now) 约等于 B0 = 2.0。
        assert!(
            (grace_boost(now, now) - 2.0).abs() < 1e-9,
            "新生记忆 grace 应约等于 2.0"
        );
        // 30 天前创建，boost 应几乎归零。
        let old_boost = grace_boost(days_ago(now, 30.0), now);
        assert!(
            old_boost < 0.001,
            "30 天前的 grace 应 < 0.001，实得 {old_boost}"
        );

        // 新 L3 记忆：created_at = now，单次访问 = now。
        let m = make_memory(Level::L3, 0.2, now, vec![now]);
        let eff = effective(&m, now);
        // 去掉 grace 项的基线 = importance + activation。
        let baseline = m.importance + activation(&m.access_log, now, params(Level::L3).d);
        // effective 应比基线高约 2.0（即 grace_boost(now,now)）。
        assert!(
            ((eff - baseline) - 2.0).abs() < 1e-9,
            "新记忆 effective 应比无 grace 基线高约 2.0：eff={eff} baseline={baseline}"
        );
    }

    // 6. pinned 置顶：effective 为正无穷。
    #[test]
    fn pinned_is_infinite() {
        let now = 1_000_000_000.0;
        let mut m = make_memory(Level::L3, 0.5, now, vec![now]);
        m.pinned = true;
        assert_eq!(effective(&m, now), f64::INFINITY);
    }

    // 7. promotion_score 不含 grace：新生记忆的 promotion_score 应等于
    //    importance + activation，比同条件 effective 少了一个约 2.0 的 grace。
    #[test]
    fn promotion_score_excludes_grace() {
        let now = 1_000_000_000.0;
        let m = make_memory(Level::L3, 0.2, now, vec![now]);
        let baseline = m.importance + activation(&m.access_log, now, params(Level::L3).d);
        let ps = promotion_score(&m, now);
        assert!(
            (ps - baseline).abs() < 1e-9,
            "promotion_score 应等于 importance+activation（无 grace）：ps={ps} baseline={baseline}"
        );
        // effective 含约 2.0 的 grace，应明显高于 promotion_score。
        assert!(
            effective(&m, now) > ps + 1.5,
            "新记忆 effective 应因 grace 明显高于 promotion_score"
        );
    }

    // 8. promotion_score 不含 floor：长期不访问的 L1 记忆，effective 被 floor
    //    托在 4.5，但 promotion_score 应低于该 floor（未被托底）。
    #[test]
    fn promotion_score_excludes_floor() {
        let now = 1_000_000_000.0;
        let m = make_memory(
            Level::L1,
            0.3,
            days_ago(now, 400.0),
            vec![days_ago(now, 400.0)],
        );
        assert!(effective(&m, now) >= 4.5, "L1 effective 应被 floor 托住");
        assert!(
            promotion_score(&m, now) < 4.5,
            "promotion_score 不应被 floor 托底"
        );
    }

    // 9. pinned 的 promotion_score 为正无穷。
    #[test]
    fn promotion_score_pinned_is_infinite() {
        let now = 1_000_000_000.0;
        let mut m = make_memory(Level::L1, 0.0, now, vec![now]);
        m.pinned = true;
        assert_eq!(promotion_score(&m, now), f64::INFINITY);
    }
}
