# -*- coding: utf-8 -*-
"""弃权阈值的可移植性验证。

calibrate.py 实测：BM25 top1 原始分是最好的弃权判据（阈值 7.48 时噪声弃权 92%、
golden 误弃 2%）。但 **7.48 这个绝对值不可移植**——BM25 分数的量纲就是 IDF 的量纲，
而 IDF 随语料规模 N 变，换个库（或同一个库长大以后）阈值就不对了。

假设：把阈值表达成 `K * idf_max` 就能自动缩放。其中

    idf_max = ln(1 + (N - 1 + 0.5) / (1 + 0.5))

是该语料里最稀有词（df=1）的 IDF，即单个词元能贡献的 IDF 上限。

验证方法：在不同规模的子语料（80 / 120 / 160 / 200 / 261 条）上各自重新标定
最佳阈值，看 K = thr / idf_max 是否稳定。K 稳定 => 可移植，写进 Rust 用相对式；
K 不稳定 => 只能退回结构判据或让阈值可配置。

注意子语料要保证 golden 的 gold 条目仍在库里，否则「库里有」这个前提就没了——
所以每个子语料都强制包含全部 gold 条目，再从其余条目里随机补足。
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
    """语料规模 n 下，单个词元的 IDF 上限（df=1）。"""
    return math.log(1.0 + (n - 1 + 0.5) / (1 + 0.5))


def top1_score(bm, n_docs, query):
    tokens = tokenize_query(query)
    if not tokens:
        return 0.0
    best = 0.0
    for i in range(n_docs):
        s = bm.score(i, tokens)
        if s > best:
            best = s
    return best


def best_threshold(pos_scores, neg_scores, need_noise=0.90, max_wrong=0.05):
    """扫阈值：低于阈值即弃权。返回满足约束且误弃最低的阈值。"""
    vals = sorted(set(pos_scores + neg_scores))
    best = None
    for i, v in enumerate(vals):
        thr = v if i == len(vals) - 1 else (v + vals[i + 1]) / 2.0
        na = sum(1 for s in neg_scores if s < thr) / float(len(neg_scores))
        ga = sum(1 for s in pos_scores if s < thr) / float(len(pos_scores))
        if na >= need_noise and ga <= max_wrong:
            if best is None or ga < best[2] or (ga == best[2] and na > best[1]):
                best = (thr, na, ga)
    return best


def main():
    mems = json.load(io.open(os.path.join(HERE, "corpus.json"), encoding="utf-8"))
    qs = []
    for line in io.open(os.path.join(HERE, "memory-golden.jsonl"), encoding="utf-8"):
        line = line.strip()
        if line:
            qs.append(json.loads(line))
    noise = [l.strip() for l in io.open(os.path.join(HERE, "noise-queries.txt"), encoding="utf-8")
             if l.strip() and not l.strip().startswith("#")]

    gold_ids = set()
    for q in qs:
        gold_ids.update(q["gold"])
    gold_mems = [m for m in mems if m["id"] in gold_ids]
    other_mems = [m for m in mems if m["id"] not in gold_ids]

    lines = []
    lines.append("全量语料 %d 条，其中 gold 条目 %d 条、其余 %d 条" % (
        len(mems), len(gold_mems), len(other_mems)))
    lines.append("每个子语料强制含全部 gold，其余随机补足（seed=13）")
    lines.append("")
    lines.append("%-8s %-10s %-10s %-8s %-8s %-8s" % (
        "N", "idf_max", "最佳阈值", "K=thr/idfmax", "噪声弃权", "误弃"))
    lines.append("-" * 60)

    ks = []
    for n in (80, 120, 160, 200, 261):
        if n < len(gold_mems):
            continue
        random.seed(13)
        sub = list(gold_mems)
        pool = list(other_mems)
        random.shuffle(pool)
        sub.extend(pool[:max(0, n - len(sub))])
        bm = BM25(sub)
        pos = [top1_score(bm, len(sub), q["query"]) for q in qs]
        neg = [top1_score(bm, len(sub), nq) for nq in noise]
        got = best_threshold(pos, neg)
        im = idf_max_of(len(sub))
        if got:
            thr, na, ga = got
            k = thr / im
            ks.append(k)
            lines.append("%-8d %-10.3f %-10.3f %-8.3f %-8.0f%% %-8.0f%%" % (
                len(sub), im, thr, k, na * 100, ga * 100))
        else:
            lines.append("%-8d %-10.3f %-10s %-8s %-8s %-8s" % (
                len(sub), im, "无解", "-", "-", "-"))

    lines.append("")
    if len(ks) >= 3:
        kmin, kmax = min(ks), max(ks)
        kavg = sum(ks) / len(ks)
        spread = (kmax - kmin) / kavg if kavg else 0.0
        lines.append("K 取值：min=%.3f  max=%.3f  mean=%.3f  相对离散度=%.1f%%" % (
            kmin, kmax, kavg, spread * 100))
        if spread <= 0.25:
            lines.append("=> K 稳定，阈值可写成相对式 K * idf_max，建议 K = %.2f（取偏保守的下界侧）"
                         % (kmin * 0.95))
            lines.append("   取下界侧是因为：宁可少弃权（多返回几条噪音，用户看得见）")
            lines.append("   也不要多弃权（静默漏检，用户永远不知道自己漏了什么）。")
        else:
            lines.append("=> K 不稳定，相对式不可靠，须退回结构判据或让阈值可配置。")

    # 用建议的 K 在全量语料上复核一次，并列出误弃/漏网明细
    if ks:
        K = min(ks) * 0.95
        bm = BM25(mems)
        im = idf_max_of(len(mems))
        thr = K * im
        lines.append("")
        lines.append("── 用 K=%.2f（阈值=%.3f）在全量 %d 条语料上复核 ──" % (K, thr, len(mems)))
        wrong = []
        for q in qs:
            s = top1_score(bm, len(mems), q["query"])
            if s < thr:
                wrong.append((q["id"], q["kind"], s, q["query"]))
        leak = []
        for nq in noise:
            s = top1_score(bm, len(mems), nq)
            if s >= thr:
                leak.append((s, nq))
        lines.append("噪声弃权率 = %.0f%%   golden 误弃权率 = %.0f%%" % (
            (1 - len(leak) / float(len(noise))) * 100, len(wrong) / float(len(qs)) * 100))
        lines.append("")
        lines.append("误弃权的 golden 题（静默漏检，最危险，逐条核）：")
        if wrong:
            for qid, kind, s, query in wrong:
                lines.append("  %-6s %-13s score=%.3f  %s" % (qid, kind, s, query[:34]))
        else:
            lines.append("  （无）")
        lines.append("")
        lines.append("漏网的噪声 query（返回了噪音，可接受但要看）：")
        if leak:
            for s, nq in sorted(leak, reverse=True):
                lines.append("  score=%.3f  %s" % (s, nq[:34]))
        else:
            lines.append("  （无）")

    text = "\n".join(lines)
    io.open(os.path.join(HERE, "portability-report.txt"), "w", encoding="utf-8").write(text)
    print("OK -> portability-report.txt")
    return 0


if __name__ == "__main__":
    sys.exit(main())
