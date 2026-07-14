//! 回放题 REPLAY-01/02（held-out，**只验不调**；方案 §9.4 + 铁律 4）。
//!
//! 数据源：`engram export` 目录（批2新增命令，逐条 Memory JSON，一文件一记忆）。
//! 按 `--cutoff`（unix 秒）切分每条记忆的 access_log：cutoff 前作快照、
//! cutoff 后作「未来真用」。在 cutoff 时刻重建状态（consolidate + gc 判定），
//! 用未来真用检验「当时该留的有没有留」。
//!
//! ⚠ 幸存者偏差（铁律 4，输出里硬编码声明）：access_log 由**旧参数**产生，
//! 被旧策略早早淘汰、从没进过热层的记忆没有 access 记录 → 信号缺失，
//! 结果只当校准+验证集，**绝不当优化目标**。
//!
//! 重建近似（如实声明）：export 反映的是当前时点的 level/importance，
//! cutoff 时点的历史层级不可考，故快照沿用导出值并以一次 consolidate
//! 在 cutoff 收敛状态——这是有偏近似，同样只供验证参考。
//!
//! 三率口径补充（上述有偏近似的延伸）：future-used 记忆若在快照中为
//! Superseded/Tombstone，则 hot/recall/lost 三率的分子均不计入它
//! （非 Active 不算 hot、recall_candidates 只搜 Active+Cold、gc 未删则
//! 不算 lost），但它仍占三率共同的分母 `n_future_used`——故三率之和可不为 1。

use std::path::Path;

use engram::commands::gc_should_delete;
use engram::consolidate::consolidate_with;
use engram::model::{EngramConfig, Memory, Status};

use crate::events::{sort_events, Event, EventKind, Replayer};

/// 回放段的量化结果。
#[derive(serde::Serialize)]
pub struct ReplayOutcome {
    /// 快照条数（cutoff 前已存在的记忆）。
    pub n_snapshot: usize,
    /// 有未来真用的记忆条数（评分分母）。
    pub n_future_used: usize,
    /// REPLAY-01：未来真用者在 cutoff 时刻仍在热层的比例（免费命中）。
    pub hot_rate: f64,
    /// REPLAY-01：未来真用者可被词法 recall 命中的比例（至少搜得到）。
    pub recall_rate: f64,
    /// REPLAY-01：未来真用者已被 gc 删除的比例（丢失）。
    pub lost_rate: f64,
    /// REPLAY-02：cutoff 时已在冷库且有未来真用者的 recall 覆盖率。
    pub cold_recall_coverage: f64,
    /// REPLAY-02 的分母（冷库且未来真用的条数）。
    pub n_cold_future_used: usize,
    /// 硬编码的偏差声明（铁律 4）。
    pub bias_note: String,
}

/// 幸存者偏差声明（铁律 4：只验不调，必须打在输出里）。
const BIAS_NOTE: &str =
    "REPLAY 结果只验不调，含幸存者偏差：access_log 由旧参数产生，被旧策略早早淘汰者无未来真用信号，绝不当优化目标";

/// 执行真实回放：读 export 目录 → cutoff 切分 → 重建 → 量化三率 + 冷库覆盖。
///
/// 出错（目录不存在 / 无可解析记忆 / cutoff 不合理）返回中文错误说明。
pub fn run(dir: &Path, cutoff: f64, cfg: &EngramConfig) -> Result<ReplayOutcome, String> {
    let exported = load_export_dir(dir)?;
    if exported.is_empty() {
        return Err(format!(
            "export 目录 {} 里没有可解析的记忆 JSON",
            dir.display()
        ));
    }

    // 快照重建：cutoff 前已存在的记忆，access_log 截断到 cutoff 前；
    // Active/Cold 一律先置 Active，交由 consolidate 在 cutoff 收敛
    // （历史层级不可考的有偏近似，见模块头声明）；Superseded/Tombstone 原样保留。
    let mut snapshot: Vec<Memory> = Vec::new();
    for m in &exported {
        if m.created_at > cutoff {
            continue;
        }
        let mut s = m.clone();
        s.access_log.retain(|&t| t <= cutoff);
        if matches!(s.status, Status::Active | Status::Cold) {
            s.status = Status::Active;
        }
        snapshot.push(s);
    }
    if snapshot.is_empty() {
        return Err(format!(
            "cutoff={cutoff} 早于全部记忆的创建时间，无快照可回放"
        ));
    }
    consolidate_with(&mut snapshot, cutoff, cfg);

    // gc 判定（lib 本体）：cutoff 时刻应删者视为 Gone。
    let mut gone: Vec<String> = Vec::new();
    snapshot.retain(|m| {
        if gc_should_delete(
            m,
            cutoff,
            cfg.ttl_days,
            cfg.tombstone_ttl_days,
            cfg.min_uses,
        ) {
            gone.push(m.id.clone());
            false
        } else {
            true
        }
    });
    let n_snapshot = snapshot.len();

    // 未来真用清单：cutoff 后有 access 记录、且 cutoff 前已存在的记忆。
    let future_used: Vec<&Memory> = exported
        .iter()
        .filter(|m| m.created_at <= cutoff && m.access_log.iter().any(|&t| t > cutoff))
        .collect();
    let n_future = future_used.len();

    // 词法召回观测：以每条记忆自己的 cue 作 query，经事件回放器的 Query
    // 事件调 lib 的 recall_candidates（active+cold，top10）。
    let mut rp = Replayer::with_memories(cfg, snapshot);
    let mut queries: Vec<Event> = future_used
        .iter()
        .map(|m| Event {
            t: cutoff,
            id: m.id.clone(),
            kind: EventKind::Query {
                query: m.cue.clone(),
            },
        })
        .collect();
    sort_events(&mut queries);
    rp.run(&queries);
    let hit_of = |id: &str| {
        rp.stats
            .query_hits
            .iter()
            .find(|(qid, _)| qid == id)
            .map(|(_, hit)| *hit)
            .unwrap_or(false)
    };

    let mut hot = 0usize;
    let mut recallable = 0usize;
    let mut lost = 0usize;
    let mut cold_future = 0usize;
    let mut cold_hit = 0usize;
    for m in &future_used {
        let state = rp.find(&m.id).map(|s| s.status);
        match state {
            Some(Status::Active) => hot += 1,
            Some(Status::Cold) => {
                cold_future += 1;
                if hit_of(&m.id) {
                    cold_hit += 1;
                }
            }
            // 快照里不存在（被 gc 判删 / 被过滤）= 丢失。
            _ => {
                if gone.contains(&m.id) {
                    lost += 1;
                }
            }
        }
        if hit_of(&m.id) {
            recallable += 1;
        }
    }

    let denom = n_future.max(1) as f64;
    Ok(ReplayOutcome {
        n_snapshot,
        n_future_used: n_future,
        hot_rate: hot as f64 / denom,
        recall_rate: recallable as f64 / denom,
        lost_rate: lost as f64 / denom,
        cold_recall_coverage: cold_hit as f64 / cold_future.max(1) as f64,
        n_cold_future_used: cold_future,
        bias_note: BIAS_NOTE.to_string(),
    })
}

/// 打印回放结果（含硬编码的幸存者偏差声明）。
pub fn print(o: &ReplayOutcome) {
    println!("== 真实回放（REPLAY-01/02） ==");
    println!(
        "快照 {} 条；未来真用 {} 条（其中冷库 {} 条）",
        o.n_snapshot, o.n_future_used, o.n_cold_future_used
    );
    println!(
        "REPLAY-01 前瞻命中：hot_rate={:.4} recall_rate={:.4} lost_rate={:.4}",
        o.hot_rate, o.recall_rate, o.lost_rate
    );
    println!(
        "REPLAY-02 冷库召回覆盖：coverage={:.4}",
        o.cold_recall_coverage
    );
    // 铁律 4：偏差声明硬编码打印，谁也别删。
    println!("[警示] {}", o.bias_note);
}

/// 读 export 目录：逐个 `*.json` 反序列化为 [`Memory`]，解析失败的文件跳过并告警。
fn load_export_dir(dir: &Path) -> Result<Vec<Memory>, String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("读取 export 目录 {} 失败：{e}", dir.display()))?;
    let mut out = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("跳过读取失败的文件 {}：{e}", path.display());
                continue;
            }
        };
        match serde_json::from_str::<Memory>(&content) {
            Ok(m) => out.push(m),
            Err(e) => eprintln!("跳过解析失败的文件 {}：{e}", path.display()),
        }
    }
    Ok(out)
}
