//! 记忆原型库（oracle 侧）——打破循环论证的支点（方案 §4）。
//!
//! # ⚠ 铁律 1：oracle 与机制正交（本文件的硬约束）
//!
//! 本文件生成每条记忆隐藏的「未来真使用节律」。节律**只允许**用
//! 周期 + 抖动 + 泊松式稀疏三种原语生成，**绝不使用 ACT-R / floor / 阈值
//! / promotion_score / effective 等任何引擎公式**——否则就是让 engram 去
//! 拟合另一个 engram（循环论证）。engram 只看得到 access_log；
//! oracle 知道原型、据此判对错。
//!
//! 五种正交原型及混合占比（方案 §9.3 混合负载）：
//! Evergreen 10% / HotBug 30% / SeasonalRotation 5% / OneShotNoise 45% /
//! RefutedError 10%。

use crate::events::Reach;
use crate::questions::DAY;
use crate::rng::XorShift64Star;

/// 五种记忆原型（与引擎机制完全正交）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto {
    /// 长青身份/偏好：极低频、永不停、永不矛盾。
    Evergreen,
    /// 热点 bug：写入后短期密集真用 → 解决后骤停、再不出现。
    HotBug,
    /// 密钥轮换：长周期稀疏，但每次必真用（180 天窗口内按比例缩放周期）。
    SeasonalRotation,
    /// 一次性噪声：写入后 0~1 次即永久沉默。
    OneShotNoise,
    /// 被推翻的错知识：用几次 → 被纠正 → 之后偶发「差点重蹈覆辙」需要。
    RefutedError,
}

impl Proto {
    /// 原型的显示名（报告用）。
    pub fn name(self) -> &'static str {
        match self {
            Proto::Evergreen => "Evergreen",
            Proto::HotBug => "HotBug",
            Proto::SeasonalRotation => "SeasonalRotation",
            Proto::OneShotNoise => "OneShotNoise",
            Proto::RefutedError => "RefutedError",
        }
    }
}

/// 按混合占比抽一个原型（Evergreen 10% / HotBug 30% / Seasonal 5% /
/// OneShot 45% / Refuted 10%）。
pub fn sample_proto(rng: &mut XorShift64Star) -> Proto {
    let x = rng.next_f64();
    if x < 0.10 {
        Proto::Evergreen
    } else if x < 0.40 {
        Proto::HotBug
    } else if x < 0.45 {
        Proto::SeasonalRotation
    } else if x < 0.90 {
        Proto::OneShotNoise
    } else {
        Proto::RefutedError
    }
}

/// 一条带隐藏节律的原型实例：engram 只见 Write/Use/Correct 事件，
/// 本结构体的原型标签与节律参数只在 oracle（打分侧）可见。
pub struct ProtoInstance {
    /// 记忆 id。
    pub id: String,
    /// 隐藏原型。
    pub proto: Proto,
    /// 写入时的 importance。
    pub importance: f64,
    /// 写入时刻（unix 秒）。
    pub birth: f64,
    /// 未来真用时刻列表（升序）。
    pub uses: Vec<f64>,
    /// 纠正时刻列表（升序；仅 RefutedError 非空）。
    pub corrects: Vec<f64>,
}

/// 给定原型与出生时刻，生成隐藏节律（只用周期+抖动+泊松式稀疏，见文件头铁律）。
pub fn schedule(
    id: &str,
    proto: Proto,
    birth: f64,
    horizon_end: f64,
    rng: &mut XorShift64Star,
) -> ProtoInstance {
    let mut uses: Vec<f64> = Vec::new();
    let mut corrects: Vec<f64> = Vec::new();
    let importance;

    match proto {
        Proto::Evergreen => {
            // 极低频但永不停：周期 25~40 天，每步 ±20% 抖动。
            importance = rng.range_f64(0.55, 0.90);
            let period = rng.range_f64(25.0, 40.0);
            let mut t = birth;
            loop {
                t += period * rng.range_f64(0.8, 1.2) * DAY;
                if t >= horizon_end {
                    break;
                }
                uses.push(t);
            }
        }
        Proto::HotBug => {
            // 短期密集：3~10 天冲刺、每天 1~4 次真用，随后永久沉默。
            importance = rng.range_f64(0.15, 0.50);
            let burst_days = rng.range_usize(3, 10);
            for k in 0..burst_days {
                let n_today = rng.range_usize(1, 4);
                for _ in 0..n_today {
                    let t = birth + (k as f64 + rng.next_f64()) * DAY;
                    if t < horizon_end {
                        uses.push(t);
                    }
                }
            }
        }
        Proto::SeasonalRotation => {
            // 长周期稀疏但每次必真用：75~160 天周期（年周期按 180 天窗口缩放）。
            importance = rng.range_f64(0.30, 0.60);
            let period = rng.range_f64(75.0, 160.0);
            let mut t = birth;
            loop {
                t += period * rng.range_f64(0.9, 1.1) * DAY;
                if t >= horizon_end {
                    break;
                }
                uses.push(t);
            }
        }
        Proto::OneShotNoise => {
            // 0~1 次即沉默：50% 概率在头 2 天内被用一次。
            importance = rng.range_f64(0.05, 0.30);
            if rng.chance(0.5) {
                let t = birth + rng.range_f64(0.0, 2.0) * DAY;
                if t < horizon_end {
                    uses.push(t);
                }
            }
        }
        Proto::RefutedError => {
            // 先用 2~4 次（头 5~15 天内），随后 1~3 次纠正（第 10~30 天），
            // 之后仅偶发「差点重蹈覆辙」（60~120 天稀疏周期）。
            importance = rng.range_f64(0.20, 0.40);
            let n_early = rng.range_usize(2, 4);
            for _ in 0..n_early {
                let t = birth + rng.range_f64(0.5, 15.0) * DAY;
                if t < horizon_end {
                    uses.push(t);
                }
            }
            let n_corr = rng.range_usize(1, 3);
            for _ in 0..n_corr {
                let t = birth + rng.range_f64(10.0, 30.0) * DAY;
                if t < horizon_end {
                    corrects.push(t);
                }
            }
            let period = rng.range_f64(60.0, 120.0);
            let mut t = birth + 30.0 * DAY;
            loop {
                t += period * rng.range_f64(0.8, 1.2) * DAY;
                if t >= horizon_end {
                    break;
                }
                uses.push(t);
            }
        }
    }

    uses.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    corrects.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    ProtoInstance {
        id: id.to_string(),
        proto,
        importance,
        birth,
        uses,
        corrects,
    }
}

/// QUAL-04 的 oracle 判定：负载结束时刻 `t_end`，该原型的「应然可达档」
/// 与实际可达档是否匹配。
///
/// 判据只来自原型语义（写在方案 §9.3），与引擎公式无关：
/// - Evergreen：应仍在热层（Hot）；
/// - HotBug：冲刺结束 45 天以上 → 不应再占热层；仍在冲刺余温内 → 应 Hot；
/// - SeasonalRotation：绝不能被删（Gone 即错，Hot/Cold 皆可）；
/// - OneShotNoise：写入超 30 天 → 不应还赖在热层（Cold/Gone 皆可）；
/// - RefutedError：作为「差点重蹈覆辙」的防线，不能被删（Gone 即错）。
pub fn expected_end_ok(inst: &ProtoInstance, reach: Reach, t_end: f64) -> bool {
    match inst.proto {
        Proto::Evergreen => reach == Reach::Hot,
        Proto::HotBug => {
            let last = inst.uses.last().copied().unwrap_or(inst.birth);
            if t_end - last > 45.0 * DAY {
                reach != Reach::Hot
            } else {
                reach == Reach::Hot
            }
        }
        Proto::SeasonalRotation => reach != Reach::Gone,
        Proto::OneShotNoise => {
            if t_end - inst.birth > 30.0 * DAY {
                reach != Reach::Hot
            } else {
                true
            }
        }
        Proto::RefutedError => reach != Reach::Gone,
    }
}
