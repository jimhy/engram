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

/// 升级判定的虚拟视界（天），默认值。语义见
/// [`EngramConfig::promotion_horizon_days`]。
pub const PROMOTION_HORIZON_DAYS: f64 = 1.0;

/// 重要度门阈值，默认值。语义见 [`EngramConfig::importance_gate`]。
pub const IMPORTANCE_GATE: f64 = 0.8;

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

/// 当前 [`Memory`] 序列化格式的 schema 版本（write 等新建路径写入此值）。
pub const MEMORY_SCHEMA_VERSION: u32 = 1;

/// **库级**数据版本：标记整库存量记忆是按哪一代引擎规则分布的。
///
/// 与 [`MEMORY_SCHEMA_VERSION`]（管**单条记忆的序列化字段布局**）正交——本常量管
/// **整库记忆的分布规则**：每当引擎改变了会影响存量记忆「该在哪一层 / 该不该常驻」
/// 的规则时（升降语义、字符预算、tie-break、门阈……），就 `+1`，并配套一次性
/// 数据迁移把老库按新规则重洗（见 `engram migrate` / [`crate::store::read_data_version`]）。
///
/// 版本语义：
/// - `1` = v0.8 原始规则（真实 now 采样、纯条数容量、无 importance 门、无 schema_version）；
/// - `2` = 虚拟视界 promotion_horizon、importance 门、字符预算双约束、显式 tie-break、
///   新增 `schema_version` 字段。老用户从 `1` 升到 `2` 后，库里仍是按旧规则分布的脏数据
///   （旧近因尖峰升上来的 L2、超新预算的层、缺 `schema_version` 的行），需迁移重洗。
///
/// 读取约定：老库无 meta 表 → 视为版本 `1`（见 [`crate::store::read_data_version`]）。
pub const ENGRAM_DATA_VERSION: u32 = 2;

/// serde 缺省值函数：旧数据行（无 `schema_version` 字段）一律按版本 1 解析。
fn default_schema_version() -> u32 {
    1
}

/// 返回某层级的 **v5 重要度层锚区间** `(下界, 上界)`（闭区间，取值均在 `0.0..=1.0`）。
///
/// 层锚是「某层记忆的 `importance` 理应落在的区间」的经验约定，用于**体检 / 迁移的
/// 语义性诊断**：`importance` 落在所属层区间之外，往往意味着这条记忆的重要度标注与
/// 其实际所在层不匹配（例如一条 `importance=0.1` 却身处 L1 的记忆）。
///
/// **纪律**：层锚偏离属**语义性**问题——迁移只把它收进报告，**绝不自动改** `importance`
/// 值（重要度是用户/复盘者的显式判断，机器无权篡改）。各层区间：
///
/// | 层    | 下界 | 上界 |
/// |-------|------|------|
/// | L1    | 0.85 | 1.00 |
/// | L2    | 0.55 | 0.80 |
/// | L3    | 0.00 | 0.40 |
/// | L4.1  | 0.70 | 0.90 |
/// | L4.2  | 0.55 | 0.75 |
/// | L4.3  | 0.30 | 0.50 |
pub fn importance_anchor_band(level: Level) -> (f64, f64) {
    match level {
        Level::L1 => (0.85, 1.0),
        Level::L2 => (0.55, 0.8),
        Level::L3 => (0.0, 0.4),
        Level::L4_1 => (0.7, 0.9),
        Level::L4_2 => (0.55, 0.75),
        Level::L4_3 => (0.3, 0.5),
    }
}

/// 一条记忆条目。
///
/// 一条记忆 = cue（一句话总结）+ 指针（指向 ground truth）。
/// 派生量 `activation` / `effective` 不存储，按需懒计算（见 [`crate::activation`]）。
///
/// # ⚠ 序列化演进政策（必读）
///
/// **今后所有新增字段必须 `#[serde(default)]`（或 `default = "..."`），禁止新增
/// 必填字段——库里的数据比代码活得久（tombstone TTL 10 年）。** 旧版引擎写下的
/// 记忆行会被今后任意新版引擎反序列化：任何没有缺省值的新字段都会让老行整条解析
/// 失败、被 [`crate::store::all`] 静默跳过，等价于**丢记忆**。版本号语义：
/// - 读：无 `schema_version` 字段的旧行按 1 解析（见 [`default_schema_version`]）；
/// - 写：新建记忆一律填 [`MEMORY_SCHEMA_VERSION`]；
/// - 若将来出现「无法用 default 表达」的不兼容变更，才允许提升
///   [`MEMORY_SCHEMA_VERSION`] 并配套写迁移逻辑——在那之前不要动它。
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
    /// 序列化格式版本：写入时填 [`MEMORY_SCHEMA_VERSION`]；旧数据行无此字段，
    /// 按 1 解析（见结构体文档的「序列化演进政策」）。
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
}

/// 某一层级的衰减/容量参数。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TierParams {
    /// 该层容量上限（条目数）。
    pub capacity: usize,
    /// 该层字符预算：本层全部常驻条目的**累计渲染字符数**上限（计量单位见
    /// [`crate::render::memory_render_cost`]，即热索引渲染行的 `char` 数；
    /// 换算参考：中文约 1.5~2 字符/token，英文约 4 字符/token）。
    ///
    /// 与 `capacity` 构成**双约束**：consolidate 溢出步按保留优先序取前缀，
    /// 直到「条数 ≤ capacity 且 累计渲染字符 ≤ char_budget」双满足，先触发者
    /// 生效；每层至少保留 1 条（防饿死）。`0` 表示不设字符预算（仅条数约束）。
    ///
    /// **分工**：容量治理管根源——consolidate 把每层常驻记忆的注入体量钳在
    /// 预算内，超出者级联下推/转 Cold（状态真的改变）；渲染预算
    /// [`crate::render::HOT_INDEX_CHAR_BUDGET`]（24000 字符）是保险丝——只截
    /// 显示、不动状态，仅在复盘者停摆、堆积未及巩固时兜底。
    ///
    /// **默认值推导**（见 [`EngramConfig::default`]）：L1=2000 / L2=6000 /
    /// L3=4000 / L4.1=2000 / L4.2=5000 / L4.3=3000，合计 22000 字符
    /// ≈ 8k~14k token，略低于渲染兜底预算 24000——正常状态下保险丝永不熔断。
    /// 记忆功能是要省 token 的：条数上限从此只是理论天花板，实际常驻规模由
    /// 字符预算按 token 治理（如 L3 名义 150 条，按典型 cue 行 60~90 字符
    /// 实际约容 45~65 条；L4.3 名义 200 条，实际约容 35~50 条）。
    pub char_budget: usize,
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
    /// 升级判定的虚拟视界（天）。
    ///
    /// 升级判定在**虚拟时刻** `now + promotion_horizon_days` 求
    /// `promotion_score`，即“**明天它还够格吗**”——刚发生的访问在 1 天视界下
    /// 瞬态贡献归零（Δt 从分钟级抬到 ≥1 天，近因尖峰被抹平），升级必须靠
    /// 跨日的真实使用历史攒出来。若无此视界，“刚用完/刚写完”时刻采样会让
    /// Δt 被钳到 1 分钟、activation 出现 ≈3.6 的瞬时尖峰，任何单次访问都能
    /// 越过升级阈值，“频率门”退化为“单次门”。
    ///
    /// 注意：**降级/淘汰判定仍用真实 `now`**——升降不对称是刻意设计
    /// （升级看“明天还够格吗”，降级看“现在已经不行了吗”）。
    pub promotion_horizon_days: f64,
    /// 重要度门阈值（设计文档 §6“第二扇门：重要度显式空降”）。
    ///
    /// Active、未 pinned、`importance >= importance_gate` 且当前在底层
    /// （L3/L4.3）的条目，在 consolidate 时**直接升至中层**（L2/L4.2），
    /// 不经 `promotion_score` 频率门。这解决了量纲失衡：importance ∈ `[0,1]`
    /// 对上 activation ±3.6 的动态范围，否则“你标了重要”几乎不影响层级。
    /// 重要度门**不**送顶层（L1 入口仍只有 pin/手动指定）；对应地，
    /// `importance >= importance_gate` 的中层条目豁免阈值降回底层，
    /// 避免“门抬上去→阈值打下来”的每场乒乓。
    pub importance_gate: f64,
}

impl Default for EngramConfig {
    /// 历来硬编码的实测候选值（设计文档 §14 列为待整定项，此处取本指令给定值）。
    fn default() -> Self {
        EngramConfig {
            // char_budget 默认值合计 22000 字符 ≈ 8k~14k token（推导与分工见
            // TierParams::char_budget 的 rustdoc），与渲染兜底预算 24000 对齐。
            l1: TierParams {
                capacity: 7,
                char_budget: 2000,
                d: 0.10,
                floor: 4.5,
                load_full: true,
            },
            l2: TierParams {
                capacity: 30,
                char_budget: 6000,
                d: 0.25,
                floor: 1.0,
                load_full: true,
            },
            l3: TierParams {
                capacity: 150,
                char_budget: 4000,
                d: 0.50,
                floor: -10.0,
                load_full: false,
            },
            l4_1: TierParams {
                capacity: 10,
                char_budget: 2000,
                d: 0.15,
                floor: 3.0,
                load_full: true,
            },
            l4_2: TierParams {
                capacity: 50,
                char_budget: 5000,
                d: 0.30,
                floor: 0.5,
                load_full: true,
            },
            l4_3: TierParams {
                capacity: 200,
                char_budget: 3000,
                d: 0.50,
                floor: -10.0,
                load_full: false,
            },
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
            promotion_horizon_days: PROMOTION_HORIZON_DAYS,
            importance_gate: IMPORTANCE_GATE,
        }
    }
}

impl EngramConfig {
    /// 取指定层级的 [`TierParams`]（容量 / 字符预算 / d / floor / 加载详略）。
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
    fn schema_version_defaults_to_one_for_legacy_rows() {
        // 旧数据行（无 schema_version 字段）必须照常解析，且版本按 1 补齐——
        // 这是「序列化演进政策」的核心兼容承诺（库里的数据比代码活得久）。
        let legacy = r#"{
            "id": "mem-legacy",
            "cue": "旧版引擎写下的记忆",
            "pointer": {"kind": "none", "reference": null, "detail": null},
            "level": "L2",
            "project": null,
            "importance": 0.5,
            "pinned": false,
            "access_log": [1000.0],
            "status": "active",
            "superseded_by": null,
            "created_at": 1000.0,
            "tags": []
        }"#;
        let m: Memory = serde_json::from_str(legacy).expect("旧行应照常解析");
        assert_eq!(m.schema_version, 1, "无 schema_version 的旧行应按 1 解析");
        assert_eq!(m.id, "mem-legacy");

        // 往返：序列化后应携带 schema_version，且读回一致。
        let json = serde_json::to_string(&m).expect("序列化应成功");
        assert!(
            json.contains("\"schema_version\":1"),
            "序列化输出应含 schema_version 字段，实得：{json}"
        );
        let back: Memory = serde_json::from_str(&json).expect("回读应成功");
        assert_eq!(back, m, "往返应完全一致");
    }

    #[test]
    fn params_values() {
        let p = params(Level::L1);
        assert_eq!(p.capacity, 7);
        assert_eq!(p.char_budget, 2000);
        assert!((p.d - 0.10).abs() < 1e-12);
        assert!((p.floor - 4.5).abs() < 1e-12);
        assert!(p.load_full);

        let p3 = params(Level::L3);
        assert_eq!(p3.capacity, 150);
        assert_eq!(p3.char_budget, 4000);
        assert!(!p3.load_full);
    }

    #[test]
    fn importance_anchor_bands_are_valid_ranges() {
        // 每层锚区间应满足 0 ≤ 下界 < 上界 ≤ 1。
        for level in [
            Level::L1,
            Level::L2,
            Level::L3,
            Level::L4_1,
            Level::L4_2,
            Level::L4_3,
        ] {
            let (lo, hi) = importance_anchor_band(level);
            assert!(
                (0.0..=1.0).contains(&lo) && (0.0..=1.0).contains(&hi) && lo < hi,
                "{level:?} 层锚区间不合法：({lo}, {hi})"
            );
        }
        // 抽查 L1 / L3 的具体端点（v5 约定）。
        assert_eq!(importance_anchor_band(Level::L1), (0.85, 1.0));
        assert_eq!(importance_anchor_band(Level::L3), (0.0, 0.4));
    }

    #[test]
    fn data_version_is_two() {
        // 当前库级数据版本应为 2（虚拟视界 + importance 门 + 字符预算 + tie-break）。
        assert_eq!(ENGRAM_DATA_VERSION, 2);
    }

    #[test]
    fn char_budget_defaults_align_with_render_fuse() {
        // 各层字符预算合计 22000，须低于渲染兜底预算 24000（容量治理管根源、
        // 渲染预算是保险丝——正常状态下保险丝永不熔断）。
        let cfg = EngramConfig::default();
        let total: usize = [cfg.l1, cfg.l2, cfg.l3, cfg.l4_1, cfg.l4_2, cfg.l4_3]
            .iter()
            .map(|t| t.char_budget)
            .sum();
        assert_eq!(total, 22000, "各层默认字符预算合计应为 22000");
        assert!(
            total < crate::render::HOT_INDEX_CHAR_BUDGET,
            "容量治理预算合计（{total}）应低于渲染兜底预算（{}）",
            crate::render::HOT_INDEX_CHAR_BUDGET
        );
    }
}
