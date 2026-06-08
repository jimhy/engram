//! 数据模型层。
//!
//! 定义 Engram 记忆系统的核心数据结构（[`Memory`]、[`Pointer`] 等），
//! 以及分层级别 [`Level`]、状态 [`Status`]，并提供按层级查询衰减/容量
//! 参数的 [`params`] 函数与全局常量。
//!
//! 设计文档参考：§5 衰减模型、§12 数据结构。

use serde::{Deserialize, Serialize};

/// 一天的秒数（用于把 unix 秒换算成天）。
pub const SECS_PER_DAY: f64 = 86400.0;

/// Δt 的下限（天），等于 1 分钟。
///
/// 对每个时间差取此下限，避免 `now == t` 时出现除零或无穷大。
pub const MIN_DT_DAYS: f64 = 1.0 / 1440.0;

/// 冷启动宽限期（grace boost）的初始幅度 B₀。
pub const GRACE_B0: f64 = 2.0;

/// 冷启动宽限期的时间常数 τ（天），控制 boost 衰减速度。
pub const GRACE_TAU_DAYS: f64 = 3.0;

/// 淘汰阈值。`effective` 低于此值的记忆被视为应淘汰。
pub const EVICT_THRESHOLD: f64 = -3.0;

/// 升 L2 的阈值：当一条 L3（或同构的 L4.3）记忆的 `promotion_score`
/// 大于等于此值时，升一级到 L2（或 L4.2）。
///
/// 与降级阈值之间留出死区（见 [`DEMOTE_L2`]），构成迟滞，防止边界抖动。
pub const PROMOTE_L2: f64 = 2.5;

/// 升 L1 的阈值：当一条 L2（或同构的 L4.2）记忆的 `promotion_score`
/// 大于等于此值时，升一级到 L1（或 L4.1）。
pub const PROMOTE_L1: f64 = 5.0;

/// 降到 L3 的阈值：当一条 L2（或同构的 L4.2）记忆的 `effective`
/// 小于此值时，降一级到 L3（或 L4.3）。
///
/// 注意降级用 `effective`（含 grace/floor），与升级用的 `promotion_score`
/// 不同；二者之间的间隔即迟滞死区。
pub const DEMOTE_L2: f64 = 1.5;

/// 降到 L2 的阈值：当一条 L1（或同构的 L4.1）记忆的 `effective`
/// 小于此值时，降一级到 L2（或 L4.2）。
///
/// 因 L1 的 floor 高达 4.5（见 [`params`]），正常情况下 `effective`
/// 不会跌破此值——这正是 L1“极难忘”的有意设计。
pub const DEMOTE_L1: f64 = 4.0;

/// 记忆层级。
///
/// L1-L3 为通用记忆（跨项目）；L4.1-L4.3 为项目级记忆，规则与 L1-L3 同构。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Level {
    /// 潜意识层：极难进、极难忘，全文常驻。
    L1,
    /// 重要层：难进、慢忘，全文常驻。
    L2,
    /// 普通层：易进、易忘，仅载一行 cue。
    L3,
    /// 项目潜意识层。
    #[serde(rename = "L4.1")]
    L4_1,
    /// 项目重要层。
    #[serde(rename = "L4.2")]
    L4_2,
    /// 项目普通层。
    #[serde(rename = "L4.3")]
    L4_3,
}

/// 记忆状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// 活跃：在热层正常参与升降级。
    Active,
    /// 冷藏：已降级到冷库，仅显式 recall 时被搜。
    Cold,
    /// 被取代：与新信息冲突，被新记忆推翻。
    Superseded,
    /// 墓碑：负知识，记录"曾认为 X，后被 Y 推翻"。
    Tombstone,
}

/// 指针：指向 ground truth，让"取细节"成为验证而非重建。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pointer {
    /// 指针种类：`file` | `doc` | `url` | `none`。
    pub kind: String,
    /// 引用位置：文件路径:行号 / 文档 id / url。无引用时为 `None`。
    pub reference: Option<String>,
    /// 可选细节正文：仅当无法从 artifact 恢复时才在此存储。
    pub detail: Option<String>,
}

/// 一条记忆条目。
///
/// 一条记忆 = cue（一句话总结）+ 指针（指向 ground truth）。
/// 派生量 `activation` / `effective` 不存储，按需懒计算（见 [`crate::activation`]）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    /// 全局唯一 id。
    pub id: String,
    /// 一句话总结，热索引里显示的就是它。
    pub cue: String,
    /// 指向 ground truth 的指针。
    pub pointer: Pointer,
    /// 所在层级。
    pub level: Level,
    /// 所属项目；L4 专用，通用记忆为 `None`。
    pub project: Option<String>,
    /// 创建时定的重要度，取值 `0.0..1.0`，是重要度门 / pin 的依据。
    pub importance: f64,
    /// 是否置顶。`true` 表示 importance 拉满、豁免衰减。
    pub pinned: bool,
    /// 真使用时间戳列表（unix 秒，升序）；懒计算 activation 用。
    pub access_log: Vec<f64>,
    /// 当前状态。
    pub status: Status,
    /// 若被取代，指向取代它的记忆 id。
    pub superseded_by: Option<String>,
    /// 创建时间（unix 秒）。
    pub created_at: f64,
    /// 标签，如 `intent` / `open-loop` / `dead-end`。
    pub tags: Vec<String>,
}

/// 某一层级的衰减/容量参数。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TierParams {
    /// 该层容量上限（条目数）。
    pub capacity: usize,
    /// 衰减率 d，越大衰减越快。
    pub d: f64,
    /// 地板值 `tier_floor`，`effective` 不低于此值。
    pub floor: f64,
    /// 是否全文常驻（`true` 显示细节，`false` 仅显示 cue）。
    pub load_full: bool,
}

/// 全部可整定参数的运行时配置（设计文档 §14 #1 列的待整定项汇成一处）。
///
/// 引入它的唯一目的，是把历来散落、硬编码的 6 类参数——各层容量 / 衰减率 d /
/// tier_floor / 升降迟滞阈值 / grace boost / TTL 硬删除——收拢成**一个可注入的值**，
/// 以便整定 harness 系统化扫描，并让生产侧将来可从配置加载。
///
/// **行为不变保证**：[`EngramConfig::default`] 即历来硬编码的实测候选值；所有不带
/// `cfg` 的旧签名函数（[`params`]、[`crate::activation::effective`]、
/// [`crate::consolidate::consolidate`] 等）都委托到 `default()`，故现有调用方与测试
/// 的行为与重构前**逐位一致**。需要扫参的新路径改调带 `_with` 后缀的同名函数并传入
/// 自定义 `EngramConfig`。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EngramConfig {
    /// L1 潜意识层参数。
    pub l1: TierParams,
    /// L2 重要层参数。
    pub l2: TierParams,
    /// L3 普通层参数。
    pub l3: TierParams,
    /// L4.1 项目潜意识层参数。
    pub l4_1: TierParams,
    /// L4.2 项目重要层参数。
    pub l4_2: TierParams,
    /// L4.3 项目普通层参数。
    pub l4_3: TierParams,
    /// 升 L2 / L4.2 的 `promotion_score` 阈值。
    pub promote_l2: f64,
    /// 升 L1 / L4.1 的 `promotion_score` 阈值。
    pub promote_l1: f64,
    /// 降到 L3 / L4.3 的 `effective` 阈值。
    pub demote_l2: f64,
    /// 降到 L2 / L4.2 的 `effective` 阈值。
    pub demote_l1: f64,
    /// 淘汰阈值：底层 `effective` 低于此值转 Cold。
    pub evict_threshold: f64,
    /// grace boost 初始幅度 B₀。
    pub grace_b0: f64,
    /// grace boost 时间常数 τ（天）。
    pub grace_tau_days: f64,
    /// gc 冷条目存活上限（天）。须与 `main.rs` 的 `gc --ttl-days` 缺省一致。
    pub ttl_days: f64,
    /// gc 墓碑存活上限（天）。须与 `main.rs` 的 `gc --tombstone-ttl-days` 缺省一致。
    pub tombstone_ttl_days: f64,
    /// gc「与门」第二臂阈值：真使用次数不超过此值才算「极少使用」。
    /// 须与 `main.rs` 的 `gc --min-uses` 缺省一致。
    pub min_uses: usize,
}

impl Default for EngramConfig {
    /// 历来硬编码的实测候选值（设计文档 §14 列为待整定项，此处取本指令给定值）。
    fn default() -> Self {
        EngramConfig {
            l1: TierParams { capacity: 7, d: 0.10, floor: 4.5, load_full: true },
            l2: TierParams { capacity: 30, d: 0.25, floor: 1.0, load_full: true },
            l3: TierParams { capacity: 150, d: 0.50, floor: -10.0, load_full: false },
            l4_1: TierParams { capacity: 10, d: 0.15, floor: 3.0, load_full: true },
            l4_2: TierParams { capacity: 50, d: 0.30, floor: 0.5, load_full: true },
            l4_3: TierParams { capacity: 200, d: 0.50, floor: -10.0, load_full: false },
            promote_l2: PROMOTE_L2,
            promote_l1: PROMOTE_L1,
            demote_l2: DEMOTE_L2,
            demote_l1: DEMOTE_L1,
            evict_threshold: EVICT_THRESHOLD,
            grace_b0: GRACE_B0,
            grace_tau_days: GRACE_TAU_DAYS,
            ttl_days: 180.0,
            tombstone_ttl_days: 3650.0,
            min_uses: 1,
        }
    }
}

impl EngramConfig {
    /// 取指定层级的 [`TierParams`]（容量 / d / floor / 加载详略）。
    pub fn tier(&self, level: Level) -> TierParams {
        match level {
            Level::L1 => self.l1,
            Level::L2 => self.l2,
            Level::L3 => self.l3,
            Level::L4_1 => self.l4_1,
            Level::L4_2 => self.l4_2,
            Level::L4_3 => self.l4_3,
        }
    }
}

/// 查询指定层级的参数（等价于 [`EngramConfig::default`] 的对应层）。
///
/// 历史签名，保留以兼容现有调用方与测试；需要自定义参数时改用
/// [`EngramConfig::tier`]。各层默认值见 [`EngramConfig::default`]。
pub fn params(level: Level) -> TierParams {
    EngramConfig::default().tier(level)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_serde_rename() {
        // L4 子层必须序列化成带点号的字符串。
        assert_eq!(serde_json::to_string(&Level::L4_1).unwrap(), "\"L4.1\"");
        assert_eq!(serde_json::to_string(&Level::L4_2).unwrap(), "\"L4.2\"");
        assert_eq!(serde_json::to_string(&Level::L4_3).unwrap(), "\"L4.3\"");
        assert_eq!(serde_json::to_string(&Level::L1).unwrap(), "\"L1\"");
        // 反序列化回来一致。
        let lv: Level = serde_json::from_str("\"L4.2\"").unwrap();
        assert_eq!(lv, Level::L4_2);
    }

    #[test]
    fn status_serde_lowercase() {
        assert_eq!(
            serde_json::to_string(&Status::Active).unwrap(),
            "\"active\""
        );
        assert_eq!(
            serde_json::to_string(&Status::Tombstone).unwrap(),
            "\"tombstone\""
        );
        let st: Status = serde_json::from_str("\"superseded\"").unwrap();
        assert_eq!(st, Status::Superseded);
    }

    #[test]
    fn params_values() {
        let p = params(Level::L1);
        assert_eq!(p.capacity, 7);
        assert!((p.d - 0.10).abs() < 1e-12);
        assert!((p.floor - 4.5).abs() < 1e-12);
        assert!(p.load_full);

        let p3 = params(Level::L3);
        assert_eq!(p3.capacity, 150);
        assert!(!p3.load_full);
    }
}
