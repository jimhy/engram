//! 检索质量评测（`eval` 子命令的核心判定逻辑，纯函数、不碰模型与库）。
//!
//! 给检索侧一把与引擎题库对等的「尺子」：golden 集（`eval/golden.jsonl`）逐条跑
//! 混合检索，量化 recall@k / MRR，让分块、分词、融合参数的每次调整都锚定在
//! 数字上而非感觉上（对应知识库设计文档 §10 v2 路线第 5 条「召回质量评测集」）。
//!
//! 判定口径（与 golden 集出题约定一致）：
//! - **命中** = top-k 内出现 `expect_doc`，**且**该命中 chunk 的正文或标题面包屑
//!   含 `expect_hint` 子串——只对上文档不算，必须真的召回到期望的章节内容；
//! - **rank** = 第一条满足上述条件的命中的名次（1 起）；
//! - `recall@k` = rank ≤ k 的题目占比；`MRR` = 全体题目 `1/rank` 的均值（未命中记 0）。
//!
//! 本模块只做解析 / 判定 / 汇总 / 报告渲染，全部纯函数，单测不依赖嵌入模型
//! 与 LanceDB；检索执行与文件 IO 由 main.rs 的 `run_eval` 编排。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::store::Hit;

/// golden 集里的一条评测题。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoldenCase {
    /// 查询文本（送入混合检索的原文）。
    pub query: String,
    /// 期望命中的文档路径（须与库内 doc_path 完全一致，如 `docs/设计文档.md`）。
    pub expect_doc: String,
    /// 期望命中章节的特征子串（匹配 chunk 正文**或**面包屑，人工可验证）。
    pub expect_hint: String,
    /// 题目类别：`zh`（中文自然语言）/ `en`（英文查询）/ `ident`（代码标识符）。
    pub kind: String,
}

/// golden 集合法的题目类别（防 kind 拼写错导致细分表静默漏项）。
const VALID_KINDS: [&str; 3] = ["zh", "en", "ident"];

/// 解析 golden JSONL 文本：`#` 开头行与空行视为注释跳过，其余每行一个 JSON 对象。
///
/// # Errors
/// 任一行 JSON 解析失败、字段为空或 kind 非法时返回中文错误（含行号，便于定位）。
pub fn parse_golden(text: &str) -> Result<Vec<GoldenCase>, String> {
    let mut cases = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let case: GoldenCase = serde_json::from_str(line)
            .map_err(|e| format!("golden 第 {} 行 JSON 解析失败：{e}", i + 1))?;
        if case.query.trim().is_empty()
            || case.expect_doc.trim().is_empty()
            || case.expect_hint.trim().is_empty()
        {
            return Err(format!(
                "golden 第 {} 行字段不完整（query / expect_doc / expect_hint 均不得为空）",
                i + 1
            ));
        }
        if !VALID_KINDS.contains(&case.kind.as_str()) {
            return Err(format!(
                "golden 第 {} 行 kind「{}」非法（应为 {} 之一）",
                i + 1,
                case.kind,
                VALID_KINDS.join("/")
            ));
        }
        cases.push(case);
    }
    Ok(cases)
}

/// 判定命中名次：返回第一条「doc_path 等于 `expect_doc` 且正文或面包屑含
/// `expect_hint`」的命中的 1 起名次；无命中返回 `None`。
pub fn judge_rank(hits: &[Hit], expect_doc: &str, expect_hint: &str) -> Option<usize> {
    hits.iter()
        .position(|h| {
            h.doc_path == expect_doc
                && (h.text.contains(expect_hint) || h.breadcrumb.contains(expect_hint))
        })
        .map(|i| i + 1)
}

/// 失败清单里展示的实得命中摘要条数。
const TOP_SUMMARY: usize = 3;
/// 命中摘要里正文预览的最大字符数。
const PREVIEW_CHARS: usize = 60;

/// 一条实得命中的摘要（失败清单 / JSON 输出用）。
#[derive(Debug, Clone, Serialize)]
pub struct TopHit {
    /// 命中的文档路径。
    pub doc_path: String,
    /// 命中的标题面包屑。
    pub breadcrumb: String,
    /// 正文预览（截前 [`PREVIEW_CHARS`] 字符）。
    pub preview: String,
}

/// 一条评测题的判定结果。
#[derive(Debug, Clone, Serialize)]
pub struct CaseResult {
    /// 原题（query / 期望 / 类别）。
    #[serde(flatten)]
    pub case: GoldenCase,
    /// 命中名次（1 起；未命中为 `None`，JSON 里为 null）。
    pub rank: Option<usize>,
    /// 实得 top-N 摘要（N = [`TOP_SUMMARY`]）。
    pub top: Vec<TopHit>,
}

/// 用检索命中判定一条题目：算 rank + 截 top-3 摘要。
pub fn evaluate_case(case: GoldenCase, hits: &[Hit]) -> CaseResult {
    let rank = judge_rank(hits, &case.expect_doc, &case.expect_hint);
    let top = hits
        .iter()
        .take(TOP_SUMMARY)
        .map(|h| TopHit {
            doc_path: h.doc_path.clone(),
            breadcrumb: h.breadcrumb.clone(),
            preview: preview(&h.text),
        })
        .collect();
    CaseResult { case, rank, top }
}

/// 正文预览：截前 [`PREVIEW_CHARS`] 字符、压平换行（清单里一行放得下）。
fn preview(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .take(PREVIEW_CHARS)
        .collect();
    if text.chars().count() > PREVIEW_CHARS {
        format!("{flat}…")
    } else {
        flat
    }
}

/// 一组题目的汇总指标。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EvalStats {
    /// 题目数。
    pub total: usize,
    /// rank ≤ 1 的占比。
    pub recall_at_1: f64,
    /// rank ≤ 4 的占比。
    pub recall_at_4: f64,
    /// rank ≤ 8 的占比。
    pub recall_at_8: f64,
    /// Mean Reciprocal Rank：`mean(1/rank)`，未命中记 0；rank 可超过 8
    /// （`--limit` 大于 8 时），彼时仍计入 MRR、只是不进 recall@8。
    pub mrr: f64,
}

/// 由一组 rank 计算汇总指标；空集合返回全 0（不除零）。
pub fn compute_stats(ranks: &[Option<usize>]) -> EvalStats {
    let total = ranks.len();
    if total == 0 {
        return EvalStats {
            total: 0,
            recall_at_1: 0.0,
            recall_at_4: 0.0,
            recall_at_8: 0.0,
            mrr: 0.0,
        };
    }
    let n = total as f64;
    let recall_at = |k: usize| -> f64 {
        ranks
            .iter()
            .filter(|r| matches!(r, Some(x) if *x <= k))
            .count() as f64
            / n
    };
    let mrr = ranks
        .iter()
        .map(|r| r.map_or(0.0, |x| 1.0 / x as f64))
        .sum::<f64>()
        / n;
    EvalStats {
        total,
        recall_at_1: recall_at(1),
        recall_at_4: recall_at(4),
        recall_at_8: recall_at(8),
        mrr,
    }
}

/// 全体结果的总指标。
pub fn overall_stats(results: &[CaseResult]) -> EvalStats {
    let ranks: Vec<Option<usize>> = results.iter().map(|r| r.rank).collect();
    compute_stats(&ranks)
}

/// 按 kind 分组的细分指标（BTreeMap 保证输出顺序稳定）。
pub fn stats_by_kind(results: &[CaseResult]) -> BTreeMap<String, EvalStats> {
    let mut groups: BTreeMap<String, Vec<Option<usize>>> = BTreeMap::new();
    for r in results {
        groups.entry(r.case.kind.clone()).or_default().push(r.rank);
    }
    groups
        .into_iter()
        .map(|(k, ranks)| (k, compute_stats(&ranks)))
        .collect()
}

/// 渲染可读文本报告：总体指标 + 分 kind 细分表 + 逐条失败清单。
pub fn render_text_report(limit: usize, results: &[CaseResult]) -> String {
    use std::fmt::Write as _;

    let overall = overall_stats(results);
    let mut s = String::new();
    let _ = writeln!(
        s,
        "检索质量评测：{} 条 golden，limit={limit}",
        overall.total
    );
    let _ = writeln!(
        s,
        "总体：recall@1 {:.1}%  recall@4 {:.1}%  recall@8 {:.1}%  MRR {:.3}",
        overall.recall_at_1 * 100.0,
        overall.recall_at_4 * 100.0,
        overall.recall_at_8 * 100.0,
        overall.mrr
    );
    let _ = writeln!(s, "分 kind：");
    let _ = writeln!(
        s,
        "  {:<6} {:>3} {:>8} {:>8} {:>8} {:>7}",
        "kind", "n", "r@1", "r@4", "r@8", "MRR"
    );
    for (kind, st) in stats_by_kind(results) {
        let _ = writeln!(
            s,
            "  {:<6} {:>3} {:>7.1}% {:>7.1}% {:>7.1}% {:>7.3}",
            kind,
            st.total,
            st.recall_at_1 * 100.0,
            st.recall_at_4 * 100.0,
            st.recall_at_8 * 100.0,
            st.mrr
        );
    }

    let failures: Vec<&CaseResult> = results.iter().filter(|r| r.rank.is_none()).collect();
    if failures.is_empty() {
        let _ = writeln!(s, "失败清单：无（全部题目在 top-{limit} 内命中）。");
    } else {
        let _ = writeln!(
            s,
            "失败清单（top-{limit} 未命中，共 {} 条）：",
            failures.len()
        );
        for (i, r) in failures.iter().enumerate() {
            let _ = writeln!(s, "  {}. [{}] {}", i + 1, r.case.kind, r.case.query);
            let _ = writeln!(
                s,
                "     期望 {} ｜ hint「{}」",
                r.case.expect_doc, r.case.expect_hint
            );
            if r.top.is_empty() {
                let _ = writeln!(s, "     实得：<空结果>");
            } else {
                let _ = writeln!(s, "     实得 top{}：", r.top.len());
                for (j, t) in r.top.iter().enumerate() {
                    let _ = writeln!(
                        s,
                        "       {}) {} | {} | {}",
                        j + 1,
                        t.doc_path,
                        t.breadcrumb,
                        t.preview
                    );
                }
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一条最小命中（正文 / 面包屑可指定）。
    fn hit(doc: &str, crumb: &str, text: &str) -> Hit {
        Hit {
            id: String::new(),
            doc_path: doc.to_string(),
            breadcrumb: crumb.to_string(),
            text: text.to_string(),
            score: None,
        }
    }

    fn case(kind: &str) -> GoldenCase {
        GoldenCase {
            query: "问".to_string(),
            expect_doc: "docs/a.md".to_string(),
            expect_hint: "线索".to_string(),
            kind: kind.to_string(),
        }
    }

    /// 解析：跳过注释与空行；字段齐全的行正常入列。
    #[test]
    fn parse_skips_comments_and_blank_lines() {
        let text = "# 头部注释\n\n\
            {\"query\":\"q1\",\"expect_doc\":\"docs/a.md\",\"expect_hint\":\"h\",\"kind\":\"zh\"}\n\
            # 中途注释\n\
            {\"query\":\"q2\",\"expect_doc\":\"docs/b.md\",\"expect_hint\":\"h2\",\"kind\":\"ident\"}\n";
        let cases = parse_golden(text).expect("应解析成功");
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].query, "q1");
        assert_eq!(cases[1].kind, "ident");
    }

    /// 解析：坏 JSON / 空字段 / 非法 kind 均报错且含行号。
    #[test]
    fn parse_rejects_bad_lines() {
        let bad_json = "# c\n{not json}\n";
        let err = parse_golden(bad_json).expect_err("坏 JSON 应报错");
        assert!(err.contains("第 2 行"), "实得：{err}");

        let empty_field =
            "{\"query\":\" \",\"expect_doc\":\"d\",\"expect_hint\":\"h\",\"kind\":\"zh\"}\n";
        let err = parse_golden(empty_field).expect_err("空 query 应报错");
        assert!(err.contains("不完整"), "实得：{err}");

        let bad_kind =
            "{\"query\":\"q\",\"expect_doc\":\"d\",\"expect_hint\":\"h\",\"kind\":\"para\"}\n";
        let err = parse_golden(bad_kind).expect_err("非法 kind 应报错");
        assert!(err.contains("非法"), "实得：{err}");
    }

    /// 命中判定：doc 与 hint 必须同时满足；hint 匹配正文或面包屑均可。
    #[test]
    fn judge_requires_doc_and_hint() {
        let hits = vec![
            hit("docs/b.md", "", "含线索但文档不对"),
            hit("docs/a.md", "", "文档对但没有那个词"),
            hit("docs/a.md", "a.md > 有线索的章节", "正文没有"),
        ];
        // 第 3 条靠面包屑命中 → rank 3。
        assert_eq!(judge_rank(&hits, "docs/a.md", "线索"), Some(3));
        // 正文命中：第 1 条文档不对不算，换期望文档后 rank 1。
        assert_eq!(judge_rank(&hits, "docs/b.md", "线索"), Some(1));
        // doc 对但 hint 全无 → 未命中。
        assert_eq!(judge_rank(&hits, "docs/a.md", "不存在的词"), None);
        // 空结果 → 未命中。
        assert_eq!(judge_rank(&[], "docs/a.md", "线索"), None);
    }

    /// 命中判定：取第一条同时满足的（前面有同文档但 hint 不符的行不抢名次）。
    #[test]
    fn judge_takes_first_qualified() {
        let hits = vec![
            hit("docs/a.md", "", "无关段落"),
            hit("docs/a.md", "", "这里有线索"),
            hit("docs/a.md", "", "后面也有线索"),
        ];
        assert_eq!(judge_rank(&hits, "docs/a.md", "线索"), Some(2));
    }

    /// 指标计算：recall@k 按 rank≤k 计，MRR 未命中记 0、rank>8 仍计入 MRR。
    #[test]
    fn stats_math() {
        let ranks = vec![Some(1), Some(3), None, Some(9)];
        let st = compute_stats(&ranks);
        assert_eq!(st.total, 4);
        assert!((st.recall_at_1 - 0.25).abs() < 1e-12);
        assert!((st.recall_at_4 - 0.5).abs() < 1e-12);
        // rank 9 > 8：不进 recall@8。
        assert!((st.recall_at_8 - 0.5).abs() < 1e-12);
        let expect_mrr = (1.0 + 1.0 / 3.0 + 0.0 + 1.0 / 9.0) / 4.0;
        assert!((st.mrr - expect_mrr).abs() < 1e-12, "实得 {}", st.mrr);
    }

    /// 空集合：全 0、不除零 panic。
    #[test]
    fn stats_empty_is_zero() {
        let st = compute_stats(&[]);
        assert_eq!(st.total, 0);
        assert_eq!(st.mrr, 0.0);
        assert_eq!(st.recall_at_8, 0.0);
    }

    /// 分 kind 细分：各组独立计算、组名字典序稳定。
    #[test]
    fn stats_grouped_by_kind() {
        let results = vec![
            CaseResult {
                case: case("zh"),
                rank: Some(1),
                top: vec![],
            },
            CaseResult {
                case: case("zh"),
                rank: None,
                top: vec![],
            },
            CaseResult {
                case: case("ident"),
                rank: Some(2),
                top: vec![],
            },
        ];
        let by_kind = stats_by_kind(&results);
        let kinds: Vec<&String> = by_kind.keys().collect();
        assert_eq!(kinds, ["ident", "zh"]); // BTreeMap 字典序。
        assert_eq!(by_kind["zh"].total, 2);
        assert!((by_kind["zh"].recall_at_1 - 0.5).abs() < 1e-12);
        assert_eq!(by_kind["ident"].total, 1);
        assert!((by_kind["ident"].mrr - 0.5).abs() < 1e-12);
    }

    /// 单题判定打包：rank + top3 摘要（换行压平、超长截断加省略号）。
    #[test]
    fn evaluate_case_summarizes_top3() {
        let long_text = format!("第一行\n第二行{}", "很长".repeat(60));
        let hits = vec![
            hit("docs/x.md", "x.md > 甲", &long_text),
            hit("docs/a.md", "a.md > 乙", "这里有线索"),
            hit("docs/y.md", "y.md > 丙", "短文"),
            hit("docs/z.md", "z.md > 丁", "第四条不进摘要"),
        ];
        let r = evaluate_case(case("zh"), &hits);
        assert_eq!(r.rank, Some(2));
        assert_eq!(r.top.len(), 3);
        assert!(!r.top[0].preview.contains('\n'), "预览应压平换行");
        assert!(r.top[0].preview.ends_with('…'), "超长应截断");
        assert_eq!(r.top[2].doc_path, "docs/y.md");
    }

    /// 文本报告：含总体指标、分 kind 行与失败清单细节。
    #[test]
    fn text_report_contains_key_sections() {
        let results = vec![
            CaseResult {
                case: case("zh"),
                rank: Some(1),
                top: vec![],
            },
            CaseResult {
                case: case("ident"),
                rank: None,
                top: vec![TopHit {
                    doc_path: "docs/b.md".to_string(),
                    breadcrumb: "b.md > 某节".to_string(),
                    preview: "预览".to_string(),
                }],
            },
        ];
        let report = render_text_report(8, &results);
        assert!(report.contains("2 条 golden"), "实得：{report}");
        assert!(report.contains("ident"), "应有分 kind 行");
        assert!(report.contains("失败清单"), "应有失败清单");
        assert!(
            report.contains("docs/b.md | b.md > 某节 | 预览"),
            "失败项应带实得摘要\n{report}"
        );
        // 全命中时报告明确说「无」。
        let all_pass = vec![CaseResult {
            case: case("zh"),
            rank: Some(1),
            top: vec![],
        }];
        assert!(render_text_report(8, &all_pass).contains("失败清单：无"));
    }
}
