//! 引擎命令的可测纯逻辑（write / recall / list 的算法内核）。
//!
//! 本模块只放**不做 IO、确定可测**的纯函数，供 `main.rs` 的 CLI 编排调用：
//! - [`parse_level`] / [`parse_status_filter`]：把 CLI 字符串解析为枚举；
//! - [`is_l4`]：层级是否属 L4 项目轨道（多库路由依据）；
//! - [`parse_project_dbs`]：把可重复的 `--project-db name=path` 解析为名称→路径映射；
//! - [`generate_id`]：无外部依赖（不引入 uuid/rand）的唯一 id 生成；
//! - [`parse_tags`]：逗号分隔标签解析；
//! - [`score_query`] / [`tokenize_query`]：recall 词法打分；
//! - [`recall_candidates`]：把一批记忆按 query 打分、过滤、排序、截断；
//! - [`list_visible`]：把一批记忆按 status/level/project 过滤后排序；
//! - [`revived_level`]：confirm-use 复活冷记忆时的重入层（通用→L3 / 项目→L4.3）；
//! - [`level_rank`]：层级重要度序（用于 merge 取最高层）；
//! - [`merge_level`] / [`merge_access_log`] / [`merge_tags`] / [`build_merged_memory`]：
//!   merge 命令的字段合并纯逻辑；
//! - [`last_touch`] / [`gc_should_delete`]：gc 命令的 TTL 判定纯逻辑。
//! - [`resolve_paths`] / [`home_dir`]：hook 辅助命令（session-start / resolve）的
//!   **路径解析约定**（纯计算，不碰文件系统），把"general 库 / project 库 /
//!   project 名"三者的推导收进引擎，让 hook 侧近乎零逻辑。
//!
//! 设计文档参考：§13 交付形态（多库分置）、§4 显式召回 recall、§7.4 TTL 硬删除。

use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use crate::activation::effective;
use crate::model::{Level, Memory, Pointer, Status};

/// 判断一个层级是否属于 L4 轨道（项目记忆）。
///
/// L4.1/L4.2/L4.3 属项目库；L1/L2/L3 属公共库。多库路由的唯一依据。
pub fn is_l4(level: Level) -> bool {
    matches!(level, Level::L4_1 | Level::L4_2 | Level::L4_3)
}

/// 把可重复的 `--project-db <name>=<path>` 原始参数解析为 `名称 → 路径` 映射。
///
/// 每个元素形如 `name=path`：取**第一个** `=` 左侧为项目名、右侧为该项目 L4
/// 库路径（路径中允许再含 `=`）。映射用 [`BTreeMap`] 以保证遍历顺序稳定
/// （按项目名升序，便于渲染与测试确定）。
///
/// # 参数
/// - `raw`：CLI 收集到的若干 `name=path` 串（可为空，得到空映射）。
///
/// # Errors
/// - 某元素不含 `=`（malformed）时返回错误说明。
/// - 项目名为空（如 `=path`）时返回错误说明。
/// - 同一项目名重复出现时返回错误说明（避免歧义路由）。
pub fn parse_project_dbs(raw: &[String]) -> Result<BTreeMap<String, PathBuf>, String> {
    let mut map: BTreeMap<String, PathBuf> = BTreeMap::new();
    for item in raw {
        let Some(eq) = item.find('=') else {
            return Err(format!(
                "无法解析 --project-db {item}（应为 name=path 形式，缺少 '='）"
            ));
        };
        let name = &item[..eq];
        let path = &item[eq + 1..];
        if name.is_empty() {
            return Err(format!("--project-db {item} 的项目名为空"));
        }
        if map.contains_key(name) {
            return Err(format!("--project-db 出现重复的项目名 {name}"));
        }
        map.insert(name.to_string(), PathBuf::from(path));
    }
    Ok(map)
}

/// hook 辅助命令解析出的三个路径/名称约定结果。
///
/// 由 [`resolve_paths`] 纯计算得出，供 `session-start` / `resolve` 子命令使用：
/// - `general_db`：公共库（L1/L2/L3 通用记忆）的 redb 文件路径；
/// - `project_db`：当前项目库（L4 记忆）的 redb 文件路径；
/// - `project_name`：当前项目名（用于 `--project-db name=path` 映射）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPaths {
    /// 公共库 redb 文件路径。
    pub general_db: PathBuf,
    /// 当前项目库 redb 文件路径。
    pub project_db: PathBuf,
    /// 当前项目名（`project_dir` 的最后一段目录名，取不到则为 `"root"`）。
    pub project_name: String,
}

/// 用注入式的环境变量查找闭包推导用户主目录（HOME）。
///
/// 优先取 `USERPROFILE`（Windows 主目录变量），缺省再取 `HOME`（类 Unix）。
/// 把"环境变量读取"做成可注入的闭包，是为了让 [`resolve_paths`] 可被纯单测覆盖：
/// 测试时传入假 env 查找，不依赖真实进程环境变量。
///
/// # 参数
/// - `env_lookup`：给定变量名返回其值（`Some`）或不存在（`None`）的查找闭包。
///   生产代码传入 `|k| std::env::var(k).ok()`。
///
/// # Errors
/// 当 `USERPROFILE` 与 `HOME` 都取不到（或都为空串）时，返回中文说明的错误字符串。
pub fn home_dir<F>(env_lookup: F) -> Result<PathBuf, String>
where
    F: Fn(&str) -> Option<String>,
{
    for key in ["USERPROFILE", "HOME"] {
        if let Some(v) = env_lookup(key) {
            if !v.is_empty() {
                return Ok(PathBuf::from(v));
            }
        }
    }
    Err("无法确定用户主目录：环境变量 USERPROFILE 与 HOME 均未设置".to_string())
}

/// 解析 hook 辅助命令所需的三条路径/名称约定（**纯计算，不碰文件系统**）。
///
/// 约定：
/// - `general_db` = `general_override` 若给定；否则 `<HOME>/.engram/general.redb`，
///   其中 HOME 由 [`home_dir`] 用注入的 `env_lookup` 推导（`USERPROFILE` 优先，
///   缺省再取 `HOME`）。两者都取不到时返回错误（不 panic、不 unwrap）。
/// - `project_db` = `<project_dir>/.claude/engram.redb`。
/// - `project_name` = `project_dir` 的最后一段目录名（[`Path::file_name`]）；取不到
///   （如根盘符 `C:\` / `/`）时回退为 `"root"`。
///
/// 本函数只拼路径、读 env，不创建目录、不打开库（便于纯单测）。确保父目录存在
/// 是调用方（CLI 编排层）的职责。
///
/// # 参数
/// - `project_dir`：当前项目根目录。
/// - `general_override`：`--general-db` 显式指定的公共库路径（`None` 时走默认约定）。
/// - `env_lookup`：环境变量查找闭包（生产传 `|k| std::env::var(k).ok()`）。
///
/// # Errors
/// 当 `general_override` 为 `None` 且 [`home_dir`] 无法确定主目录时，返回其错误说明。
pub fn resolve_paths<F>(
    project_dir: &Path,
    general_override: Option<&Path>,
    env_lookup: F,
) -> Result<ResolvedPaths, String>
where
    F: Fn(&str) -> Option<String>,
{
    let general_db = match general_override {
        Some(p) => p.to_path_buf(),
        None => {
            let home = home_dir(env_lookup)?;
            home.join(".engram").join("general.redb")
        }
    };

    let project_db = project_dir.join(".claude").join("engram.redb");

    let project_name = project_dir
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "root".to_string());

    Ok(ResolvedPaths {
        general_db,
        project_db,
        project_name,
    })
}

/// 把 CLI 传入的层级字符串解析为 [`Level`]。
///
/// 接受 `L1` / `L2` / `L3` / `L4.1` / `L4.2` / `L4.3`（大小写不敏感）。
///
/// # 参数
/// - `s`：层级字符串。
///
/// # Errors
/// 当字符串不是任一已知层级时，返回中文说明的错误字符串。
pub fn parse_level(s: &str) -> Result<Level, String> {
    match s.trim().to_ascii_uppercase().as_str() {
        "L1" => Ok(Level::L1),
        "L2" => Ok(Level::L2),
        "L3" => Ok(Level::L3),
        "L4.1" => Ok(Level::L4_1),
        "L4.2" => Ok(Level::L4_2),
        "L4.3" => Ok(Level::L4_3),
        other => Err(format!(
            "无法识别的层级 {other}（应为 L1|L2|L3|L4.1|L4.2|L4.3）"
        )),
    }
}

/// list 命令的状态过滤值：要么是某个具体 [`Status`]，要么是 `All`（不过滤）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFilter {
    /// 仅保留该具体状态。
    Only(Status),
    /// 不按状态过滤（保留全部状态）。
    All,
}

/// 把 CLI 传入的状态过滤字符串解析为 [`StatusFilter`]。
///
/// 接受 `active` / `cold` / `superseded` / `tombstone` / `all`（大小写不敏感）。
///
/// # 参数
/// - `s`：状态过滤字符串。
///
/// # Errors
/// 当字符串不是任一已知取值时，返回中文说明的错误字符串。
pub fn parse_status_filter(s: &str) -> Result<StatusFilter, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "active" => Ok(StatusFilter::Only(Status::Active)),
        "cold" => Ok(StatusFilter::Only(Status::Cold)),
        "superseded" => Ok(StatusFilter::Only(Status::Superseded)),
        "tombstone" => Ok(StatusFilter::Only(Status::Tombstone)),
        "all" => Ok(StatusFilter::All),
        other => Err(format!(
            "无法识别的状态 {other}（应为 active|cold|superseded|tombstone|all）"
        )),
    }
}

/// 为新记忆生成一个唯一 id：`mem-{now_nanos:x}-{hash:x}`。
///
/// `now_nanos` 取自传入的 `now`（unix 秒）换算的纳秒整数，`hash` 用标准库
/// [`DefaultHasher`] 对 `cue` 求得。不引入 uuid/rand 等外部依赖。
///
/// 同一纳秒内对同一 cue 会得到相同 id；调用方（write）以系统真实时间作 `now`，
/// 纳秒粒度下连续两次创建几乎必然不同纳秒，足以保证唯一。
///
/// # 参数
/// - `cue`：记忆的一句话总结，参与 hash。
/// - `now`：当前时间（unix 秒）。
pub fn generate_id(cue: &str, now: f64) -> String {
    // 把 unix 秒换算为纳秒整数（取非负，防御 now 为负的异常输入）。
    let now_nanos = (now.max(0.0) * 1_000_000_000.0) as u128;
    let mut hasher = DefaultHasher::new();
    cue.hash(&mut hasher);
    let hash = hasher.finish();
    format!("mem-{now_nanos:x}-{hash:x}")
}

/// 把逗号分隔的标签字符串解析为标签列表。
///
/// 按逗号切分、去除每段首尾空白、丢弃空段。`None` 或空串得到空列表。
///
/// # 参数
/// - `tags`：形如 `a,b,c` 的字符串，或 `None`。
pub fn parse_tags(tags: Option<&str>) -> Vec<String> {
    match tags {
        None => Vec::new(),
        Some(s) => s
            .split(',')
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .map(|t| t.to_string())
            .collect(),
    }
}

/// 把 query 按空白分词并小写化，返回去重后的词列表。
///
/// 用于 recall 打分：分词后逐词在记忆文本里做大小写不敏感子串匹配。
///
/// # 参数
/// - `query`：原始查询串。
pub fn tokenize_query(query: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for w in query.split_whitespace() {
        let lw = w.to_lowercase();
        if !lw.is_empty() && !seen.contains(&lw) {
            seen.push(lw);
        }
    }
    seen
}

/// 计算一条记忆对一组 query 词的命中分。
///
/// 分数 = 在该记忆的 `cue + tags(空格连接)`（统一小写）里，
/// **命中的不同 query 词数**（大小写不敏感子串匹配）。词已由
/// [`tokenize_query`] 去重，故每词至多贡献 1 分。
///
/// # 参数
/// - `m`：被打分的记忆。
/// - `tokens`：已分词、已小写、已去重的 query 词。
pub fn score_query(m: &Memory, tokens: &[String]) -> usize {
    let mut haystack = m.cue.to_lowercase();
    if !m.tags.is_empty() {
        haystack.push(' ');
        haystack.push_str(&m.tags.join(" ").to_lowercase());
    }
    tokens
        .iter()
        .filter(|t| haystack.contains(t.as_str()))
        .count()
}

/// 一条 recall 候选：记忆引用 + 算出的分数与 effective。
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    /// 命中的记忆。
    pub memory: &'a Memory,
    /// 命中的不同 query 词数。
    pub score: usize,
    /// 该记忆在 `now` 时刻的 effective（用于同分排序）。
    pub effective: f64,
}

/// 一条记忆在 recall 中是否“可被检索”。
///
/// superseded / tombstone 永不返回。`active_only` 为真时只允许 active；
/// 否则 active 与 cold 都允许（recall 的用途即“以前是否处理过 X”，冷库正是要搜的）。
fn recallable(status: Status, active_only: bool) -> bool {
    match status {
        Status::Active => true,
        Status::Cold => !active_only,
        Status::Superseded | Status::Tombstone => false,
    }
}

/// 在一批记忆里按 query 检索候选：过滤状态 → 打分 → 去零分 → 排序 → 截断。
///
/// 排序规则：先按 `score` 降序，同分再按 `effective(now)` 降序。
/// `score == 0`（一个 query 词都没命中）的记忆不进候选。
///
/// 本函数**不修改任何记忆、不做任何 IO**。
///
/// # 参数
/// - `mems`：候选记忆集合（通常是 general+project 合并集）。
/// - `tokens`：已由 [`tokenize_query`] 处理的 query 词。
/// - `active_only`：为真时只搜 active；否则 active+cold 都搜。
/// - `limit`：最多返回的候选数。
/// - `now`：当前时间（unix 秒）。
pub fn recall_candidates<'a>(
    mems: &'a [Memory],
    tokens: &[String],
    active_only: bool,
    limit: usize,
    now: f64,
) -> Vec<Candidate<'a>> {
    let mut cands: Vec<Candidate<'a>> = mems
        .iter()
        .filter(|m| recallable(m.status, active_only))
        .filter_map(|m| {
            let score = score_query(m, tokens);
            if score == 0 {
                None
            } else {
                Some(Candidate {
                    memory: m,
                    score,
                    effective: effective(m, now),
                })
            }
        })
        .collect();

    // 先 score 降序，同分按 effective 降序；NaN（不会出现）视作相等。
    cands.sort_by(|a, b| {
        b.score.cmp(&a.score).then_with(|| {
            b.effective
                .partial_cmp(&a.effective)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    });
    cands.truncate(limit);
    cands
}

/// 在一批记忆里按 status/level/project 过滤后，按 `effective(now)` 降序排序。
///
/// 过滤条件全部为“与”关系；`None` 表示该维度不过滤。
///
/// 本函数**不修改任何记忆、不做任何 IO**，返回引用，便于上层渲染或转 JSON。
///
/// # 参数
/// - `mems`：候选记忆集合。
/// - `status`：状态过滤（[`StatusFilter::All`] 不过滤）。
/// - `level`：层级过滤（`None` 不过滤）。
/// - `project`：项目过滤（`None` 不过滤；过滤时要求 `m.project == Some(project)`）。
/// - `now`：当前时间（unix 秒）。
pub fn list_visible<'a>(
    mems: &'a [Memory],
    status: StatusFilter,
    level: Option<Level>,
    project: Option<&str>,
    now: f64,
) -> Vec<&'a Memory> {
    let mut out: Vec<&'a Memory> = mems
        .iter()
        .filter(|m| match status {
            StatusFilter::All => true,
            StatusFilter::Only(s) => m.status == s,
        })
        .filter(|m| level.is_none_or(|lv| m.level == lv))
        .filter(|m| match project {
            None => true,
            Some(p) => m.project.as_deref() == Some(p),
        })
        .collect();

    out.sort_by(|a, b| {
        effective(b, now)
            .partial_cmp(&effective(a, now))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// 一条冷记忆被 `confirm-use` 复活时，从冷库重入到的普通层。
///
/// 复活不恢复其历史层级（那已是淘汰前的过去），而是按作用域回到本轨道的**普通层**：
/// - 通用记忆（`project == None`）→ [`Level::L3`]；
/// - 项目记忆（`project == Some(_)`）→ [`Level::L4_3`]。
///
/// 之后若它再次被高频使用，会经 consolidate 自然爬升。
///
/// # 参数
/// - `project`：该记忆的所属项目（`None` 为通用）。
pub fn revived_level(project: Option<&str>) -> Level {
    match project {
        None => Level::L3,
        Some(_) => Level::L4_3,
    }
}

/// 层级的“重要度序号”：序号越小越重要（越靠近顶层）。
///
/// 两条轨道按同构关系统一排序：L1 与 L4.1 同序、L2 与 L4.2 同序、L3 与 L4.3 同序。
/// 即重要度 `L1=L4.1 > L2=L4.2 > L3=L4.3`。merge 取“最重要的层”即取本序号最小者。
///
/// # 参数
/// - `level`：待排序的层级。
pub fn level_rank(level: Level) -> u8 {
    match level {
        Level::L1 | Level::L4_1 => 0,
        Level::L2 | Level::L4_2 => 1,
        Level::L3 | Level::L4_3 => 2,
    }
}

/// 计算 merge 合并记忆的层级：取所有源中**最重要的层**（[`level_rank`] 最小者）。
///
/// 源已由调用方校验为同作用域（要么全通用、要么全同一项目），故各源层级同轨道，
/// 取最高层后仍落在该轨道内。空源切片返回 `None`（调用方应已拦截空源）。
///
/// # 参数
/// - `sources`：合并的源记忆切片。
pub fn merge_level(sources: &[Memory]) -> Option<Level> {
    sources
        .iter()
        .min_by_key(|m| level_rank(m.level))
        .map(|m| m.level)
}

/// 计算 merge 合并记忆的重要度：取所有源 `importance` 的**最大值**。
///
/// 空源切片返回 `0.0`（调用方应已拦截空源）。
///
/// # 参数
/// - `sources`：合并的源记忆切片。
pub fn merge_importance(sources: &[Memory]) -> f64 {
    sources.iter().map(|m| m.importance).fold(0.0_f64, f64::max)
}

/// 计算 merge 合并记忆的 `access_log`：所有源 `access_log` 的**并集并升序**。
///
/// “并集”意味着频率累积：合并后的记忆继承全部源的真使用痕迹。完全相同的时间戳
/// （`f64` 位级相等）去重，避免人为放大频率。结果按升序排列，与 [`Memory::access_log`]
/// 的“升序”约定一致。
///
/// # 参数
/// - `sources`：合并的源记忆切片。
pub fn merge_access_log(sources: &[Memory]) -> Vec<f64> {
    let mut out: Vec<f64> = Vec::new();
    for m in sources {
        for &t in &m.access_log {
            // 用位级相等去重（同一时间戳只算一次），避免虚增频率。
            if !out.iter().any(|&x| x.to_bits() == t.to_bits()) {
                out.push(t);
            }
        }
    }
    out.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// 计算 merge 合并记忆的 `created_at`：取所有源中**最早**的创建时间。
///
/// 空源切片返回 `fallback`（调用方传入的 now，作为安全兜底）。
///
/// # 参数
/// - `sources`：合并的源记忆切片。
/// - `fallback`：源为空时的兜底时间（通常是 now）。
pub fn merge_created_at(sources: &[Memory], fallback: f64) -> f64 {
    sources
        .iter()
        .map(|m| m.created_at)
        .fold(None, |acc: Option<f64>, c| match acc {
            None => Some(c),
            Some(prev) => Some(prev.min(c)),
        })
        .unwrap_or(fallback)
}

/// 计算 merge 合并记忆的 `tags`：所有源 tags 的**并集**，再补一个 `"merged"` 标记。
///
/// 去重保持**首次出现顺序**（便于稳定输出与测试）。若源里已含 `"merged"`，不会重复添加。
///
/// # 参数
/// - `sources`：合并的源记忆切片。
pub fn merge_tags(sources: &[Memory]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in sources {
        for t in &m.tags {
            if !out.contains(t) {
                out.push(t.clone());
            }
        }
    }
    let merged = "merged".to_string();
    if !out.contains(&merged) {
        out.push(merged);
    }
    out
}

/// merge 命令的作用域校验结果：合并源的共同作用域。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeScope {
    /// 全部源为通用记忆（`project == None`）。
    General,
    /// 全部源同属一个项目。
    Project(String),
}

/// 校验 merge 的所有源是否同作用域：要么全 `project == None`，要么全同一 `Some(name)`。
///
/// 这是 merge 的硬前置：跨作用域合并会破坏“通用/项目分轨”不变量，必须拒绝。
///
/// # 参数
/// - `sources`：合并的源记忆切片（不应为空，空源由调用方先行拦截）。
///
/// # Errors
/// - 源为空时返回错误说明（无可合并对象）。
/// - 源混用通用与项目、或跨多个不同项目时返回错误说明。
pub fn validate_merge_scope(sources: &[Memory]) -> Result<MergeScope, String> {
    let Some(first) = sources.first() else {
        return Err("merge 失败：没有可合并的源记忆".to_string());
    };
    let scope = match &first.project {
        None => MergeScope::General,
        Some(p) => MergeScope::Project(p.clone()),
    };
    for m in &sources[1..] {
        let same = match (&scope, &m.project) {
            (MergeScope::General, None) => true,
            (MergeScope::Project(p0), Some(p)) => p0 == p,
            _ => false,
        };
        if !same {
            return Err(format!(
                "merge 失败：源记忆 {} 与其它源不在同一作用域（要么全部通用，要么全部同一项目）",
                m.id
            ));
        }
    }
    Ok(scope)
}

/// 构造 merge 的新合并记忆（纯逻辑，不做 IO、不生成 id）。
///
/// 字段来源（见切片 4b 指令）：
/// - `cue` = 传入的 `cue`；
/// - `level` = `level_override` 或源中最重要的层（[`merge_level`]）；
/// - `importance` = `importance_override` 或源中最大值（[`merge_importance`]）；
/// - `project` = 由作用域 `scope` 决定（通用为 `None`，项目为 `Some(name)`）；
/// - `access_log` = 所有源 access_log 并集并升序（[`merge_access_log`]）；
/// - `created_at` = 源中最早（[`merge_created_at`]，源空则取 `now`）；
/// - `pointer` = `{ kind: "none", reference: None, detail: None }`；
/// - `tags` = 源 tags 并集 + `"merged"`（[`merge_tags`]）；
/// - `status` = [`Status::Active`]；`pinned = false`；`superseded_by = None`。
///
/// `id` 由调用方填入（`--id` 或 [`generate_id`]），本函数把传入的 `id` 原样放入。
///
/// # 参数
/// - `id`：新记忆 id（调用方已决定）。
/// - `cue`：合并记忆的一句话总结。
/// - `sources`：合并的源记忆切片。
/// - `scope`：已校验的共同作用域。
/// - `level_override`：`--level` 指定的层级；`None` 时取源中最高层。
/// - `importance_override`：`--importance` 指定的重要度；`None` 时取源中最大值。
/// - `now`：当前时间（源为空时作 created_at 兜底）。
pub fn build_merged_memory(
    id: &str,
    cue: &str,
    sources: &[Memory],
    scope: &MergeScope,
    level_override: Option<Level>,
    importance_override: Option<f64>,
    now: f64,
) -> Memory {
    let level = level_override
        .or_else(|| merge_level(sources))
        .unwrap_or(Level::L3);
    let importance = importance_override.unwrap_or_else(|| merge_importance(sources));
    let project = match scope {
        MergeScope::General => None,
        MergeScope::Project(p) => Some(p.clone()),
    };
    Memory {
        id: id.to_string(),
        cue: cue.to_string(),
        pointer: Pointer {
            kind: "none".to_string(),
            reference: None,
            detail: None,
        },
        level,
        project,
        importance,
        pinned: false,
        access_log: merge_access_log(sources),
        status: Status::Active,
        superseded_by: None,
        created_at: merge_created_at(sources, now),
        tags: merge_tags(sources),
    }
}

/// 计算一条记忆的“最后触碰时间” `last_touch`：`access_log` 的最大值；为空则取 `created_at`。
///
/// gc 用它作 TTL 计时基准：任何 `recall`→`confirm-use` 给 `access_log` 追加新时间戳，
/// 都会刷新 `last_touch`、重置 TTL（见 [`gc_should_delete`] 的设计说明）。
///
/// # 参数
/// - `m`：待计算的记忆。
pub fn last_touch(m: &Memory) -> f64 {
    m.access_log
        .iter()
        .copied()
        .fold(None, |acc: Option<f64>, t| match acc {
            None => Some(t),
            Some(prev) => Some(prev.max(t)),
        })
        .unwrap_or(m.created_at)
}

/// 判定一条记忆是否应被 gc 硬删除（§7.4 TTL 硬删除）。
///
/// 规则（`age_days = (now - last_touch) / 86400`，[`last_touch`] 见上）：
/// - `status == Cold` 且 `age_days > ttl_days` → 删除；
/// - `status == Tombstone` 且 `age_days > tombstone_ttl_days` → 删除；
/// - `Active` / `Superseded` → **永不删**。
///
/// 设计说明：
/// - **冷条目以 age 为闸**：冷条目本就是被淘汰的低价值记忆，长期无人 `recall`
///   即可清理；任何 `recall`→`confirm-use` 会向 `access_log` 追加新时间戳、刷新
///   `last_touch`，从而重置 TTL 计时（重新被需要的记忆不会被误删）。
/// - **tombstone TTL 极长**：墓碑是“负知识”（曾认为 X、后被 Y 推翻），长期保留以
///   防重蹈覆辙，故其 TTL 远长于冷条目（缺省 3650 天 vs 180 天）。
/// - **Active/Superseded 永不删**：活跃记忆显然要留；superseded 仍是有效的指向链
///   （`superseded_by` 链路）的一环，由后续可能的 supersede→merge 流程接管，不在
///   gc 的硬删除范围内。
///
/// # 参数
/// - `m`：待判定的记忆。
/// - `now`：当前时间（unix 秒）。
/// - `ttl_days`：冷条目存活上限（天）。
/// - `tombstone_ttl_days`：墓碑存活上限（天）。
pub fn gc_should_delete(m: &Memory, now: f64, ttl_days: f64, tombstone_ttl_days: f64) -> bool {
    let age_days = (now - last_touch(m)) / crate::model::SECS_PER_DAY;
    match m.status {
        Status::Cold => age_days > ttl_days,
        Status::Tombstone => age_days > tombstone_ttl_days,
        Status::Active | Status::Superseded => false,
    }
}

// ============================================================================
// hot-index：动态按需挂载子项目 L4 的活跃子项目判定（纯逻辑，便于单测）
// ============================================================================

/// 判断一个 workspace-root 直接子目录名是否「可作为活跃子项目挂载」。
///
/// workspace-root 下的真实子目录里，有不少是工具/构建/版本控制的基础设施目录
/// （如 `.claude`、`.git`、`node_modules`、`target`），它们虽是真实子目录，但
/// 绝不是用户的「子项目」。真实工作区的 transcript 必然频繁触碰这些路径
/// （例如 `<root>/.claude/...`），若不排除，活跃子项目判定会被它们污染——把
/// `.claude` 当成活跃子项目而乱挂、状态乱跳。
///
/// 排除规则：满足以下任一即**不可挂载**（返回 `false`）：
/// - 名字以 `.` 开头（dotdir，如 `.claude`、`.git`、`.vscode`）；
/// - 名字在 denylist 内：`node_modules`、`target`、`dist`、`build`、`out`、`.`、`..`。
///
/// 其余名字（如 `engram`、`ai-2d-engine`）返回 `true`。
///
/// 本判定供三处复用：① prompt 信号候选（[`active_from_prompt`]）；② transcript
/// 信号候选（[`active_from_transcript`]）；③ `main.rs` 枚举可挂载子目录时的过滤。
///
/// # 参数
/// - `name`：待判定的子目录名（workspace-root 的某个直接子目录名）。
pub fn is_mountable_subproject_name(name: &str) -> bool {
    /// 即便不以 `.` 开头、也绝不作为子项目挂载的工具/构建目录名。
    const DENYLIST: &[&str] = &["node_modules", "target", "dist", "build", "out", ".", ".."];
    if name.starts_with('.') {
        return false;
    }
    !DENYLIST.contains(&name)
}

/// 判断某字符是否为「词/路径段」边界字符。
///
/// 用于信号 A（prompt 显式提及）的整词匹配：一个子目录名要被认作「被提及」，
/// 其在 prompt 文本里的出现两侧必须都是边界字符（或文本端点），避免把
/// `engram` 误命中在 `engrams` / `myengram` 之类的子串里。
///
/// 边界字符包括：ASCII 字母数字与下划线**之外**的一切字符（含路径分隔符
/// `/` `\`、空白、标点、CJK 等）。即：只有当两侧不是「标识符字符」时才算边界。
///
/// # 参数
/// - `c`：待判定字符。
fn is_word_boundary(c: char) -> bool {
    !(c.is_ascii_alphanumeric() || c == '_')
}

/// 在 `haystack` 中查找 `needle` 作为「完整词/路径段」**最后一次**出现的字节位置。
///
/// 「完整词」指该出现的左右两侧要么是文本端点，要么是 [`is_word_boundary`] 字符。
/// 返回最后一次合法出现的起始字节下标；从不合法出现（如仅作为更长标识符的子串）
/// 则返回 `None`。`needle` 为空时返回 `None`。
///
/// 用于信号 A：判断某子目录名是否作为完整词出现在 prompt 中、并取其位置以选「最后」。
///
/// # 参数
/// - `haystack`：被搜索文本（如 prompt）。
/// - `needle`：待查找的词（如子目录名）。
fn last_word_occurrence(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    let nbytes = needle.len();
    let mut last: Option<usize> = None;
    let mut search_from = 0usize;
    while let Some(rel) = haystack[search_from..].find(needle) {
        let start = search_from + rel;
        let end = start + nbytes;
        // 左边界：start 处的前一个字符（若有）须为边界字符。
        let left_ok = start == 0
            || haystack[..start]
                .chars()
                .next_back()
                .is_some_and(is_word_boundary);
        // 右边界：end 处的后一个字符（若有）须为边界字符。
        let right_ok =
            end >= haystack.len() || haystack[end..].chars().next().is_some_and(is_word_boundary);
        if left_ok && right_ok {
            last = Some(start);
        }
        // 继续向后找下一处（步进 1 字节避免死循环；needle 至少 1 字节）。
        search_from = start + 1;
        if search_from >= haystack.len() {
            break;
        }
    }
    last
}

/// 信号 A：从 prompt 文本里挑出**最后被显式提及**的子目录名。
///
/// 对每个候选子目录名做「完整词」匹配，在 prompt 里找其作为完整词
/// 的最后出现位置；在所有「确有出现」的候选中，取**出现位置最靠后**者。位置相同
/// （理论上不会，因 needle 非空且互不为同一字符串起点）时取候选列表中靠后者。
///
/// # 参数
/// - `prompt`：用户本次 prompt 文本。
/// - `subdirs`：workspace_root 的直接子目录名列表。
///
/// # 返回
/// 最后被提及的子目录名（`Some`），或都未被提及（`None`）。
pub fn active_from_prompt(prompt: &str, subdirs: &[String]) -> Option<String> {
    let mut best: Option<(usize, &String)> = None;
    for name in subdirs {
        // 排除 dotdir / 基础设施目录（`.claude`、`.git`、`node_modules` 等）：
        // 它们虽是真实子目录，但绝不作为活跃子项目。
        if !is_mountable_subproject_name(name) {
            continue;
        }
        if let Some(pos) = last_word_occurrence(prompt, name) {
            match best {
                Some((bpos, _)) if bpos >= pos => {}
                _ => best = Some((pos, name)),
            }
        }
    }
    best.map(|(_, name)| name.clone())
}

/// 信号 B：从 transcript 文本里挑出**最近被触碰**的子目录名。
///
/// 在 transcript 整体文本里寻找形如「`<workspace_root><分隔符><段名>」的出现，
/// 分隔符同时匹配 `/`、`\`、`\\`（后者是 JSON 字符串里反斜杠的转义形态）。取
/// **最后一次**出现所对应的段名。段名以下一个「非段字符」（路径分隔符、引号、
/// 空白等）为界截取。
///
/// 实现细节：
/// - workspace_root 自身的路径分隔符差异（`/` vs `\` vs `\\`）也会被归一比较，
///   即先把 transcript 与 root 都按「分隔符归一」后再做前缀匹配，使
///   `F:/ClaudeWorkspaces` 能匹配到 transcript 里写成 `F:\\ClaudeWorkspaces`
///   的同一路径。
/// - 只认 root 之后**紧跟一个分隔符**再接段名的形态；段名为空（root 后直接是
///   另一个分隔符或结束）则跳过该处。
///
/// # 参数
/// - `transcript`：transcript 文件的整体文本。
/// - `workspace_root`：工作区根目录路径字符串。
/// - `subdirs`：workspace_root 的直接子目录名列表（用于校验段名确为真实子目录）。
///
/// # 返回
/// 最后被触碰、且确为真实子目录的段名（`Some`），或无任何命中（`None`）。
pub fn active_from_transcript(
    transcript: &str,
    workspace_root: &str,
    subdirs: &[String],
) -> Option<String> {
    // 把任意路径分隔符序列（`/` `\` `\\`）归一为单个 '/'，便于跨写法匹配。
    let norm = normalize_separators(transcript);
    let root_norm = normalize_separators(workspace_root);
    // root 归一后去掉尾部分隔符，统一在其后强制要求一个 '/'。
    let root_trimmed = root_norm.trim_end_matches('/');
    if root_trimmed.is_empty() {
        return None;
    }

    let needle = format!("{root_trimmed}/");
    let mut last: Option<String> = None;
    let mut from = 0usize;
    while let Some(rel) = norm[from..].find(&needle) {
        let seg_start = from + rel + needle.len();
        // root 后紧跟的段：取到下一个「段终止符」（路径分隔符 '/'、引号、空白、
        // 或其它非段字符）为界。段名可含 '.' '-' '_' 等普通文件名字符，但不含
        // 分隔符与引号/空白——transcript 里路径常被引号或空白包裹。
        let rest = &norm[seg_start..];
        let seg_end = rest.find(is_segment_terminator).unwrap_or(rest.len());
        let seg = &rest[..seg_end];
        // 校验该段确为真实子目录名（精确相等，避免把更长名误判），且为「可挂载子项目」。
        // 关键：`.claude` / `.git` / `node_modules` 等段即便确是真实子目录也**跳过**，
        // **不更新** `last`——这样 `<root>/engram/y.rs` 之后又出现 `<root>/.claude/x`
        // 时，active 仍取剩余里最后触碰的 `engram`，而不会被 `.claude` 顶掉变 None。
        if is_mountable_subproject_name(seg) && subdirs.iter().any(|s| s == seg) {
            last = Some(seg.to_string());
        }
        from = seg_start.max(from + rel + 1);
        if from >= norm.len() {
            break;
        }
    }
    last
}

/// 判断某字符是否为「路径段终止符」：归一后的分隔符 `/`、引号、空白等。
///
/// 用于 [`active_from_transcript`] 从 root 之后截取段名：transcript 里路径常被
/// 双引号包裹或以空白/换行结尾，故这些字符都视作段的边界。
fn is_segment_terminator(c: char) -> bool {
    c == '/' || c == '"' || c == '\'' || c.is_whitespace() || c.is_control()
}

/// 把字符串里的路径分隔符序列（任意数量连续的 `/` 或 `\`）归一为单个 `/`。
///
/// 这样 `F:\\ClaudeWorkspaces\\engram`、`F:/ClaudeWorkspaces/engram`、
/// `F:\ClaudeWorkspaces\engram` 归一后都成 `F:/ClaudeWorkspaces/engram`，
/// 供 [`active_from_transcript`] 跨写法做前缀匹配。
///
/// # 参数
/// - `s`：原始文本。
fn normalize_separators(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_sep = false;
    for c in s.chars() {
        if c == '/' || c == '\\' {
            if !prev_sep {
                out.push('/');
                prev_sep = true;
            }
        } else {
            out.push(c);
            prev_sep = false;
        }
    }
    out
}

/// 综合信号 A / B，判定本次的活跃子项目名。
///
/// 优先级：信号 A（prompt 显式提及，[`active_from_prompt`]）优先；其次信号 B
/// （transcript 最近触碰，[`active_from_transcript`]）；都没有则 `None`。本函数
/// 只做「字符串/正则层面的判定」，不校验目录是否真实存在（真实性由调用方在
/// 拿到候选后用文件系统二次校验）；但信号 B 内部已要求段名属 `subdirs`。
///
/// 注意：信号 A 的候选 `subdirs` 也应是真实子目录列表（由调用方枚举得到），
/// 因此返回值已是「真实子目录名」级别的候选。
///
/// # 参数
/// - `prompt`：本次 prompt 文本（`None` 表示无 prompt）。
/// - `transcript`：transcript 整体文本（`None` 表示无 transcript）。
/// - `workspace_root`：工作区根目录路径字符串。
/// - `subdirs`：workspace_root 的直接子目录名列表。
///
/// # 返回
/// 活跃子项目名（`Some`）或无法判定（`None`）。
pub fn decide_active(
    prompt: Option<&str>,
    transcript: Option<&str>,
    workspace_root: &str,
    subdirs: &[String],
) -> Option<String> {
    if let Some(p) = prompt {
        if let Some(name) = active_from_prompt(p, subdirs) {
            return Some(name);
        }
    }
    if let Some(t) = transcript {
        if let Some(name) = active_from_transcript(t, workspace_root, subdirs) {
            return Some(name);
        }
    }
    None
}

/// 状态门控比较：本次 active 名与上次是否**相同**（相同则无需重注入）。
///
/// 把 `None`（无活跃子项目）归一为给定的 `root_name`，与状态文件里记录的上次名
/// （同样用 `root_name` 表示「无活跃子项目」）做相等比较。
///
/// # 参数
/// - `current`：本次判定出的活跃子项目名（`None` 表示无、即根）。
/// - `last`：状态文件里上次记录的名（`None` 表示状态文件不存在/为空）。
/// - `root_name`：根项目名（用于把「无活跃子项目」归一表示）。
///
/// # 返回
/// `true` 表示与上次相同（应输出空、不重注入）；`false` 表示有变化（应渲染）。
pub fn active_unchanged(current: Option<&str>, last: Option<&str>, root_name: &str) -> bool {
    let cur = current.unwrap_or(root_name);
    match last {
        Some(prev) => prev == cur,
        None => false,
    }
}

/// 把**当前挂载集**的 active 记忆分布压成一行紧凑状态串，供状态栏显示。
///
/// 状态栏要常显「Engram ● …」概况，又不能每次渲染都开 redb（慢且与 hook 抢
/// 文件锁），故让 hot-index 顺手把本函数算出的串落到一个状态小文件，状态栏只读它。
///
/// 串格式（固定前缀 + 通用三层 + 各项目一段）：
/// `● Engram | L1:<n1> L2:<n2> L3:<n3>`，若挂载集里出现了任何项目的 L4 active 记忆，
/// 再为**每个出现过的项目**追加一段 ` | <项目名>:<该项目 active 的 L4 总数>`，
/// 项目段按项目名升序排列。L4 各子层（L4.1/L4.2/L4.3）合并计数到其所属项目。
///
/// 只统计 `status == Active` 的记忆；其它状态（cold/superseded/tombstone）不计。
/// `now` 仅为接口一致而保留（本统计不依赖时间，纯按层级/项目计数），便于将来
/// 若要加入「按 effective 加权」之类扩展时无需改签名。
///
/// 本函数为**纯函数、无 IO、可单测**，使用 `●`（UTF-8 实心圆点）作状态指示。
///
/// # 参数
/// - `memories`：当前挂载集（公共库 + 各挂载项目库的合并记忆切片）。
/// - `now`：当前时间（unix 秒）；当前实现未用于计数，保留以稳定接口。
pub fn oneline_status(memories: &[Memory], now: f64) -> String {
    // now 暂未参与计数（纯按层级/项目计数），显式忽略以表意图、避免 unused 警告。
    let _ = now;

    // 通用三层（project == None）各自的 active 计数。
    let mut l1 = 0usize;
    let mut l2 = 0usize;
    let mut l3 = 0usize;
    // 各项目（project == Some(name)）的 active L4 总数（合并三个子层），按名升序。
    let mut projects: BTreeMap<String, usize> = BTreeMap::new();

    for m in memories {
        if m.status != Status::Active {
            continue;
        }
        match &m.project {
            None => match m.level {
                Level::L1 => l1 += 1,
                Level::L2 => l2 += 1,
                Level::L3 => l3 += 1,
                // 通用记忆按约定不会落在 L4 子层；防御性忽略，不计入任何段。
                Level::L4_1 | Level::L4_2 | Level::L4_3 => {}
            },
            Some(name) => {
                // 项目记忆只统计 L4 子层（合并计数）；其它层级（异常数据）忽略。
                if is_l4(m.level) {
                    *projects.entry(name.clone()).or_insert(0) += 1;
                }
            }
        }
    }

    let mut out = format!("● Engram | L1:{l1} L2:{l2} L3:{l3}");
    // BTreeMap 遍历即按项目名升序，逐个项目追加一段。
    for (name, count) in &projects {
        out.push_str(&format!(" | {name}:{count}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Pointer, Status};

    /// 构造一条用于测试的记忆。
    fn mem(id: &str, level: Level, project: Option<&str>, status: Status, cue: &str) -> Memory {
        Memory {
            id: id.to_string(),
            cue: cue.to_string(),
            pointer: Pointer {
                kind: "none".to_string(),
                reference: None,
                detail: None,
            },
            level,
            project: project.map(|s| s.to_string()),
            importance: 0.5,
            pinned: false,
            access_log: vec![1_000_000_000.0],
            status,
            superseded_by: None,
            created_at: 1_000_000_000.0,
            tags: vec![],
        }
    }

    #[test]
    fn parse_project_dbs_builds_map() {
        let raw = vec!["a=/tmp/pa.redb".to_string(), "b=/tmp/pb.redb".to_string()];
        let map = parse_project_dbs(&raw).expect("应能解析");
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("a"), Some(&PathBuf::from("/tmp/pa.redb")));
        assert_eq!(map.get("b"), Some(&PathBuf::from("/tmp/pb.redb")));
    }

    #[test]
    fn parse_project_dbs_empty_is_ok() {
        let map = parse_project_dbs(&[]).expect("空输入应得到空映射");
        assert!(map.is_empty());
    }

    #[test]
    fn parse_project_dbs_path_may_contain_eq() {
        // 仅按第一个 '=' 切分，路径里可再含 '='。
        let raw = vec!["proj=/tmp/a=b.redb".to_string()];
        let map = parse_project_dbs(&raw).expect("应能解析");
        assert_eq!(map.get("proj"), Some(&PathBuf::from("/tmp/a=b.redb")));
    }

    #[test]
    fn parse_project_dbs_malformed_errors() {
        // 无 '=' → 报错。
        assert!(parse_project_dbs(&["noeq".to_string()]).is_err());
        // 项目名为空 → 报错。
        assert!(parse_project_dbs(&["=/tmp/x.redb".to_string()]).is_err());
    }

    #[test]
    fn parse_project_dbs_duplicate_name_errors() {
        let raw = vec!["a=/tmp/x.redb".to_string(), "a=/tmp/y.redb".to_string()];
        assert!(parse_project_dbs(&raw).is_err(), "重复项目名应报错");
    }

    // ====================== hook 辅助命令：resolve_paths / home_dir 纯单测 ======================

    #[test]
    fn home_dir_prefers_userprofile_then_home() {
        // 同时给两者：取 USERPROFILE。
        let both = home_dir(|k| match k {
            "USERPROFILE" => Some("C:\\Users\\sea".to_string()),
            "HOME" => Some("/home/sea".to_string()),
            _ => None,
        })
        .expect("两者都有时应取 USERPROFILE");
        assert_eq!(both, PathBuf::from("C:\\Users\\sea"));

        // 只有 HOME：回退取 HOME。
        let only_home = home_dir(|k| {
            if k == "HOME" {
                Some("/home/sea".to_string())
            } else {
                None
            }
        })
        .expect("仅 HOME 时应取 HOME");
        assert_eq!(only_home, PathBuf::from("/home/sea"));
    }

    #[test]
    fn home_dir_missing_both_errors() {
        // 两者都取不到 → 错误。
        assert!(
            home_dir(|_| None).is_err(),
            "HOME 与 USERPROFILE 都缺应报错"
        );
        // 空串等同于未设置。
        assert!(
            home_dir(|_| Some(String::new())).is_err(),
            "空串应等同未设置而报错"
        );
    }

    #[test]
    fn resolve_paths_default_general_uses_injected_home() {
        // 注入假 USERPROFILE，不依赖真实环境变量。
        let env = |k: &str| {
            if k == "USERPROFILE" {
                Some("C:\\Users\\sea".to_string())
            } else {
                None
            }
        };
        let project_dir = Path::new("D:\\work\\engram");
        let r = resolve_paths(project_dir, None, env).expect("应能解析");

        // general_db = <HOME>/.engram/general.redb。
        assert_eq!(
            r.general_db,
            PathBuf::from("C:\\Users\\sea")
                .join(".engram")
                .join("general.redb"),
            "general_db 应为 <HOME>/.engram/general.redb"
        );
        // project_db = <project_dir>/.claude/engram.redb。
        assert_eq!(
            r.project_db,
            project_dir.join(".claude").join("engram.redb"),
            "project_db 应为 <project_dir>/.claude/engram.redb"
        );
        // project_name = 最后一段目录名。
        assert_eq!(r.project_name, "engram", "project_name 应为最后一段目录名");
    }

    #[test]
    fn resolve_paths_general_override_takes_precedence() {
        // 给了 general_override：忽略 HOME（env 闭包即便返回值也不应被用到）。
        let override_path = Path::new("E:\\custom\\g.redb");
        let project_dir = Path::new("D:\\work\\proj");
        let r = resolve_paths(project_dir, Some(override_path), |_| {
            Some("C:\\Users\\should_not_be_used".to_string())
        })
        .expect("应能解析");
        assert_eq!(
            r.general_db,
            PathBuf::from("E:\\custom\\g.redb"),
            "general_override 应优先于默认 HOME 约定"
        );
        assert_eq!(r.project_name, "proj");
    }

    #[test]
    fn resolve_paths_root_dir_name_falls_back_to_root() {
        // 根盘符无 file_name → project_name 回退 "root"。给 override 以免触发 HOME。
        let override_path = Path::new("E:\\g.redb");
        let root = Path::new("C:\\");
        let r = resolve_paths(root, Some(override_path), |_| None).expect("应能解析");
        assert_eq!(
            r.project_name, "root",
            "根盘符无最后一段目录名时应回退 root"
        );
        assert_eq!(
            r.project_db,
            root.join(".claude").join("engram.redb"),
            "project_db 仍按约定拼在根下"
        );
    }

    #[test]
    fn resolve_paths_missing_home_without_override_errors() {
        // 无 general_override 且 HOME/USERPROFILE 都缺 → 报错（不 panic）。
        let project_dir = Path::new("D:\\work\\engram");
        assert!(
            resolve_paths(project_dir, None, |_| None).is_err(),
            "默认约定下 HOME 缺失应返回错误"
        );
    }

    #[test]
    fn parse_level_ok_and_err() {
        assert_eq!(parse_level("l1").expect("应解析 L1"), Level::L1);
        assert_eq!(parse_level("L4.2").expect("应解析 L4.2"), Level::L4_2);
        assert_eq!(parse_level(" L3 ").expect("应去空白"), Level::L3);
        assert!(parse_level("L9").is_err(), "未知层级应报错");
    }

    #[test]
    fn parse_status_filter_ok_and_err() {
        assert_eq!(
            parse_status_filter("ACTIVE").expect("应解析 active"),
            StatusFilter::Only(Status::Active)
        );
        assert_eq!(
            parse_status_filter("all").expect("应解析 all"),
            StatusFilter::All
        );
        assert!(parse_status_filter("warm").is_err(), "未知状态应报错");
    }

    #[test]
    fn is_l4_classifies() {
        assert!(is_l4(Level::L4_1));
        assert!(is_l4(Level::L4_3));
        assert!(!is_l4(Level::L1));
        assert!(!is_l4(Level::L3));
    }

    #[test]
    fn generate_id_unique_across_time() {
        // 同 cue、不同 now（不同纳秒）应得到不同 id。
        let a = generate_id("同一句话", 1_000_000_000.0);
        let b = generate_id("同一句话", 1_000_000_000.5);
        assert_ne!(a, b, "不同纳秒应产生不同 id");
        assert!(a.starts_with("mem-"), "id 应以 mem- 前缀开头");
    }

    #[test]
    fn generate_id_differs_by_cue() {
        // 同 now、不同 cue 应（极大概率）得到不同 id（hash 不同）。
        let a = generate_id("cue 甲", 1_000_000_000.0);
        let b = generate_id("cue 乙", 1_000_000_000.0);
        assert_ne!(a, b, "不同 cue 应产生不同 id");
    }

    #[test]
    fn parse_tags_splits_and_trims() {
        assert_eq!(parse_tags(None), Vec::<String>::new());
        assert_eq!(parse_tags(Some("")), Vec::<String>::new());
        assert_eq!(
            parse_tags(Some(" a , b ,, c ")),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn tokenize_dedups_and_lowercases() {
        let t = tokenize_query("Redb Redb LOCK");
        assert_eq!(t, vec!["redb".to_string(), "lock".to_string()]);
    }

    #[test]
    fn score_counts_distinct_hits() {
        let mut m = mem("x", Level::L3, None, Status::Active, "redb 文件锁问题");
        m.tags = vec!["lock".to_string()];
        // query: "redb lock missing" → 命中 redb（cue）、lock（tag）= 2。
        let tokens = tokenize_query("redb lock missing");
        assert_eq!(score_query(&m, &tokens), 2);
    }

    #[test]
    fn recall_orders_by_score_then_effective() {
        let now = 1_000_000_000.0;
        let hit2 = mem("a", Level::L3, None, Status::Active, "redb lock 冲突");
        let hit1 = mem("b", Level::L3, None, Status::Active, "redb 入门");
        let miss = mem("c", Level::L3, None, Status::Active, "无关内容");
        let mems = vec![hit1, hit2, miss];
        let tokens = tokenize_query("redb lock");
        let cands = recall_candidates(&mems, &tokens, false, 10, now);
        assert_eq!(cands.len(), 2, "命中 2 条，miss 不进候选");
        assert_eq!(cands[0].memory.id, "a", "命中词多者排前");
        assert_eq!(cands[0].score, 2);
        assert_eq!(cands[1].memory.id, "b");
    }

    #[test]
    fn recall_limit_truncates() {
        let now = 1_000_000_000.0;
        let mems: Vec<Memory> = (0..5)
            .map(|i| mem(&format!("m{i}"), Level::L3, None, Status::Active, "redb"))
            .collect();
        let tokens = tokenize_query("redb");
        let cands = recall_candidates(&mems, &tokens, false, 3, now);
        assert_eq!(cands.len(), 3, "limit=3 应截断到 3 条");
    }

    #[test]
    fn recall_default_includes_cold_active_only_excludes() {
        let now = 1_000_000_000.0;
        let cold = mem("cold", Level::L3, None, Status::Cold, "redb 冷藏笔记");
        let active = mem("act", Level::L3, None, Status::Active, "redb 活跃笔记");
        let mems = vec![cold, active];
        let tokens = tokenize_query("redb");

        let default = recall_candidates(&mems, &tokens, false, 10, now);
        assert_eq!(default.len(), 2, "默认应搜到 cold + active");

        let only = recall_candidates(&mems, &tokens, true, 10, now);
        assert_eq!(only.len(), 1, "active-only 应排除 cold");
        assert_eq!(only[0].memory.id, "act");
    }

    #[test]
    fn recall_never_returns_superseded_or_tombstone() {
        let now = 1_000_000_000.0;
        let sup = mem("sup", Level::L3, None, Status::Superseded, "redb 被取代");
        let tomb = mem("tomb", Level::L3, None, Status::Tombstone, "redb 墓碑");
        let mems = vec![sup, tomb];
        let tokens = tokenize_query("redb");
        // 即便 active_only=false 也不返回。
        let cands = recall_candidates(&mems, &tokens, false, 10, now);
        assert!(
            cands.is_empty(),
            "superseded/tombstone 永不进候选，实得 {} 条",
            cands.len()
        );
    }

    #[test]
    fn list_filters_by_status_level_project() {
        let now = 1_000_000_000.0;
        let mems = vec![
            mem("a", Level::L1, None, Status::Active, "甲"),
            mem("b", Level::L3, None, Status::Cold, "乙"),
            mem("c", Level::L4_2, Some("engram"), Status::Active, "丙"),
            mem("d", Level::L4_2, Some("other"), Status::Active, "丁"),
        ];

        // 按状态 active：a、c、d。
        let by_status = list_visible(&mems, StatusFilter::Only(Status::Active), None, None, now);
        let mut ids: Vec<&str> = by_status.iter().map(|m| m.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["a", "c", "d"]);

        // 按层级 L4.2：c、d。
        let by_level = list_visible(&mems, StatusFilter::All, Some(Level::L4_2), None, now);
        let mut ids: Vec<&str> = by_level.iter().map(|m| m.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["c", "d"]);

        // 按项目 engram：只 c。
        let by_proj = list_visible(&mems, StatusFilter::All, None, Some("engram"), now);
        let ids: Vec<&str> = by_proj.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["c"]);
    }

    // ====================== 切片 4b：四个维护命令的纯逻辑单测 ======================

    /// 测试记忆构造规格（聚成结构体以规避 clippy 的 too_many_arguments）。
    struct MemSpec<'a> {
        id: &'a str,
        level: Level,
        project: Option<&'a str>,
        status: Status,
        importance: f64,
        created_at: f64,
        access_log: Vec<f64>,
        tags: Vec<&'a str>,
    }

    /// 按 [`MemSpec`] 构造一条可细粒度配置各字段的测试记忆。
    fn mem_full(spec: MemSpec<'_>) -> Memory {
        Memory {
            id: spec.id.to_string(),
            cue: format!("cue-{}", spec.id),
            pointer: Pointer {
                kind: "none".to_string(),
                reference: None,
                detail: None,
            },
            level: spec.level,
            project: spec.project.map(|s| s.to_string()),
            importance: spec.importance,
            pinned: false,
            access_log: spec.access_log,
            status: spec.status,
            superseded_by: None,
            created_at: spec.created_at,
            tags: spec.tags.into_iter().map(|s| s.to_string()).collect(),
        }
    }

    /// 便捷构造：仅指定 id/level/project/status/importance/created_at/access_log，
    /// tags 取空（需要 tags 的测试单独用 [`mem_full`]）。
    fn ms<'a>(
        id: &'a str,
        level: Level,
        project: Option<&'a str>,
        status: Status,
        importance: f64,
        created_at: f64,
        access_log: Vec<f64>,
    ) -> Memory {
        mem_full(MemSpec {
            id,
            level,
            project,
            status,
            importance,
            created_at,
            access_log,
            tags: vec![],
        })
    }

    // --- confirm-use 复活层 ---
    #[test]
    fn revived_level_general_is_l3_project_is_l43() {
        assert_eq!(revived_level(None), Level::L3, "通用复活应回 L3");
        assert_eq!(
            revived_level(Some("engram")),
            Level::L4_3,
            "项目复活应回 L4.3"
        );
    }

    // --- merge：层级重要度序 ---
    #[test]
    fn level_rank_orders_by_importance() {
        assert!(level_rank(Level::L1) < level_rank(Level::L2));
        assert!(level_rank(Level::L2) < level_rank(Level::L3));
        // 两轨道同构同序。
        assert_eq!(level_rank(Level::L1), level_rank(Level::L4_1));
        assert_eq!(level_rank(Level::L3), level_rank(Level::L4_3));
    }

    // --- merge：取最重要的层 ---
    #[test]
    fn merge_level_picks_highest() {
        let now = 1_000_000_000.0;
        let sources = vec![
            ms("s1", Level::L3, None, Status::Active, 0.1, now, vec![now]),
            ms("s2", Level::L1, None, Status::Active, 0.2, now, vec![now]),
            ms("s3", Level::L2, None, Status::Active, 0.3, now, vec![now]),
        ];
        assert_eq!(
            merge_level(&sources),
            Some(Level::L1),
            "应取源中最重要的层 L1"
        );
        assert_eq!(merge_level(&[]), None, "空源应返回 None");
    }

    // --- merge：取最大重要度 ---
    #[test]
    fn merge_importance_picks_max() {
        let now = 1_000_000_000.0;
        let sources = vec![
            ms("s1", Level::L3, None, Status::Active, 0.2, now, vec![now]),
            ms("s2", Level::L3, None, Status::Active, 0.9, now, vec![now]),
            ms("s3", Level::L3, None, Status::Active, 0.5, now, vec![now]),
        ];
        assert!(
            (merge_importance(&sources) - 0.9).abs() < 1e-12,
            "应取源中最大 importance 0.9"
        );
    }

    // --- merge：access_log 并集并升序、去重 ---
    #[test]
    fn merge_access_log_unions_dedups_sorts() {
        let sources = vec![
            ms(
                "s1",
                Level::L3,
                None,
                Status::Active,
                0.0,
                0.0,
                vec![3.0, 1.0],
            ),
            ms(
                "s2",
                Level::L3,
                None,
                Status::Active,
                0.0,
                0.0,
                vec![2.0, 3.0],
            ),
        ];
        // 并集 {1,2,3}（3 去重），升序。
        assert_eq!(merge_access_log(&sources), vec![1.0, 2.0, 3.0]);
    }

    // --- merge：created_at 取最早 ---
    #[test]
    fn merge_created_at_picks_earliest() {
        let sources = vec![
            ms("s1", Level::L3, None, Status::Active, 0.0, 500.0, vec![]),
            ms("s2", Level::L3, None, Status::Active, 0.0, 100.0, vec![]),
            ms("s3", Level::L3, None, Status::Active, 0.0, 300.0, vec![]),
        ];
        assert!((merge_created_at(&sources, 9999.0) - 100.0).abs() < 1e-12);
        // 空源用 fallback。
        assert!((merge_created_at(&[], 42.0) - 42.0).abs() < 1e-12);
    }

    // --- merge：tags 并集 + merged 标记，且不重复添加 merged ---
    #[test]
    fn merge_tags_unions_and_appends_merged() {
        let now = 1_000_000_000.0;
        let sources = vec![
            mem_full(MemSpec {
                id: "s1",
                level: Level::L3,
                project: None,
                status: Status::Active,
                importance: 0.0,
                created_at: now,
                access_log: vec![now],
                tags: vec!["a", "b"],
            }),
            mem_full(MemSpec {
                id: "s2",
                level: Level::L3,
                project: None,
                status: Status::Active,
                importance: 0.0,
                created_at: now,
                access_log: vec![now],
                tags: vec!["b", "c"],
            }),
        ];
        let tags = merge_tags(&sources);
        assert_eq!(
            tags,
            vec![
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "merged".to_string()
            ],
            "应并集去重并补 merged"
        );

        // 源里已含 merged：不应重复添加。
        let with_merged = vec![mem_full(MemSpec {
            id: "s3",
            level: Level::L3,
            project: None,
            status: Status::Active,
            importance: 0.0,
            created_at: now,
            access_log: vec![now],
            tags: vec!["merged", "x"],
        })];
        let tags2 = merge_tags(&with_merged);
        assert_eq!(tags2, vec!["merged".to_string(), "x".to_string()]);
        assert_eq!(
            tags2.iter().filter(|t| t.as_str() == "merged").count(),
            1,
            "merged 不应重复"
        );
    }

    // --- merge：作用域校验（同通用 / 同项目 / 混用报错 / 跨项目报错 / 空源报错）---
    #[test]
    fn validate_merge_scope_cases() {
        let now = 1_000_000_000.0;
        // 全通用 → General。
        let gen = vec![
            ms("g1", Level::L3, None, Status::Active, 0.0, now, vec![now]),
            ms("g2", Level::L2, None, Status::Active, 0.0, now, vec![now]),
        ];
        assert_eq!(
            validate_merge_scope(&gen).expect("全通用应通过"),
            MergeScope::General
        );

        // 全同一项目 → Project。
        let proj = vec![
            ms(
                "p1",
                Level::L4_3,
                Some("engram"),
                Status::Active,
                0.0,
                now,
                vec![now],
            ),
            ms(
                "p2",
                Level::L4_2,
                Some("engram"),
                Status::Active,
                0.0,
                now,
                vec![now],
            ),
        ];
        assert_eq!(
            validate_merge_scope(&proj).expect("全同项目应通过"),
            MergeScope::Project("engram".to_string())
        );

        // 通用 + 项目混用 → 报错。
        let mixed = vec![
            ms("g1", Level::L3, None, Status::Active, 0.0, now, vec![now]),
            ms(
                "p1",
                Level::L4_3,
                Some("engram"),
                Status::Active,
                0.0,
                now,
                vec![now],
            ),
        ];
        assert!(validate_merge_scope(&mixed).is_err(), "混作用域应报错");

        // 跨两个不同项目 → 报错。
        let cross = vec![
            ms(
                "p1",
                Level::L4_3,
                Some("alpha"),
                Status::Active,
                0.0,
                now,
                vec![now],
            ),
            ms(
                "p2",
                Level::L4_3,
                Some("beta"),
                Status::Active,
                0.0,
                now,
                vec![now],
            ),
        ];
        assert!(validate_merge_scope(&cross).is_err(), "跨项目应报错");

        // 空源 → 报错。
        assert!(validate_merge_scope(&[]).is_err(), "空源应报错");
    }

    // --- merge：build_merged_memory 综合（无 override 时取源派生值）---
    #[test]
    fn build_merged_memory_derives_fields() {
        let now = 2_000_000_000.0;
        let sources = vec![
            mem_full(MemSpec {
                id: "s1",
                level: Level::L4_3,
                project: Some("engram"),
                status: Status::Active,
                importance: 0.2,
                created_at: 100.0,
                access_log: vec![10.0, 20.0],
                tags: vec!["x"],
            }),
            mem_full(MemSpec {
                id: "s2",
                level: Level::L4_2,
                project: Some("engram"),
                status: Status::Active,
                importance: 0.7,
                created_at: 50.0,
                access_log: vec![20.0, 30.0],
                tags: vec!["y"],
            }),
        ];
        let scope = validate_merge_scope(&sources).expect("应通过校验");
        let m = build_merged_memory("new_id", "合并后的 cue", &sources, &scope, None, None, now);

        assert_eq!(m.id, "new_id");
        assert_eq!(m.cue, "合并后的 cue");
        assert_eq!(m.level, Level::L4_2, "无 --level 应取最高层 L4.2");
        assert!(
            (m.importance - 0.7).abs() < 1e-12,
            "无 --importance 应取最大 0.7"
        );
        assert_eq!(m.project, Some("engram".to_string()), "应继承共同项目");
        assert_eq!(
            m.access_log,
            vec![10.0, 20.0, 30.0],
            "access_log 应并集去重升序"
        );
        assert!((m.created_at - 50.0).abs() < 1e-12, "created_at 应取最早");
        assert_eq!(m.pointer.kind, "none");
        assert_eq!(m.status, Status::Active);
        assert!(!m.pinned);
        assert_eq!(m.superseded_by, None);
        assert!(m.tags.contains(&"merged".to_string()), "tags 应含 merged");
        assert!(m.tags.contains(&"x".to_string()) && m.tags.contains(&"y".to_string()));

        // override 生效。
        let m2 = build_merged_memory(
            "id2",
            "c",
            &sources,
            &scope,
            Some(Level::L4_1),
            Some(0.95),
            now,
        );
        assert_eq!(m2.level, Level::L4_1, "--level 应覆盖派生层");
        assert!((m2.importance - 0.95).abs() < 1e-12, "--importance 应覆盖");
    }

    // --- gc：last_touch 取 access_log 最大值，空则取 created_at ---
    #[test]
    fn last_touch_uses_max_access_or_created() {
        let m = ms(
            "x",
            Level::L3,
            None,
            Status::Cold,
            0.0,
            100.0,
            vec![300.0, 200.0],
        );
        assert!(
            (last_touch(&m) - 300.0).abs() < 1e-12,
            "应取 access_log 最大值"
        );

        let empty = ms("y", Level::L3, None, Status::Cold, 0.0, 555.0, vec![]);
        assert!(
            (last_touch(&empty) - 555.0).abs() < 1e-12,
            "access_log 空时应取 created_at"
        );
    }

    // --- gc：删除判定矩阵 ---
    #[test]
    fn gc_should_delete_matrix() {
        // now，TTL 180 天，墓碑 TTL 3650 天。
        let now = 1_000_000_000.0;
        let day = crate::model::SECS_PER_DAY;
        let ttl = 180.0;
        let tomb_ttl = 3650.0;

        // cold 超 ttl（200 天未触碰）→ 删。
        let cold_old = ms(
            "c1",
            Level::L3,
            None,
            Status::Cold,
            0.0,
            now - 200.0 * day,
            vec![now - 200.0 * day],
        );
        assert!(
            gc_should_delete(&cold_old, now, ttl, tomb_ttl),
            "cold 超 ttl 应删"
        );

        // cold 未超 ttl（100 天）→ 保留。
        let cold_fresh = ms(
            "c2",
            Level::L3,
            None,
            Status::Cold,
            0.0,
            now - 100.0 * day,
            vec![now - 100.0 * day],
        );
        assert!(
            !gc_should_delete(&cold_fresh, now, ttl, tomb_ttl),
            "cold 未超 ttl 应保留"
        );

        // cold 但最近 confirm-use 过（last_touch 新，仅 1 天前）→ 保留（即便创建很久）。
        let cold_touched = ms(
            "c3",
            Level::L3,
            None,
            Status::Cold,
            0.0,
            now - 999.0 * day,
            vec![now - 999.0 * day, now - day],
        );
        assert!(
            !gc_should_delete(&cold_touched, now, ttl, tomb_ttl),
            "最近 confirm-use 刷新 last_touch 的 cold 不应被删"
        );

        // tombstone 未超长 ttl（1000 天 < 3650）→ 保留。
        let tomb_fresh = ms(
            "t1",
            Level::L3,
            None,
            Status::Tombstone,
            0.0,
            now - 1000.0 * day,
            vec![now - 1000.0 * day],
        );
        assert!(
            !gc_should_delete(&tomb_fresh, now, ttl, tomb_ttl),
            "tombstone 未超长 ttl 应保留"
        );

        // tombstone 超长 ttl（4000 天 > 3650）→ 删。
        let tomb_old = ms(
            "t2",
            Level::L3,
            None,
            Status::Tombstone,
            0.0,
            now - 4000.0 * day,
            vec![now - 4000.0 * day],
        );
        assert!(
            gc_should_delete(&tomb_old, now, ttl, tomb_ttl),
            "tombstone 超长 ttl 应删"
        );

        // active / superseded 永不删（即便极久）。
        let active_old = ms(
            "a1",
            Level::L3,
            None,
            Status::Active,
            0.0,
            now - 9999.0 * day,
            vec![now - 9999.0 * day],
        );
        let sup_old = ms(
            "s1",
            Level::L3,
            None,
            Status::Superseded,
            0.0,
            now - 9999.0 * day,
            vec![now - 9999.0 * day],
        );
        assert!(
            !gc_should_delete(&active_old, now, ttl, tomb_ttl),
            "active 永不删"
        );
        assert!(
            !gc_should_delete(&sup_old, now, ttl, tomb_ttl),
            "superseded 永不删"
        );
    }

    // ====================== hot-index：活跃子项目判定纯逻辑单测 ======================

    /// 便捷：把 `&[&str]` 转成 `Vec<String>`（构造 subdirs 列表用）。
    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn last_word_occurrence_requires_full_word() {
        // 完整词命中。
        assert!(last_word_occurrence("做 engram 这个项目", "engram").is_some());
        // 作为更长标识符的子串不命中。
        assert!(last_word_occurrence("engrams 复数形式", "engram").is_none());
        assert!(last_word_occurrence("myengram 前缀", "engram").is_none());
        // 路径分隔符算边界。
        assert!(last_word_occurrence("path/engram/x", "engram").is_some());
        assert!(last_word_occurrence("path\\engram\\x", "engram").is_some());
        // 空 needle。
        assert!(last_word_occurrence("anything", "").is_none());
    }

    #[test]
    fn last_word_occurrence_picks_last() {
        // 两次出现取靠后那次的位置。
        let pos = last_word_occurrence("engram 然后又 engram", "engram").expect("应命中");
        // 第二次出现在更后的位置。
        assert!(pos > 0, "应取最后一次出现的位置，得 {pos}");
        let first = "engram 然后又 engram".find("engram").expect("有首次");
        assert!(pos > first, "最后一次位置应晚于首次");
    }

    #[test]
    fn active_from_prompt_picks_last_mentioned() {
        let subdirs = names(&["engram", "ai-2d-engine", "other"]);
        // 先提 engram 再提 ai-2d-engine → 取最后的 ai-2d-engine。
        let p = "先看 engram 再去 ai-2d-engine 那边";
        assert_eq!(
            active_from_prompt(p, &subdirs),
            Some("ai-2d-engine".to_string())
        );
        // 都没提 → None。
        assert_eq!(active_from_prompt("无关内容", &subdirs), None);
        // 伪造名（不在 subdirs）不会被 prompt 信号选中（因为根本不在候选里）。
        let fake = names(&["engram"]);
        assert_eq!(active_from_prompt("去 nonexistent 项目", &fake), None);
    }

    #[test]
    fn active_from_transcript_matches_separators_and_picks_last() {
        let root = "F:/ClaudeWorkspaces";
        let subdirs = names(&["engram", "ai-2d-engine"]);
        // 先触碰 engram 再触碰 ai-2d-engine（正斜杠）→ 取最后 ai-2d-engine。
        let t = "F:/ClaudeWorkspaces/engram/src/x.rs 然后 F:/ClaudeWorkspaces/ai-2d-engine/y.rs";
        assert_eq!(
            active_from_transcript(t, root, &subdirs),
            Some("ai-2d-engine".to_string())
        );

        // JSON 转义的双反斜杠分隔符变体，root 用反斜杠写法。
        let root_bs = "F:\\ClaudeWorkspaces";
        let t2 =
            r"F:\\ClaudeWorkspaces\\engram\\a.rs 接着 F:\\ClaudeWorkspaces\\ai-2d-engine\\b.rs";
        assert_eq!(
            active_from_transcript(t2, root_bs, &subdirs),
            Some("ai-2d-engine".to_string())
        );

        // 单反斜杠分隔符变体。
        let t3 = r"F:\ClaudeWorkspaces\engram\a.rs";
        assert_eq!(
            active_from_transcript(t3, root_bs, &subdirs),
            Some("engram".to_string())
        );

        // root 用正斜杠、transcript 用双反斜杠，跨写法也应匹配。
        let t4 = r"F:\\ClaudeWorkspaces\\engram\\a.rs";
        assert_eq!(
            active_from_transcript(t4, root, &subdirs),
            Some("engram".to_string())
        );
    }

    #[test]
    fn active_from_transcript_rejects_non_subdir_segment() {
        let root = "F:/ClaudeWorkspaces";
        let subdirs = names(&["engram"]);
        // 段名 fake 不在 subdirs → 不命中。
        let t = "F:/ClaudeWorkspaces/fake/x.rs";
        assert_eq!(active_from_transcript(t, root, &subdirs), None);
        // 真实子目录命中。
        let t2 = "F:/ClaudeWorkspaces/engram/x.rs";
        assert_eq!(
            active_from_transcript(t2, root, &subdirs),
            Some("engram".to_string())
        );
    }

    #[test]
    fn decide_active_prompt_takes_precedence_over_transcript() {
        let root = "F:/ClaudeWorkspaces";
        let subdirs = names(&["engram", "ai-2d-engine"]);
        // transcript 指向 ai-2d-engine，但 prompt 显式提 engram → 取 prompt 的 engram。
        let prompt = "我现在要做 engram";
        let transcript = "F:/ClaudeWorkspaces/ai-2d-engine/y.rs";
        assert_eq!(
            decide_active(Some(prompt), Some(transcript), root, &subdirs),
            Some("engram".to_string()),
            "prompt 信号应优先于 transcript"
        );

        // prompt 无命中时回退 transcript。
        let prompt_none = "无任何子项目名";
        assert_eq!(
            decide_active(Some(prompt_none), Some(transcript), root, &subdirs),
            Some("ai-2d-engine".to_string()),
            "prompt 无命中应回退 transcript"
        );

        // 都无 → None。
        assert_eq!(decide_active(None, None, root, &subdirs), None);
        assert_eq!(
            decide_active(Some("无关"), Some("也无关"), root, &subdirs),
            None
        );
    }

    #[test]
    fn active_unchanged_gates_correctly() {
        // 上次无记录（None）→ 视为有变化（要渲染）。
        assert!(!active_unchanged(Some("engram"), None, "root"));
        // 同名 → 不变。
        assert!(active_unchanged(Some("engram"), Some("engram"), "root"));
        // 变化。
        assert!(!active_unchanged(
            Some("engram"),
            Some("ai-2d-engine"),
            "root"
        ));
        // 本次无活跃子项目（None）归一为 root_name；上次也记的是 root → 不变。
        assert!(active_unchanged(None, Some("root"), "root"));
        // 本次 None（→root）但上次是某子项目 → 有变化。
        assert!(!active_unchanged(None, Some("engram"), "root"));
    }

    #[test]
    fn normalize_separators_collapses_runs() {
        assert_eq!(normalize_separators(r"a\\b/c\d"), "a/b/c/d");
        assert_eq!(normalize_separators("no-sep"), "no-sep");
        assert_eq!(normalize_separators(r"x\\\\y"), "x/y");
    }

    // ====================== 排除 dotdir / 基础设施目录（bug 修复） ======================

    #[test]
    fn is_mountable_subproject_name_excludes_dotdirs_and_denylist() {
        // dotdir：以 '.' 开头一律排除。
        assert!(!is_mountable_subproject_name(".claude"));
        assert!(!is_mountable_subproject_name(".git"));
        assert!(!is_mountable_subproject_name(".vscode"));
        // denylist：即便不以 '.' 开头也排除。
        assert!(!is_mountable_subproject_name("node_modules"));
        assert!(!is_mountable_subproject_name("target"));
        assert!(!is_mountable_subproject_name("dist"));
        assert!(!is_mountable_subproject_name("build"));
        assert!(!is_mountable_subproject_name("out"));
        // 当前/上级目录名。
        assert!(!is_mountable_subproject_name("."));
        assert!(!is_mountable_subproject_name(".."));
        // 真实子项目名：放行。
        assert!(is_mountable_subproject_name("engram"));
        assert!(is_mountable_subproject_name("ai-2d-engine"));
    }

    #[test]
    fn active_from_transcript_skips_dotdir_and_keeps_last_real() {
        // transcript 先触碰 engram，再触碰 .claude：active 应取剩余里最后触碰的 engram，
        // 不能被 .claude 顶掉变 None（即便 .claude 真实存在于 subdirs）。
        let root = "F:/ClaudeWorkspaces";
        let subdirs = names(&["engram", ".claude"]);
        let t = "F:/ClaudeWorkspaces/engram/y.rs 然后 F:/ClaudeWorkspaces/.claude/x";
        assert_eq!(
            active_from_transcript(t, root, &subdirs),
            Some("engram".to_string()),
            ".claude 段应被跳过，active 取剩余最后触碰的 engram"
        );
    }

    #[test]
    fn active_from_transcript_only_infra_dirs_is_none() {
        // transcript 只触碰 .git 与 node_modules → 无可挂载子项目，active = None。
        let root = "F:/ClaudeWorkspaces";
        let subdirs = names(&[".git", "node_modules"]);
        let t = "F:/ClaudeWorkspaces/.git/HEAD 还有 F:/ClaudeWorkspaces/node_modules/pkg/index.js";
        assert_eq!(
            active_from_transcript(t, root, &subdirs),
            None,
            "只触碰基础设施目录时不应选出任何活跃子项目"
        );
    }

    #[test]
    fn active_from_prompt_does_not_select_dotdir() {
        // prompt 里提到 .claude（且 .claude 在 subdirs 里）也不被选为 active。
        let subdirs = names(&[".claude", "engram"]);
        assert_eq!(
            active_from_prompt("我去看看 .claude 目录", &subdirs),
            None,
            ".claude 不应作为活跃子项目被 prompt 信号选中"
        );
        // 同时提 .claude 与 engram → 只会选可挂载的 engram。
        assert_eq!(
            active_from_prompt("先 .claude 再 engram", &subdirs),
            Some("engram".to_string())
        );
    }

    // ====================== oneline_status：状态栏一行串纯逻辑单测 ======================

    #[test]
    fn oneline_status_counts_general_levels_only_active() {
        let now = 1_000_000_000.0;
        let mems = vec![
            mem("a", Level::L1, None, Status::Active, "甲"),
            mem("b", Level::L1, None, Status::Active, "乙"),
            mem("c", Level::L3, None, Status::Active, "丙"),
            // 非 active 不计入：cold L1、superseded L2、tombstone L3。
            mem("d", Level::L1, None, Status::Cold, "丁"),
            mem("e", Level::L2, None, Status::Superseded, "戊"),
            mem("f", Level::L3, None, Status::Tombstone, "己"),
        ];
        // L1:2（a,b）L2:0 L3:1（c）；无项目段。
        assert_eq!(oneline_status(&mems, now), "● Engram | L1:2 L2:0 L3:1");
    }

    #[test]
    fn oneline_status_empty_is_all_zero() {
        // 空挂载集 → 三层全 0、无项目段。
        assert_eq!(oneline_status(&[], 0.0), "● Engram | L1:0 L2:0 L3:0");
    }

    #[test]
    fn oneline_status_appends_projects_sorted_by_name() {
        let now = 1_000_000_000.0;
        let mems = vec![
            mem("g", Level::L1, None, Status::Active, "通用"),
            // beta：两条 active L4（不同子层合并计数）。
            mem("b1", Level::L4_1, Some("beta"), Status::Active, "b一"),
            mem("b2", Level::L4_3, Some("beta"), Status::Active, "b二"),
            // alpha：一条 active L4。
            mem("a1", Level::L4_2, Some("alpha"), Status::Active, "a一"),
            // alpha 的一条 cold L4 不计入。
            mem("a2", Level::L4_2, Some("alpha"), Status::Cold, "a二"),
        ];
        // 通用 L1:1；项目段按名升序：alpha 在 beta 前；alpha:1、beta:2。
        assert_eq!(
            oneline_status(&mems, now),
            "● Engram | L1:1 L2:0 L3:0 | alpha:1 | beta:2"
        );
    }

    #[test]
    fn oneline_status_uses_utf8_dot_and_pipe_format() {
        let now = 1_000_000_000.0;
        let mems = vec![mem(
            "p",
            Level::L4_1,
            Some("engram"),
            Status::Active,
            "项目",
        )];
        let s = oneline_status(&mems, now);
        assert!(s.starts_with("● Engram | "), "应以 UTF-8 实心圆点前缀开头");
        assert!(s.contains(" | engram:1"), "应含 engram 项目段，实得：{s}");
    }
}
