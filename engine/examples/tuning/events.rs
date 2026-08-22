//! 事件流与回放器：把 Write/Use/Correct/Tick/Query 事件序列应用到内存
//! `Vec<Memory>` 上（零磁盘零库）。
//!
//! 引擎侧行为一律调用 lib 本体：`consolidate_with`（Tick）、
//! `gc_should_delete`（Tick 末回收）、`revived_level`（冷记忆复活重入层）、
//! `recall_candidates`（Query）。harness 不复刻任何公式。
//!
//! `Correct` 的建模（方案 §6 待项目维护者拍板的取舍 1，此处按现行草案实现）：
//! **+1 次真使用 + importance 加 0.2（封顶 1.0）**；若目标是冷记忆，
//! 与 confirm-use 一样触发复活。

use std::collections::{BTreeMap, BTreeSet};

use engram::commands::{
    gc_should_delete, recall_candidates, revived_level, tokenize_query, RecallQuery,
};
use engram::consolidate::{consolidate_with, TransitionKind};
use engram::model::{EngramConfig, Level, Memory, Pointer, Status, MEMORY_SCHEMA_VERSION};

/// 一条记忆在某时刻的可达档（打分侧的三档粒度，方案 §9.3 QUAL-02）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// 热层可达（Active）：免费命中。
    Hot,
    /// 冷库可达（Cold）：要显式 recall 才捞得回。
    Cold,
    /// 已丢失（被 gc 删除，或 Superseded/Tombstone 不可召回）。
    Gone,
}

impl Reach {
    /// 可达度得分：Hot=1 / Cold=0.5 / Gone=0。
    pub fn score(self) -> f64 {
        match self {
            Reach::Hot => 1.0,
            Reach::Cold => 0.5,
            Reach::Gone => 0.0,
        }
    }

    /// 档位排序值（Hot > Cold > Gone），偏好题比较偏序用。
    pub fn rank(self) -> u8 {
        match self {
            Reach::Hot => 2,
            Reach::Cold => 1,
            Reach::Gone => 0,
        }
    }

    /// 显示名。
    pub fn name(self) -> &'static str {
        match self {
            Reach::Hot => "Hot",
            Reach::Cold => "Cold",
            Reach::Gone => "Gone",
        }
    }
}

/// 事件种类。
pub enum EventKind {
    /// 写入一条新记忆（隐藏原型在 oracle 侧，不进 Memory 本体）。
    Write {
        /// 初始层级。
        level: Level,
        /// 所属项目（L4 用；混合负载全为通用 None）。
        project: Option<String>,
        /// 写入时的 importance。
        importance: f64,
    },
    /// 真使用（confirm-use 语义）：追加 access_log；冷记忆复活重入普通层。
    Use,
    /// 纠正：+1 真使用 + importance+0.2 封顶 1.0（见模块头注释）。
    Correct,
    /// 推进时间：跑一次 `consolidate_with`，随后按 TTL 与门做 gc 回收。
    Tick,
    /// 词法召回观测：对 `query` 跑 `recall_candidates`，记录目标 id 是否命中。
    Query {
        /// 查询串（通常取该记忆自己的 cue）。
        query: String,
    },
}

/// 一个带时刻与目标 id 的事件（Tick 的 id 为空串）。
pub struct Event {
    /// 发生时刻（unix 秒）。
    pub t: f64,
    /// 目标记忆 id（Tick 不针对具体记忆，置空串）。
    pub id: String,
    /// 事件种类。
    pub kind: EventKind,
}

/// 事件排序键：同一时刻先 Write，再 Use/Correct/Query，最后 Tick（先落数据再巩固）。
fn kind_rank(kind: &EventKind) -> u8 {
    match kind {
        EventKind::Write { .. } => 0,
        EventKind::Use => 1,
        EventKind::Correct => 2,
        EventKind::Query { .. } => 3,
        EventKind::Tick => 4,
    }
}

/// 把事件序列按（时刻, 种类序）升序排序。
pub fn sort_events(events: &mut [Event]) {
    events.sort_by(|a, b| {
        a.t.partial_cmp(&b.t)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| kind_rank(&a.kind).cmp(&kind_rank(&b.kind)))
    });
}

/// 构造一条测试记忆（各题共用的骨架构造器）。
pub fn make_memory(
    id: &str,
    level: Level,
    project: Option<&str>,
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
        project: project.map(|s| s.to_string()),
        importance,
        pinned: false,
        access_log,
        status: Status::Active,
        superseded_by: None,
        created_at,
        tags: vec![],
        schema_version: MEMORY_SCHEMA_VERSION,
    }
}

/// 回放过程中累积的量化观测（质量题/回放题的数据源）。
#[derive(Default)]
pub struct ReplayStats {
    /// consolidate 执行次数（= Tick 数）。
    pub consolidations: usize,
    /// 全程 transition 总数（QUAL-05 thrash）。
    pub transitions_total: usize,
    /// 每次 Tick 后热索引渲染字符数序列（QUAL-01；仅 `record_render` 开时记录）。
    pub render_chars: Vec<f64>,
    /// 每个 Use 事件「前一刻」的 (id, 可达度得分)（QUAL-02）。
    pub pre_use: Vec<(String, f64)>,
    /// 每条记忆第一次转 Cold 的时刻（QUAL-03 噪声驻留）。
    pub first_cold: BTreeMap<String, f64>,
    /// 满容乒乓次数：importance≥gate 条目被溢出挤下后又被门抬回（QUAL-06；
    /// 计数口径为「任意后续场抬回」而非仅「下一场」，详见 `apply_tick` 内注释）。
    pub pingpong: usize,
    /// Query 观测结果：(id, 是否在 recall 候选中命中)。
    pub query_hits: Vec<(String, bool)>,
    /// 已被 gc 删除的记忆 id。
    pub gone: BTreeSet<String>,
}

/// 事件回放器：持有内存记忆集与被评估配置，逐事件推进。
pub struct Replayer {
    /// 被评估的参数配置。
    pub cfg: EngramConfig,
    /// 当前记忆集（gc 删除的条目会被移除并记入 `stats.gone`）。
    pub mems: Vec<Memory>,
    /// 量化观测累积。
    pub stats: ReplayStats,
    /// 是否在每次 Tick 后记录渲染字符数（QUAL-01 需要，其他场景省时关闭）。
    pub record_render: bool,
    /// QUAL-06 中间态：importance≥gate 且被溢出从中层挤到底层的 id。
    /// 集合无回合期限——进入后直到被门抬回（计乒乓并移除）为止一直保留。
    pushed_down: BTreeSet<String>,
}

impl Replayer {
    /// 以空记忆集构造。
    pub fn new(cfg: &EngramConfig) -> Self {
        Self::with_memories(cfg, Vec::new())
    }

    /// 以既有记忆集构造（回放题把 export 快照灌进来）。
    pub fn with_memories(cfg: &EngramConfig, mems: Vec<Memory>) -> Self {
        Replayer {
            cfg: *cfg,
            mems,
            stats: ReplayStats::default(),
            record_render: false,
            pushed_down: BTreeSet::new(),
        }
    }

    /// 按 id 查找当前记忆。
    pub fn find(&self, id: &str) -> Option<&Memory> {
        self.mems.iter().find(|m| m.id == id)
    }

    /// 某记忆此刻的可达档：gc 删除 → Gone；Active → Hot；Cold → Cold；
    /// Superseded/Tombstone 不参与召回，计 Gone。
    pub fn reach_of(&self, id: &str) -> Reach {
        if self.stats.gone.contains(id) {
            return Reach::Gone;
        }
        match self.find(id) {
            None => Reach::Gone,
            Some(m) => match m.status {
                Status::Active => Reach::Hot,
                Status::Cold => Reach::Cold,
                Status::Superseded | Status::Tombstone => Reach::Gone,
            },
        }
    }

    /// 顺序应用一批事件（调用方应先 [`sort_events`]）。
    pub fn run(&mut self, events: &[Event]) {
        for ev in events {
            self.apply(ev);
        }
    }

    /// 应用单个事件。
    fn apply(&mut self, ev: &Event) {
        match &ev.kind {
            EventKind::Write {
                level,
                project,
                importance,
            } => {
                self.mems.push(make_memory(
                    &ev.id,
                    *level,
                    project.as_deref(),
                    *importance,
                    ev.t,
                    vec![],
                ));
            }
            EventKind::Use => {
                // 「未来真用前一刻」的可达度采样必须发生在追加 access 之前。
                let reach = self.reach_of(&ev.id);
                self.stats.pre_use.push((ev.id.clone(), reach.score()));
                self.touch(&ev.id, ev.t, 0.0);
            }
            EventKind::Correct => {
                // 纠正 = 真使用 + importance 加成；不计入 QUAL-02 的
                // 「需要时刻」采样（纠正发生时该记忆是「错」而非「需要」）。
                self.touch(&ev.id, ev.t, 0.2);
            }
            EventKind::Tick => self.apply_tick(ev.t),
            EventKind::Query { query } => {
                let tokens = tokenize_query(query);
                let out = recall_candidates(&self.mems, &RecallQuery::new(&tokens, false, 10, ev.t));
                let hit = out.candidates.iter().any(|c| c.memory.id == ev.id);
                self.stats.query_hits.push((ev.id.clone(), hit));
            }
        }
    }

    /// 真使用落账：追加 access_log；冷记忆按引擎 `revived_level` 复活；
    /// `imp_delta` > 0 时叠加 importance（封顶 1.0，Correct 用）。
    fn touch(&mut self, id: &str, t: f64, imp_delta: f64) {
        if let Some(m) = self.mems.iter_mut().find(|m| m.id == id) {
            match m.status {
                Status::Active => {}
                Status::Cold => {
                    // confirm-use 复活语义：重入本轨道普通层（调 lib 本体）。
                    m.status = Status::Active;
                    m.level = revived_level(m.project.as_deref());
                }
                // 被取代/墓碑不再接受真使用。
                Status::Superseded | Status::Tombstone => return,
            }
            m.access_log.push(t);
            if imp_delta > 0.0 {
                m.importance = (m.importance + imp_delta).min(1.0);
            }
        }
    }

    /// Tick：consolidate（lib 本体）→ 变迁观测 → gc 回收 → 可选渲染量测。
    fn apply_tick(&mut self, t: f64) {
        let transitions = consolidate_with(&mut self.mems, t, &self.cfg);
        self.stats.consolidations += 1;
        self.stats.transitions_total += transitions.len();

        for tr in &transitions {
            // 满容乒乓观测（QUAL-06）口径：门级条目（importance≥gate）被溢出从中层
            // 挤下后，**任意后续场**再被重要度门抬回即计 1 次乒乓——`pushed_down`
            // 集合无回合期限，比方案文案「下一场又被门抬回」略宽。量级等价的理由：
            // 底层的重要度门判定只看 importance、不看分数，挤下的门级条目只要下一场
            // 仍 Active 就必然被抬回，「任意后续场」与「下一场」仅在该条目恰在中间
            // 被转 Cold / gc 打断、之后又被 recall 复活再抬回时才有差异——此时乒乓
            // 事实上仍发生（只是隔了几场），属可忽略的尾部计数偏差。
            let imp = self
                .mems
                .iter()
                .find(|m| m.id == tr.id)
                .map(|m| m.importance)
                .unwrap_or(0.0);
            let from_middle = matches!(tr.from, Level::L2 | Level::L4_2);
            let from_bottom = matches!(tr.from, Level::L3 | Level::L4_3);
            match tr.kind {
                TransitionKind::Overflow
                    if from_middle && tr.to_level.is_some() && imp >= self.cfg.importance_gate =>
                {
                    self.pushed_down.insert(tr.id.clone());
                }
                TransitionKind::Promote if from_bottom && self.pushed_down.remove(&tr.id) => {
                    self.stats.pingpong += 1;
                }
                _ => {}
            }
            // 首次转 Cold 时刻（QUAL-03 噪声驻留）。
            if tr.to_status == Some(Status::Cold) {
                self.stats.first_cold.entry(tr.id.clone()).or_insert(t);
            }
        }

        // gc 与门（lib 本体判定）：先收集应删 id，再统一移除。
        let doomed: Vec<String> = self
            .mems
            .iter()
            .filter(|m| {
                gc_should_delete(
                    m,
                    t,
                    self.cfg.ttl_days,
                    self.cfg.tombstone_ttl_days,
                    self.cfg.min_uses,
                )
            })
            .map(|m| m.id.clone())
            .collect();
        if !doomed.is_empty() {
            self.mems.retain(|m| !doomed.contains(&m.id));
            self.stats.gone.extend(doomed);
        }

        if self.record_render {
            // 热索引渲染量测（QUAL-01）：调 lib 渲染函数取全量热层字符数。
            // 渲染展示参数（load_full/容量标注）用引擎默认——层级归属已由
            // consolidate_with(cfg) 决定，展示分组不影响条目总量。
            let text = engram::render::render(&self.mems, t);
            self.stats.render_chars.push(text.chars().count() as f64);
        }
    }
}
