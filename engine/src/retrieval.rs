//! BM25 检索打分与弃权判据。
//!
//! 取代 [`crate::commands::score_query`] 那套「子串命中率」做 recall 的排序键。
//! 旧打分的两个结构性缺陷（均有实测）：
//!
//! 1. **长文档通吃**：它对文档根本不分词，只做 `haystack.contains(token)`，
//!    cue 越长越容易偶然命中——实测被选中条目的 cue 长度中位 740 字符，
//!    而候选池中位只有 468。
//! 2. **短 query 虚高**：分数是 `命中词元数 / query 词元总数`，
//!    query 只有两三个词元时，命中一个烂大街的词就得满分——
//!    实测 `只回复两个字：好的` 这种 query 拿到 1.00。
//!
//! BM25 的 tf 饱和（k1）与长度归一（b）恰好分别治这两条。
//!
//! # 与 query 侧分词的一致性
//!
//! 文档侧分词走 [`tokenize_doc`]，它与 [`crate::commands::tokenize_query`]
//! 共用同一个 [`crate::commands::segment_field`]——**绝不能各自复制一份**，
//! 两套分词一旦漂移，打分就会在中文上静默失真且没有任何测试会红。
//! 唯一的区别是文档侧**保留重复**（BM25 要词频），query 侧去重。
//!
//! # 参数标定
//!
//! `k1` / `b` 取信息检索领域的通用默认值；弃权阈值则是在本项目真实记忆库上
//! 实测标定的，标定过程与它的**已知局限**见 [`ABSTAIN_MIN_QUERY_INFO`]。

use std::collections::HashMap;

use crate::commands::{segment_field, tokenize_query};
use crate::model::Memory;

/// BM25 的词频饱和参数。
///
/// 控制「同一个词在文档里出现更多次」的边际收益衰减速度。1.2 是通用默认值。
pub const BM25_K1: f64 = 1.2;

/// BM25 的文档长度归一参数，取值 `0.0..=1.0`。
///
/// `0.0` 完全不做长度归一（长文档通吃，正是旧打分的病）；
/// `1.0` 完全按长度线性折算。0.75 是通用默认值，也是本模块治「长 cue 通吃」的关键——
/// 把它改成 0.0，`bm25_penalizes_long_documents` 测试会立刻变红。
pub const BM25_B: f64 = 0.75;

/// 弃权判据一：query 自身的信息量下限，单位是「等效稀有词个数」
/// （即 query 全部词元的 IDF 之和 ÷ [`Corpus::idf_max`]）。
///
/// 用信息量而不是**词元个数**，是因为二者在中文上差别很大：
/// `好的` / `明白了` 是一两个烂大街的二元 ngram，信息量极低；
/// 而 `PowerShell ErrorActionPreference NativeCommandError` 只有三个词元、
/// 却个个稀有——按词元个数设闸会把后者一起误伤（实测确实误伤了它）。
///
/// # 标定过程与已知局限（务必读完再改这个值）
///
/// 在本项目真实记忆库（261 条可召回记忆）上，用 50 道人工 golden 题作正类
/// （「库里确实有相关记忆」）、50 条纯噪声 query 作负类（「库里没有」）标定，
/// 判据为 `query_info >= 本常量 && hit_strength >= ABSTAIN_MIN_HIT_STRENGTH`。
///
/// 为验证阈值不随语料规模漂移，在五个规模的子语料上分别复核同一组参数
/// （子语料强制包含全部 gold 条目，其余随机补足；`engine/eval/` 下的脚本可复现）：
///
/// | 语料规模 | 噪声弃权率 | golden 误弃权率 |
/// |---|---|---|
/// | 80 条 | 88% | **0%** |
/// | 120 条 | 82% | **0%** |
/// | 160 条 | 82% | **0%** |
/// | 200 条 | 84% | **0%** |
/// | 261 条（全量） | 82% | **0%** |
///
/// 对照：**现状 [`crate::commands::score_query`] 的噪声弃权率只有 18%**
/// （50 条噪声里只有 9 条一个词元都没蹭上；其余 41 条都会返回一串低分噪音）。
///
/// **已知局限**：达不到最初拟定的「噪声弃权 ≥ 90%」，稳定在 82~88%。
/// 之所以按这组参数定案而不是继续往上调：
///
/// - 误弃权率在全部五个规模上都是 **0%**，而**误弃权（静默漏检）才是危险的那一侧**——
///   用户永远不知道自己漏了什么；噪声漏网只是多返回几条，用户看得见、能自己判断。
///   继续抬高阈值能把噪声弃权率推过 90%，代价是误弃权从 0% 起步往上走，不划算。
/// - 相对现状的 18% 已经是 4.5 倍改进，且不存在「不如现状」的风险。
///
/// 先后被实测否掉的三种更简单的判据，别再走回头路：
/// - BM25 原始分绝对阈值：只在 261 条上有解，80~200 条全部无解（分数量纲随语料规模漂移）
/// - 分数落差比（top1/top2、top1/top5、top1/median、(top1-top2)/top1）：**所有规模全部无解**
/// - 命中词元最高 IDF 单独作判据：噪声弃权率只有 65%，误弃却高达 24%
pub const ABSTAIN_MIN_QUERY_INFO: f64 = 2.05;

/// 启用统计型弃权判据所需的**最小语料规模**。
///
/// 低于这个条数时，[`judge`] 只保留「一个词元都没命中」这种确定性弃权，
/// 不做 [`ABSTAIN_MIN_QUERY_INFO`] / [`ABSTAIN_MIN_HIT_STRENGTH`] 的统计判断。
///
/// # 为什么必须有这道门
///
/// IDF 是在语料上估出来的统计量，文档数太少时它毫无意义，而且**两个方向都会错**：
///
/// - 库里没有的词（df = 0）IDF 极高 → `query_info` 虚高 → 该弃权时不弃权；
/// - 库里常见的词 IDF 被压得极低 → `query_info` 虚低 → **不该弃权时弃权**。
///
/// 后者是实打实踩到的：一个 5 条记忆的库里查 `redb lock`，`redb` 出现在 4/5 条上、
/// IDF 只有 0.288，整个 query 的信息量算出来 1.207 < 2.05，于是把一次完全正常的
/// 检索判成了「本库无相关记忆」（`recall_search_ranking_and_filters` 当场变红）。
///
/// 取值 80 = 弃权阈值实际标定过的**最小语料规模**。80 以下没有标定数据，
/// 就不外推——宁可让小库完全没有弃权保护（退回今天的行为，多返回几条噪音），
/// 也不能靠猜出来的阈值制造静默漏检。
pub const ABSTAIN_MIN_CORPUS: usize = 80;

/// 弃权判据二：最强候选的归一化命中强度下限
/// （即 top1 的 BM25 分 ÷ [`Corpus::idf_max`]）。
///
/// 除以 `idf_max` 是为了跨语料规模可比——BM25 分数的量纲本身就是 IDF 的量纲，
/// 不归一的话同一个阈值在不同大小的库上含义完全不同。
///
/// 标定同 [`ABSTAIN_MIN_QUERY_INFO`]。
pub const ABSTAIN_MIN_HIT_STRENGTH: f64 = 1.20;

/// 文档侧分词：与 query 侧同规则，但**保留重复**（BM25 要词频）。
///
/// 与 [`crate::commands::tokenize_query`] 的唯一区别就是不去重。
pub fn tokenize_doc(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for field in text.split_whitespace() {
        out.extend(segment_field(field));
    }
    out
}

/// 一条记忆参与检索的文本：`cue` 加上空格连接的 `tags`。
///
/// 与 [`crate::commands::score_query`] 的 haystack 口径保持一致，
/// 这样新旧打分器比较的是同一批文本，对拍才有意义。
pub fn doc_text(m: &Memory) -> String {
    if m.tags.is_empty() {
        m.cue.clone()
    } else {
        let mut s = String::with_capacity(m.cue.len() + 1 + m.tags.iter().map(|t| t.len() + 1).sum::<usize>());
        s.push_str(&m.cue);
        s.push(' ');
        s.push_str(&m.tags.join(" "));
        s
    }
}

/// 一个 query 词元在某文档上的命中详情，用于弃权判据与 `--json` 归因。
#[derive(Debug, Clone, PartialEq)]
pub struct TokenHit {
    /// 命中的词元。
    pub token: String,
    /// 该词元在本语料下的 IDF（越高越有判别力）。
    pub idf: f64,
    /// 该词元在这篇文档里出现的次数。
    pub tf: u32,
}

/// 语料统计：BM25 打分所需的 N / df / 文档长度 / 平均文档长度。
///
/// 每次 recall 现算即可——本项目单个作用域的记忆量在几百条量级，
/// 建表耗时可忽略。**刻意不做跨调用缓存**：缓存会引入「记忆改了而统计没更新」
/// 的一致性问题，收益却接近于零。
#[derive(Debug, Clone, Default)]
pub struct Corpus {
    /// 文档数 N。
    n: usize,
    /// 每篇文档的词频表。
    tf: Vec<HashMap<String, u32>>,
    /// 每篇文档的长度（词元数，含重复）。
    dl: Vec<usize>,
    /// 每个词元的文档频率 df。
    df: HashMap<String, usize>,
    /// 平均文档长度。
    avgdl: f64,
}

impl Corpus {
    /// 用一批记忆建语料统计。文档顺序即后续 [`Corpus::score`] 的 `idx` 口径。
    pub fn build(mems: &[&Memory]) -> Self {
        let n = mems.len();
        let mut tf: Vec<HashMap<String, u32>> = Vec::with_capacity(n);
        let mut dl: Vec<usize> = Vec::with_capacity(n);
        let mut df: HashMap<String, usize> = HashMap::new();
        let mut total_len = 0usize;

        for m in mems {
            let toks = tokenize_doc(&doc_text(m));
            let mut counter: HashMap<String, u32> = HashMap::new();
            for t in &toks {
                *counter.entry(t.clone()).or_insert(0) += 1;
            }
            for t in counter.keys() {
                *df.entry(t.clone()).or_insert(0) += 1;
            }
            total_len += toks.len();
            dl.push(toks.len());
            tf.push(counter);
        }

        let avgdl = if n == 0 { 0.0 } else { total_len as f64 / n as f64 };
        Corpus { n, tf, dl, df, avgdl }
    }

    /// 文档数。
    pub fn len(&self) -> usize {
        self.n
    }

    /// 语料是否为空。
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// 词元的 IDF：`ln(1 + (N - df + 0.5) / (df + 0.5))`。
    ///
    /// 用这个 `+0.5` 平滑形式而非经典 Robertson-Sparck-Jones 式，是因为它**恒为正**：
    /// 后者在 `df > N/2` 时会翻负，让「命中了一个烂大街的词」反而扣分，
    /// 那会毁掉弃权判据赖以成立的零点语义。
    ///
    /// `df = 0`（query 词元在语料里根本不存在）时返回一个大于 [`Corpus::idf_max`]
    /// 的值。这是刻意保留的：它不会贡献任何分数（tf 恒为 0），但会抬高
    /// [`Corpus::query_info`]——「问了库里完全没有的专有名词」确实是**信息量高**的 query，
    /// 该由命中强度那一条闸去拦，而不是在这里假装它信息量低。
    pub fn idf(&self, token: &str) -> f64 {
        let df = self.df.get(token).copied().unwrap_or(0) as f64;
        let n = self.n as f64;
        (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
    }

    /// 单个词元的 IDF 上限，即 `df = 1` 时的取值。
    ///
    /// 用作各种阈值的归一化基准，让阈值随语料规模自动缩放。
    /// 空语料返回 `1.0`（而不是让公式产生负数或 NaN），使调用方无需特判。
    pub fn idf_max(&self) -> f64 {
        if self.n == 0 {
            return 1.0;
        }
        let n = self.n as f64;
        let v = (1.0 + (n - 1.0 + 0.5) / 1.5).ln();
        if v.is_finite() && v > 0.0 {
            v
        } else {
            1.0
        }
    }

    /// query 的信息量，单位「等效稀有词个数」：全部词元 IDF 之和 ÷ [`Corpus::idf_max`]。
    pub fn query_info(&self, tokens: &[String]) -> f64 {
        if tokens.is_empty() {
            return 0.0;
        }
        let sum: f64 = tokens.iter().map(|t| self.idf(t)).sum();
        sum / self.idf_max()
    }

    /// 给第 `idx` 篇文档按 BM25 打分。
    ///
    /// `idx` 越界、空语料、空 query、`avgdl` 为 0 时一律返回 `0.0`，不 panic。
    pub fn score(&self, idx: usize, tokens: &[String]) -> f64 {
        if idx >= self.n || tokens.is_empty() || self.avgdl <= 0.0 {
            return 0.0;
        }
        let tf = &self.tf[idx];
        let dl = self.dl[idx] as f64;
        let mut total = 0.0f64;
        for t in tokens {
            let f = tf.get(t).copied().unwrap_or(0) as f64;
            if f <= 0.0 {
                continue;
            }
            let denom = f + BM25_K1 * (1.0 - BM25_B + BM25_B * dl / self.avgdl);
            if denom <= 0.0 {
                continue;
            }
            total += self.idf(t) * (f * (BM25_K1 + 1.0)) / denom;
        }
        if total.is_finite() {
            total
        } else {
            0.0
        }
    }

    /// 第 `idx` 篇文档命中了哪些 query 词元，按 IDF 降序。
    ///
    /// 供弃权判据与 `--json` 归因使用；`idx` 越界时返回空 Vec。
    pub fn hits(&self, idx: usize, tokens: &[String]) -> Vec<TokenHit> {
        if idx >= self.n {
            return Vec::new();
        }
        let tf = &self.tf[idx];
        let mut out: Vec<TokenHit> = tokens
            .iter()
            .filter_map(|t| {
                let c = tf.get(t).copied().unwrap_or(0);
                if c == 0 {
                    None
                } else {
                    Some(TokenHit { token: t.clone(), idf: self.idf(t), tf: c })
                }
            })
            .collect();
        out.sort_by(|a, b| b.idf.partial_cmp(&a.idf).unwrap_or(std::cmp::Ordering::Equal));
        out
    }
}

/// 为什么这次检索该弃权。
///
/// 弃权即「本库没有相关记忆」，是本模块相对旧打分器最重要的新能力：
/// 旧实现只要有**一个**词元子串命中就会返回候选，于是查一个库里根本不存在的
/// 主题时，会返回一串低分噪音，诱导调用它的 agent 把噪音当答案。
#[derive(Debug, Clone, PartialEq)]
pub enum AbstainReason {
    /// query 一个词元都切不出来（空串、纯标点等）。
    EmptyQuery,
    /// 语料为空（库里一条可召回记忆都没有）。
    EmptyCorpus,
    /// 没有任何记忆命中哪怕一个 query 词元。
    NoCandidate,
    /// query 自身信息量不足——多半是「好的」「继续」这类零信息短语。
    ThinQuery {
        /// 实测信息量（等效稀有词个数）。
        info: f64,
        /// 下限 [`ABSTAIN_MIN_QUERY_INFO`]。
        floor: f64,
    },
    /// 有候选，但最强的那条也只是弱命中——多半只蹭到了烂大街的词。
    WeakMatch {
        /// 实测归一化命中强度。
        strength: f64,
        /// 下限 [`ABSTAIN_MIN_HIT_STRENGTH`]。
        floor: f64,
        /// 最强候选命中的词元里 IDF 最高的那个，用于生成「换个词试试」的建议。
        top_token: Option<String>,
    },
}

impl AbstainReason {
    /// 给调用它的 agent 看的一句话解释。
    ///
    /// 刻意写成「可操作」而不是「没找到」——弃权的价值在于让对方知道下一步该干什么。
    pub fn explain(&self) -> String {
        match self {
            AbstainReason::EmptyQuery => "查询词为空（切不出任何词元），请给出具体的查询词。".to_string(),
            AbstainReason::EmptyCorpus => "本库当前没有任何可召回的记忆。".to_string(),
            AbstainReason::NoCandidate => "本库无相关记忆：没有任何一条记忆命中查询词。".to_string(),
            AbstainReason::ThinQuery { info, floor } => format!(
                "查询词信息量不足（{info:.2} < {floor:.2}），这种泛泛的短语匹配不出有意义的结果，请换成具体的名词或错误信息。"
            ),
            AbstainReason::WeakMatch { strength, floor, top_token } => match top_token {
                Some(t) => format!(
                    "本库无相关记忆：最强候选也只蹭到了「{t}」这类泛用词（命中强度 {strength:.2} < {floor:.2}）。"
                ),
                None => format!("本库无相关记忆（最强候选命中强度 {strength:.2} < {floor:.2}）。"),
            },
        }
    }
}

/// 判断这次检索是否该弃权。
///
/// `best` 是全部候选里最高的 BM25 分，`best_hits` 是那条候选的命中详情
/// （由 [`Corpus::hits`] 得到）。返回 `None` 表示不该弃权、正常出结果。
///
/// 两条闸是**与**关系的反面：只要任意一条不达标就弃权。
/// 先判 query 侧（[`ABSTAIN_MIN_QUERY_INFO`]）再判命中侧
/// （[`ABSTAIN_MIN_HIT_STRENGTH`]），这样返回的理由更贴近根因——
/// 「你问得太泛」和「库里真没有」对调用方是两种完全不同的下一步。
pub fn judge(
    corpus: &Corpus,
    tokens: &[String],
    best: f64,
    best_hits: &[TokenHit],
) -> Option<AbstainReason> {
    if tokens.is_empty() {
        return Some(AbstainReason::EmptyQuery);
    }
    if corpus.is_empty() {
        return Some(AbstainReason::EmptyCorpus);
    }
    if best <= 0.0 {
        return Some(AbstainReason::NoCandidate);
    }
    // 样本量不足时只认上面那些确定性判据，不做统计判断（见 ABSTAIN_MIN_CORPUS）。
    if corpus.len() < ABSTAIN_MIN_CORPUS {
        return None;
    }

    let info = corpus.query_info(tokens);
    if info < ABSTAIN_MIN_QUERY_INFO {
        return Some(AbstainReason::ThinQuery { info, floor: ABSTAIN_MIN_QUERY_INFO });
    }

    let strength = best / corpus.idf_max();
    if strength < ABSTAIN_MIN_HIT_STRENGTH {
        return Some(AbstainReason::WeakMatch {
            strength,
            floor: ABSTAIN_MIN_HIT_STRENGTH,
            top_token: best_hits.first().map(|h| h.token.clone()),
        });
    }
    None
}

/// A 档（直接注入记忆正文）的归一化命中强度下限。
///
/// # 这个阈值解决的是什么问题
///
/// `UserPromptSubmit` hook 每条 prompt 都会做一次检索，但**注入是有代价的**：
/// 无关记忆挤占上下文，且每轮都发生。所以不是「查到就注入」，而是分档——
/// 只有强到「top-3 里几乎肯定有对的」才值得把正文塞进去。
///
/// # 标定（2026-09-20，本项目真实库）
///
/// 正类取 `engine/eval/memory-golden.jsonl` 的 47 道可用题（gold 是**具体记忆 id**，
/// 结构上不存在 ground-truth 泄漏），判准是「gold 是否进了 top-3」；
/// 负类取 `engine/eval/noise-queries.txt` 的 50 条噪声 query。
/// 检索池按注入口径排除常驻层（见 [`INJECT_SKIP_RESIDENT_LEVELS`]），实得 600 条。
///
/// | strength ≥ | 触发的题里 top-3 真有对的 | 漏掉的好题 | 噪声误触发 |
/// |---|---|---|---|
/// | 1.2（即弃权线） | 49% | 0 | 14/50 |
/// | 2.0 | 54% | 3 | 1/50 |
/// | 3.0 | 80% | 7 | 0/50 |
/// | **4.0** | **100%** | 10 | **0/50** |
/// | 8.0 | 100% | 20 | 0/50 |
///
/// 取 4.0：**精确率 100%、噪声零误触发**的最低点。再往上只是白白少注入。
/// 被它挡下的 10 道好题不是丢掉，而是降级到 [`InjectTier::Hint`]——
/// 那一档只花一行字，由 agent 自己决定要不要查。
///
/// # 已知做不到的事（别据此以为痛点已解）
///
/// 「发个版」这类**极短口语 prompt** 仍然接不住：实测那条发版铁律记忆在
/// BM25 下压根未命中（换语义向量也只排到第 108 位）。词法、向量、写入端补措辞
/// 三条路都实测过，都到不了可注入的水平。本档位机制的价值在**确定性触发**，
/// 不在「把对的记忆递到嘴边」。
pub const INJECT_FULL_MIN_STRENGTH: f64 = 4.0;

/// 注入时**跳过**的层级：这些层已由热索引常驻注入，再注入一遍纯属重复烧 token。
///
/// 热索引每次会话开始就把 L1/L2/L4.1/L4.2 整个铺进上下文（本项目实测 8152 字符），
/// 所以 prompt 级注入只该去捞**按需层**（L3 / L4.3 / 冷库）——
/// 那正是热索引末尾那行「不检索就看不到」所指的部分。
///
/// 本项目实测：613 条可召回记忆里常驻层只占 13 条，注入池 600 条。
pub const INJECT_SKIP_RESIDENT_LEVELS: [crate::model::Level; 4] = [
    crate::model::Level::L1,
    crate::model::Level::L2,
    crate::model::Level::L4_1,
    crate::model::Level::L4_2,
];

/// 一次 prompt 级检索该怎么注入。
///
/// 分档而非二元判断，是因为**闸门必然有折中**：实测把闸门收紧到能拦住
/// 「把这段代码格式化一下」这类无需记忆的机械指令，就必然会误拦
/// 「准备发布新版本」这类真任务意图——两者的词法信号区间重叠。
/// 与其在一条线上二选一，不如承认折中、让中间档也有用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectTier {
    /// 强证据：直接把 top-N 条记忆正文注入上下文。
    Full,
    /// 弱证据：只注入一行线索，让 agent 自己决定要不要检索。
    ///
    /// 这一档才是痛点（「每次都要我提醒才去搜索记忆」）的直接解药——
    /// 它不要求检索**准**，只要求判断出「这儿可能有东西」，
    /// 而把「提醒」这个动作从用户手里挪到了 hook 上。
    Hint,
    /// 无证据：什么都不注入。
    Silent,
}

/// 判定一次 prompt 级检索的注入档位。
///
/// - `abstained`：本次检索是否已被 [`judge`] 判为该弃权；
/// - `hit_strength`：最强候选的归一化命中强度（`top1 分 ÷ idf_max`），
///   即 [`crate::commands::RecallOutcome::hit_strength`]。
///
/// 判定顺序：
/// 1. `abstained` → [`InjectTier::Silent`]（弃权判据对闲聊很准，实测 10/10 全拦）；
/// 2. 强度达 [`INJECT_FULL_MIN_STRENGTH`] → [`InjectTier::Full`]；
/// 3. 其余 → [`InjectTier::Hint`]。
///
/// **静默闸刻意直接吃 [`judge`] 的结论**（做成入参而不是在函数里另判），
/// 这样「注入的静默档 ≡ recall 的弃权」是结构性保证、不可能漂移：
/// 那套阈值是在真实库上以 50 正 + 50 负、跨五个语料规模标定过的，
/// 另起炉灶只会多一组没标定过的常数。
pub fn inject_tier(abstained: bool, hit_strength: f64) -> InjectTier {
    if abstained {
        return InjectTier::Silent;
    }
    if hit_strength >= INJECT_FULL_MIN_STRENGTH {
        InjectTier::Full
    } else {
        InjectTier::Hint
    }
}

/// 便捷入口：对一批记忆按 query 打分并排序，同时给出弃权判定。
///
/// 返回 `(按分数降序的 (下标, 分数) 列表, 弃权理由)`。
/// 弃权时列表仍然返回（调用方可能要做归因或调试），
/// 但**上层不得把它当结果展示**——弃权就是弃权，"但也许你想看这几条"正是要治的病。
pub fn rank(mems: &[&Memory], query: &str) -> (Vec<(usize, f64)>, Option<AbstainReason>) {
    let tokens = tokenize_query(query);
    let corpus = Corpus::build(mems);
    let mut scored: Vec<(usize, f64)> = (0..mems.len())
        .map(|i| (i, corpus.score(i, &tokens)))
        .filter(|(_, s)| *s > 0.0)
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let (best, best_hits) = match scored.first() {
        Some(&(idx, s)) => (s, corpus.hits(idx, &tokens)),
        None => (0.0, Vec::new()),
    };
    let reason = judge(&corpus, &tokens, best, &best_hits);
    (scored, reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Level, Pointer, Status, MEMORY_SCHEMA_VERSION};

    fn mem(id: &str, cue: &str, tags: &[&str]) -> Memory {
        Memory {
            id: id.to_string(),
            cue: cue.to_string(),
            pointer: Pointer { kind: "none".to_string(), reference: None, detail: None },
            level: Level::L3,
            project: None,
            importance: 0.5,
            pinned: false,
            access_log: vec![],
            status: Status::Active,
            superseded_by: None,
            created_at: 0.0,
            tags: tags.iter().map(|s| s.to_string()).collect(),
            schema_version: MEMORY_SCHEMA_VERSION,
        }
    }

    fn refs(v: &[Memory]) -> Vec<&Memory> {
        v.iter().collect()
    }

    #[test]
    fn tokenize_doc_keeps_duplicates_query_side_dedups() {
        // 文档侧要词频，query 侧去重——两者只该在这一点上不同。
        let doc = tokenize_doc("重复 重复");
        assert_eq!(doc.len(), 2, "文档侧必须保留重复，实得 {doc:?}");
        let q = tokenize_query("重复 重复");
        assert_eq!(q.len(), 1, "query 侧必须去重，实得 {q:?}");
    }

    #[test]
    fn doc_text_joins_cue_and_tags() {
        let m = mem("a", "标题", &["tag1", "tag2"]);
        assert_eq!(doc_text(&m), "标题 tag1 tag2");
        let m2 = mem("b", "只有标题", &[]);
        assert_eq!(doc_text(&m2), "只有标题");
    }

    #[test]
    fn rare_token_has_higher_idf_than_common_one() {
        let mems = vec![
            mem("a", "公共词 稀有甲", &[]),
            mem("b", "公共词 别的", &[]),
            mem("c", "公共词 又别的", &[]),
        ];
        let c = Corpus::build(&refs(&mems));
        // 「公共」三篇都有，「稀有」只有一篇
        assert!(
            c.idf("稀有") > c.idf("公共"),
            "稀有词 IDF({}) 应高于公共词 IDF({})",
            c.idf("稀有"),
            c.idf("公共")
        );
    }

    #[test]
    fn bm25_penalizes_long_documents() {
        // 同样命中一次「目标」，短文档应当排在长文档前面。
        // 这条测试守护 BM25_B：把它改成 0.0，长度归一失效，本测试立刻变红。
        let long_tail = "填充 内容 ".repeat(60);
        let mems = vec![
            mem("short", "目标 词", &[]),
            mem("long", &format!("目标 词 {long_tail}"), &[]),
        ];
        let (ranked, _) = rank(&refs(&mems), "目标");
        assert_eq!(ranked.len(), 2, "两条都该命中");
        assert_eq!(ranked[0].0, 0, "短文档应排第一，实得排序 {ranked:?}");
        assert!(
            ranked[0].1 > ranked[1].1,
            "短文档分数({})应严格高于长文档({})",
            ranked[0].1,
            ranked[1].1
        );
    }

    #[test]
    fn tf_saturates_rather_than_growing_linearly() {
        // 出现 8 次不该是出现 1 次的 8 倍——这是 k1（词频饱和）的作用。
        //
        // ⚠ 两篇文档必须**等长**，否则测的是 b 不是 k1：长度不同的话，
        //   长度归一自己就会把高频那篇压下去，比值到不了 8，
        //   于是把 k1 调到 1e9（等于关掉饱和）测试**依然是绿的**——这是原版的假绿成因，
        //   反证时当场抓出来的。这里两篇都是 8 个词元，dl 相同、归一因子相同，
        //   分数比值就只由 k1 决定了。
        //
        // 理论值：dl = avgdl = 8 时归一因子为 1.0，
        //   k1 = 1.2  -> s8/s1 = (8*2.2/(8+1.2)) / (1*2.2/(1+1.2)) ≈ 1.91
        //   k1 -> ∞   -> s8/s1 -> 8.0（完全线性，无饱和）
        // 取 4.0 作分界，两侧都有充足余量。
        let mems = vec![
            mem("one", "target f1 f2 f3 f4 f5 f6 f7", &[]),
            mem("many", "target target target target target target target target", &[]),
        ];
        let c = Corpus::build(&refs(&mems));
        let toks = tokenize_query("target");
        let s1 = c.score(0, &toks);
        let s8 = c.score(1, &toks);
        assert!(s8 > 0.0 && s1 > 0.0, "两条都该有分，实得 s1={s1} s8={s8}");
        assert!(s8 > s1, "命中更多次仍应得分更高，实得 s1={s1} s8={s8}");
        assert!(
            s8 < s1 * 4.0,
            "8 次命中的分({s8})必须远小于 1 次({s1})的 8 倍——否则 tf 没有饱和；\
             实测比值 {:.3}",
            s8 / s1
        );
    }

    /// 造一个规模接近真实库的语料（默认 30 条互不相同的填充记忆）。
    ///
    /// 弃权阈值是以 [`Corpus::idf_max`] 为基准归一的，而 `idf_max` 在语料只有
    /// 一两条时会小到失真（见 `tiny_corpus_leans_against_abstaining`），
    /// 所以凡是断言弃权行为的测试都必须用接近真实规模的语料，否则测的不是判据本身。
    fn filler_corpus(n: usize) -> Vec<Memory> {
        (0..n)
            .map(|i| {
                mem(
                    &format!("f{i}"),
                    &format!("第{i}条填充记忆 讲的是编号{i}的独立主题 互不重复"),
                    &[],
                )
            })
            .collect()
    }

    #[test]
    fn abstains_on_thin_query() {
        // 「好的」在库里**确实出现过**（所以能命中、best > 0），
        // 但它自身信息量太低——这正是旧实现会返回噪音、新实现该弃权的场景。
        // 语料必须 >= ABSTAIN_MIN_CORPUS，否则统计判据整个不启用（见下一条测试）。
        let mut mems = filler_corpus(ABSTAIN_MIN_CORPUS + 5);
        mems.push(mem("y", "海风哥说好的 然后就没有然后了", &[]));
        let (_, reason) = rank(&refs(&mems), "好的");
        assert!(
            matches!(reason, Some(AbstainReason::ThinQuery { .. })),
            "零信息短语必须弃权，实得 {reason:?}"
        );
    }

    #[test]
    fn small_corpus_disables_statistical_abstain() {
        // 守护 ABSTAIN_MIN_CORPUS 这道样本量门。
        //
        // 小语料上 IDF 毫无统计意义，且**两个方向都会错**。这里复现的是危险的那一侧：
        // 一个 5 条记忆的库里查 `redb lock`，`redb` 出现在 4/5 条上、IDF 被压到 0.288，
        // query_info 只有 1.2 < 2.05——若不设这道门，一次完全正常的检索会被判成
        // 「本库无相关记忆」（这正是 recall_search_ranking_and_filters 当场抓到的回归）。
        let mems = vec![
            mem("hit2", "redb lock 冲突", &[]),
            mem("hit1", "redb 入门", &[]),
            mem("cold", "redb 冷藏笔记", &[]),
            mem("other", "redb 项目笔记", &[]),
            mem("misc", "完全无关的内容", &[]),
        ];
        let c = Corpus::build(&refs(&mems));
        let toks = tokenize_query("redb lock");
        assert!(
            c.query_info(&toks) < ABSTAIN_MIN_QUERY_INFO,
            "前提：小语料下这个正常 query 的 query_info（实得 {}）确实低于阈值——\
             正因如此才需要样本量门",
            c.query_info(&toks)
        );
        let (ranked, reason) = rank(&refs(&mems), "redb lock");
        assert!(reason.is_none(), "语料不足 {ABSTAIN_MIN_CORPUS} 条时不得做统计弃权，实得 {reason:?}");
        assert_eq!(ranked[0].0, 0, "命中两个词元的 hit2 仍应排第一");
    }

    #[test]
    fn small_corpus_still_abstains_on_zero_hits() {
        // 样本量门只关掉**统计型**判据；「一个词元都没命中」是确定性事实，
        // 与语料大小无关，任何规模下都必须照常弃权。
        let mems = vec![mem("a", "编译器崩溃", &[]), mem("b", "另一条记忆", &[])];
        let (_, reason) = rank(&refs(&mems), "完全无关的查询词");
        assert_eq!(reason, Some(AbstainReason::NoCandidate));
    }

    #[test]
    fn abstains_when_nothing_matches() {
        let mems = vec![mem("a", "编译器崩溃", &[])];
        let c = Corpus::build(&refs(&mems));
        let toks = tokenize_query("完全无关的查询词");
        let reason = judge(&c, &toks, 0.0, &[]);
        assert_eq!(reason, Some(AbstainReason::NoCandidate));
    }

    #[test]
    fn does_not_abstain_on_a_real_hit() {
        // 一条内容明确的记忆 + 一个用了它原词的 query，绝不能弃权
        // （误弃权是静默漏检，比返回噪音危险得多）。
        let mems = vec![
            mem("a", "反斜杠被 shell 吃掉后被当成驱动器相对路径", &[]),
            mem("b", "完全不相干的另一条记忆内容", &[]),
            mem("c", "又一条不相干的记忆", &[]),
        ];
        let (ranked, reason) = rank(&refs(&mems), "反斜杠 驱动器相对路径");
        assert!(reason.is_none(), "真命中不该弃权，实得 {reason:?}");
        assert_eq!(ranked[0].0, 0, "应命中第一条");
    }

    #[test]
    fn empty_corpus_and_empty_query_do_not_panic() {
        let empty: Vec<&Memory> = Vec::new();
        let (ranked, reason) = rank(&empty, "任意查询");
        assert!(ranked.is_empty());
        assert_eq!(reason, Some(AbstainReason::EmptyCorpus));

        let mems = vec![mem("a", "内容", &[])];
        let (ranked2, reason2) = rank(&refs(&mems), "");
        assert!(ranked2.is_empty());
        assert_eq!(reason2, Some(AbstainReason::EmptyQuery));

        // 纯标点 query 也切不出词元
        let (_, reason3) = rank(&refs(&mems), "，。？");
        assert_eq!(reason3, Some(AbstainReason::EmptyQuery));
    }

    #[test]
    fn scores_are_finite_on_edge_cases() {
        let c0 = Corpus::build(&[]);
        assert_eq!(c0.score(0, &["x".to_string()]), 0.0, "空语料越界不该 panic");
        assert!(c0.idf_max() > 0.0, "空语料的 idf_max 必须为正，避免除零");
        assert!(c0.hits(0, &["x".to_string()]).is_empty());

        let mems = vec![mem("a", "单条语料", &[])];
        let c1 = Corpus::build(&refs(&mems));
        let toks = tokenize_query("单条");
        assert!(c1.score(0, &toks).is_finite(), "单文档语料分数必须有限");
        assert!(c1.score(99, &toks) == 0.0, "越界 idx 返回 0 而不是 panic");
        assert!(c1.query_info(&[]).is_finite());
    }

    #[test]
    fn inject_tier_silent_iff_abstained() {
        // 静默档必须与弃权判据**完全一致**——这里 abstained 是入参，
        // 所以这条等价关系是结构性的；测试守的是「别有人往里加别的静默条件」。
        assert_eq!(inject_tier(true, 0.0), InjectTier::Silent);
        assert_eq!(inject_tier(true, 999.0), InjectTier::Silent, "弃权压过一切强度");
        assert_ne!(inject_tier(false, 0.0), InjectTier::Silent, "没弃权就不该静默");
    }

    #[test]
    fn inject_tier_full_requires_strong_hit() {
        // 恰好达线要算 Full（闭区间），差一点点就降级到 Hint。
        assert_eq!(inject_tier(false, INJECT_FULL_MIN_STRENGTH), InjectTier::Full);
        assert_eq!(
            inject_tier(false, INJECT_FULL_MIN_STRENGTH - 0.01),
            InjectTier::Hint
        );
        assert_eq!(inject_tier(false, INJECT_FULL_MIN_STRENGTH * 2.0), InjectTier::Full);
    }

    #[test]
    fn hint_tier_actually_exists() {
        // A 档线必须严于弃权线，否则 Hint 档是空的——而 Hint 正是痛点的解药，
        // 谁把 INJECT_FULL_MIN_STRENGTH 调到 <= 弃权线，这条立刻变红。
        // 用 const block 做**编译期**断言：这是两个常量之间的不变式，
        // 谁把 INJECT_FULL_MIN_STRENGTH 调到 <= 弃权线，代码根本编译不过，
        // 比等到跑测试才红更早一步（clippy::assertions_on_constants 也要求这么写）。
        const {
            assert!(
                INJECT_FULL_MIN_STRENGTH > ABSTAIN_MIN_HIT_STRENGTH,
                "A 档线必须严于弃权线，否则 Hint 档是空的"
            );
        }
        // 取两线之间的一点，必须落 Hint。
        let mid = (INJECT_FULL_MIN_STRENGTH + ABSTAIN_MIN_HIT_STRENGTH) / 2.0;
        assert_eq!(inject_tier(false, mid), InjectTier::Hint);
    }

    #[test]
    fn resident_levels_are_exactly_the_hot_index_ones() {
        // 注入跳过的层必须与热索引常驻的层一一对应：多跳会漏掉按需层，
        // 少跳会把已在上下文里的记忆再注入一遍。
        // 用切片的 contains 而不是 HashSet：Level 没实现 Hash（model.rs），
        // 而切片 contains 只要 PartialEq——为一条测试给领域类型加 derive 是本末倒置。
        let has = |lv: Level| INJECT_SKIP_RESIDENT_LEVELS.contains(&lv);
        assert_eq!(INJECT_SKIP_RESIDENT_LEVELS.len(), 4);
        for lv in [Level::L1, Level::L2, Level::L4_1, Level::L4_2] {
            assert!(has(lv), "{lv:?} 是常驻层，注入时必须跳过");
        }
        for lv in [Level::L3, Level::L4_3] {
            assert!(!has(lv), "{lv:?} 是按需层，正是注入要捞的");
        }
    }

    #[test]
    fn hits_are_sorted_by_idf_desc() {
        let mems = vec![
            mem("a", "公共 稀有甲", &[]),
            mem("b", "公共 其它", &[]),
            mem("c", "公共 另外", &[]),
        ];
        let c = Corpus::build(&refs(&mems));
        let toks = tokenize_query("公共 稀有");
        let hits = c.hits(0, &toks);
        assert_eq!(hits.len(), 2, "两个词元都该命中，实得 {hits:?}");
        assert!(
            hits[0].idf >= hits[1].idf,
            "命中详情必须按 IDF 降序，实得 {hits:?}"
        );
        assert_eq!(hits[0].token, "稀有", "稀有词该排在前面");
    }
}
