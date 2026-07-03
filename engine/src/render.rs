//! 热索引渲染。
//!
//! 提供纯函数式渲染热索引文本 [`render`]，以及从 JSON 目录加载记忆的
//! [`load_store`] / [`load_store_entries`]。自本切片起持久化主存储已迁到
//! redb（见 [`crate::store`]）：`render` / `consolidate` 的数据来自 redb，
//! 这里的 JSON 目录扫描**仅服务 `import` 命令**（把旧 JSON 库迁入 redb），
//! 其行为保持与前两切片完全一致。
//!
//! 渲染只读取 `effective` 排序、不修改任何状态——降级/淘汰是 consolidate
//! 的职责，本模块不改状态，只在输出里把候选标注出来。
//!
//! 设计文档参考：§3 总体架构、§5 懒计算、§13 渲染热索引。

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::activation::effective;
use crate::model::{params, Level, Memory, Status, EVICT_THRESHOLD};

/// 一个加载自磁盘的存储条目：记忆本身 + 它所在的文件路径。
///
/// 用于需要把更新后的记忆**写回原文件**的场景（如 consolidate）。
#[derive(Debug, Clone, PartialEq)]
pub struct StoreEntry {
    /// 该记忆所在的 `*.json` 文件路径。
    pub path: PathBuf,
    /// 反序列化得到的记忆。
    pub memory: Memory,
}

/// 扫描目录下所有 `*.json` 文件，每个文件反序列化为一条 [`StoreEntry`]
/// （记忆 + 原文件路径）。约定**一文件一记忆**。
///
/// 解析失败的文件会被跳过并把错误写到 stderr，不会 panic，也不会中断整体加载。
///
/// # 参数
/// - `dir`：存储目录。
///
/// # Errors
/// 当读取目录本身失败（如目录不存在、无权限）时，返回 [`std::io::Error`]。
/// 单个文件的读取/解析失败不会上抛，仅跳过。
pub fn load_store_entries(dir: &Path) -> std::io::Result<Vec<StoreEntry>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!("跳过无法读取的目录项：{e}");
                continue;
            }
        };
        let path = entry.path();
        // 仅处理 .json 文件。
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("跳过读取失败的文件 {}：{e}", path.display());
                continue;
            }
        };
        match serde_json::from_str::<Memory>(&content) {
            Ok(memory) => out.push(StoreEntry { path, memory }),
            Err(e) => {
                eprintln!("跳过解析失败的文件 {}：{e}", path.display());
                continue;
            }
        }
    }
    Ok(out)
}

/// 扫描目录下所有 `*.json` 文件，每个文件反序列化为一条 [`Memory`]。
///
/// 行为与 [`load_store_entries`] 一致，只是丢弃文件路径、只返回记忆列表。
/// 解析失败的文件会被跳过并把错误写到 stderr，不会 panic，也不会中断整体加载。
///
/// # 参数
/// - `dir`：存储目录。
///
/// # Errors
/// 当读取目录本身失败（如目录不存在、无权限）时，返回 [`std::io::Error`]。
/// 单个文件的读取/解析失败不会上抛，仅跳过。
pub fn load_store(dir: &Path) -> std::io::Result<Vec<Memory>> {
    Ok(load_store_entries(dir)?
        .into_iter()
        .map(|e| e.memory)
        .collect())
}

/// 按降序比较两个 `effective` 值（把 NaN 视作最小，沉到末尾）。
fn cmp_eff_desc(a: f64, b: f64) -> Ordering {
    // partial_cmp 在遇到 NaN 时返回 None；此处把 NaN 当作最小值排到最后。
    b.partial_cmp(&a).unwrap_or(Ordering::Equal)
}

/// 通用层（`project == None`）的显示顺序与中文标题。
fn general_level_order() -> [(Level, &'static str); 3] {
    [
        (Level::L1, "L1 潜意识层"),
        (Level::L2, "L2 重要层"),
        (Level::L3, "L3 普通层"),
    ]
}

/// 项目层（L4.x）的显示顺序与中文标题模板。
fn project_level_order() -> [(Level, &'static str); 3] {
    [
        (Level::L4_1, "L4.1 项目潜意识层"),
        (Level::L4_2, "L4.2 项目重要层"),
        (Level::L4_3, "L4.3 项目普通层"),
    ]
}

/// 渲染一个层级的一节（标题 + 容量内条目 + 超容降级/淘汰候选区）。
///
/// 把传入的 `items`（已限定为同一层级、active 的记忆引用）按 `effective` 降序
/// 排序后，先输出容量内的条目，再把超出 `capacity` 的条目单列为降级/淘汰候选区。
///
/// 标题格式：通用层为 `[<层标题>] [已用/容量]`；项目层（`project` 为 `Some`）
/// 为 `[<项目名>] <层标题> [已用/容量]`，便于区分不同项目的同层小节。
fn render_level_section(
    buf: &mut String,
    level: Level,
    layer_title: &str,
    project: Option<&str>,
    items: &[&Memory],
    now: f64,
) {
    let p = params(level);

    // 复制引用切片以便排序（不改动调用方）。
    let mut items: Vec<&Memory> = items.to_vec();
    items.sort_by(|a, b| cmp_eff_desc(effective(a, now), effective(b, now)));

    let used = items.len();
    match project {
        None => buf.push_str(&format!("\n[{layer_title}] [{used}/{}]\n", p.capacity)),
        Some(name) => buf.push_str(&format!(
            "\n[{name}] {layer_title} [{used}/{}]\n",
            p.capacity
        )),
    }

    if items.is_empty() {
        buf.push_str("  （空）\n");
        return;
    }

    // 容量内的条目。
    let in_cap = items.len().min(p.capacity);
    for m in &items[..in_cap] {
        let eff = effective(m, now);
        buf.push_str(&format_line(m, eff, p.load_full));
    }

    // 超出容量的条目：降级/淘汰候选，单列标注。
    if items.len() > p.capacity {
        buf.push_str(&format!(
            "  -- 降级/淘汰候选（超出容量 {}，共 {} 条）--\n",
            p.capacity,
            items.len() - p.capacity
        ));
        for m in &items[p.capacity..] {
            let eff = effective(m, now);
            buf.push_str(&format_line(m, eff, p.load_full));
        }
    }
}

/// 渲染热索引为一段可读文本（纯函数，不做任何 IO）。
///
/// 渲染**全部传入记忆**（作用域已由调用方挂载了哪些库决定，本函数不再过滤项目）。
///
/// 规则：
/// - 只取 `status == Active` 的记忆。
/// - 先输出通用三节：L1/L2/L3（仅 `project == None` 的记忆）。
/// - 再**按项目名升序**逐个项目输出其 L4.1/L4.2/L4.3 三小节，小节标题带项目名
///   （形如 `[engram] L4.1 项目潜意识层`）。
/// - 每节内按 `effective` 降序；超出该层 `capacity` 的条目单列一节标注为
///   降级/淘汰候选（本函数不真正改状态）。
/// - `load_full` 层每条显示 `cue + pointer.reference + [imp/eff]`；
///   仅 cue 层只显示 `cue + [eff]`。
/// - 顶部打印 `now`。
///
/// # 参数
/// - `memories`：要渲染的全部记忆集合（通用 + 已挂载各项目库的合并集）。
/// - `now`：当前时间（unix 秒）。
pub fn render(memories: &[Memory], now: f64) -> String {
    render_scoped(memories, now, &[])
}

/// 同 [`render`]，但额外保证 `active_projects` 里的每个项目**一定输出其
/// L4.1/L4.2/L4.3 三小节**——即使该项目当前没有任何 active 记忆，也会渲染成
/// `[<名>] L4.x [0/容量]（空）`。
///
/// 用途：热索引注入时，把「当前作用域解析出的项目」即使 L4 为空也显式列出，
/// 给用户「该项目已被识别/挂载」的可视确认（避免空项目在热索引里完全隐身）。
pub fn render_scoped(memories: &[Memory], now: f64, active_projects: &[&str]) -> String {
    let mut buf = String::new();
    buf.push_str(&format!("== Engram 热索引 (now={now}) ==\n"));

    // 通用三节：只含 project == None 的 active 记忆。
    for (level, title) in general_level_order() {
        let items: Vec<&Memory> = memories
            .iter()
            .filter(|m| m.level == level)
            .filter(|m| m.status == Status::Active)
            .filter(|m| m.project.is_none())
            .collect();
        render_level_section(&mut buf, level, title, None, &items, now);
    }

    // 收集所有出现过的项目名（来自 active 的 L4 记忆），并入「活跃项目名」后按名升序输出。
    // 并入 active_projects 是为了让没有任何 L4 记忆的活跃项目也显示其 L4.x 空段。
    let mut projects: BTreeSet<&str> = memories
        .iter()
        .filter(|m| m.status == Status::Active)
        .filter_map(|m| m.project.as_deref())
        .collect();
    projects.extend(active_projects.iter().copied());

    for project in projects {
        for (level, title) in project_level_order() {
            let items: Vec<&Memory> = memories
                .iter()
                .filter(|m| m.level == level)
                .filter(|m| m.status == Status::Active)
                .filter(|m| m.project.as_deref() == Some(project))
                .collect();
            render_level_section(&mut buf, level, title, Some(project), &items, now);
        }
    }

    buf
}

/// 把 `effective` 格式化成稳定的可读字符串（处理无穷大）。
fn fmt_eff(eff: f64) -> String {
    if eff == f64::INFINITY {
        "INF".to_string()
    } else if eff == f64::NEG_INFINITY {
        "-INF".to_string()
    } else {
        format!("{eff:.3}")
    }
}

/// 取记忆 id 的稳定短标记（`mem-` 后的**首段**，形如 `18beb926f903e300`）。
///
/// 热索引每行前缀 `#<tok>`，让会话末复盘者能把「注入且真影响了输出」的记忆精确
/// 映射回 id 去 `confirm-use`——闭合「注入(主通道)→真使用→加固」回路，而不必对
/// 长 cue 做模糊反查。取首段是因为它由**创建时间派生、单调唯一**；次段是内容哈希，
/// 同文/近义记忆会撞，不适合当唯一标记。非 `mem-<a>-<b>` 形制的 id 原样返回。
fn id_tok(id: &str) -> &str {
    id.strip_prefix("mem-")
        .and_then(|rest| rest.split('-').next())
        .filter(|s| !s.is_empty())
        .unwrap_or(id)
}

/// 渲染单条记忆为一行文本。
///
/// 行首统一带 `#<id 首段>` 短标记（见 [`id_tok`]），供复盘者映射回 id 做加固。
/// `load_full` 为 `true` 时附带 `pointer.reference` 与 `imp`，否则只显示 cue 与 eff。
/// 当 `eff` 跌破 [`EVICT_THRESHOLD`] 时，行尾追加 `[淘汰]` 标记
/// （仅标注，不真正改状态——状态变更是 consolidate 的职责）。
fn format_line(m: &Memory, eff: f64, load_full: bool) -> String {
    // effective 跌破淘汰阈值时打标，提示这是淘汰候选。
    let evict_tag = if eff < EVICT_THRESHOLD {
        " [淘汰]"
    } else {
        ""
    };
    let tok = id_tok(&m.id);
    if load_full {
        let reference = m.pointer.reference.as_deref().unwrap_or("-");
        format!(
            "  #{} {} | {} | [imp={:.2}/eff={}]{}\n",
            tok,
            m.cue,
            reference,
            m.importance,
            fmt_eff(eff),
            evict_tag
        )
    } else {
        format!("  #{} {} | [eff={}]{}\n", tok, m.cue, fmt_eff(eff), evict_tag)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Pointer, Status};

    fn mem(id: &str, level: Level, project: Option<&str>, status: Status, eff_hint: f64) -> Memory {
        // 用单次访问（now 之前 eff_hint 秒的关系不重要，这里仅占位）构造记忆。
        Memory {
            id: id.to_string(),
            cue: format!("cue-{id}"),
            pointer: Pointer {
                kind: "file".to_string(),
                reference: Some(format!("src/{id}.rs:1")),
                detail: None,
            },
            level,
            project: project.map(|s| s.to_string()),
            importance: eff_hint,
            pinned: false,
            access_log: vec![1_000_000_000.0],
            status,
            superseded_by: None,
            created_at: 1_000_000_000.0,
            tags: vec![],
        }
    }

    #[test]
    fn render_filters_non_active() {
        let now = 1_000_000_000.0;
        let mems = vec![
            mem("a", Level::L1, None, Status::Active, 0.5),
            mem("b", Level::L1, None, Status::Cold, 0.9),
        ];
        let out = render(&mems, now);
        assert!(out.contains("cue-a"), "active 记忆应出现");
        assert!(!out.contains("cue-b"), "cold 记忆不应出现");
    }

    #[test]
    fn render_groups_all_passed_projects() {
        let now = 1_000_000_000.0;
        // 同时传入两个项目的 L4 与通用记忆：全部应渲染（作用域由挂载决定，
        // render 不再过滤项目）。
        let mems = vec![
            mem("g", Level::L1, None, Status::Active, 0.5),
            mem("pb", Level::L4_1, Some("beta"), Status::Active, 0.5),
            mem("pa", Level::L4_1, Some("alpha"), Status::Active, 0.5),
        ];
        let out = render(&mems, now);
        // 通用与两个项目的 L4 都应出现。
        assert!(out.contains("cue-g"), "通用记忆应出现");
        assert!(out.contains("cue-pa"), "alpha 项目 L4 应出现");
        assert!(out.contains("cue-pb"), "beta 项目 L4 应出现");
        // 小节标题应带项目名。
        assert!(
            out.contains("[alpha] L4.1 项目潜意识层"),
            "应有 alpha 的 L4.1 小节标题，实得：\n{out}"
        );
        assert!(
            out.contains("[beta] L4.1 项目潜意识层"),
            "应有 beta 的 L4.1 小节标题，实得：\n{out}"
        );
        // 项目应按名升序：alpha 节应排在 beta 节之前。
        let pos_alpha = out.find("[alpha] L4.1").expect("应含 alpha 节");
        let pos_beta = out.find("[beta] L4.1").expect("应含 beta 节");
        assert!(pos_alpha < pos_beta, "项目应按名升序，alpha 应在 beta 前");
    }

    #[test]
    fn render_marks_overflow() {
        let now = 1_000_000_000.0;
        // L1 容量为 7，放 9 条 active 通用记忆，应有 2 条进降级候选区。
        let mut mems = Vec::new();
        for i in 0..9 {
            mems.push(mem(
                &format!("m{i}"),
                Level::L1,
                None,
                Status::Active,
                0.1 * i as f64,
            ));
        }
        let out = render(&mems, now);
        assert!(out.contains("降级/淘汰候选"), "超容应出现降级候选区");
        assert!(out.contains("[9/7]"), "标题应显示已用/容量");
    }

    #[test]
    fn id_tok_takes_first_segment() {
        // 标准 mem-<首段>-<次段> 取首段。
        assert_eq!(id_tok("mem-18beb926f903e300-fb64bb2fae588bef"), "18beb926f903e300");
        // 非标准 id 原样返回。
        assert_eq!(id_tok("a"), "a");
        assert_eq!(id_tok("mem-"), "mem-");
    }

    #[test]
    fn render_line_prefixes_id_token() {
        let now = 1_000_000_000.0;
        let m = Memory {
            id: "mem-18beb926f903e300-fb64bb2fae588bef".to_string(),
            cue: "带标记的记忆".to_string(),
            pointer: Pointer {
                kind: "none".to_string(),
                reference: None,
                detail: None,
            },
            level: Level::L2,
            project: None,
            importance: 0.5,
            pinned: false,
            access_log: vec![now],
            status: Status::Active,
            superseded_by: None,
            created_at: now,
            tags: vec![],
        };
        let out = render(&[m], now);
        assert!(
            out.contains("#18beb926f903e300 带标记的记忆"),
            "每行应带 #<id首段> 短标记，实得：\n{out}"
        );
    }

    #[test]
    fn render_cue_only_layer_hides_reference() {
        let now = 1_000_000_000.0;
        // L3 是仅 cue 层（load_full=false），不应出现 pointer.reference。
        let mems = vec![mem("x", Level::L3, None, Status::Active, 0.5)];
        let out = render(&mems, now);
        assert!(out.contains("cue-x"));
        assert!(!out.contains("src/x.rs:1"), "仅 cue 层不应显示 reference");
    }

    #[test]
    fn render_marks_evict_candidate() {
        // 构造一条 effective 跌破 EVICT_THRESHOLD(-3.0) 的 L3 记忆：
        // importance 0、单次访问在很久以前、grace 已退、L3 floor 极低(-10) 不截断。
        let base = 1_000_000_000.0;
        let m = Memory {
            id: "old".to_string(),
            cue: "极久未用".to_string(),
            pointer: Pointer {
                kind: "none".to_string(),
                reference: None,
                detail: None,
            },
            level: Level::L3,
            project: None,
            importance: 0.0,
            pinned: false,
            // 1000 天前单次访问。
            access_log: vec![base - 1000.0 * 86400.0],
            status: Status::Active,
            superseded_by: None,
            created_at: base - 1000.0 * 86400.0,
            tags: vec![],
        };
        let out = render(&[m], base);
        assert!(out.contains("极久未用"));
        assert!(out.contains("[淘汰]"), "跌破淘汰阈值的记忆应被标 [淘汰]");
    }

    #[test]
    fn render_scoped_shows_empty_active_project() {
        let now = 1_000_000_000.0;
        // 没有任何 engram 项目的 L4 记忆，但 engram 是当前活跃项目 → 应显式输出空的 L4.x 段。
        let mems = vec![mem("g", Level::L1, None, Status::Active, 0.5)];
        let out = render_scoped(&mems, now, &["engram"]);
        assert!(
            out.contains("[engram] L4.1 项目潜意识层 [0/"),
            "活跃项目即使 L4 为空也应显示 L4.1 段，实得：\n{out}"
        );
        assert!(out.contains("[engram] L4.2 项目重要层 [0/"));
        assert!(out.contains("[engram] L4.3 项目普通层 [0/"));
        assert!(out.contains("（空）"), "空段应标（空）");
        // 普通 render（无活跃项目）则不显示该空项目。
        let out2 = render(&mems, now);
        assert!(!out2.contains("[engram]"), "无活跃项目时空项目不应出现");
    }
}
