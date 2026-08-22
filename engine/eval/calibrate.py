# -*- coding: utf-8 -*-
"""E3 弃权判据标定：找一个能把「库里有相关内容」与「库里没有」分开的判据。

背景：score.py --abstain 实测 BM25 **原始分**在两个分布上完全重叠
（真答案 P10=3.758 < 噪声 P90=7.151，间隔比 0.53）。根因是 BM25 原始分
**不跨 query 可比**——短 query 累加项少但每项不打折，命中一个中等 IDF 的
烂大街 bigram（「什么」「明白」）就能超过长 query 命中真答案的分。

所以这里测四种候选判据，看哪个（或哪几个的组合）真能分开：

    s_raw    top1 的 BM25 原始分                （已知不行，作对照）
    s_norm   top1 分 / query 全部词元的 IDF 之和  （归一到 [0,1]，跨 query 可比）
    max_idf  top1 命中的词元里最高的 IDF          （只靠烂大街词命中 => 低）
    cov      top1 命中的不同词元数 / query 词元数  （覆盖率）

判定口径：50 道 golden 题 = 「库里有」（正类），50 条噪声 = 「库里没有」（负类）。
好的判据应当让正类高、负类低，且存在一个阈值使两类的错误率都可接受。
"""

import io
import json
import math
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from score import BM25, tokenize_query, percentile  # noqa: E402


def top1_metrics(bm, mems, query):
    """算 top1 候选的四项指标。"""
    tokens = tokenize_query(query)
    if not tokens:
        return None
    best_s = 0.0
    best_i = -1
    for i in range(len(mems)):
        s = bm.score(i, tokens)
        if s > best_s:
            best_s = s
            best_i = i
    if best_i < 0:
        return {"s_raw": 0.0, "s_norm": 0.0, "max_idf": 0.0, "cov": 0.0, "ntok": len(tokens)}
    hits = bm.hit_tokens(best_i, tokens)
    idf_sum = sum(bm.idf(t) for t in tokens)
    return {
        "s_raw": best_s,
        "s_norm": best_s / idf_sum if idf_sum > 0 else 0.0,
        "max_idf": hits[0][1] if hits else 0.0,
        "cov": len(hits) / float(len(tokens)),
        "ntok": len(tokens),
    }


def sweep(pos, neg, key, lo_is_abstain=True):
    """扫阈值，找「噪声弃权率 >= 90% 且 golden 误弃权率最低」的分割点。

    返回 (最佳阈值, 噪声弃权率, golden 误弃权率, 分离度)。
    """
    vals = sorted(set([p[key] for p in pos] + [n[key] for n in neg]))
    best = None
    for i in range(len(vals)):
        # 阈值取相邻值中点，避免正好落在样本点上
        thr = vals[i] if i == len(vals) - 1 else (vals[i] + vals[i + 1]) / 2.0
        # 低于阈值 => 弃权
        noise_abstain = sum(1 for n in neg if n[key] < thr) / float(len(neg))
        gold_abstain = sum(1 for p in pos if p[key] < thr) / float(len(pos))
        if noise_abstain >= 0.90:
            if best is None or gold_abstain < best[2]:
                best = (thr, noise_abstain, gold_abstain)
    if best is None:
        # 达不到 90%，就报「最大化 (噪声弃权率 - golden 误弃权率)」的点
        for i in range(len(vals)):
            thr = vals[i] if i == len(vals) - 1 else (vals[i] + vals[i + 1]) / 2.0
            na = sum(1 for n in neg if n[key] < thr) / float(len(neg))
            ga = sum(1 for p in pos if p[key] < thr) / float(len(pos))
            if best is None or (na - ga) > (best[1] - best[2]):
                best = (thr, na, ga)
    return best


def dist(vals):
    v = sorted(vals)
    return "min=%.3f P10=%.3f P50=%.3f P90=%.3f max=%.3f" % (
        v[0], percentile(v, 10), percentile(v, 50), percentile(v, 90), v[-1])


def main():
    mems = json.load(io.open(os.path.join(HERE, "corpus.json"), encoding="utf-8"))
    qs = []
    for line in io.open(os.path.join(HERE, "memory-golden.jsonl"), encoding="utf-8"):
        line = line.strip()
        if line:
            qs.append(json.loads(line))
    noise = [l.strip() for l in io.open(os.path.join(HERE, "noise-queries.txt"), encoding="utf-8")
             if l.strip() and not l.strip().startswith("#")]

    bm = BM25(mems)
    pos = [top1_metrics(bm, mems, q["query"]) for q in qs]
    neg = [top1_metrics(bm, mems, n) for n in noise]
    pos = [p for p in pos if p]
    neg = [n for n in neg if n]

    lines = []
    lines.append("golden(库里有) n=%d   噪声(库里没有) n=%d   语料 %d 条" % (len(pos), len(neg), len(mems)))
    lines.append("")
    lines.append("── 四种判据的分布对比 ──")
    for key in ("s_raw", "s_norm", "max_idf", "cov"):
        lines.append("%-8s golden  %s" % (key, dist([p[key] for p in pos])))
        lines.append("%-8s 噪声    %s" % ("", dist([n[key] for n in neg])))
        best = sweep(pos, neg, key)
        thr, na, ga = best
        lines.append("%-8s 最佳阈值=%.4f  噪声弃权率=%.0f%%  golden误弃权率=%.0f%%  %s" % (
            "", thr, na * 100, ga * 100,
            "✓ 可用" if (na >= 0.90 and ga <= 0.05) else "✗ 不满足 (噪声≥90% 且 误弃≤5%)"))
        lines.append("")

    # ── 组合判据：max_idf 与 cov 的与门 ──
    lines.append("── 组合判据扫描：max_idf >= A 且 cov >= B 才不弃权 ──")
    best_combo = None
    a_vals = [round(x * 0.25, 2) for x in range(8, 32)]      # 2.00 ~ 7.75
    b_vals = [round(x * 0.02, 2) for x in range(0, 26)]      # 0.00 ~ 0.50
    for A in a_vals:
        for B in b_vals:
            na = sum(1 for n in neg if not (n["max_idf"] >= A and n["cov"] >= B)) / float(len(neg))
            ga = sum(1 for p in pos if not (p["max_idf"] >= A and p["cov"] >= B)) / float(len(pos))
            if na >= 0.90 and ga <= 0.05:
                score = na - ga
                if best_combo is None or score > best_combo[0]:
                    best_combo = (score, A, B, na, ga)
    if best_combo:
        _s, A, B, na, ga = best_combo
        lines.append("✓ 找到可用组合：max_idf >= %.2f 且 cov >= %.2f" % (A, B))
        lines.append("   噪声弃权率 = %.0f%%   golden 误弃权率 = %.0f%%" % (na * 100, ga * 100))
    else:
        lines.append("✗ 在扫描范围内没有同时满足「噪声弃权≥90% 且 误弃≤5%」的组合")
        # 退而求其次：报噪声弃权率最高且误弃 <= 10% 的
        alt = None
        for A in a_vals:
            for B in b_vals:
                na = sum(1 for n in neg if not (n["max_idf"] >= A and n["cov"] >= B)) / float(len(neg))
                ga = sum(1 for p in pos if not (p["max_idf"] >= A and p["cov"] >= B)) / float(len(pos))
                if ga <= 0.10 and (alt is None or na > alt[3]):
                    alt = (na - ga, A, B, na, ga)
        if alt:
            _s, A, B, na, ga = alt
            lines.append("   次优（放宽误弃到 10%%）：max_idf >= %.2f 且 cov >= %.2f"
                         " -> 噪声弃权 %.0f%%  误弃 %.0f%%" % (A, B, na * 100, ga * 100))

    # ── 误弃权的是哪几题（必须看，静默漏检比噪声更危险）──
    if best_combo:
        _s, A, B, _na, _ga = best_combo
        lines.append("")
        lines.append("── 被该组合误弃权的 golden 题（静默漏检，逐条核）──")
        for q, p in zip(qs, pos):
            if not (p["max_idf"] >= A and p["cov"] >= B):
                lines.append("  %-6s %-13s max_idf=%.2f cov=%.2f  %s" % (
                    q["id"], q["kind"], p["max_idf"], p["cov"], q["query"][:30]))
        lines.append("")
        lines.append("── 未被弃权的噪声 query（漏网，逐条核）──")
        for n_q, n in zip(noise, neg):
            if n["max_idf"] >= A and n["cov"] >= B:
                lines.append("  max_idf=%.2f cov=%.2f  %s" % (n["max_idf"], n["cov"], n_q[:30]))

    text = "\n".join(lines)
    io.open(os.path.join(HERE, "calibrate-report.txt"), "w", encoding="utf-8").write(text)
    print("OK -> calibrate-report.txt")
    return 0


if __name__ == "__main__":
    sys.exit(main())
