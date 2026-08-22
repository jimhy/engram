//! 引擎命令的可测纯逻辑（write / recall / list 的算法内核）。
//!
//! 本模块只放**不做 IO、确定可测**的纯函数，供 `main.rs` 的 CLI 编排调用：
//! - [`parse_level`] / [`parse_status_filter`]：把 CLI 字符串解析为枚举；
//! - [`is_l4`]：层级是否属 L4 项目轨道（多库路由依据）；
//! - [`parse_project_dbs`]：把可重复的 `--project-db name=path` 解析为名称→路径映射；
//! - [`generate_id`]：无外部依赖（不引入 uuid/rand）的唯一 id 生成；
//! - [`parse_tags`]：逗号分隔标签解析；
//! - [`score_query`] / [`tokenize_query`]：recall 词法打分（中文二字 ngram 分词 + 归一化命中率）；
//! - [`recall_candidates`]：把一批记忆按 query 打分、过滤、排序、截断；
//! - [`list_visible`]：把一批记忆按 status/level/project 过滤后排序；
//! - [`revived_level`]：confirm-use 复活冷记忆时的重入层（通用→L3 / 项目→L4.3）；
//! - [`level_rank`]：层级重要度序（用于 merge 取最高层）；
//! - [`merge_level`] / [`merge_access_log`] / [`merge_tags`] / [`build_merged_memory`]：
//!   merge 命令的字段合并纯逻辑；
//! - [`last_touch`] / [`gc_should_delete`]：gc 命令的 TTL 判定纯逻辑。
//! - [`home_dir`] / [`resolve_project_scope`]：hook 辅助命令（session-start /
//!   resolve / hot-index / root）的**作用域锚定约定**（纯计算，从 cwd 向上找
//!   `.engram/` 锚点，把"general 库 / 作用域库 / 作用域名"三者的推导收进引擎，
//!   让 hook 侧近乎零逻辑）。
//!
//! 设计文档参考：§13 交付形态（多库分置）、§4 显式召回 recall、§7.4 TTL 硬删除。

use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use crate::activation::effective;
use crate::retrieval::{self, AbstainReason, TokenHit};
use crate::model::{Level, Memory, Pointer, Status, MEMORY_SCHEMA_VERSION};

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

/// 用注入式的环境变量查找闭包推导用户主目录（HOME）。
///
/// 优先取 `USERPROFILE`（Windows 主目录变量），缺省再取 `HOME`（类 Unix）。
/// 把"环境变量读取"做成可注入的闭包，是为了让调用方（如 main.rs 的 `resolve_scope`）
/// 可被纯单测覆盖：测试时传入假 env 查找，不依赖真实进程环境变量。
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

/// engram 在每个项目根 / 项目管理目录下的数据目录名（取代旧的 `.claude`）。
pub const ENGRAM_DIR: &str = ".engram";
/// 项目库 / 管理库的文件名（位于 `.engram/` 内）。
pub const ENGRAM_DB_FILE: &str = "engram.redb";
/// 项目管理目录的标记文件名（位于 `.engram/` 内，仅项目管理目录才有）。
pub const WORKSPACE_MARKER: &str = "workspace";

/// 一个作用域是「具体项目」「项目管理目录」还是「无项目」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeKind {
    /// 具体项目：记忆写它的 L4 库。
    Project,
    /// 项目管理目录：只存少量「管理层」记忆；具体项目应建在它的子目录里
    /// （`cwd` 正好是管理目录本身时，agent 应主动在其下建项目目录再工作）。
    Workspace,
    /// 无项目作用域：锚点落到了 engram 运行时主目录本身（家目录 / engram 主目录，
    /// 其 `.engram/` 与公共库同处一个目录）。此时**不锚定伪项目**——只挂公共库，
    /// L4 项目记忆需在真实项目目录下才能写入。判定见 main.rs 的 `resolve_scope`
    /// （作用域库所在 `.engram` 目录 == 公共库所在目录时降级为本值）。
    None,
}

/// 从 `cwd` 向上锚定出的作用域：项目根（或管理目录）及其库路径、名字。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectScope {
    /// 是具体项目还是项目管理目录。
    pub kind: ScopeKind,
    /// 作用域根目录（项目根，或管理目录本身）。
    pub root: PathBuf,
    /// 该作用域的库：`<root>/.engram/engram.redb`。
    pub db: PathBuf,
    /// 作用域名（`root` 的末段目录名；取不到回退 `"root"`）。
    pub name: String,
}

impl ProjectScope {
    /// 是否为「无项目作用域」（[`ScopeKind::None`]）。
    ///
    /// 为真时其 `db`（`~/.engram/engram.redb`，与公共库同目录）是伪项目库，**永不挂载**、
    /// **永不作为 L4 写入目标**；调用方据此只挂公共库、并拒绝 L4 写入。
    #[must_use]
    pub fn is_none(&self) -> bool {
        self.kind == ScopeKind::None
    }
}

/// 返回 `<dir>/.engram/engram.redb`。
fn engram_db_in(dir: &Path) -> PathBuf {
    dir.join(ENGRAM_DIR).join(ENGRAM_DB_FILE)
}

/// `dir` 的末段目录名；取不到（根盘符 `C:\` / `/` 等）回退 `"root"`。
fn dir_name_or_root(dir: &Path) -> String {
    dir.file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "root".to_string())
}

/// 从 `cwd` 向上锚定项目作用域：找最近的 `.engram/`，据其是否带 `workspace` 标记
/// 区分「具体项目」与「项目管理目录」，按三种情况返回作用域。
///
/// **纯逻辑**：文件系统探测由注入的两个闭包完成（便于单测，不直接碰磁盘）。
///
/// 规则（自 `cwd` 起逐级向上，遇到的**第一个**含 `.engram/` 的目录 `D` 即停）：
/// 1. `D/.engram/workspace` 存在（项目管理目录 `M = D`）：
///    - `D == cwd`：当前就在管理目录本身 → `Workspace`（库 = `M/.engram/engram.redb`，
///      存少量管理记忆；agent 应主动在其下建项目目录）；
///    - 否则（`D` 是 `cwd` 的祖先）：项目根 = `D` 朝 `cwd` 方向的**直接下一级**子目录
///      → `Project`（即「管理目录的直接子目录就是项目」）。
/// 2. `D` 不带 `workspace` 标记（普通项目的 `.engram/`）：项目根 = `D` → `Project`
///    （你的 `项目/src` 例子：在子目录开会话向上找到 `项目/.engram/`，认定项目根=项目）。
/// 3. 向上到尽头都没有 `.engram/`：`cwd` 即项目根 → `Project`（随手放的项目，
///    将在 `cwd` 建 `.engram/`）。
///
/// # 参数
/// - `cwd`：当前工作目录。
/// - `has_engram`：判断 `<dir>/.engram/` 是否存在。
/// - `is_workspace`：判断 `<dir>/.engram/workspace` 标记是否存在。
pub fn resolve_project_scope<E, W>(cwd: &Path, has_engram: E, is_workspace: W) -> ProjectScope
where
    E: Fn(&Path) -> bool,
    W: Fn(&Path) -> bool,
{
    // ancestors(): [cwd, parent, ..., root]；下标 0 即 cwd 自己。
    let chain: Vec<&Path> = cwd.ancestors().collect();
    for (i, dir) in chain.iter().enumerate() {
        if !has_engram(dir) {
            continue;
        }
        if is_workspace(dir) {
            // 管理目录 M = dir。
            if i == 0 {
                // cwd 就是管理目录本身。
                return ProjectScope {
                    kind: ScopeKind::Workspace,
                    root: dir.to_path_buf(),
                    db: engram_db_in(dir),
                    name: dir_name_or_root(dir),
                };
            }
            // 项目根 = M 朝 cwd 方向的直接下一级 = chain[i-1]（i>=1 时必存在）。
            let proj = chain[i - 1];
            return ProjectScope {
                kind: ScopeKind::Project,
                root: proj.to_path_buf(),
                db: engram_db_in(proj),
                name: dir_name_or_root(proj),
            };
        }
        // 普通项目的 .engram/（无 workspace 标记）。
        return ProjectScope {
            kind: ScopeKind::Project,
            root: dir.to_path_buf(),
            db: engram_db_in(dir),
            name: dir_name_or_root(dir),
        };
    }
    // 向上到尽头都没有 .engram/：cwd 即项目根。
    ProjectScope {
        kind: ScopeKind::Project,
        root: cwd.to_path_buf(),
        db: engram_db_in(cwd),
        name: dir_name_or_root(cwd),
    }
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

/// 判断一个字符是否属于**书写时不靠空格断词**的东亚文字。
///
/// 收录：CJK 基本区 / 扩展 A / 兼容表意文字 / 扩展 B，以及日文平假名与片假名。
/// 刻意**不收**两类：CJK 标点（`U+3000..=U+303F`，如 `、。「」`）——它们应当
/// 充当分隔符；韩文音节（`U+AC00..=U+D7AF`）——韩语本就用空格断词，走原有的
/// 空白分词更准。
fn is_cjk(c: char) -> bool {
    matches!(
        c as u32,
        0x3040..=0x30FF          // 日文平假名 / 片假名
            | 0x3400..=0x4DBF    // CJK 扩展 A
            | 0x4E00..=0x9FFF    // CJK 基本区
            | 0xF900..=0xFAFF    // CJK 兼容表意文字
            | 0x2_0000..=0x2_A6DF // CJK 扩展 B
    )
}

/// 把一个**不含空白**的片段切成词元：CJK 段出二元 ngram，非 CJK 段原样保留。
///
/// 片段先按「是否 [`is_cjk`]」切成交替的连续段，再分别处理：
/// - CJK 段长度 ≥ 2 → 逐个相邻二元组（`怎么办` → `怎么` / `么办`）；
/// - CJK 段长度 = 1 → 取该字本身（如 `坑`），避免整段被丢掉；
/// - 非 CJK 段 → 原样保留（`redb,` 仍是 `redb,`，与旧行为一致），但
///   **不含任何字母数字**的纯标点段（如 `，` / `？`）直接丢弃——它们几乎命中
///   所有中文 cue，只会污染打分。
///
/// 返回的词元均已小写化、未去重（去重在 [`tokenize_query`] 里做）。
pub(crate) fn segment_field(field: &str) -> Vec<String> {
    let chars: Vec<char> = field.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        let cjk = is_cjk(chars[i]);
        let start = i;
        while i < chars.len() && is_cjk(chars[i]) == cjk {
            i += 1;
        }
        let seg = &chars[start..i];
        if cjk {
            if seg.len() < 2 {
                out.push(seg.iter().collect::<String>().to_lowercase());
            } else {
                for pair in seg.windows(2) {
                    out.push(pair.iter().collect::<String>().to_lowercase());
                }
            }
        } else if seg.iter().any(|c| c.is_alphanumeric()) {
            out.push(seg.iter().collect::<String>().to_lowercase());
        }
    }
    out
}

/// 把 query 分词、小写化，返回按首次出现序去重的词元列表。
///
/// 分词规则（recall 是纯词法子串匹配，故**只切 query、不切被匹配文本**）：
/// 先按空白切片段，片段内再按 [`segment_field`] 切词元——**中文走二字 ngram、
/// 英文仍按空白分词**。
///
/// 之所以必须给中文加 ngram：中文自然语句没有空白，旧的 `split_whitespace`
/// 会把整句当成**一个**词去做子串匹配，几乎永远零命中（真实库实测「我是谁」
/// 「海风哥的偏好」「热索引过长怎么办」三句全部 0 命中，而手工空格分词后的
/// 「海风哥 偏好」却有 3 命中）。二元 ngram 是零依赖的通用兜底，与知识库
/// sidecar 的 FTS `ngram(2,2)` 同策略。
///
/// # 参数
/// - `query`：原始查询串。
pub fn tokenize_query(query: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for field in query.split_whitespace() {
        for token in segment_field(field) {
            if !token.is_empty() && !seen.contains(&token) {
                seen.push(token);
            }
        }
    }
    seen
}

/// 计算一条记忆对一组 query 词元的**归一化命中率**。
///
/// 分数 = 在该记忆的 `cue + tags(空格连接)`（统一小写）里命中的**不同词元数**
/// ÷ **query 词元总数**，落在 `[0.0, 1.0]`（大小写不敏感子串匹配）。词元已由
/// [`tokenize_query`] 去重，故每个词元至多贡献一次；`tokens` 为空时恒为 `0.0`。
///
/// 归一化是为了让分数**跨 query 可比**：二字 ngram 让长 query 天然拥有更多词元，
/// 若沿用「命中词数」这种绝对计数，长 query 的候选会仅凭词元多而虚高。注意
/// **同一次 query 内分母是常数**，故归一化只改变分数量纲、不改变排序。
///
/// # 参数
/// - `m`：被打分的记忆。
/// - `tokens`：已由 [`tokenize_query`] 分词、小写、去重的 query 词元。
pub fn score_query(m: &Memory, tokens: &[String]) -> f64 {
    if tokens.is_empty() {
        return 0.0;
    }
    let mut haystack = m.cue.to_lowercase();
    if !m.tags.is_empty() {
        haystack.push(' ');
        haystack.push_str(&m.tags.join(" ").to_lowercase());
    }
    let hits = tokens
        .iter()
        .filter(|t| haystack.contains(t.as_str()))
        .count();
    hits as f64 / tokens.len() as f64
}

/// 一条 recall 候选：记忆引用 + 算出的分数与 effective。
///
/// **不再是 `Copy`**：`hits` 带了归因用的堆分配字段。调用面很小
/// （`run_recall` 与本模块测试），改动已逐处核过。
#[derive(Debug, Clone)]
pub struct Candidate<'a> {
    /// 命中的记忆。
    pub memory: &'a Memory,
    /// 打分器给出的相关性分数。
    ///
    /// 量纲取决于 [`Scorer`]：[`Scorer::Bm25`] 下是 BM25 分（无上界）；
    /// [`Scorer::LexicalLegacy`] 下是归一化命中率（落在 `(0.0, 1.0]`）。
    /// **跨 query 不可比**——两种打分器都一样，别拿它当绝对置信度用。
    pub score: f64,
    /// 该记忆在 `now` 时刻的 effective（用于同分排序）。
    pub effective: f64,
    /// 本条命中了哪些 query 词元（按 IDF 降序），供 `--json` 归因。
    ///
    /// [`Scorer::LexicalLegacy`] 下为空——旧打分器对文档不分词，没有词频概念。
    pub hits: Vec<TokenHit>,
}

/// recall 用哪个打分器。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scorer {
    /// BM25（默认）。带 tf 饱和与文档长度归一，见 [`crate::retrieval`]。
    #[default]
    Bm25,
    /// 旧的子串命中率 [`score_query`]，仅供对拍与回滚（CLI 的 `--lexical-legacy`）。
    LexicalLegacy,
}

/// 一次 recall 的完整结果。
///
/// 之所以不是裸的 `Vec<Candidate>`：**弃权**（本库没有相关记忆）是一等结果，
/// 必须能与「有结果」区分开。旧实现只要有一个词元子串命中就返回候选，
/// 于是查一个库里根本不存在的主题时会返回一串低分噪音，
/// 诱导调用它的 agent 把噪音当答案。
#[derive(Debug, Clone, Default)]
pub struct RecallOutcome<'a> {
    /// 按分数降序的候选。**弃权时调用方不得展示它**（见 `abstain`）。
    pub candidates: Vec<Candidate<'a>>,
    /// 非 `None` 即表示该弃权，值是弃权的具体理由。
    pub abstain: Option<AbstainReason>,
    /// 弃权时给调用方的下一步提示：本库里最常见的若干主题词。
    pub suggestions: Vec<String>,
}

/// recall 的查询参数。
///
/// 单独成结构体而不是长参数列表：过滤维度还会继续加，
/// 而位置参数一多就极易在调用点写错顺序（两个 `Option<&str>` 挨着尤其危险）。
#[derive(Debug, Clone, Copy)]
pub struct RecallQuery<'a> {
    /// 已由 [`tokenize_query`] 处理的 query 词元。
    pub tokens: &'a [String],
    /// 为真时只搜 active；否则 active+cold 都搜。
    pub active_only: bool,
    /// 最多返回的候选数。
    pub limit: usize,
    /// 当前时间（unix 秒）。
    pub now: f64,
    /// 打分器。
    pub scorer: Scorer,
    /// 标签过滤；`None` 不过滤。要求该记忆的 `tags` 含有此标签（精确匹配）。
    pub tag: Option<&'a str>,
    /// 层级过滤；`None` 不过滤。
    pub level: Option<Level>,
    /// 项目过滤；`None` 不过滤。要求 `m.project == Some(project)`。
    pub project: Option<&'a str>,
}

impl<'a> RecallQuery<'a> {
    /// 只给必填项的构造：BM25 打分、不加任何过滤。
    pub fn new(tokens: &'a [String], active_only: bool, limit: usize, now: f64) -> Self {
        RecallQuery {
            tokens,
            active_only,
            limit,
            now,
            scorer: Scorer::Bm25,
            tag: None,
            level: None,
            project: None,
        }
    }
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

/// 本库里最常见的若干主题词（取自 tags），弃权时作为「换个词试试」的提示。
///
/// 只统计参与本次检索的记忆（即已过状态/标签/层级/项目过滤的那批），
/// 这样在加了过滤维度时给出的建议才是该范围内真实存在的主题。
fn top_topics(pool: &[&Memory], limit: usize) -> Vec<String> {
    let mut freq: HashMap<&str, usize> = HashMap::new();
    for m in pool {
        for t in &m.tags {
            *freq.entry(t.as_str()).or_insert(0) += 1;
        }
    }
    let mut v: Vec<(&str, usize)> = freq.into_iter().collect();
    // 频次降序；同频按字典序，保证输出稳定可测。
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    v.into_iter().take(limit).map(|(t, _)| t.to_string()).collect()
}

/// 在一批记忆里按 query 检索：过滤 → 打分 → 去零分 → 排序 → 截断 → 判弃权。
///
/// 排序规则：先按 `score` 降序，同分再按 `effective(now)` 降序。
/// `score` 为 0（一个 query 词元都没命中）的记忆不进候选。
///
/// 打分器由 [`RecallQuery::scorer`] 决定，默认 [`Scorer::Bm25`]。
/// BM25 的语料统计（N / df / avgdl）在**已过滤的候选池**上现算——
/// 这一点很重要：加了 `--tag` 之类的过滤后，IDF 应当相对该子集计算，
/// 否则「在这个子集里很罕见」的词会被全库统计冲淡。
///
/// 本函数**不修改任何记忆、不做任何 IO**。
pub fn recall_candidates<'a>(mems: &'a [Memory], q: &RecallQuery<'_>) -> RecallOutcome<'a> {
    // 1. 先过滤出候选池：状态 + 标签 + 层级 + 项目，全部是「与」关系。
    let pool: Vec<&'a Memory> = mems
        .iter()
        .filter(|m| recallable(m.status, q.active_only))
        .filter(|m| match q.tag {
            Some(t) => m.tags.iter().any(|x| x == t),
            None => true,
        })
        .filter(|m| match q.level {
            Some(l) => m.level == l,
            None => true,
        })
        .filter(|m| match q.project {
            Some(p) => m.project.as_deref() == Some(p),
            None => true,
        })
        .collect();

    // 2. 打分。
    let corpus = match q.scorer {
        Scorer::Bm25 => Some(retrieval::Corpus::build(&pool)),
        Scorer::LexicalLegacy => None,
    };
    let mut cands: Vec<Candidate<'a>> = pool
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            let (score, hits) = match &corpus {
                Some(c) => (c.score(i, q.tokens), c.hits(i, q.tokens)),
                None => (score_query(m, q.tokens), Vec::new()),
            };
            if score <= 0.0 {
                None
            } else {
                Some(Candidate {
                    memory: m,
                    score,
                    effective: effective(m, q.now),
                    hits,
                })
            }
        })
        .collect();

    // 3. 先 score 降序，同分按 effective 降序；NaN（不会出现）视作相等。
    cands.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b.effective
                    .partial_cmp(&a.effective)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });

    // 4. 判弃权——必须在 truncate **之前**用全局最高分判，
    //    truncate 只影响展示条数、不该影响「本库到底有没有相关记忆」这个判断。
    let abstain = match &corpus {
        Some(c) => {
            let best = cands.first().map(|x| x.score).unwrap_or(0.0);
            let empty: Vec<TokenHit> = Vec::new();
            let best_hits = cands.first().map(|x| &x.hits).unwrap_or(&empty);
            retrieval::judge(c, q.tokens, best, best_hits)
        }
        // legacy 打分器没有 IDF 概念，判不了弃权；只保留「一条都没命中」这一种。
        None => {
            if cands.is_empty() {
                Some(AbstainReason::NoCandidate)
            } else {
                None
            }
        }
    };

    cands.truncate(q.limit);
    let suggestions = if abstain.is_some() {
        top_topics(&pool, 8)
    } else {
        Vec::new()
    };
    RecallOutcome { candidates: cands, abstain, suggestions }
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
        // 合并产物是新写入的记忆，按当前格式版本落盘。
        schema_version: MEMORY_SCHEMA_VERSION,
    }
}

/// 计算毕业（§6 L4 → L1-3 毕业通道）的目标通用层级。
///
/// 把一条**项目记忆**（L4.x）提拔进通用层时落哪一层：
/// - `to_override` 给定：必须是通用层 L1/L2/L3，否则报错（不能毕业到另一个 L4）；
/// - `to_override` 为 `None`：按**子层同构**默认映射 `L4.1→L1` / `L4.2→L2` / `L4.3→L3`。
///
/// 源层级必须是 L4.x（[`is_l4`]）——通用层记忆本就在通用轨道，无所谓「毕业」。
///
/// # 参数
/// - `src`：源记忆的层级（应为 L4.x）。
/// - `to_override`：`--to-level` 显式指定的目标层；`None` 时用同构默认。
///
/// # Errors
/// - 源不是 L4.x（通用记忆无需毕业）；
/// - `to_override` 落在 L4 轨道（毕业必须去通用层）。
pub fn graduate_target_level(src: Level, to_override: Option<Level>) -> Result<Level, String> {
    if !is_l4(src) {
        return Err(
            "graduate 失败：源不是项目记忆（L4.1/L4.2/L4.3），通用层记忆无需毕业".to_string(),
        );
    }
    match to_override {
        Some(lv) if is_l4(lv) => {
            Err("graduate 失败：--to-level 必须是通用层 L1/L2/L3，不能毕业到另一个 L4".to_string())
        }
        Some(lv) => Ok(lv),
        None => Ok(match src {
            Level::L4_1 => Level::L1,
            Level::L4_2 => Level::L2,
            Level::L4_3 => Level::L3,
            // is_l4 已保证落在 L4.x，其余分支不可达。
            _ => unreachable!("is_l4 已确保源层级为 L4.x"),
        }),
    }
}

/// 构造毕业后的**新通用记忆**（纯逻辑，不做 IO、不生成 id）。
///
/// 字段来源（§6 毕业通道：移动到通用层、不复制）：
/// - `id` = 传入的 `new_id`（调用方已决定，`--new-id` 或 [`generate_id`]）；
/// - `cue` = `cue_override` 或源 `cue`；
/// - `level` = `to_level`（[`graduate_target_level`] 算出的通用层）；
/// - `project` = `None`（脱离项目作用域，进通用轨道）；
/// - `importance` = `importance_override` 或源 `importance`；
/// - `access_log` = 源 access_log 克隆（**继承真使用痕迹**，毕业不清零频率/近因）；
/// - `created_at` = 源 `created_at`（保留资历）；
/// - `pointer` = 源 pointer 克隆（指针照旧指向 ground truth）；
/// - `pinned` = 源 `pinned`；`status` = [`Status::Active`]；`superseded_by = None`；
/// - `tags` = 源 tags 并补一个 `"graduated"` 血缘标记（不重复添加）。
///
/// 注意：原 L4 记忆**不在本函数处理**——调用方负责把它转 [`Status::Superseded`]、
/// `superseded_by = new_id`，作为留在项目库里的「已上浮」指针（§6：留指针、不复制）。
///
/// # 参数
/// - `new_id`：新通用记忆 id（调用方已决定）。
/// - `src`：源 L4 记忆。
/// - `to_level`：目标通用层。
/// - `cue_override`：`--cue` 覆盖；`None` 时沿用源 cue。
/// - `importance_override`：`--importance` 覆盖；`None` 时沿用源 importance。
pub fn build_graduated_memory(
    new_id: &str,
    src: &Memory,
    to_level: Level,
    cue_override: Option<&str>,
    importance_override: Option<f64>,
) -> Memory {
    let mut tags = src.tags.clone();
    let graduated = "graduated".to_string();
    if !tags.contains(&graduated) {
        tags.push(graduated);
    }
    Memory {
        id: new_id.to_string(),
        cue: cue_override.unwrap_or(&src.cue).to_string(),
        pointer: src.pointer.clone(),
        level: to_level,
        project: None,
        importance: importance_override.unwrap_or(src.importance),
        pinned: src.pinned,
        access_log: src.access_log.clone(),
        status: Status::Active,
        superseded_by: None,
        created_at: src.created_at,
        tags,
        // 毕业产物是新写入的记忆，按当前格式版本落盘。
        schema_version: MEMORY_SCHEMA_VERSION,
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

/// 判定一条记忆是否应被 gc 硬删除（§7.4 TTL 硬删除的「与门」）。
///
/// 删除需**两臂同时成立**（与门），缺一不可：
/// 1. **超 TTL**：`age_days = (now - last_touch) / 86400` 超过该类型的 TTL
///    （Cold 用 `ttl_days`，Tombstone 用 `tombstone_ttl_days`，[`last_touch`] 见上）。
/// 2. **极少真使用**：真使用次数 `access_log.len() <= min_uses`（缺省阈值 1）。
///    `write` 创建时 `access_log` 为空、不计入；只有 `confirm-use`（真使用）才
///    向其追加时间戳，故 `access_log.len()` 即「这条被真正用上过几次」。
///
/// 即：
/// - `Cold` 且 `age_days > ttl_days` 且 `access_log.len() <= min_uses` → 删除；
/// - `Tombstone` 且 `age_days > tombstone_ttl_days` 且 `access_log.len() <= min_uses` → 删除；
/// - `Active` / `Superseded` → **永不删**。
///
/// 设计说明（设计文档 §7.4「删除条件（与门）」）：
/// - **两扇与门，宁留勿删**（删除不可逆）：只「很久不用」不够，还得「从没真正用上过」。
///   一条被 `confirm-use` 过 ≥ `min_uses+1` 次的记忆，即便如今冷却也已**证明过有用**，
///   永久保留、不被 TTL 清理——这正是第二臂相对旧版（仅 age 一臂）补上的那条胳膊。
/// - **第一臂以 age 为闸**：任何 `recall`→`confirm-use` 既刷新 `last_touch`（重置 TTL
///   计时），又增长 `access_log`（推动第二臂转向「保留」），双重保护重新被需要的记忆。
/// - **tombstone TTL 极长**：墓碑是「负知识」（曾认为 X、后被 Y 推翻），长期保留以防
///   重蹈覆辙，故其 TTL 远长于冷条目（缺省 3650 天 vs 180 天）；其第二臂同样要求极少使用。
/// - **Active/Superseded 永不删**：活跃记忆显然要留；superseded 仍是有效指向链
///   （`superseded_by`）的一环，由后续 supersede→merge 流程接管，不在 gc 范围内。
///
/// # 参数
/// - `m`：待判定的记忆。
/// - `now`：当前时间（unix 秒）。
/// - `ttl_days`：冷条目存活上限（天）。
/// - `tombstone_ttl_days`：墓碑存活上限（天）。
/// - `min_uses`：第二臂阈值——真使用次数不超过此值才算「极少使用」（缺省 1）。
pub fn gc_should_delete(
    m: &Memory,
    now: f64,
    ttl_days: f64,
    tombstone_ttl_days: f64,
    min_uses: usize,
) -> bool {
    let age_days = (now - last_touch(m)) / crate::model::SECS_PER_DAY;
    // 第二臂：真使用次数极低。access_log 仅由 confirm-use 追加（write 时为空），
    // 故其长度即「被真正用上过几次」；<= min_uses 视为「从没真正用上过」。
    let rarely_used = m.access_log.len() <= min_uses;
    match m.status {
        Status::Cold => age_days > ttl_days && rarely_used,
        Status::Tombstone => age_days > tombstone_ttl_days && rarely_used,
        Status::Active | Status::Superseded => false,
    }
}

// ============================================================================
// hot-index：状态栏一行串（纯逻辑，便于单测）
// ============================================================================

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

/// 复盘者「疑似停摆」判定阈值（秒）：48 小时。
///
/// 存在 pending 残留、且上次成功巩固（last-review-ok）距今超过此值，说明
/// SessionEnd/SessionStart 的复盘闭环大概率断了（复盘者起不来 / 反复失败），
/// 需要在状态栏与 status 概况里显式亮警——否则停摆是完全无声的，记忆只进不出。
pub const REVIEWER_STALL_SECS: f64 = 48.0 * 3600.0;

/// 判定复盘者是否疑似停摆（纯逻辑，可单测）。
///
/// 规则：`pending_count > 0`（有待巩固残留）**且**上次成功巩固距今超过
/// [`REVIEWER_STALL_SECS`]（`last_ok_ts` 为 `None` 视作「从未成功」，同样满足）。
/// 无 pending 时永不判停摆——没有积压就谈不上停摆。
///
/// # 参数
/// - `pending_count`：pending 目录里的残留标记数。
/// - `last_ok_ts`：上次成功巩固的 unix 秒（读不到 last-review-ok 时为 `None`）。
/// - `now`：当前时间（unix 秒）。
pub fn reviewer_stalled(pending_count: usize, last_ok_ts: Option<f64>, now: f64) -> bool {
    if pending_count == 0 {
        return false;
    }
    match last_ok_ts {
        None => true,
        Some(ts) => now - ts > REVIEWER_STALL_SECS,
    }
}

/// 同 [`oneline_status`]，但可在串尾附加复盘健康标记。
///
/// `stalled` 为真（见 [`reviewer_stalled`]）时在状态栏串末尾追加 ` ⚠reviewer`，
/// 让「复盘者停摆」在状态栏可见；为假时输出与 [`oneline_status`] 逐字节一致。
pub fn oneline_status_with_health(memories: &[Memory], now: f64, stalled: bool) -> String {
    let mut out = oneline_status(memories, now);
    if stalled {
        out.push_str(" ⚠reviewer");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Pointer, Status};

    // ---- resolve_project_scope：向上找 .engram/ 锚点的项目根判定 ----

    /// 用两个目录集合构造文件系统探测闭包，跑 resolve_project_scope。
    fn scope_with(cwd: &str, engram_dirs: &[&str], workspace_dirs: &[&str]) -> ProjectScope {
        let eng: Vec<PathBuf> = engram_dirs.iter().map(PathBuf::from).collect();
        let ws: Vec<PathBuf> = workspace_dirs.iter().map(PathBuf::from).collect();
        resolve_project_scope(
            Path::new(cwd),
            |d| eng.iter().any(|e| e == d),
            |d| ws.iter().any(|w| w == d),
        )
    }

    // 1. 普通项目：在 项目/src 开会话，向上找到 项目/.engram → 项目根=项目。
    #[test]
    fn scope_project_anchor_from_subdir() {
        let s = scope_with("/ws/proj/src", &["/ws/proj"], &[]);
        assert_eq!(s.kind, ScopeKind::Project);
        assert_eq!(s.root, PathBuf::from("/ws/proj"));
        assert_eq!(s.name, "proj");
        assert_eq!(s.db, PathBuf::from("/ws/proj/.engram/engram.redb"));
    }

    // 2. 管理目录 + cwd 在其子项目深处：项目根 = 管理目录的直接子目录。
    #[test]
    fn scope_workspace_child_is_project() {
        // M=/ws/M 是管理目录；engram 还没自己的 .engram → 向上锚定到 M，项目根=engram。
        let s = scope_with("/ws/M/engram/plugin/scripts", &["/ws/M"], &["/ws/M"]);
        assert_eq!(s.kind, ScopeKind::Project);
        assert_eq!(s.root, PathBuf::from("/ws/M/engram"));
        assert_eq!(s.name, "engram");
    }

    // 3. cwd 正好是管理目录本身 → Workspace（库为管理库）。
    #[test]
    fn scope_at_workspace_itself() {
        let s = scope_with("/ws/M", &["/ws/M"], &["/ws/M"]);
        assert_eq!(s.kind, ScopeKind::Workspace);
        assert_eq!(s.root, PathBuf::from("/ws/M"));
        assert_eq!(s.db, PathBuf::from("/ws/M/.engram/engram.redb"));
    }

    // 4. 向上无任何 .engram/ → cwd 即项目根。
    #[test]
    fn scope_no_anchor_uses_cwd() {
        let s = scope_with("/some/where/deep", &[], &[]);
        assert_eq!(s.kind, ScopeKind::Project);
        assert_eq!(s.root, PathBuf::from("/some/where/deep"));
    }

    // 5. 就近优先：项目自己的 .engram/ 比上层管理目录更近 → 锚定项目，不上溯到管理目录。
    #[test]
    fn scope_nearest_project_wins_over_workspace() {
        // engram 已有自己的项目 .engram/（无 workspace 标记），M 是更上层管理目录。
        let s = scope_with(
            "/ws/M/engram/plugin",
            &["/ws/M/engram", "/ws/M"],
            &["/ws/M"],
        );
        assert_eq!(s.kind, ScopeKind::Project);
        assert_eq!(s.root, PathBuf::from("/ws/M/engram"));
        assert_eq!(s.name, "engram");
    }

    // ---- 补充测试矩阵（多 agent 穷举，仅跨平台稳定的正斜杠用例）----

    // 6. cwd 自己就是普通项目锚（i==0，无 workspace 标记）——区别于 #3 的 cwd==管理目录。
    #[test]
    fn scope_cwd_is_plain_anchor() {
        let s = scope_with("/ws/proj", &["/ws/proj"], &[]);
        assert_eq!(s.kind, ScopeKind::Project);
        assert_eq!(s.root, PathBuf::from("/ws/proj"));
        assert_eq!(s.name, "proj");
    }

    // 7. 管理目录是直接父级（i==1）→ 项目根 = chain[0] = cwd 自己（i-1 下溢边界）。
    #[test]
    fn scope_workspace_immediate_parent_root_is_cwd() {
        let s = scope_with("/ws/M/proj", &["/ws/M"], &["/ws/M"]);
        assert_eq!(s.kind, ScopeKind::Project);
        assert_eq!(s.root, PathBuf::from("/ws/M/proj"));
        assert_eq!(s.name, "proj");
    }

    // 8. 普通锚在中间层、上面多级无 .engram → 一路上溯到第一个有锚的目录。
    #[test]
    fn scope_plain_anchor_middle_layer() {
        let s = scope_with("/ws/proj/a/b/c", &["/ws/proj"], &[]);
        assert_eq!(s.kind, ScopeKind::Project);
        assert_eq!(s.root, PathBuf::from("/ws/proj"));
    }

    // 9. 管理目录在 i==2、项目根 = chain[1]，而该项目根自身还没有 .engram/（首次、待建）。
    #[test]
    fn scope_workspace_child_root_need_not_have_engram() {
        let s = scope_with("/ws/M/proj/sub", &["/ws/M"], &["/ws/M"]);
        assert_eq!(s.kind, ScopeKind::Project);
        assert_eq!(s.root, PathBuf::from("/ws/M/proj"));
    }

    // 10. 就近优先（最近是管理目录）：更上层还有普通锚也要忽略。
    #[test]
    fn scope_stop_at_nearest_workspace_ignore_higher_plain() {
        let s = scope_with("/ws/M/proj/sub", &["/ws/M", "/ws"], &["/ws/M"]);
        assert_eq!(s.kind, ScopeKind::Project);
        assert_eq!(s.root, PathBuf::from("/ws/M/proj"));
    }

    // 11. 两个管理目录都在链上（生产禁止嵌套，但 resolve 必须确定性地停在最近的）。
    #[test]
    fn scope_stop_at_nearest_workspace_ignore_higher_workspace() {
        let s = scope_with(
            "/outer/inner/proj/sub",
            &["/outer/inner", "/outer"],
            &["/outer/inner", "/outer"],
        );
        assert_eq!(s.kind, ScopeKind::Project);
        assert_eq!(s.root, PathBuf::from("/outer/inner/proj"));
    }

    // 12. 项目自带普通锚（i==0）压过父级管理目录（i==1）。
    #[test]
    fn scope_cwd_plain_anchor_beats_parent_workspace() {
        let s = scope_with("/ws/M/proj", &["/ws/M/proj", "/ws/M"], &["/ws/M"]);
        assert_eq!(s.kind, ScopeKind::Project);
        assert_eq!(s.root, PathBuf::from("/ws/M/proj"));
    }

    // 13. 文件系统根作为 cwd、无锚 → cwd 即根，name 回退 "root"（file_name()==None）。
    #[test]
    fn scope_fs_root_no_anchor_falls_back() {
        let s = scope_with("/", &[], &[]);
        assert_eq!(s.kind, ScopeKind::Project);
        assert_eq!(s.root, PathBuf::from("/"));
        assert_eq!(s.name, "root");
    }

    // 14. 非 ASCII（CJK）段名：普通锚在父级，name 保留 CJK。
    #[test]
    fn scope_non_ascii_cjk_anchor() {
        let s = scope_with("/项目/子目录", &["/项目"], &[]);
        assert_eq!(s.kind, ScopeKind::Project);
        assert_eq!(s.root, PathBuf::from("/项目"));
        assert_eq!(s.name, "项目");
    }

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
            schema_version: MEMORY_SCHEMA_VERSION,
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

    // ====================== hook 辅助命令：home_dir 纯单测 ======================

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
    fn tokenize_cjk_into_bigrams() {
        // 纯中文自然句：整句切二字 ngram（旧行为是整句一个词、几乎必然零命中）。
        assert_eq!(
            tokenize_query("热索引过长怎么办"),
            vec!["热索", "索引", "引过", "过长", "长怎", "怎么", "么办"]
        );
        // 中英混排：非 CJK 段原样保留，单字中文段取该字本身，纯标点段丢弃。
        assert_eq!(
            tokenize_query("engram的坑，怎么办？"),
            vec!["engram", "的坑", "怎么", "么办"]
        );
        assert_eq!(
            tokenize_query("windows 路径 坑"),
            vec!["windows", "路径", "坑"]
        );
        // 纯标点 / 纯空白不产生任何词元（不会因此命中全库）。
        assert!(tokenize_query("，。？").is_empty());
        assert!(tokenize_query("   ").is_empty());
    }

    #[test]
    fn score_is_normalized_hit_rate() {
        let mut m = mem("x", Level::L3, None, Status::Active, "redb 文件锁问题");
        m.tags = vec!["lock".to_string()];
        // query: "redb lock missing" → 3 词元，命中 redb（cue）、lock（tag）= 2/3。
        let tokens = tokenize_query("redb lock missing");
        let score = score_query(&m, &tokens);
        assert!(
            (score - 2.0 / 3.0).abs() < 1e-12,
            "应为归一化命中率 2/3，实得 {score}"
        );
        // 空 query（如全是标点）不得给出正分。
        assert!(score_query(&m, &tokenize_query("，。")).abs() < 1e-12);
    }

    #[test]
    fn score_cjk_sentence_hits_by_bigram() {
        let m = mem(
            "p",
            Level::L3,
            None,
            Status::Active,
            "海风哥的协作偏好：被工具拦住时先停下请他开权限",
        );
        // "海风哥的偏好" → 海风/风哥/哥的/的偏/偏好 共 5 词元，cue 命中 4 个（缺 的偏）。
        let tokens = tokenize_query("海风哥的偏好");
        assert_eq!(tokens.len(), 5);
        let score = score_query(&m, &tokens);
        assert!((score - 0.8).abs() < 1e-12, "应命中 4/5，实得 {score}");
    }

    #[test]
    fn recall_orders_by_score_then_effective() {
        // 守护**排序语义**：命中词元多的排前、没命中的不进候选。
        // 原版还断言 `score == 1.0`，那是 legacy 归一化命中率的**量纲**、不是排序语义；
        // BM25 分数无上界，故把量纲那一半拆到 recall_legacy_scorer_keeps_ratio_scale 里
        // 继续守护（legacy 打分器仍保留供 --lexical-legacy 使用，不能没有测试）。
        let now = 1_000_000_000.0;
        let hit2 = mem("a", Level::L3, None, Status::Active, "redb lock 冲突");
        let hit1 = mem("b", Level::L3, None, Status::Active, "redb 入门");
        let miss = mem("c", Level::L3, None, Status::Active, "无关内容");
        let mems = vec![hit1, hit2, miss];
        let tokens = tokenize_query("redb lock");
        let out = recall_candidates(&mems, &RecallQuery::new(&tokens, false, 10, now));
        let cands = &out.candidates;
        assert_eq!(cands.len(), 2, "命中 2 条，miss 不进候选");
        assert_eq!(cands[0].memory.id, "a", "命中词多者排前");
        assert!(
            cands[0].score > cands[1].score,
            "命中两个词元的分({})必须严格高于只命中一个的({})",
            cands[0].score,
            cands[1].score
        );
        assert_eq!(cands[1].memory.id, "b");
    }

    #[test]
    fn recall_legacy_scorer_keeps_ratio_scale() {
        // 守护 legacy 打分器的量纲：归一化命中率落在 (0,1]，两词元全中即 1.0。
        // --lexical-legacy 的回滚价值全靠它——量纲变了，对拍就没有意义了。
        let now = 1_000_000_000.0;
        let hit2 = mem("a", Level::L3, None, Status::Active, "redb lock 冲突");
        let hit1 = mem("b", Level::L3, None, Status::Active, "redb 入门");
        let mems = vec![hit1, hit2];
        let tokens = tokenize_query("redb lock");
        let q = RecallQuery {
            scorer: Scorer::LexicalLegacy,
            ..RecallQuery::new(&tokens, false, 10, now)
        };
        let out = recall_candidates(&mems, &q);
        assert_eq!(out.candidates[0].memory.id, "a");
        assert!(
            (out.candidates[0].score - 1.0).abs() < 1e-12,
            "两词元全中应为 1.0，实得 {}",
            out.candidates[0].score
        );
        assert!(
            out.candidates.iter().all(|c| c.score > 0.0 && c.score <= 1.0),
            "legacy 分数必须落在 (0,1]"
        );
    }

    #[test]
    fn recall_limit_truncates() {
        let now = 1_000_000_000.0;
        let mems: Vec<Memory> = (0..5)
            .map(|i| mem(&format!("m{i}"), Level::L3, None, Status::Active, "redb"))
            .collect();
        let tokens = tokenize_query("redb");
        let out = recall_candidates(&mems, &RecallQuery::new(&tokens, false, 3, now));
        assert_eq!(out.candidates.len(), 3, "limit=3 应截断到 3 条");
    }

    #[test]
    fn recall_default_includes_cold_active_only_excludes() {
        let now = 1_000_000_000.0;
        let cold = mem("cold", Level::L3, None, Status::Cold, "redb 冷藏笔记");
        let active = mem("act", Level::L3, None, Status::Active, "redb 活跃笔记");
        let mems = vec![cold, active];
        let tokens = tokenize_query("redb");

        let default = recall_candidates(&mems, &RecallQuery::new(&tokens, false, 10, now));
        assert_eq!(default.candidates.len(), 2, "默认应搜到 cold + active");

        let only = recall_candidates(&mems, &RecallQuery::new(&tokens, true, 10, now));
        assert_eq!(only.candidates.len(), 1, "active-only 应排除 cold");
        assert_eq!(only.candidates[0].memory.id, "act");
    }

    #[test]
    fn recall_never_returns_superseded_or_tombstone() {
        let now = 1_000_000_000.0;
        let sup = mem("sup", Level::L3, None, Status::Superseded, "redb 被取代");
        let tomb = mem("tomb", Level::L3, None, Status::Tombstone, "redb 墓碑");
        let mems = vec![sup, tomb];
        let tokens = tokenize_query("redb");
        // 即便 active_only=false 也不返回。
        let out = recall_candidates(&mems, &RecallQuery::new(&tokens, false, 10, now));
        let cands = &out.candidates;
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
            schema_version: MEMORY_SCHEMA_VERSION,
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

    // --- 复盘健康：停摆判定与状态栏标记 ---
    #[test]
    fn reviewer_stalled_requires_pending_and_stale_ok() {
        let now = 1_000_000_000.0;
        // 无 pending：无论 last-ok 多旧都不算停摆。
        assert!(!reviewer_stalled(0, None, now));
        assert!(!reviewer_stalled(0, Some(now - 10.0 * 86400.0), now));
        // 有 pending + 从未成功：停摆。
        assert!(reviewer_stalled(1, None, now));
        // 有 pending + last-ok 在 48h 内：正常（复盘者还活着）。
        assert!(!reviewer_stalled(2, Some(now - 3600.0), now));
        // 有 pending + last-ok 超 48h：停摆。
        assert!(reviewer_stalled(
            2,
            Some(now - REVIEWER_STALL_SECS - 1.0),
            now
        ));
    }

    #[test]
    fn oneline_status_health_appends_marker_only_when_stalled() {
        let now = 1_000_000_000.0;
        let mems = vec![mem("a", Level::L1, None, Status::Active, "甲")];
        let base = oneline_status(&mems, now);
        assert_eq!(
            oneline_status_with_health(&mems, now, false),
            base,
            "未停摆时应与 oneline_status 逐字节一致"
        );
        assert_eq!(
            oneline_status_with_health(&mems, now, true),
            format!("{base} ⚠reviewer"),
            "停摆时应追加 ⚠reviewer 标记"
        );
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
    fn graduate_target_level_maps_and_validates() {
        // 同构默认映射 L4.1→L1 / L4.2→L2 / L4.3→L3。
        assert_eq!(graduate_target_level(Level::L4_1, None).unwrap(), Level::L1);
        assert_eq!(graduate_target_level(Level::L4_2, None).unwrap(), Level::L2);
        assert_eq!(graduate_target_level(Level::L4_3, None).unwrap(), Level::L3);
        // override 生效（可跨档：L4.3 也能直接毕业到 L1）。
        assert_eq!(
            graduate_target_level(Level::L4_3, Some(Level::L1)).unwrap(),
            Level::L1
        );
        assert_eq!(
            graduate_target_level(Level::L4_1, Some(Level::L3)).unwrap(),
            Level::L3
        );
        // 源非 L4（通用记忆）→ 报错。
        assert!(graduate_target_level(Level::L1, None).is_err());
        assert!(graduate_target_level(Level::L3, Some(Level::L2)).is_err());
        // override 落在 L4 轨道（毕业必须去通用层）→ 报错。
        assert!(graduate_target_level(Level::L4_2, Some(Level::L4_1)).is_err());
    }

    #[test]
    fn build_graduated_memory_moves_to_general_and_keeps_lineage() {
        let src = Memory {
            id: "src_l4".to_string(),
            cue: "项目里悟出的通用原则".to_string(),
            pointer: Pointer {
                kind: "file".to_string(),
                reference: Some("a.rs:10".to_string()),
                detail: None,
            },
            level: Level::L4_2,
            project: Some("engram".to_string()),
            importance: 0.6,
            pinned: false,
            access_log: vec![100.0, 200.0, 300.0],
            status: Status::Active,
            superseded_by: None,
            created_at: 50.0,
            tags: vec!["arch".to_string()],
            schema_version: MEMORY_SCHEMA_VERSION,
        };
        // 默认映射 L4.2→L2；cue/importance 未覆盖时沿用源。
        let to = graduate_target_level(src.level, None).unwrap();
        let g = build_graduated_memory("new_gen", &src, to, None, None);
        assert_eq!(g.id, "new_gen");
        assert_eq!(g.level, Level::L2);
        assert_eq!(g.project, None, "毕业后脱离项目作用域");
        assert_eq!(g.cue, src.cue, "未覆盖时沿用源 cue");
        assert!(
            (g.importance - 0.6).abs() < 1e-12,
            "未覆盖时沿用源 importance"
        );
        assert_eq!(g.access_log, vec![100.0, 200.0, 300.0], "继承真使用痕迹");
        assert!(
            (g.created_at - 50.0).abs() < 1e-12,
            "保留资历（沿用 created_at）"
        );
        assert_eq!(
            g.pointer.reference.as_deref(),
            Some("a.rs:10"),
            "指针照旧指向 ground truth"
        );
        assert_eq!(g.status, Status::Active);
        assert!(g.superseded_by.is_none());
        assert!(g.tags.contains(&"arch".to_string()), "保留源 tags");
        assert!(
            g.tags.contains(&"graduated".to_string()),
            "补 graduated 血缘标记"
        );

        // 覆盖 cue / importance / to-level（跨档到 L1）。
        let g2 = build_graduated_memory(
            "g2",
            &src,
            graduate_target_level(src.level, Some(Level::L1)).unwrap(),
            Some("更精炼的通用 cue"),
            Some(0.9),
        );
        assert_eq!(g2.level, Level::L1);
        assert_eq!(g2.cue, "更精炼的通用 cue");
        assert!((g2.importance - 0.9).abs() < 1e-12);
    }

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
        let min_uses = 1;

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
            gc_should_delete(&cold_old, now, ttl, tomb_ttl, min_uses),
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
            !gc_should_delete(&cold_fresh, now, ttl, tomb_ttl, min_uses),
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
            !gc_should_delete(&cold_touched, now, ttl, tomb_ttl, min_uses),
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
            !gc_should_delete(&tomb_fresh, now, ttl, tomb_ttl, min_uses),
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
            gc_should_delete(&tomb_old, now, ttl, tomb_ttl, min_uses),
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
            !gc_should_delete(&active_old, now, ttl, tomb_ttl, min_uses),
            "active 永不删"
        );
        assert!(
            !gc_should_delete(&sup_old, now, ttl, tomb_ttl, min_uses),
            "superseded 永不删"
        );

        // —— 第二臂（与门）：极少真使用才删；真用过多次的即便冷掉也保留 ——

        // cold 超 ttl，但被 confirm-use 过 3 次（access_log.len()=3 > min_uses=1）→ 保留。
        // 三次真使用都在 200 天前以上，故 last_touch 仍 200 天前、age>ttl（第一臂成立），
        // 仅靠第二臂「极少使用不成立」挡住删除。
        let cold_used = ms(
            "c4",
            Level::L3,
            None,
            Status::Cold,
            0.0,
            now - 210.0 * day,
            vec![now - 210.0 * day, now - 205.0 * day, now - 200.0 * day],
        );
        assert!(
            !gc_should_delete(&cold_used, now, ttl, tomb_ttl, min_uses),
            "cold 超 ttl 但真用过多次（第二臂不成立）应保留"
        );
        // 放宽 min_uses 到 3：同一条真使用次数 3<=3，两臂皆成立 → 删（证明 min_uses 生效）。
        assert!(
            gc_should_delete(&cold_used, now, ttl, tomb_ttl, 3),
            "min_uses 放宽到 3 后，真使用 3 次的 cold 两臂皆成立应删"
        );

        // tombstone 超长 ttl，但被 recall/confirm-use 过多次（len=4 > min_uses）→ 保留。
        // 仍在持续防坑的负知识不该被清掉。
        let tomb_used = ms(
            "t3",
            Level::L3,
            None,
            Status::Tombstone,
            0.0,
            now - 4000.0 * day,
            vec![
                now - 4000.0 * day,
                now - 3990.0 * day,
                now - 3980.0 * day,
                now - 3970.0 * day,
            ],
        );
        assert!(
            !gc_should_delete(&tomb_used, now, ttl, tomb_ttl, min_uses),
            "tombstone 超长 ttl 但仍被反复 recall（第二臂不成立）应保留"
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
