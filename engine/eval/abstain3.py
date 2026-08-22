# -*- coding: utf-8 -*-
"""弃权判据第三轮（定稿轮）：query 信息量 × 归一化命中强度的二维判据。

前两轮的实证结论：
1. BM25 top1 原始分：只在全量 261 条上有解，80~200 条全部无解 => 对语料规模不稳健
2. 落差比值（top1/top2、top1/top5、top1/median、gap）：**所有规模全部无解**
   => 「形状比绝对值稳健」这个假设被否掉
3. 但「query 词元数 < 4 就弃权」这道纯 query 侧的闸：拦掉 25/50 条噪声、只误伤 1 题
   => 噪声里有一半是零信息短语，它们才是把阈值挤死的主因

第三轮假设：把「词元个数」换成「**query 信息量**」，并对语料规模归一。

    idf_max = ln(1 + (N - 1 + 0.5) / 1.5)        单词元 IDF 上限，随 N 缩放
    qinfo   = sum(idf(t) for t in query) / idf_max   query 信息量（单位：等效稀有词个数）
    hit     = top1_bm25 / idf_max                    归一化命中强度

「好的」「明白了」这种 1~2 个烂大街中文 bigram 的 qinfo 很低；
而「PowerShell ErrorActionPreference NativeCommandError」虽然只有 3 个词元，
但个个稀有，qinfo 很高——这正是词元个数闸误伤 E-14 而信息量闸不会的原因。

两个量都除以 idf_max，所以都跨语料规模可比。二维扫描找稳定的阈值对。
"""

import io
import json
import math
import os
import random
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from score import BM25, tokenize_query  # noqa: E402


def idf_max_of(n):
    return math.log(1.0 + (n - 1 + 0.5) / 1.5)


def metrics(bm, n_docs, query):
    tokens = tokenize_query(query)
    im = idf_max_of(n_docs)
    if not tokens or im <= 0:
        return {"qinfo": 0.0, "hit": 0.0}
    qinfo = sum(bm.idf(t) for t in tokens) / im
    best = 0.0
    for i in range(n_docs):
        s = bm.score(i, tokens)
        if s > best:
            best = s
    return {"qinfo": qinfo, "hit": best / im}


def scan2d(pos, neg, need_noise=0.90, max_wrong=0.05):
    """二维扫描：qinfo >= A 且 hit >= B 才不弃权。返回满足约束且误弃最低的 (A,B)。"""
    best = None
    a_vals = [round(x * 0.05, 2) for x in range(1, 121)]   # 0.05 ~ 6.00
    b_vals = [round(x * 0.05, 2) for x in range(1, 81)]    # 0.05 ~ 4.00
    for A in a_vals:
        for B in b_vals:
            na = sum(1 for n in neg if not (n["qinfo"] >= A and n["hit"] >= B)) / float(len(neg))
            if na < need_noise:
                continue
            ga = sum(1 for p in pos if not (p["qinfo"] >= A and p["hit"] >= B)) / float(len(pos))
            if ga <= max_wrong:
                if best is None or ga < best[3] or (ga == best[3] and na > best[2]):
                    best = (A, B, na, ga)
    return best


def build_subcorpora(mems, qs):
    gold_ids = set()
    for q in qs:
        gold_ids.update(q["gold"])
    gold_mems = [m for m in mems if m["id"] in gold_ids]
    other = [m for m in mems if m["id"] not in gold_ids]
    out = []
    for n in (80, 120, 160, 200, len(mems)):
        random.seed(13)
        pool = list(other)
        random.shuffle(pool)
        out.append(list(gold_mems) + pool[:max(0, n - len(gold_mems))])
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
    lines.append("判据：qinfo >= A 且 hit >= B 才不弃权；两量均已除以 idf_max，跨语料规模可比")
    lines.append("")
    lines.append("%-8s %-10s %-8s %-8s %-10s %-8s" % ("N", "idf_max", "A", "B", "噪声弃权", "误弃"))
    lines.append("-" * 56)

    sols = []
    for sub in build_subcorpora(mems, qs):
        bm = BM25(sub)
        n = len(sub)
        pos = [metrics(bm, n, q["query"]) for q in qs]
        neg = [metrics(bm, n, nq) for nq in noise]
        got = scan2d(pos, neg)
        if got:
            A, B, na, ga = got
            sols.append((n, A, B))
            lines.append("%-8d %-10.3f %-8.2f %-8.2f %-10.0f%% %-8.0f%%" % (
                n, idf_max_of(n), A, B, na * 100, ga * 100))
        else:
            lines.append("%-8d %-10.3f %-8s %-8s %-10s %-8s" % (
                n, idf_max_of(n), "无解", "-", "-", "-"))

    lines.append("")
    if len(sols) == 5:
        As = [s[1] for s in sols]
        Bs = [s[2] for s in sols]
        lines.append("── 全部 5 个规模都有解 ──")
        lines.append("A（query 信息量下限）范围 %.2f ~ %.2f" % (min(As), max(As)))
        lines.append("B（归一化命中强度下限）范围 %.2f ~ %.2f" % (min(Bs), max(Bs)))
        # 取能在所有规模上同时成立的最保守组合（A、B 都取各自的最小值，
        # 这样弃权最少——宁可返回噪音也不要静默漏检）
        A0, B0 = min(As), min(Bs)
        lines.append("")
        lines.append("── 用统一参数 A=%.2f B=%.2f 在各规模上复核 ──" % (A0, B0))
        lines.append("%-8s %-10s %-8s" % ("N", "噪声弃权", "误弃"))
        ok_all = True
        for sub in build_subcorpora(mems, qs):
            bm = BM25(sub)
            n = len(sub)
            pos = [metrics(bm, n, q["query"]) for q in qs]
            neg = [metrics(bm, n, nq) for nq in noise]
            na = sum(1 for x in neg if not (x["qinfo"] >= A0 and x["hit"] >= B0)) / float(len(neg))
            ga = sum(1 for x in pos if not (x["qinfo"] >= A0 and x["hit"] >= B0)) / float(len(pos))
            flag = "" if (na >= 0.90 and ga <= 0.05) else "  <-- 不达标"
            if flag:
                ok_all = False
            lines.append("%-8d %-10.0f%% %-8.0f%%%s" % (n, na * 100, ga * 100, flag))
        lines.append("")
        lines.append("统一参数在所有规模上%s" % ("**全部达标** => 可以写进 Rust" if ok_all
                                          else "有不达标项 => 需按语料规模自适应或放宽验收线"))

        # 明细
        bm = BM25(mems)
        n = len(mems)
        lines.append("")
        lines.append("── 全量语料下的误弃/漏网明细（A=%.2f B=%.2f）──" % (A0, B0))
        lines.append("误弃权的 golden 题（静默漏检，最危险）：")
        any_w = False
        for q in qs:
            m = metrics(bm, n, q["query"])
            if not (m["qinfo"] >= A0 and m["hit"] >= B0):
                any_w = True
                lines.append("  %-6s %-13s qinfo=%.2f hit=%.2f  %s" % (
                    q["id"], q["kind"], m["qinfo"], m["hit"], q["query"][:30]))
        if not any_w:
            lines.append("  （无）")
        lines.append("漏网的噪声 query：")
        any_l = False
        for nq in noise:
            m = metrics(bm, n, nq)
            if m["qinfo"] >= A0 and m["hit"] >= B0:
                any_l = True
                lines.append("  qinfo=%.2f hit=%.2f  %s" % (m["qinfo"], m["hit"], nq[:30]))
        if not any_l:
            lines.append("  （无）")
    else:
        lines.append("只有 %d/5 个规模有解 => 二维判据同样不稳健，须放宽验收线或让参数可配置。" % len(sols))

    text = "\n".join(lines)
    io.open(os.path.join(HERE, "abstain3-report.txt"), "w", encoding="utf-8").write(text)
    print("OK -> abstain3-report.txt")
    return 0


if __name__ == "__main__":
    sys.exit(main())
