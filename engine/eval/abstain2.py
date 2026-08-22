# -*- coding: utf-8 -*-
"""弃权判据第二轮：找一个**对语料规模稳健**的判据。

第一轮结论（calibrate.py + portability.py）：
- BM25 top1 原始分在全量 261 条上可用（阈值 7.48 → 噪声弃权 92%、误弃 2%）
- 但在 80/120/160/200 条的子语料上**全部无解**，且 K=thr/idf_max 只有一个样本点
  => 分数阈值对语料规模不稳健，不能直接写进 Rust

第二轮假设：**形状比绝对值稳健**。真答案通常有明显的分数落差（top1 显著高于其后），
噪声则是一堆半吊子匹配、分数平坦。比值天然跨 query、跨语料规模可比。

测这些判据（都是「越大越像真命中」）：
    r12    top1 / top2
    r15    top1 / top5
    r1med  top1 / median(top10)
    gap    (top1 - top2) / top1
    ntok   query 词元数（不是判据，是闸：零信息短语单独拦）

并在 80/120/160/200/261 五个语料规模上各自标定，看最佳阈值是否稳定。
"""

import io
import json
import math
import os
import random
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from score import BM25, tokenize_query, percentile  # noqa: E402


def topn_scores(bm, n_docs, query, n=10):
    tokens = tokenize_query(query)
    if not tokens:
        return [], 0
    scores = []
    for i in range(n_docs):
        s = bm.score(i, tokens)
        if s > 0.0:
            scores.append(s)
    scores.sort(reverse=True)
    return scores[:n], len(tokens)


def metrics(bm, n_docs, query):
    top, ntok = topn_scores(bm, n_docs, query)
    if not top:
        return {"r12": 0.0, "r15": 0.0, "r1med": 0.0, "gap": 0.0,
                "ntok": ntok, "ncand": 0, "s1": 0.0}
    s1 = top[0]
    s2 = top[1] if len(top) > 1 else 0.0
    s5 = top[4] if len(top) > 4 else 0.0
    med = top[len(top) // 2] if len(top) > 1 else 0.0
    return {
        "r12": s1 / s2 if s2 > 0 else float("inf"),
        "r15": s1 / s5 if s5 > 0 else float("inf"),
        "r1med": s1 / med if med > 0 else float("inf"),
        "gap": (s1 - s2) / s1 if s1 > 0 else 0.0,
        "ntok": ntok,
        "ncand": len(top),
        "s1": s1,
    }


def sweep(pos, neg, key, need_noise=0.90, max_wrong=0.05):
    """低于阈值即弃权。返回 (thr, 噪声弃权率, 误弃率) 或 None。"""
    def val(d):
        v = d[key]
        return 1e9 if v == float("inf") else v
    vals = sorted(set([val(p) for p in pos] + [val(n) for n in neg]))
    best = None
    for i, v in enumerate(vals):
        thr = v if i == len(vals) - 1 else (v + vals[i + 1]) / 2.0
        na = sum(1 for n in neg if val(n) < thr) / float(len(neg))
        ga = sum(1 for p in pos if val(p) < thr) / float(len(pos))
        if na >= need_noise and ga <= max_wrong:
            if best is None or ga < best[2] or (ga == best[2] and na > best[1]):
                best = (thr, na, ga)
    return best


def build_subcorpora(mems, qs):
    gold_ids = set()
    for q in qs:
        gold_ids.update(q["gold"])
    gold_mems = [m for m in mems if m["id"] in gold_ids]
    other = [m for m in mems if m["id"] not in gold_ids]
    out = []
    for n in (80, 120, 160, 200, len(mems)):
        if n < len(gold_mems):
            continue
        random.seed(13)
        pool = list(other)
        random.shuffle(pool)
        sub = list(gold_mems) + pool[:max(0, n - len(gold_mems))]
        out.append(sub)
    return out


def main():
    mems = json.load(io.open(os.path.join(HERE, "corpus.json"), encoding="utf-8"))
    qs = []
    for line in io.open(os.path.join(HERE, "memory-golden.jsonl"), encoding="utf-8"):
        line = line.strip()
        if line:
            qs.append(json.loads(line))
    noise = [l.strip() for l in io.open(os.path.join(HERE, "noise-queries.txt"), encoding="utf-8")
             if l.strip() and not l.strip().startswith("#")]

    lines = []
    lines.append("golden %d 题 | 噪声 %d 条 | 全量语料 %d 条" % (len(qs), len(noise), len(mems)))
    lines.append("")

    KEYS = ("s1", "r12", "r15", "r1med", "gap")
    table = {}
    for sub in build_subcorpora(mems, qs):
        bm = BM25(sub)
        pos = [metrics(bm, len(sub), q["query"]) for q in qs]
        neg = [metrics(bm, len(sub), nq) for nq in noise]
        for key in KEYS:
            got = sweep(pos, neg, key)
            table.setdefault(key, []).append((len(sub), got))

    lines.append("── 各判据在 5 个语料规模上的标定结果（要求：噪声弃权≥90% 且 误弃≤5%）──")
    lines.append("%-8s %-8s %-12s %-10s %-8s" % ("判据", "N", "最佳阈值", "噪声弃权", "误弃"))
    lines.append("-" * 52)
    stable = []
    for key in KEYS:
        solved = 0
        thrs = []
        for n, got in table[key]:
            if got:
                thr, na, ga = got
                thrs.append(thr)
                solved += 1
                lines.append("%-8s %-8d %-12.3f %-10.0f%% %-8.0f%%" % (key, n, thr, na * 100, ga * 100))
            else:
                lines.append("%-8s %-8d %-12s %-10s %-8s" % (key, n, "无解", "-", "-"))
        if solved == len(table[key]) and thrs:
            spread = (max(thrs) - min(thrs)) / (sum(thrs) / len(thrs))
            stable.append((spread, key, min(thrs), max(thrs), sum(thrs) / len(thrs)))
        lines.append("")

    lines.append("── 结论 ──")
    if stable:
        stable.sort()
        spread, key, lo, hi, avg = stable[0]
        lines.append("全部 5 个规模都有解的判据：" + ", ".join(s[1] for s in stable))
        lines.append("最稳的是 %s：阈值 %.3f~%.3f（均值 %.3f，相对离散度 %.1f%%）"
                     % (key, lo, hi, avg, spread * 100))
        lines.append("建议取偏保守的下界侧 %.2f——宁可少弃权（返回噪音，用户看得见），"
                     "也不要多弃权（静默漏检，用户永远不知道自己漏了什么）。" % (lo * 0.95))
    else:
        lines.append("没有任何单判据在全部 5 个规模上都有解。")

    # ── 零信息短语单独看：它们是漏网主力 ──
    bm = BM25(mems)
    lines.append("")
    lines.append("── 零信息短语（query 词元数）与漏网的关系 ──")
    short_noise = [nq for nq in noise if len(tokenize_query(nq)) <= 3]
    lines.append("噪声里词元数 <= 3 的有 %d/%d 条" % (len(short_noise), len(noise)))
    gold_short = [q for q in qs if len(tokenize_query(q["query"])) <= 3]
    lines.append("golden 里词元数 <= 3 的有 %d/%d 题" % (len(gold_short), len(qs)))
    if gold_short:
        for q in gold_short:
            lines.append("   %s %s (%d 词元)" % (q["id"], q["query"][:26], len(tokenize_query(q["query"]))))
    lines.append("=> 若加「词元数 < 4 一律弃权」这道闸，会误伤 %d 道 golden 题、"
                 "拦掉 %d 条噪声。" % (len(gold_short), len(short_noise)))

    text = "\n".join(lines)
    io.open(os.path.join(HERE, "abstain2-report.txt"), "w", encoding="utf-8").write(text)
    print("OK -> abstain2-report.txt")
    return 0


if __name__ == "__main__":
    sys.exit(main())
