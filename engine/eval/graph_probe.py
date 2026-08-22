# -*- coding: utf-8 -*-
"""图谱关联跳转的价值验证（廉价近似）。

要回答的问题：把记忆之间的关系连起来、检索时沿关系扩散，在这个库上到底有没有用？

HippoRAG 那套的真实形态是「LLM 抽实体关系建图 → 向量找入口种子 → Personalized
PageRank 扩散」。它的代价是**每条记忆一次 LLM 调用**（328 条就是 328 次，新写一条还要再跑），
直接砸掉 engram「零依赖、单二进制、离线可用」的定位。所以在付这个代价之前，
先用零成本的方式把它的**核心机制**单独拎出来量一量。

近似方法（保留机制、去掉 LLM）：
- 建边：两条记忆共享的**高 IDF 词元**越多、边越重（用词元共现代替 LLM 抽的实体关系）
- 种子：BM25 的 top-N（代替向量入口）
- 扩散：Personalized PageRank，种子分布按 BM25 分数加权
- 重排：最终分 = (1-alpha) * BM25归一 + alpha * PPR归一

这个实验**能**证明的：在这个库上，「沿关联扩散」这个机制本身有没有带来提升。
**不能**证明的：LLM 抽出来的边质量更高，理论上可能更好——但结合它的成本，
若廉价版连一点苗头都没有，更贵的版本就更难 justify。

重点看 implicit（需要跨条关联）与 paraphrase 两类，它们才是图谱该发力的地方。
"""

import io
import json
import math
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from score import BM25, tokenize_query, tokenize_doc, doc_text  # noqa: E402

LIMIT = 10
# 只有 IDF 高于这个门槛的词元才配当"关系"——否则"的/了/是"这类词会把全库连成一坨。
EDGE_MIN_IDF = 4.0
TOP_SEEDS = 5          # 取 BM25 前几名当扩散种子
PPR_ALPHA = 0.15       # 重启概率
PPR_ITERS = 20


def build_graph(mems, bm):
    """用共享的高 IDF 词元建无向带权图。

    边权 = 两条记忆共享的高 IDF 词元的 IDF 之和（共享越多越稀有的词，关系越强）。
    """
    # 每条记忆的高 IDF 词元集合
    sets = []
    for i, m in enumerate(mems):
        toks = set(tokenize_doc(doc_text(m)))
        sets.append({t for t in toks if bm.idf(t) >= EDGE_MIN_IDF})

    # 倒排：词元 -> 含它的记忆下标。只在共享词元上连边，避免 O(N^2) 全比。
    inv = {}
    for i, s in enumerate(sets):
        for t in s:
            inv.setdefault(t, []).append(i)

    adj = [{} for _ in mems]
    for t, idxs in inv.items():
        if len(idxs) < 2 or len(idxs) > 40:
            # 只出现在一条里的词连不了边；出现在几十条里的词说明它不够"关系"
            continue
        w = bm.idf(t)
        for a in range(len(idxs)):
            for b in range(a + 1, len(idxs)):
                i, j = idxs[a], idxs[b]
                adj[i][j] = adj[i].get(j, 0.0) + w
                adj[j][i] = adj[j].get(i, 0.0) + w
    return adj


def ppr(adj, seeds, alpha=PPR_ALPHA, iters=PPR_ITERS):
    """Personalized PageRank：从种子出发沿边扩散。"""
    n = len(adj)
    total = sum(seeds.values())
    if total <= 0:
        return {}
    reset = {i: v / total for i, v in seeds.items()}
    score = dict(reset)
    for _ in range(iters):
        nxt = {}
        for i, s in score.items():
            if s <= 0:
                continue
            nbrs = adj[i]
            deg = sum(nbrs.values())
            if deg <= 0:
                continue
            share = (1.0 - alpha) * s
            for j, w in nbrs.items():
                nxt[j] = nxt.get(j, 0.0) + share * (w / deg)
        for i, v in reset.items():
            nxt[i] = nxt.get(i, 0.0) + alpha * v
        score = nxt
    return score


def norm(d):
    if not d:
        return {}
    lo = min(d.values())
    hi = max(d.values())
    if hi - lo <= 0:
        return {k: 0.0 for k in d}
    return {k: (v - lo) / (hi - lo) for k, v in d.items()}


def rank_bm25_idx(bm, mems, query, limit=LIMIT):
    toks = tokenize_query(query)
    scored = [(i, bm.score(i, toks)) for i in range(len(mems))]
    scored = [(i, s) for i, s in scored if s > 0]
    scored.sort(key=lambda x: -x[1])
    return scored[:limit], toks


def rank_graph(bm, mems, adj, query, blend, limit=LIMIT):
    all_scored, toks = rank_bm25_idx(bm, mems, query, limit=len(mems))
    if not all_scored:
        return []
    seeds = dict(all_scored[:TOP_SEEDS])
    p = ppr(adj, seeds)
    nb = norm(dict(all_scored))
    np_ = norm(p)
    keys = set(nb) | set(np_)
    final = [(i, (1 - blend) * nb.get(i, 0.0) + blend * np_.get(i, 0.0)) for i in keys]
    final.sort(key=lambda x: -x[1])
    return final[:limit]


def first_rank(idxs, mems, gold):
    g = set(gold)
    for pos, (i, _s) in enumerate(idxs, start=1):
        if mems[i]["id"] in g:
            return pos
    return None


def tally(ranks_by_kind, kinds):
    rows = []
    tot = {"n": 0, "h1": 0, "h3": 0, "h8": 0, "mrr": 0.0}
    for k in kinds:
        rs = ranks_by_kind.get(k, [])
        if not rs:
            continue
        st = {"n": len(rs),
              "h1": sum(1 for r in rs if r and r <= 1),
              "h3": sum(1 for r in rs if r and r <= 3),
              "h8": sum(1 for r in rs if r and r <= 8),
              "mrr": sum(1.0 / r for r in rs if r)}
        for f in tot:
            tot[f] += st[f]
        rows.append((k, st))
    return rows, tot


def main():
    mems = json.load(io.open(os.path.join(HERE, "corpus.json"), encoding="utf-8"))
    qs = [json.loads(l) for l in io.open(os.path.join(HERE, "memory-golden.jsonl"),
                                         encoding="utf-8") if l.strip()]
    kinds = ["explicit", "paraphrase", "prescriptive", "implicit"]
    bm = BM25(mems)
    adj = build_graph(mems, bm)

    n_edges = sum(len(a) for a in adj) // 2
    isolated = sum(1 for a in adj if not a)
    degs = sorted(len(a) for a in adj)

    out = []
    out.append("图谱关联跳转价值验证（廉价近似：词元共现建边 + Personalized PageRank）")
    out.append("语料 %d 条 | 题 %d 道 | 边 %d 条 | 孤立点 %d | 度数中位 %d 最大 %d"
               % (len(mems), len(qs), n_edges, isolated,
                  degs[len(degs) // 2], degs[-1]))
    out.append("参数：边最低 IDF=%.1f，种子 top%d，PPR alpha=%.2f" % (EDGE_MIN_IDF, TOP_SEEDS, PPR_ALPHA))
    out.append("")

    # 基线
    base = {}
    for q in qs:
        r, _ = rank_bm25_idx(bm, mems, q["query"])
        base.setdefault(q["kind"], []).append(first_rank(r, mems, q["gold"]))
    rows, tot_b = tally(base, kinds)

    out.append("%-16s %-13s %4s %7s %7s %7s %8s" % ("检索器", "kind", "n", "hit@1", "hit@3", "hit@8", "MRR"))
    out.append("-" * 70)
    for k, st in rows:
        out.append("%-16s %-13s %4d %7s %7s %7s %8.3f" % (
            "BM25(基线)", k, st["n"], "%d/%d" % (st["h1"], st["n"]),
            "%d/%d" % (st["h3"], st["n"]), "%d/%d" % (st["h8"], st["n"]),
            st["mrr"] / st["n"]))
    out.append("%-16s %-13s %4d %7s %7s %7s %8.3f" % (
        "BM25(基线)", "** 合计 **", tot_b["n"], "%d/%d" % (tot_b["h1"], tot_b["n"]),
        "%d/%d" % (tot_b["h3"], tot_b["n"]), "%d/%d" % (tot_b["h8"], tot_b["n"]),
        tot_b["mrr"] / tot_b["n"]))
    out.append("")

    # 扫不同的图谱权重，给它最好的机会
    best = None
    for blend in (0.1, 0.2, 0.3, 0.5):
        g = {}
        for q in qs:
            r = rank_graph(bm, mems, adj, q["query"], blend)
            g.setdefault(q["kind"], []).append(first_rank(r, mems, q["gold"]))
        rows_g, tot_g = tally(g, kinds)
        mrr_g = tot_g["mrr"] / tot_g["n"]
        for k, st in rows_g:
            out.append("%-16s %-13s %4d %7s %7s %7s %8.3f" % (
                "+图谱 w=%.1f" % blend, k, st["n"], "%d/%d" % (st["h1"], st["n"]),
                "%d/%d" % (st["h3"], st["n"]), "%d/%d" % (st["h8"], st["n"]),
                st["mrr"] / st["n"]))
        out.append("%-16s %-13s %4d %7s %7s %7s %8.3f" % (
            "+图谱 w=%.1f" % blend, "** 合计 **", tot_g["n"], "%d/%d" % (tot_g["h1"], tot_g["n"]),
            "%d/%d" % (tot_g["h3"], tot_g["n"]), "%d/%d" % (tot_g["h8"], tot_g["n"]), mrr_g))
        out.append("")
        if best is None or mrr_g > best[1]:
            best = (blend, mrr_g, tot_g, g)

    blend, mrr_g, tot_g, g = best
    mrr_b = tot_b["mrr"] / tot_b["n"]
    out.append("── 判据核算 ──")
    out.append("基线 BM25 MRR = %.3f" % mrr_b)
    out.append("图谱最优 (w=%.1f) MRR = %.3f，差 %+.3f" % (blend, mrr_g, mrr_g - mrr_b))
    # implicit 是图谱该发力的地方，单独看
    imp_b = base.get("implicit", [])
    imp_g = g.get("implicit", [])
    mb = sum(1.0 / r for r in imp_b if r) / len(imp_b) if imp_b else 0
    mg = sum(1.0 / r for r in imp_g if r) / len(imp_g) if imp_g else 0
    out.append("implicit（图谱最该发力的一类）：MRR %.3f -> %.3f，差 %+.3f" % (mb, mg, mg - mb))
    out.append("")
    verdict = "有苗头，值得进一步评估" if (mrr_g - mrr_b) >= 0.03 else "**没有提升**——沿关联扩散这个机制在本库上不成立"
    out.append("结论：%s" % verdict)
    out.append("")
    out.append("注意本实验的边界：这里用【词元共现】代替 LLM 抽取的实体关系，边的质量更低。")
    out.append("它证明的是「扩散机制本身在这个库上没带来提升」，不能证明高质量边一定也没用；")
    out.append("但结合『每条记忆一次 LLM 调用 + 破坏离线单二进制定位』的代价，")
    out.append("廉价版连苗头都没有的话，更贵的版本更难 justify。")

    io.open(os.path.join(HERE, "graph-probe.txt"), "w", encoding="utf-8").write("\n".join(out))
    print("OK -> graph-probe.txt")


if __name__ == "__main__":
    main()
