//! 文档分块：markdown 标题感知切 section + text-splitter 语义分块 + 标题面包屑。
//!
//! 管线（设计文档 §5.1）：
//! 1. pulldown-cmark 解析出 H1/H2 标题（offset + 文本），按标题把原文切成 section，
//!    每个 section 记录「标题面包屑」（`文档名 > H1 > H2`，H3 及以下不进面包屑）；
//! 2. section 内用 [`text_splitter::MarkdownSplitter`] 做语义分块——优先在更高
//!    语义层级（标题/换行/句子）断开、尽量填满容量；
//! 3. 容量按 **bge tokenizer 真实 token 数**计（[`Chunker::with_tokenizer`]）；
//!    tokenizer 不可得时退化为字符计数（中文 1 字 ≈ 1 token 的近似，
//!    [`Chunker::with_chars`]）。
//!
//! 容量预算（设计文档 §9.8 不对称性约束的一部分，变更须递增
//! [`crate::manifest::PIPELINE_VERSION`]）：chunk 上限 400 token + 面包屑预算
//! ~100 token，嵌入文本总长压在 bge 的 512 token 之内，防静默截断。

use text_splitter::{Characters, ChunkConfig, MarkdownSplitter};

/// token 计数模式下的 chunk 容量（尽量 > 256、不超 400）。
const TOKEN_CAPACITY: std::ops::Range<usize> = 256..400;
/// token 计数模式下相邻 chunk 的重叠。
const TOKEN_OVERLAP: usize = 40;
/// 字符退化模式下的 chunk 容量。中文最坏情形 1 字 ≈ 1 token，因此字符容量
/// 必须**不超过** token 容量并留余量（不是放大——数字/全角标点在 WordPiece
/// 下还可能 1 字多 token），保守取 220..340。
const CHAR_CAPACITY: std::ops::Range<usize> = 220..340;
/// 字符退化模式下相邻 chunk 的重叠。
const CHAR_OVERLAP: usize = 34;
/// 嵌入文本里面包屑的字符预算：与正文容量（≤400 token）相加须压在 bge 的
/// 512 token 之内；中文 100 字符 ≈ ≤100 token，400 + 100 + 空行 < 512。
const BREADCRUMB_BUDGET_CHARS: usize = 100;

/// 一个待入库的分块。
#[derive(Debug, Clone)]
pub struct Chunk {
    /// 标题面包屑：`文档名 > H1 > H2`（无标题时只有文档名）。
    pub breadcrumb: String,
    /// chunk 正文（不含面包屑）。
    pub text: String,
    /// 在文档内的序号（0 起）。
    pub ord: i32,
}

impl Chunk {
    /// 实际送入嵌入模型的文本：面包屑 + 空行 + 正文（上下文头，提升召回命中率；
    /// 文档侧**不加** query 指令前缀——BGE 嵌入是不对称的）。
    ///
    /// 面包屑超出 [`BREADCRUMB_BUDGET_CHARS`] 时**仅在嵌入文本里**截断（库中
    /// 存的 breadcrumb 保持完整，它是展示用指针）——否则超长标题会把嵌入文本
    /// 顶过 bge 的 512 token，正文尾部被模型静默截掉。
    pub fn embedding_text(&self) -> String {
        if self.breadcrumb.chars().count() > BREADCRUMB_BUDGET_CHARS {
            let truncated: String =
                self.breadcrumb.chars().take(BREADCRUMB_BUDGET_CHARS - 1).collect();
            format!("{truncated}…\n\n{}", self.text)
        } else {
            format!("{}\n\n{}", self.breadcrumb, self.text)
        }
    }
}

/// 文档分块器：持有按 token 或按字符计数的 markdown 语义分块器。
pub struct Chunker {
    inner: SplitterKind,
}

/// 两种计数模式的分块器（enum 静态分发，避免 trait object 的泛型擦除麻烦）。
/// Token 变体装箱：tokenizers::Tokenizer 自身上 KB 级（vocab 表），不装箱会把
/// 整个 enum 撑大（clippy large_enum_variant）。
enum SplitterKind {
    /// 按 HuggingFace tokenizer（bge 的 tokenizer.json）计 token。
    Token(Box<MarkdownSplitter<tokenizers::Tokenizer>>),
    /// 按字符计数（退化模式）。
    Chars(MarkdownSplitter<Characters>),
}

impl Chunker {
    /// 用 bge 的 tokenizer.json 构造按 token 计数的分块器（首选）。
    ///
    /// # Errors
    /// 读取 / 解析 tokenizer 文件失败，或 overlap 配置非法时返回中文错误说明。
    pub fn with_tokenizer(tokenizer_json: &std::path::Path) -> Result<Chunker, String> {
        let tokenizer = tokenizers::Tokenizer::from_file(tokenizer_json)
            .map_err(|e| format!("加载 tokenizer {} 失败：{e}", tokenizer_json.display()))?;
        let cfg = ChunkConfig::new(TOKEN_CAPACITY)
            .with_sizer(tokenizer)
            .with_overlap(TOKEN_OVERLAP)
            .map_err(|e| format!("分块 overlap 配置非法：{e}"))?;
        Ok(Chunker { inner: SplitterKind::Token(Box::new(MarkdownSplitter::new(cfg))) })
    }

    /// 构造按字符计数的退化分块器（tokenizer 不可得时的兜底）。
    ///
    /// # Errors
    /// overlap 配置非法时返回中文错误说明（常量配置下不应发生）。
    pub fn with_chars() -> Result<Chunker, String> {
        let cfg = ChunkConfig::new(CHAR_CAPACITY)
            .with_overlap(CHAR_OVERLAP)
            .map_err(|e| format!("分块 overlap 配置非法：{e}"))?;
        Ok(Chunker { inner: SplitterKind::Chars(MarkdownSplitter::new(cfg)) })
    }

    /// 对 section 正文做语义分块。
    fn split<'t>(&self, body: &'t str) -> Vec<&'t str> {
        match &self.inner {
            SplitterKind::Token(s) => s.chunks(body).collect(),
            SplitterKind::Chars(s) => s.chunks(body).collect(),
        }
    }

    /// 整文档分块：切 section → 各 section 语义分块 → 带面包屑的 [`Chunk`] 列表。
    ///
    /// `doc_name` 用作面包屑首段（通常传文件名或相对路径）；纯文本（.txt）同样
    /// 适用——没有标题就只有一个「前言」section，面包屑只剩文档名。
    pub fn chunk_document(&self, doc_name: &str, text: &str) -> Vec<Chunk> {
        let mut out = Vec::new();
        let mut ord = 0;
        for section in collect_sections(doc_name, text) {
            for piece in self.split(&section.body) {
                let trimmed = piece.trim();
                if trimmed.is_empty() {
                    continue;
                }
                out.push(Chunk {
                    breadcrumb: section.breadcrumb.clone(),
                    text: trimmed.to_string(),
                    ord,
                });
                ord += 1;
            }
        }
        out
    }
}

/// 一个按 H1/H2 切出的 section。
struct Section {
    /// 标题面包屑（`文档名 > H1 > H2`）。
    breadcrumb: String,
    /// section 原文（含标题行本身，让 chunk 自含标题语境）。
    body: String,
}

/// 解析出的标题：在原文中的字节起点、级别（1/2）与标题文本。
struct Heading {
    start: usize,
    level: u8,
    title: String,
}

/// 用 pulldown-cmark 收集 H1/H2 标题（offset 升序）。
fn collect_headings(text: &str) -> Vec<Heading> {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};

    let mut headings = Vec::new();
    // (起点, 级别, 累积中的标题文本)；H3+ 不收集。
    let mut current: Option<(usize, u8, String)> = None;
    for (event, range) in Parser::new(text).into_offset_iter() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let lv = level as u8; // HeadingLevel as u8：H1=1 … H6=6。
                if lv <= 2 {
                    current = Some((range.start, lv, String::new()));
                }
            }
            Event::Text(t) | Event::Code(t) => {
                if let Some((_, _, title)) = current.as_mut() {
                    title.push_str(&t);
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((start, level, title)) = current.take() {
                    headings.push(Heading { start, level, title: title.trim().to_string() });
                }
            }
            _ => {}
        }
    }
    headings
}

/// 按 H1/H2 标题把文档切成 section，并为每段构造标题面包屑。
fn collect_sections(doc_name: &str, text: &str) -> Vec<Section> {
    let headings = collect_headings(text);
    let mut sections = Vec::new();

    // 前言：第一个 H1/H2 之前的内容（没有任何标题时即整个文档）。
    let first_cut = headings.first().map_or(text.len(), |h| h.start);
    if !text[..first_cut].trim().is_empty() {
        sections.push(Section {
            breadcrumb: doc_name.to_string(),
            body: text[..first_cut].to_string(),
        });
    }

    // 各标题段：跟踪 H1/H2 上下文构造面包屑。
    let mut h1: Option<String> = None;
    for (i, h) in headings.iter().enumerate() {
        let end = headings.get(i + 1).map_or(text.len(), |next| next.start);
        let body = &text[h.start..end];

        let mut h2: Option<&str> = None;
        if h.level == 1 {
            h1 = Some(h.title.clone());
        } else {
            h2 = Some(h.title.as_str());
        }

        let mut breadcrumb = doc_name.to_string();
        if let Some(t1) = h1.as_deref() {
            breadcrumb.push_str(" > ");
            breadcrumb.push_str(t1);
        }
        if let Some(t2) = h2 {
            breadcrumb.push_str(" > ");
            breadcrumb.push_str(t2);
        }

        if !body.trim().is_empty() {
            sections.push(Section { breadcrumb, body: body.to_string() });
        }
    }
    sections
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 标题收集：抓 H1/H2、忽略 H3，offset 升序。
    #[test]
    fn headings_h1_h2_only() {
        let md = "前言。\n\n# 甲\n\n正文A\n\n## 乙\n\n正文B\n\n### 丙\n\n正文C\n";
        let hs = collect_headings(md);
        assert_eq!(hs.len(), 2);
        assert_eq!(hs[0].title, "甲");
        assert_eq!(hs[0].level, 1);
        assert_eq!(hs[1].title, "乙");
        assert_eq!(hs[1].level, 2);
        assert!(hs[0].start < hs[1].start);
    }

    /// section 切分与面包屑：前言只有文档名；H2 在 H1 之下拼出三段路径。
    #[test]
    fn sections_with_breadcrumbs() {
        let md = "开头的前言。\n\n# 架构\n\n总体说明。\n\n## 存储\n\n存储细节。\n";
        let ss = collect_sections("设计.md", md);
        assert_eq!(ss.len(), 3);
        assert_eq!(ss[0].breadcrumb, "设计.md");
        assert!(ss[0].body.contains("前言"));
        assert_eq!(ss[1].breadcrumb, "设计.md > 架构");
        assert!(ss[1].body.contains("总体说明"));
        assert_eq!(ss[2].breadcrumb, "设计.md > 架构 > 存储");
        assert!(ss[2].body.contains("存储细节"));
    }

    /// 新 H1 重置 H2 上下文。
    #[test]
    fn new_h1_resets_h2() {
        let md = "# 甲\n\nA\n\n## 乙\n\nB\n\n# 丙\n\nC\n";
        let ss = collect_sections("d.md", md);
        let crumbs: Vec<&str> = ss.iter().map(|s| s.breadcrumb.as_str()).collect();
        assert_eq!(crumbs, vec!["d.md > 甲", "d.md > 甲 > 乙", "d.md > 丙"]);
    }

    /// 字符模式整链路：中文长文被切成多块、每块带面包屑、ord 递增、正文非空。
    #[test]
    fn chunk_document_chars_mode() {
        let chunker = Chunker::with_chars().expect("构造退化分块器失败");
        let para = "知识库的检索质量取决于分块策略与嵌入模型的配合。".repeat(40);
        let md = format!("# 检索\n\n{para}\n\n## 细节\n\n{para}\n");
        let chunks = chunker.chunk_document("kb.md", &md);
        assert!(chunks.len() >= 2, "长文应切出多块，实得 {}", chunks.len());
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.ord, i as i32);
            assert!(c.breadcrumb.starts_with("kb.md"));
            assert!(!c.text.trim().is_empty());
        }
        // 嵌入文本 = 面包屑 + 空行 + 正文。
        let et = chunks[0].embedding_text();
        assert!(et.starts_with(&chunks[0].breadcrumb));
        assert!(et.contains("\n\n"));
    }

    /// 纯文本（无标题）：单一前言 section、面包屑只有文档名。
    #[test]
    fn plain_text_single_section() {
        let chunker = Chunker::with_chars().expect("构造退化分块器失败");
        let chunks = chunker.chunk_document("note.txt", "没有标题的纯文本内容。");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].breadcrumb, "note.txt");
    }

    /// 超长面包屑只在嵌入文本里截断，库存的 breadcrumb 保持完整。
    #[test]
    fn embedding_text_truncates_long_breadcrumb() {
        let long_crumb = "超长标题".repeat(60); // 240 字符，远超预算。
        let c = Chunk { breadcrumb: long_crumb.clone(), text: "正文".to_string(), ord: 0 };
        let et = c.embedding_text();
        let head = et.split("\n\n").next().expect("应有面包屑段");
        assert_eq!(head.chars().count(), BREADCRUMB_BUDGET_CHARS); // 99 字符 + '…'。
        assert!(head.ends_with('…'));
        assert!(et.ends_with("正文"));
        assert_eq!(c.breadcrumb, long_crumb); // 原值不动。
    }
}
