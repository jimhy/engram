# -*- coding: utf-8 -*-
"""失败案例诊断：BM25 没命中的题，到底病在哪。

BM25 只对 query 与文档**共享的词元**打分。所以每道失败题必属两类之一：

  硬鸿沟  query 词元与 gold 文档词元交集为空（或只剩烂大街词）
          -> BM25 结构上不可能命中，再怎么调参数也没用。
             药是「把词汇对上」：查询扩展、同义词、或改写入端。

  排序病  有共享的判别性词元，但 gold 排在了别人后面
          -> 是打分/加权的问题，调得动。
             药是「调权重」：字段加权（tags 与 cue 分开）、词元位置、长度归一等。

两类的比例决定了下一步该往哪走，所以先量这个，别先急着上方案。
"""

import io
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from score import BM25, tokenize_query, tokenize_doc, doc_text  # noqa: E402

LIMIT = 10
STRONG_IDF = 4.0   # 判别性词元的 IDF 门槛（与 graph_probe 的建边门槛一致）


def main():
    mems = json.load(io.open(os.path.join(HERE, "corpus.json"), encoding="utf-8"))
    qs = [json.loads(l) for l in io.open(os.path.join(HERE, "memory-golden.jsonl"),
                                         encoding="utf-8") if l.strip()]
    bm = BM25(mems)
    id2idx = {m["id"]: i for i, m in enumerate(mems)}
    doc_toks = [set(tokenize_doc(doc_text(m))) for m in mems]

    out = []
    out.append("失败案例诊断｜语料 %d 条｜题 %d 道｜limit=%d" % (len(mems), len(qs), LIMIT))
    out.append("判别性词元门槛：IDF >= %.1f" % STRONG_IDF)
    out.append("")

    stats = {}
    detail = []
    for q in qs:
        toks = tokenize_query(q["query"])
        scored = [(i, bm.score(i, toks)) for i in range(len(mems))]
        scored = [(i, s) for i, s in scored if s > 0]
        scored.sort(key=lambda x: -x[1])
        ranked = scored[:LIMIT]
        gold_idxs = [id2idx[g] for g in q["gold"] if g in id2idx]

        rank = None
        for pos, (i, _s) in enumerate(ranked, start=1):
            if i in gold_idxs:
                rank = pos
                break

        kind = q["kind"]
        st = stats.setdefault(kind, {"n": 0, "hit": 0, "gap": 0, "order": 0})
        st["n"] += 1
        if rank:
            st["hit"] += 1
            continue

        # 未命中：判断是硬鸿沟还是排序病
        best_overlap = set()
        best_strong = set()
        best_gold = None
        for gi in gold_idxs:
            ov = set(toks) & doc_toks[gi]
            strong = {t for t in ov if bm.idf(t) >= STRONG_IDF}
            if len(strong) > len(best_strong) or best_gold is None:
                best_overlap, best_strong, best_gold = ov, strong, gi

        # gold 在全库里实际排第几（不截断）
        full_rank = None
        for pos, (i, _s) in enumerate(scored, start=1):
            if i in gold_idxs:
                full_rank = pos
                break

        if not best_strong:
            st["gap"] += 1
            cls = "硬鸿沟"
        else:
            st["order"] += 1
            cls = "排序病"

        detail.append({
            "id": q["id"], "kind": kind, "cls": cls, "query": q["query"],
            "overlap": len(best_overlap), "strong": sorted(best_strong)[:6],
            "full_rank": full_rank,
            "gold_cue": mems[best_gold]["cue"][:60] if best_gold is not None else "",
        })

    out.append("%-14s %4s %6s %8s %8s" % ("kind", "n", "命中", "硬鸿沟", "排序病"))
    out.append("-" * 46)
    tot = {"n": 0, "hit": 0, "gap": 0, "order": 0}
    for k in ("explicit", "paraphrase", "prescriptive", "implicit"):
        st = stats.get(k)
        if not st:
            continue
        for f in tot:
            tot[f] += st[f]
        out.append("%-14s %4d %6d %8d %8d" % (k, st["n"], st["hit"], st["gap"], st["order"]))
    out.append("%-14s %4d %6d %8d %8d" % ("** 合计 **", tot["n"], tot["hit"], tot["gap"], tot["order"]))
    out.append("")

    miss = tot["gap"] + tot["order"]
    if miss:
        out.append("未命中 %d 题中：硬鸿沟 %d 题（%.0f%%）、排序病 %d 题（%.0f%%）" % (
            miss, tot["gap"], tot["gap"] * 100.0 / miss,
            tot["order"], tot["order"] * 100.0 / miss))
        out.append("")
        if tot["gap"] > tot["order"]:
            out.append("=> 主要是**硬鸿沟**：query 与记忆压根没有判别性的共同词。")
            out.append("   调打分参数无效（BM25 只对共享词元打分）。")
            out.append("   有效的方向：查询扩展 / 同义词 / 改写入端让 cue 带上用户视角措辞。")
        else:
            out.append("=> 主要是**排序病**：共同词有，但 gold 被别人压下去了。")
            out.append("   这是调得动的：字段加权（tags vs cue）、长度归一、词元权重都可试。")
    out.append("")

    out.append("── 逐题明细（仅未命中）──")
    out.append("%-6s %-13s %-8s %6s %8s  %s" % ("题", "kind", "类型", "重叠", "全库名次", "判别性共同词"))
    for d in detail:
        out.append("%-6s %-13s %-8s %6d %8s  %s" % (
            d["id"], d["kind"], d["cls"], d["overlap"],
            str(d["full_rank"]), ",".join(d["strong"]) if d["strong"] else "（无）"))
    out.append("")
    out.append("── 未命中题的 query 与 gold 对照 ──")
    for d in detail:
        out.append("%s [%s|%s]" % (d["id"], d["kind"], d["cls"]))
        out.append("   问：%s" % d["query"])
        out.append("   答：%s" % d["gold_cue"])

    io.open(os.path.join(HERE, "diagnose.txt"), "w", encoding="utf-8").write("\n".join(out))
    print("OK -> diagnose.txt")


if __name__ == "__main__":
    main()
