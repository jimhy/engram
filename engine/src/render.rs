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
//! **常驻 / 按需二分**：只有 [`crate::model::TierParams::resident`] 为 `true` 的层
//! （默认 L1/L2/L4.1/L4.2）在热索引里渲染正文；非常驻层（默认 L3/L4.3）连节标题都
//! 不渲，只在输出末尾占一段「按需层目录」（条数 + 主题词 + recall 查法，预算见
//! [`ONDEMAND_INDEX_CHAR_BUDGET`]）。这条二分是为了让整段注入落在
//! [`HOT_INDEX_CHAR_BUDGET`] 内——宿主 Claude Code 对 hook 返回的
//! `additionalContext` 有 10000 字符硬上限，超限会把全文落盘、上下文只留前 2000 字符。
//!
//! 设计文档参考：§3 总体架构、§5 懒计算、§13 渲染热索引。

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
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

/// hot-index / session-start 注入路径的默认字符预算。
///
/// **9600 的出处是宿主硬上限，不是估算**：Claude Code 对 hook 返回的
/// `additionalContext` 有 **10000 字符**上限（编译进二进制的常量，无配置项可覆盖）；
/// 超上限的注入会被整段落盘，上下文里只保留前 2000 字符。9600 = 10000 − 400 字符
/// 安全余量（吸收 eff 数字宽度、长项目名等个位到十位数的渲染噪声）。
///
/// **本预算是「注入体总额」，不是「渲染产物额度」**：注入体 = 前言 + 可选的管理目录
/// 提示行 + 渲染产物，调用方（`main.rs` 的 `run_hot_index`）先按实际字符数扣掉前缀
/// 开销、再把余额传给 [`render_budgeted`]。**不要**把前缀开销写成本常量里预留的魔数
/// 余量——前言从 56 字符扩到 277 字符那次就证明了魔数会被悄悄吃光（400 的余量当场
/// 只剩 123），而实扣不会随文案变动而失效。
/// 计数按 `char`（中文一字一符），与容量治理的 [`crate::render::memory_render_cost`]
/// 同源。CLI 用 `--budget` 覆盖，传 0 表示不限（与历史行为一致）。
///
/// 设计承诺「常驻注入成本 = 常数」：复盘者停摆、底层堆积时，若无预算上限，
/// 超容候选区会把注入量撑到无界。
///
/// **与容量治理的分工**：常驻四层（L1/L2/L4.1/L4.2）的
/// [`crate::model::TierParams::char_budget`] 合计 8000 字符，加上节标题与尾部
/// 「按需层目录」（[`ONDEMAND_INDEX_CHAR_BUDGET`]）仍低于本预算——正常状态下保险丝
/// 不熔断；非常驻层（L3/L4.3）不再渲染正文，故其 `char_budget` 不占注入体量。
pub const HOT_INDEX_CHAR_BUDGET: usize = 9600;

/// 尾部「按需层目录」整段的字符预算（层名/条数行 + recall 查法行 + 主题词行）。
///
/// 400 字符约占 [`HOT_INDEX_CHAR_BUDGET`] 的 4%：用它换来「非常驻层不再渲正文」
/// 省下的全部空间，同时保证模型知道这些记忆**存在**、且知道**用什么词**能捞回来。
/// 超预算时只截主题词表（逐词丢尾并补 `…`），层名/条数行与查法行必留。
pub const ONDEMAND_INDEX_CHAR_BUDGET: usize = 400;

/// 「按需层目录」主题词表最多列出的词数。
///
/// 与 [`ONDEMAND_INDEX_CHAR_BUDGET`] 构成双约束，先触发者生效：字符预算防止长标签
/// 撑爆整段，词数上限防止一堆 `tag×1` 的长尾把有效信号淹掉。
const ONDEMAND_INDEX_MAX_TAGS: usize = 16;

/// hot-index / session-start 注入路径的**前言**：贴在渲染文本最前面的一段固定文案。
///
/// 与渲染同源放在本模块：它和 [`render_budgeted`] 的产物一起构成注入正文，两者字符数
/// 共同受宿主上限约束（Claude Code 的 `additionalContext` 硬上限 **10000 字符**，超出
/// 则整段落盘、上下文只留前 2000 字符，且不可配置），改一处必须同时算另一处。
///
/// 本文案 **277 字符**（按 `char` 计，不含末尾换行——换行由调用方补），四行分别是：
/// 1. 按指针查 ground truth、不要凭印象（历史前言的语义，逐字保留）；
/// 2. 声明常驻层只有 L1/L2/L4.1/L4.2，L3 普通层与 L4.3 项目普通层是按需层；
/// 3. 三个必须主动检索的时机（易踩坑操作前 / 不可逆决定前 / 用户提「上次」「之前」）；
/// 4. `recall` 的命令形态，以及「查不到时它会明说」这一行为承诺。
///
/// 它是**每轮都注入**的常驻成本，任何增补都要按「每个字都要挣得其位」审。
///
/// 第 4 行原本写的是「中文必须空格分词」，那是 [`crate::commands::segment_field`]
/// 引入 CJK 二元 ngram **之前**的约束，早已与实现不符（照它写反而会误导）。
/// 换成两条今天真实成立、且能改变调用方行为的信息：中文可直接写自然语句；
/// 查不到时会明确弃权而不是硬凑候选（见 [`crate::retrieval`]）。
pub const HOT_INDEX_PREAMBLE: &str = concat!(
    "「以下是你的 engram 长期记忆热索引。需要细节时按每条的指针去查 ground truth，不要凭印象。\n",
    "常驻的只有 L1/L2/L4.1/L4.2 四层；L3 普通层与 L4.3 项目普通层是按需层——不在下面、不检索就看不到（末尾[按需层]一行给了条数与主题词）。\n",
    "三种时机务必先检索再动手：① 要做易踩坑的事之前 ② 要做不可逆决定之前 ③ 用户说「上次 / 之前 / 还记得吗」。\n",
    "检索：engram recall --query \"直接写自然语句，中文会自动切词\"（库路径用 engram resolve 拿；库里没有时它会明说「无相关记忆」，不会硬凑）」",
);

/// 不参与逐条裁尾的渲染片所用的节组 id（见 [`RenderPiece::group`]）。
const GROUP_NONE: usize = usize::MAX;

/// 一段可截断的渲染片：文本 + 截断优先级 + 所属节组 + 条目数（截断提示行用）。
struct RenderPiece {
    /// 截断优先级：0 = 结构行与常驻全文层容量内正文；1 = 常驻但仅 cue 的层的
    /// 容量内条目；2 = 超容降级/淘汰候选区。超预算时从数值大的类开始截断：
    /// 2/1 类**整段**截断，0 类正文**按条裁尾**（详见 [`assemble_with_budget`]）。
    cut_class: u8,
    /// 所属节组 id：同一节容量内正文的各条共享一个 id，供逐条裁尾时把多条的
    /// 「…另有 N 条」合并成一行、并落实「每节至少留 1 条」。不参与逐条裁尾的片
    /// （节标题、空段标记、顶部标题行、整段截断的候选区、尾部按需层目录）取
    /// [`GROUP_NONE`]。
    group: usize,
    /// 该段包含的记忆条目数（生成「…另有 N 条」提示行用；结构行为 0）。
    /// 常驻全文层的正文改为**逐条 push**，故这些片恒为 1。
    entries: usize,
    /// 段文本（自带换行）。
    text: String,
}

/// 渲染一个层级的一节（标题 + 容量内条目 + 超容降级/淘汰候选区），产出为
/// 可按预算截断的 [`RenderPiece`] 序列（显示顺序即 push 顺序）。
///
/// **非常驻层直接返回、什么都不 push**：[`crate::model::TierParams::resident`]
/// 为 `false` 的层（默认 L3/L4.3）在热索引里既不占节标题也不占正文，统一由输出
/// 末尾的「按需层目录」一段列出条数与主题词（私有 `render_ondemand_index`）。
///
/// 常驻层：把传入的 `items`（已限定为同一层级、active 的记忆引用）按 `effective`
/// 降序排序后，先输出容量内的条目，再把超出 `capacity` 的条目单列为降级/淘汰候选区。
///
/// 标题格式：通用层为 `[<层标题>] [已用/容量]`；项目层（`project` 为 `Some`）
/// 为 `[<项目名>] <层标题> [已用/容量]`，便于区分不同项目的同层小节。
///
/// 截断优先级（见 [`RenderPiece::cut_class`]）：常驻全文层（`load_full = true`）的
/// 容量内正文**逐条 push**，超紧预算时可按条裁尾、每节保底 1 条；常驻但仅 cue 的层
/// 合成一段整体可截；超容候选区最先被截。
fn render_level_section(
    pieces: &mut Vec<RenderPiece>,
    level: Level,
    layer_title: &str,
    project: Option<&str>,
    items: &[&Memory],
    now: f64,
) {
    let p = params(level);

    // 非常驻层：热索引里不渲任何东西（连空段标记都不渲），细节走 recall 显式取回。
    if !p.resident {
        return;
    }

    // 本节的节组 id：取节标题即将落到的下标，天然唯一且不与 GROUP_NONE 相撞。
    let group = pieces.len();

    // 复制引用切片以便排序（不改动调用方）。
    let mut items: Vec<&Memory> = items.to_vec();
    items.sort_by(|a, b| cmp_eff_desc(effective(a, now), effective(b, now)));

    let used = items.len();
    let header = match project {
        None => format!("\n[{layer_title}] [{used}/{}]\n", p.capacity),
        Some(name) => format!("\n[{name}] {layer_title} [{used}/{}]\n", p.capacity),
    };
    pieces.push(RenderPiece {
        cut_class: 0,
        group: GROUP_NONE,
        entries: 0,
        text: header,
    });

    if items.is_empty() {
        pieces.push(RenderPiece {
            cut_class: 0,
            group: GROUP_NONE,
            entries: 0,
            text: "  （空）\n".to_string(),
        });
        return;
    }

    // 容量内的条目。常驻全文层逐条 push（cut_class 0，可按条裁尾）；常驻但仅 cue
    // 的层合成一段（cut_class 1，整段截断）——默认参数下无此组合，保留以支持
    // EngramConfig 自定义。
    let in_cap = items.len().min(p.capacity);
    if p.load_full {
        for m in &items[..in_cap] {
            let eff = effective(m, now);
            pieces.push(RenderPiece {
                cut_class: 0,
                group,
                entries: 1,
                text: format_line(m, eff, true),
            });
        }
    } else {
        let mut body = String::new();
        for m in &items[..in_cap] {
            let eff = effective(m, now);
            body.push_str(&format_line(m, eff, false));
        }
        pieces.push(RenderPiece {
            cut_class: 1,
            group,
            entries: in_cap,
            text: body,
        });
    }

    // 超出容量的条目：降级/淘汰候选，单列标注；预算吃紧时最先被整段截断。
    // 整段截断不需要节组归并，故取 GROUP_NONE。
    if items.len() > p.capacity {
        let mut over = format!(
            "  -- 降级/淘汰候选（超出容量 {}，共 {} 条）--\n",
            p.capacity,
            items.len() - p.capacity
        );
        for m in &items[p.capacity..] {
            let eff = effective(m, now);
            over.push_str(&format_line(m, eff, p.load_full));
        }
        pieces.push(RenderPiece {
            cut_class: 2,
            group: GROUP_NONE,
            entries: items.len() - p.capacity,
            text: over,
        });
    }
}

/// 截断提示行——三趟截断共用的**唯一**文案产出点。
fn truncation_hint(entries: usize) -> String {
    format!("  …另有 {entries} 条（预算截断，recall 可查）\n")
}

/// 把渲染片按字符预算拼装成最终文本。
///
/// `budget == 0` 或总量未超预算时逐段原样拼接（与无预算渲染逐字节一致）。
/// 超预算时分三趟，优先级从低到高：
///
/// 1. **第一趟**（`cut_class == 2`）：超容降级/淘汰候选区，按**逆显示序**逐段
///    **整段**截断；
/// 2. **第二趟**（`cut_class == 1`）：常驻但仅 cue 的层的容量内条目，同样整段截断；
/// 3. **第三趟**（`cut_class == 0` 且 `entries > 0`，即常驻全文层的容量内正文）：
///    按**逆显示序逐条裁尾**——先裁最后一个项目的 L4.2，再 L4.1，再 L2，最后 L1；
///    同一节内先裁 `effective` 最低的那条（节内条目本就按 `effective` 降序 push）。
///    **每节至少保留 1 条**，避免某层在热索引里彻底消失。
///
/// 被截处输出一行 [`truncation_hint`]；第三趟同一节内被裁多条只合并出**一行**提示，
/// 挂在该节最靠前的被裁位置（即紧跟保留下来的条目之后）。节标题、空段标记、顶部
/// 标题行与尾部「按需层目录」是结构行（`cut_class == 0` 且 `entries == 0`），
/// **永不截**。
///
/// **预算不是硬保证**：结构行 + 每节保底 1 条构成不可压缩的地板；且被裁行若比提示行
/// 还短，单次替换可能不减反增（下一轮继续裁尾，提示行只涨位数、总量收敛）。因此极端
/// 情况下输出仍可能略超 `budget`——注入路径另有宿主侧上限兜底（见
/// [`HOT_INDEX_CHAR_BUDGET`]）。
fn assemble_with_budget(pieces: Vec<RenderPiece>, budget: usize) -> String {
    // 预算按字符（char）计而非字节：中文一字一符。计数对象是渲染片文本本身
    // （正文行即 format_line 的输出），与容量治理的 memory_render_cost 同源——
    // 全系统只有这一套计量。
    let mut total: usize = pieces.iter().map(|p| p.text.chars().count()).sum();
    let mut texts: Vec<String> = pieces.iter().map(|p| p.text.clone()).collect();

    if budget == 0 || total <= budget {
        return texts.concat();
    }

    // 第一、二趟：cut_class 2 → 1，逆显示序整段截断。
    for class in [2u8, 1u8] {
        if total <= budget {
            break;
        }
        for i in (0..pieces.len()).rev() {
            if total <= budget {
                break;
            }
            if pieces[i].cut_class != class || pieces[i].entries == 0 {
                continue;
            }
            let trunc = truncation_hint(pieces[i].entries);
            total = total - texts[i].chars().count() + trunc.chars().count();
            texts[i] = trunc;
        }
    }

    if total <= budget {
        return texts.concat();
    }

    // 第三趟：常驻全文层正文按条裁尾。
    // live：该节还活着的条目数（保底 1）；cut：该节已裁条数；hint_at：该节提示行下标。
    let mut live: BTreeMap<usize, usize> = BTreeMap::new();
    for p in &pieces {
        if p.cut_class == 0 && p.entries > 0 && p.group != GROUP_NONE {
            *live.entry(p.group).or_insert(0) += 1;
        }
    }
    let mut cut: BTreeMap<usize, usize> = BTreeMap::new();
    let mut hint_at: BTreeMap<usize, usize> = BTreeMap::new();
    for i in (0..pieces.len()).rev() {
        if total <= budget {
            break;
        }
        let piece = &pieces[i];
        if piece.cut_class != 0 || piece.entries == 0 || piece.group == GROUP_NONE {
            continue;
        }
        let remaining = live.get(&piece.group).copied().unwrap_or(0);
        if remaining <= 1 {
            // 每节保底 1 条：这一节不再裁。
            continue;
        }
        live.insert(piece.group, remaining - 1);

        // 撤掉该节上一轮写在更靠后位置的提示行，本轮把提示行改写到 i（逆序遍历
        // 保证 i 始终是该节当前最靠前的被裁位置）。
        if let Some(prev) = hint_at.insert(piece.group, i) {
            total -= texts[prev].chars().count();
            texts[prev].clear();
        }
        total -= texts[i].chars().count();
        let n = cut.entry(piece.group).or_insert(0);
        *n += 1;
        let hint = truncation_hint(*n);
        total += hint.chars().count();
        texts[i] = hint;
    }

    texts.concat()
}

/// 渲染尾部的「按需层目录」：非常驻层的条数 + 冷库条数 + 主题词表 + recall 查法。
///
/// 这是「非常驻层不再渲正文」的补偿：模型必须知道**这些记忆存在**、以及**用什么词**
/// 能把它们捞回来，否则按需层等同于不存在。
///
/// 统计口径与渲染主体**刻意不同**（这是本函数唯一读取非 Active 记忆的地方）：
/// - 逐层计数只算 `status == Active` 的非常驻层记忆——即改动前会被渲染、现在被收
///   起来的那批；顺序与主体一致（通用层在前，项目层按项目名升序）；
/// - 另单列一项「冷库 N 条」：`status == Cold` 的记忆虽不属于任何常驻层，但 `recall`
///   缺省连冷库一起搜（`--active-only` 才只搜热），不告诉模型就等于把它们藏起来；
/// - 主题词表在「非常驻层 Active + 全部 Cold」这个并集上按 `tags` 频次聚合，输出
///   `tag×次数`，频次降序、同频按字典序升序（保证输出稳定可测）；空白 tag 丢弃。
///
/// 整段被 [`ONDEMAND_INDEX_CHAR_BUDGET`] 与 [`ONDEMAND_INDEX_MAX_TAGS`] 双约束钳住：
/// 超出时只丢主题词表尾部的词并补 `…`，层名/条数行与查法行必留。没有任何可列内容
/// 时返回 `None`（一个字符都不占）。
///
/// # 参数
/// - `memories`：与 [`render_budgeted`] 同一批记忆（含非 Active，故能统计冷库）。
fn render_ondemand_index(memories: &[Memory]) -> Option<String> {
    // 1. 目录项：非常驻层的 active 条数，显示顺序与主体一致。
    let mut items: Vec<String> = Vec::new();
    for (level, title) in general_level_order() {
        if params(level).resident {
            continue;
        }
        let n = memories
            .iter()
            .filter(|m| m.status == Status::Active && m.level == level && m.project.is_none())
            .count();
        if n > 0 {
            items.push(format!("{title} {n} 条"));
        }
    }
    let projects: BTreeSet<&str> = memories
        .iter()
        .filter(|m| m.status == Status::Active && !params(m.level).resident)
        .filter_map(|m| m.project.as_deref())
        .collect();
    for project in projects {
        for (level, title) in project_level_order() {
            if params(level).resident {
                continue;
            }
            let n = memories
                .iter()
                .filter(|m| {
                    m.status == Status::Active
                        && m.level == level
                        && m.project.as_deref() == Some(project)
                })
                .count();
            if n > 0 {
                items.push(format!("[{project}] {title} {n} 条"));
            }
        }
    }
    let cold = memories.iter().filter(|m| m.status == Status::Cold).count();
    if cold > 0 {
        items.push(format!("冷库 {cold} 条"));
    }
    if items.is_empty() {
        return None;
    }

    // 2. 固定两行：目录行 + 查法行。
    const HOW: &str =
        "  查法：engram recall --query \"直接写自然语句\"（库路径见 engram resolve；匹配 cue+tags，中文自动切词；可加 --tag/--level 缩小范围）\n";
    const TAGS_PREFIX: &str = "  主题词（可直接作查询词）：";
    let mut out = format!("\n[按需层] 未常驻、须显式检索：{}\n", items.join(" · "));
    out.push_str(HOW);

    // 3. 主题词表：在「非常驻层 active + 全部 cold」并集上聚合 tags。
    let mut hist: BTreeMap<&str, usize> = BTreeMap::new();
    for m in memories.iter().filter(|m| {
        m.status == Status::Cold || (m.status == Status::Active && !params(m.level).resident)
    }) {
        for tag in &m.tags {
            let tag = tag.trim();
            if !tag.is_empty() {
                *hist.entry(tag).or_insert(0) += 1;
            }
        }
    }
    if hist.is_empty() {
        return Some(out);
    }
    // BTreeMap 已按字典序给出；sort_by_key 是稳定排序，故同频保持字典序。
    let mut ranked: Vec<(&str, usize)> = hist.into_iter().collect();
    ranked.sort_by_key(|e| std::cmp::Reverse(e.1));

    // 逐词追加，放不下就停并补「…」；预留 2 字符给省略号。
    let fixed = out.chars().count() + TAGS_PREFIX.chars().count() + 1;
    let mut line = String::new();
    let mut dropped = ranked.len() > ONDEMAND_INDEX_MAX_TAGS;
    for (tag, n) in ranked.iter().take(ONDEMAND_INDEX_MAX_TAGS) {
        let word = format!("{tag}×{n}");
        let extra = usize::from(!line.is_empty()) + word.chars().count();
        if fixed + line.chars().count() + extra + 2 > ONDEMAND_INDEX_CHAR_BUDGET {
            dropped = true;
            break;
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(&word);
    }
    if line.is_empty() {
        return Some(out);
    }
    out.push_str(TAGS_PREFIX);
    out.push_str(&line);
    if dropped {
        out.push_str(" …");
    }
    out.push('\n');
    Some(out)
}

/// 渲染热索引为一段可读文本（纯函数，不做任何 IO）。
///
/// 渲染**全部传入记忆**（作用域已由调用方挂载了哪些库决定，本函数不再过滤项目）。
///
/// 规则：
/// - 正文只取 `status == Active` 的记忆（尾部按需层目录另计冷条目，见下）。
/// - 先输出通用的**常驻**层 L1/L2（仅 `project == None` 的记忆）。
/// - 再**按项目名升序**逐个项目输出其常驻层 L4.1/L4.2 两小节，小节标题带项目名
///   （形如 `[engram] L4.1 项目潜意识层`）。
/// - **非常驻层**（[`crate::model::TierParams::resident`] 为 `false`，默认 L3/L4.3）
///   既不渲节标题也不渲正文，改由输出末尾的「按需层目录」一段列出各层条数、冷库
///   条数与主题词表（私有 `render_ondemand_index`，预算见
///   [`ONDEMAND_INDEX_CHAR_BUDGET`]）。
/// - 每节内按 `effective` 降序；超出该层 `capacity` 的条目单列一节标注为
///   降级/淘汰候选（本函数不真正改状态）。
/// - 常驻层每条显示 `cue + pointer.reference + [imp/eff]`。
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
///
/// 本函数**不限预算**（等价于 [`render_budgeted`] 传 `budget = 0`），供 `render`
/// 命令等「要看全量」的路径使用；注入路径请用 [`render_budgeted`]。
pub fn render_scoped(memories: &[Memory], now: f64, active_projects: &[&str]) -> String {
    render_budgeted(memories, now, active_projects, 0)
}

/// 同 [`render_scoped`]，但带**字符预算**：`budget > 0` 且总渲染量超出时，按
/// 优先级从低到高截断——超容候选区整段截断 → 常驻仅 cue 层整段截断 → 常驻全文层
/// 正文按**逆显示序逐条裁尾**（每节保底 1 条），截断处输出一行
/// `…另有 N 条（预算截断，recall 可查）`；节标题与尾部按需层目录**保证完整**。
/// 详见 `assemble_with_budget`。`budget = 0` 表示不限，输出与 [`render_scoped`]
/// 逐字节一致。
///
/// 这是 hot-index / session-start 注入路径的入口（默认预算
/// [`HOT_INDEX_CHAR_BUDGET`]）：兑现「常驻注入成本 = 常数」——复盘者停摆导致
/// 底层/候选区无界堆积时，注入量也被钳在预算内，细节仍可经 recall 显式取回。
pub fn render_budgeted(
    memories: &[Memory],
    now: f64,
    active_projects: &[&str],
    budget: usize,
) -> String {
    let mut pieces: Vec<RenderPiece> = Vec::new();
    pieces.push(RenderPiece {
        cut_class: 0,
        group: GROUP_NONE,
        entries: 0,
        text: format!("== Engram 热索引 (now={now}) ==\n"),
    });

    // 通用三节：只含 project == None 的 active 记忆。
    for (level, title) in general_level_order() {
        let items: Vec<&Memory> = memories
            .iter()
            .filter(|m| m.level == level)
            .filter(|m| m.status == Status::Active)
            .filter(|m| m.project.is_none())
            .collect();
        render_level_section(&mut pieces, level, title, None, &items, now);
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
            render_level_section(&mut pieces, level, title, Some(project), &items, now);
        }
    }

    // 尾部「按需层目录」：非常驻层（默认 L3/L4.3）与冷库不渲正文，但必须让模型
    // 知道这些记忆存在、以及用什么词 recall 得回来。属结构行，永不被预算截断。
    if let Some(tail) = render_ondemand_index(memories) {
        pieces.push(RenderPiece {
            cut_class: 0,
            group: GROUP_NONE,
            entries: 0,
            text: tail,
        });
    }

    assemble_with_budget(pieces, budget)
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

/// 计量一条记忆在热索引中渲染行的字符数——**容量治理与渲染预算共用的单一计量来源**。
///
/// 直接构造与热索引逐字节同源的渲染行（私有 `format_line`）并对其 `.chars().count()`：
/// - `load_full = true`（L1/L2/L4.1/L4.2）按**全文行**计：cue + pointer.reference + imp/eff；
/// - `load_full = false`（L3/L4.3）按 **cue 行**计：cue + eff。
///
/// 注意：`load_full = false` 的层默认是**非常驻层**，其渲染行已不出现在 [`render`]
/// 的输出里（只在尾部按需层目录里以条数聚合）；但 consolidate 的 `char_budget`
/// 治理仍按这条 cue 行计量——那是「若要常驻会占多少」的度量，与是否实际渲染无关。
///
/// `eff` 字段以占位值 `0.0`（渲染为 `0.000`，5 字符）代入——实际渲染时 eff 的数字
/// 宽度随取值有 ±2 字符浮动（`INF` / 负号 / 多位整数），淘汰候选另有 ` [淘汰]`
/// 标记，均属个位数噪声，不影响预算治理判断。
///
/// **用途与纪律**：consolidate 溢出步（[`crate::consolidate`]）按它累计各层常驻
/// 字符量、对照 [`crate::model::TierParams::char_budget`] 做条数+预算双约束；
/// [`render_budgeted`] 的预算计数对象即 `format_line` 产出的渲染行文本本身。
/// 两处同源于 `format_line`，**禁止另起第二套计数**——渲染行格式变化时本函数
/// 自动同步。
///
/// **换算参考**：中文约 1.5~2 字符/token，英文约 4 字符/token。
pub fn memory_render_cost(m: &Memory, load_full: bool) -> usize {
    // 占位 eff=0.0：高于淘汰阈值（不带 [淘汰] 标记），格式化为定宽 "0.000"。
    format_line(m, 0.0, load_full).chars().count()
}

/// 渲染单条记忆为一行文本。
///
/// 行首统一带 `#<id 首段>` 短标记（见 [`id_tok`]），供复盘者映射回 id 做加固。
/// `load_full` 为 `true` 时附带 `pointer.reference` 与 `imp`，否则只显示 cue 与 eff。
/// 当 `eff` 跌破 [`EVICT_THRESHOLD`] 时，行尾追加 `[淘汰]` 标记
/// （仅标注，不真正改状态——状态变更是 consolidate 的职责）。
///
/// 默认参数下该标记在热索引里**已不可达**：常驻四层的 `tier_floor` 全部高于
/// [`EVICT_THRESHOLD`]（L1 4.5 / L2 1.0 / L4.1 3.0 / L4.2 0.5，`effective` 被托底），
/// 而 floor 低到 -10 的 L3/L4.3 已是非常驻层、不渲正文。保留该分支是给自定义
/// `EngramConfig` 与 consolidate 计量用。
///
/// 本函数是热索引行的**唯一**产出点：容量治理的计量（[`memory_render_cost`]）与
/// 渲染预算的计数（[`assemble_with_budget`] 对片文本计 `char`）都以它的输出为源。
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
        format!(
            "  #{} {} | [eff={}]{}\n",
            tok,
            m.cue,
            fmt_eff(eff),
            evict_tag
        )
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
            schema_version: crate::model::MEMORY_SCHEMA_VERSION,
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
        // 但 cold 条目会在尾部按需层目录里被计数——recall 缺省搜得到，不能藏起来。
        assert!(
            out.contains("冷库 1 条"),
            "cold 条目应计入按需层目录，实得：\n{out}"
        );
        assert!(
            !out.contains("主题词"),
            "无 tag 时不应输出主题词行，实得：\n{out}"
        );
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
        assert_eq!(
            id_tok("mem-18beb926f903e300-fb64bb2fae588bef"),
            "18beb926f903e300"
        );
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
            schema_version: crate::model::MEMORY_SCHEMA_VERSION,
        };
        let out = render(&[m], now);
        assert!(
            out.contains("#18beb926f903e300 带标记的记忆"),
            "每行应带 #<id首段> 短标记，实得：\n{out}"
        );
    }

    #[test]
    fn render_hides_nonresident_layer_entirely() {
        let now = 1_000_000_000.0;
        // L3 是非常驻层：既不渲节标题也不渲正文，只在尾部按需层目录里出现。
        let mut m = mem("x", Level::L3, None, Status::Active, 0.5);
        m.tags = vec!["rust".to_string()];
        let out = render(&[m], now);
        assert!(!out.contains("cue-x"), "非常驻层不应渲正文，实得：\n{out}");
        assert!(!out.contains("src/x.rs:1"), "非常驻层不应显示 reference");
        assert!(
            !out.contains("[L3 普通层]"),
            "非常驻层不应再渲节标题，实得：\n{out}"
        );
        assert!(
            out.contains("[按需层] 未常驻、须显式检索：L3 普通层 1 条"),
            "应在尾部按需层目录里列出 L3，实得：\n{out}"
        );
        assert!(out.contains("rust×1"), "主题词表应聚合 tags，实得：\n{out}");
    }

    #[test]
    fn cue_only_line_hides_reference() {
        // load_full=false 的渲染行只有 cue + eff。非常驻层已不进热索引，故直接
        // 测行格式（consolidate 的 char_budget 治理仍按这条行计量）。
        let m = mem("x", Level::L3, None, Status::Active, 0.5);
        let line = format_line(&m, 0.5, false);
        assert!(line.contains("cue-x"));
        assert!(!line.contains("src/x.rs:1"), "仅 cue 行不应显示 reference");
        assert_eq!(
            memory_render_cost(&m, false),
            format_line(&m, 0.0, false).chars().count(),
            "仅 cue 层成本计量必须与 cue 行字符数一致"
        );
    }

    #[test]
    fn format_line_marks_evict_candidate() {
        // 跌破 EVICT_THRESHOLD(-3.0) 的条目行尾应带 [淘汰]。改直测 format_line：
        // 常驻四层的 tier_floor 均高于淘汰阈值（L1 4.5 / L2 1.0 / L4.1 3.0 /
        // L4.2 0.5，effective 被托底），floor 低到 -10 的 L3/L4.3 又已不渲正文，
        // 该标记在默认参数下无法再经 render 输出复现。
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
            schema_version: crate::model::MEMORY_SCHEMA_VERSION,
        };
        let eff = effective(&m, base);
        assert!(
            eff < EVICT_THRESHOLD,
            "前置：eff 应跌破淘汰阈值，实得 {eff}"
        );
        let line = format_line(&m, eff, false);
        assert!(line.contains("极久未用"));
        assert!(
            line.ends_with(" [淘汰]\n"),
            "跌破淘汰阈值的记忆应被标 [淘汰]，实得：{line}"
        );
    }

    // ---- 渲染预算（HOT_INDEX_CHAR_BUDGET / render_budgeted）----

    /// 构造「L1 满员 + L2 大量超容」的集合：L1 7 条（容量内）、L2 40 条
    /// （容量 30，超容 10 条进候选区）。两层都是常驻全文层——非常驻层（L3/L4.3）
    /// 已不进热索引，无法再用来测预算截断。
    ///
    /// 全部条目的 `importance` 与 `access_log` 同值 → `effective` 相等 →
    /// `sort_by` 稳定排序保持输入序，故容量内恰为 `l2_00..l2_29`、候选区恰为
    /// `l2_30..l2_39`（编号补零，避免 `contains` 前缀误匹配）。
    fn budget_fixture() -> Vec<Memory> {
        let mut mems = Vec::new();
        for i in 0..7 {
            mems.push(mem(
                &format!("l1_{i}"),
                Level::L1,
                None,
                Status::Active,
                0.9,
            ));
        }
        for i in 0..40 {
            mems.push(mem(
                &format!("l2_{i:02}"),
                Level::L2,
                None,
                Status::Active,
                0.1,
            ));
        }
        mems
    }

    #[test]
    fn render_budget_zero_matches_unbudgeted() {
        let now = 1_000_000_000.0;
        let mems = budget_fixture();
        // budget=0 = 不限：与 render_scoped 逐字节一致（回归保护）。
        assert_eq!(
            render_budgeted(&mems, now, &[], 0),
            render_scoped(&mems, now, &[]),
        );
    }

    #[test]
    fn render_budget_truncates_lowest_priority_first() {
        let now = 1_000_000_000.0;
        let mems = budget_fixture();
        let full = render_scoped(&mems, now, &[]);
        let full_chars = full.chars().count();
        assert!(full_chars > 2000, "前置：全量渲染应明显超过测试预算");

        // 预算只比全量少一点：最低优先级的超容候选区（cut_class 2）应先被整段
        // 截断，且省下的量足够——L1/L2 容量内正文一条不少（第三趟根本不触发）。
        let out = render_budgeted(&mems, now, &[], full_chars - 200);
        assert!(
            out.contains("…另有 10 条（预算截断，recall 可查）"),
            "候选区应被整段截断并输出提示行，实得：\n{out}"
        );
        assert!(
            !out.contains("cue-l2_39"),
            "候选区条目应被截掉，实得：\n{out}"
        );
        assert!(
            out.chars().count() <= full_chars - 200,
            "截断后应落在预算内"
        );
        // 常驻层容量内正文完好。
        for i in 0..7 {
            assert!(
                out.contains(&format!("cue-l1_{i}")),
                "L1 条目 l1_{i} 应完整保留，实得：\n{out}"
            );
        }
        for i in 0..30 {
            assert!(
                out.contains(&format!("cue-l2_{i:02}")),
                "L2 容量内条目 l2_{i:02} 应完整保留，实得：\n{out}"
            );
        }
    }

    #[test]
    fn render_budget_trims_resident_body_but_keeps_one_per_section() {
        let now = 1_000_000_000.0;
        let mems = budget_fixture();
        // 极紧预算：候选区整段砍掉后仍不够 → 第三趟对常驻正文按逆显示序逐条裁尾。
        // L2 显示序在 L1 之后，先被裁；每节至少保留最靠前的 1 条。
        let out = render_budgeted(&mems, now, &[], 300);
        assert!(out.contains("cue-l1_0"), "L1 应保底留 1 条，实得：\n{out}");
        assert!(out.contains("cue-l2_00"), "L2 应保底留 1 条，实得：\n{out}");
        assert!(
            !out.contains("cue-l2_29"),
            "L2 尾部条目应先被裁掉，实得：\n{out}"
        );
        // 同一节被裁多条只合并出一行提示：至多 3 行（候选区 1 + L1 1 + L2 1）。
        let hints = out.matches("（预算截断，recall 可查）").count();
        assert!(
            (1..=3).contains(&hints),
            "同节裁尾应合并成一行提示，实得 {hints} 行：\n{out}"
        );
        // 节标题仍在（截断不抹掉结构）。
        assert!(
            out.contains("[L1 潜意识层] [7/7]"),
            "L1 节标题应保留，实得：\n{out}"
        );
        assert!(
            out.contains("[L2 重要层] [40/30]"),
            "L2 节标题应保留，实得：\n{out}"
        );
    }

    #[test]
    fn memory_render_cost_matches_rendered_line() {
        let now = 1_000_000_000.0;
        // 一条全文层（L2，常驻）记忆。构造使实际 eff 落在 [0,10) 且高于淘汰阈值
        // ——渲染行的 eff 字段为定宽 "x.xxx"（与成本计量的占位 "0.000" 同宽、
        // 无 [淘汰] 标记），此时 memory_render_cost（consolidate 溢出步用的计数）
        // 必须与 render 输出行的字符数逐字节相等（单一计量来源）。
        //
        // 仅 cue 层（L3）已是非常驻层、不进 render 输出，其计量在
        // `cue_only_line_hides_reference` 里对 format_line 校验。
        let full = mem("costfull", Level::L2, None, Status::Active, 0.5);
        let out = render(std::slice::from_ref(&full), now);
        let eff = effective(&full, now);
        assert!(
            (0.0..10.0).contains(&eff),
            "前置：{} 的 eff 应落在 [0,10) 保证定宽格式，实得 {eff}",
            full.id
        );
        let line = out
            .lines()
            .find(|l| l.contains(&full.cue))
            .unwrap_or_else(|| panic!("渲染输出应含 {} 的行", full.id));
        // lines() 已剥掉行尾 \n，补回 1 字符再比。
        assert_eq!(
            memory_render_cost(&full, true),
            line.chars().count() + 1,
            "{} 的成本计量必须与 render 输出行字符数逐字节相等",
            full.id
        );
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
        // L4.3 是非常驻层：连空段都不渲（有条目时才进尾部按需层目录）。
        assert!(
            !out.contains("[engram] L4.3 项目普通层"),
            "非常驻层不应再渲节标题，实得：\n{out}"
        );
        assert!(out.contains("（空）"), "空段应标（空）");
        // 普通 render（无活跃项目）则不显示该空项目。
        let out2 = render(&mems, now);
        assert!(!out2.contains("[engram]"), "无活跃项目时空项目不应出现");
    }

    // ---- 按需层目录（render_ondemand_index / ONDEMAND_INDEX_CHAR_BUDGET）----

    #[test]
    fn ondemand_index_lists_nonresident_and_cold_with_topics() {
        let now = 1_000_000_000.0;
        let mut a = mem("n1", Level::L3, None, Status::Active, 0.5);
        a.tags = vec!["pitfall".to_string(), "windows".to_string()];
        let mut b = mem("n2", Level::L4_3, Some("engram"), Status::Active, 0.5);
        b.tags = vec!["pitfall".to_string()];
        let mut c = mem("n3", Level::L2, None, Status::Cold, 0.5);
        // 含一个纯空白 tag：应被丢弃，不进主题词表。
        c.tags = vec!["pitfall".to_string(), "  ".to_string()];
        let out = render(&[a, b, c], now);
        assert!(
            out.contains(
                "[按需层] 未常驻、须显式检索：L3 普通层 1 条 · [engram] L4.3 项目普通层 1 条 · 冷库 1 条"
            ),
            "按需层目录应逐层列条数并单列冷库，实得：\n{out}"
        );
        assert!(
            out.contains("engram recall"),
            "应给出 recall 查法，实得：\n{out}"
        );
        // 主题词按频次降序、同频按字典序；冷条目的 tag 也计入。
        assert!(
            out.contains("主题词（可直接作查询词）：pitfall×3 windows×1"),
            "主题词表应按频次降序聚合，实得：\n{out}"
        );
    }

    #[test]
    fn ondemand_index_absent_when_nothing_to_list() {
        let now = 1_000_000_000.0;
        let out = render(&[mem("g", Level::L1, None, Status::Active, 0.5)], now);
        assert!(
            !out.contains("[按需层]"),
            "无非常驻层条目、无冷条目时不应占任何字符，实得：\n{out}"
        );
    }

    #[test]
    fn ondemand_topics_fit_char_budget() {
        let now = 1_000_000_000.0;
        // 200 条冷记忆、各带一个很长的独立 tag：主题词表必须被字符预算钳住并补「…」。
        let mems: Vec<Memory> = (0..200)
            .map(|i| {
                let mut m = mem(&format!("c{i:03}"), Level::L3, None, Status::Cold, 0.5);
                m.tags = vec![format!("超长主题标签压爆预算用编号{i:03}")];
                m
            })
            .collect();
        let out = render(&mems, now);
        let tail = out
            .split("\n[按需层]")
            .nth(1)
            .unwrap_or_else(|| panic!("应含按需层目录，实得：\n{out}"));
        let tail_chars = tail.chars().count() + "[按需层]".chars().count();
        assert!(
            tail_chars <= ONDEMAND_INDEX_CHAR_BUDGET,
            "按需层目录应被 {ONDEMAND_INDEX_CHAR_BUDGET} 字符预算钳住，实得 {tail_chars} 字符：\n{tail}"
        );
        assert!(tail.contains(" …"), "词被丢弃时应补省略号，实得：\n{tail}");
    }
}
