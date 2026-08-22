# -*- coding: utf-8 -*-
"""从题源生成 memory-golden.jsonl，并做出题质量自查。

为什么不直接手写 mem id：库里短 id 前缀（前 8 位 hex）**有重复**
（18cdbfff / 18ce0171 / 18ce017d / 18cdc386 / 18cdc38b / 18cddf8d 都各有两条），
手抄极易抄错，而抄错的 gold 会让整把尺子静默失真且不报错。
所以题源用「cue 里的独特子串」定位，由本脚本解析成完整 id，
匹配到 0 条或 >1 条都当场报错。

自查项：
1. gold 定位子串必须唯一命中一条记忆
2. paraphrase 题：query 词元与 gold cue 词元的重叠里不该有判别力强（高 IDF）的词——
   否则它其实是 explicit 题，会虚高分数
"""

import io
import json
import math
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from score import BM25, tokenize_query, tokenize_doc, doc_text  # noqa: E402

# ─────────────────────────────────────────────────────────────────────
# 题源：id | kind | query | gold 定位子串（多个用 ;; 分隔）
#
# 出题纪律：先想「用户会怎么问」再定 gold，绝不跑一次检索拿返回值当 gold
# （那是给现状背书）。paraphrase 题刻意避开 cue 原词。
# ─────────────────────────────────────────────────────────────────────
SRC_FILE = "golden-src.txt"


def load_src():
    """题源放外部文件：它含 gold 的 cue 定位子串，会带出具体记忆库的内容，
    因此**不进公开仓库**（见 .gitignore）。格式：

        id | kind | query | gold 定位子串（多个用 ;; 分隔）

    没有这个文件就先照 README 的说明自己出题。"""
    p = os.path.join(HERE, SRC_FILE)
    if not os.path.exists(p):
        raise SystemExit(
            "缺少题源 %s。它是针对**你自己的记忆库**出的题，需自行编写；"
            "格式与出题纪律见 README.md。" % SRC_FILE)
    return io.open(p, encoding="utf-8").read()



def parse_src():
    out = []
    for raw in load_src().splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        parts = [p.strip() for p in line.split("|")]
        if len(parts) != 4:
            raise SystemExit("题源格式错误（应为 4 段）：%s" % line)
        qid, kind, query, goldsrc = parts
        needles = [n.strip() for n in goldsrc.split(";;") if n.strip()]
        out.append({"id": qid, "kind": kind, "query": query, "needles": needles})
    return out


def main():
    mems = json.load(io.open(os.path.join(HERE, "corpus.json"), encoding="utf-8"))
    qs = parse_src()

    problems = []
    resolved = []
    for q in qs:
        gold = []
        for needle in q["needles"]:
            hits = [m for m in mems if needle in m.get("cue", "")]
            if len(hits) == 0:
                problems.append("%s: 定位子串无命中 -> %r" % (q["id"], needle))
            elif len(hits) > 1:
                problems.append("%s: 定位子串命中 %d 条（须唯一）-> %r | %s" % (
                    q["id"], len(hits), needle,
                    " / ".join(h["id"][4:20] for h in hits)))
            else:
                gold.append(hits[0]["id"])
        resolved.append({"id": q["id"], "kind": q["kind"], "query": q["query"],
                         "gold": gold, "needles": q["needles"]})

    if problems:
        text = "定位失败（必须先修题源）：\n" + "\n".join("  " + p for p in problems)
        io.open(os.path.join(HERE, "build-report.txt"), "w", encoding="utf-8").write(text)
        print("FAIL: %d problems -> build-report.txt" % len(problems))
        return 1

    # ── 自查：paraphrase 题是否真的换了词 ──
    bm = BM25(mems)
    id2mem = dict((m["id"], m) for m in mems)
    leak_lines = []
    for q in resolved:
        if q["kind"] not in ("paraphrase", "prescriptive"):
            continue
        qtok = set(tokenize_query(q["query"]))
        for g in q["gold"]:
            gtok = set(tokenize_doc(doc_text(id2mem[g])))
            overlap = qtok & gtok
            strong = sorted(((bm.idf(t), t) for t in overlap), reverse=True)[:5]
            # IDF 越高越有判别力；高 IDF 重叠 = 出题时泄漏了原词
            leaky = [t for idf, t in strong if idf > 4.0]
            leak_lines.append("%-6s %-13s gold=%s 重叠%d 高判别力重叠=%s" % (
                q["id"], q["kind"], g[4:16], len(overlap),
                ",".join(leaky) if leaky else "无"))

    with io.open(os.path.join(HERE, "memory-golden.jsonl"), "w", encoding="utf-8") as f:
        for q in resolved:
            f.write(json.dumps({"id": q["id"], "kind": q["kind"], "query": q["query"],
                                "gold": q["gold"]}, ensure_ascii=False) + "\n")

    kinds = {}
    for q in resolved:
        kinds[q["kind"]] = kinds.get(q["kind"], 0) + 1

    rep = ["生成 memory-golden.jsonl：%d 题" % len(resolved),
           "kind 分布：" + "  ".join("%s=%d" % (k, v) for k, v in sorted(kinds.items())),
           "",
           "── paraphrase / prescriptive 换词自查（高判别力重叠 = 出题泄漏原词）──"]
    rep.extend(leak_lines)
    io.open(os.path.join(HERE, "build-report.txt"), "w", encoding="utf-8").write("\n".join(rep))
    print("OK: %d questions -> memory-golden.jsonl (see build-report.txt)" % len(resolved))
    return 0


if __name__ == "__main__":
    sys.exit(main())
