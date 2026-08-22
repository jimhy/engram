# -*- coding: utf-8 -*-
"""写入端干预的价值验证：给 cue 补上「现象侧措辞」能救回多少题。

前面四条检索侧路线全部实测否掉：
  向量 hybrid  +0pp（vector_probe.py）
  图谱扩散     -0.001（graph_probe.py）
  unigram      MRR +0.023 但噪声弃权 82%->50%，不划算（unigram_probe.py）
  打分调参     无效——诊断显示 12 道未命中 100% 是硬鸿沟，没有共同词可调（diagnose.py）

只剩最后一条：**改数据本身**。记忆全由复盘者写、查询全由用户提，
两边措辞来自两个不同的过程——这是硬鸿沟的结构性根因。
本轮已经把这条指导写进 reviewer-prompt.md（对未来新写的记忆生效），
但存量 328 条怎么办？先量一量补齐值多少，再决定要不要花这个功夫。

做法：给那 12 道未命中题的 gold 记忆，各补一句「遇到这个问题的人会怎么描述现象」，
其余记忆一字不动，然后重跑全部 50 题 + 50 条噪声。

## 过拟合风险（必须如实标注）

我是**看过 query 之后**写这些补充的，存在照着答案改的嫌疑。三条缓解：
  1. 写的时候按「复盘者当时会怎么补充」来写通用现象描述，不刻意套 query 的词；
  2. 脚本会算出补充文本与对应 query 的词元重叠率并报告——重叠越高，过拟合嫌疑越大；
  3. 同时跑另外 38 道**没有动过 gold** 的题与 50 条噪声，检验副作用。
所以本实验的正确读法是：它验证「补现象侧措辞」这个**机制**的上限，
不能当作「随便补都有这个效果」的证据。
"""

import io
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from score import BM25, tokenize_query, tokenize_doc, doc_text  # noqa: E402

LIMIT = 10
A_INFO, A_HIT = 2.05, 1.20

# 定位子串 -> 要追加到 cue 后面的「现象侧措辞」。
# 写法要求：站在**遇到问题的人**的角度描述症状，而不是复盘者的事后归因。
PATCH_FILE = "writeside-patches.txt"


def load_patches():
    """补丁表放外部文件：它含 cue 定位子串，会带出具体记忆库的内容，
    因此**不进公开仓库**（见 .gitignore）。每行格式：

        cue 定位子串 | 要追加的现象侧措辞
    """
    p = os.path.join(HERE, PATCH_FILE)
    if not os.path.exists(p):
        raise SystemExit("缺少补丁表 %s，它针对你自己的记忆库编写，见 README.md。" % PATCH_FILE)
    out = []
    for line in io.open(p, encoding="utf-8"):
        line = line.strip()
        if line and not line.startswith("#") and "|" in line:
            a, b = line.split("|", 1)
            out.append((a.strip(), b.strip()))
    return out


PATCHES = []


def apply_patches(mems):
    patched, hit_ids = [], {}
    for m in mems:
        m2 = dict(m)
        for needle, extra in PATCHES:
            if needle in m["cue"]:
                m2["cue"] = m["cue"] + " " + extra
                hit_ids[m["id"]] = extra
                break
        patched.append(m2)
    return patched, hit_ids


def rank(bm, mems, query, limit=LIMIT):
    toks = tokenize_query(query)
    sc = [(i, bm.score(i, toks)) for i in range(len(mems))]
    sc = [(i, s) for i, s in sc if s > 0]
    sc.sort(key=lambda x: -x[1])
    return sc[:limit], toks


def first_rank(ranked, mems, gold):
    g = set(gold)
    for pos, (i, _s) in enumerate(ranked, start=1):
        if mems[i]["id"] in g:
            return pos
    return None


def idf_max(bm):
    """score.py 的 BM25 没有这两个辅助量（它们是后来加在 Rust 侧的），这里就地补。"""
    import math
    if bm.N == 0:
        return 1.0
    return math.log(1.0 + (bm.N - 1 + 0.5) / 1.5)


def query_info(bm, toks):
    if not toks:
        return 0.0
    return sum(bm.idf(t) for t in toks) / idf_max(bm)


def abstains(bm, mems, text):
    ranked, toks = rank(bm, mems, text, limit=1)
    if not toks or not ranked:
        return True
    if bm.N < 80:
        return False
    if query_info(bm, toks) < A_INFO:
        return True
    return (ranked[0][1] / idf_max(bm)) < A_HIT


def evaluate(mems, qs, noise, kinds):
    bm = BM25(mems)
    by, ranks = {}, {}
    for q in qs:
        r, _ = rank(bm, mems, q["query"])
        fr = first_rank(r, mems, q["gold"])
        by.setdefault(q["kind"], []).append(fr)
        ranks[q["id"]] = fr
    tot = {"n": 0, "h1": 0, "h3": 0, "mrr": 0.0}
    per = {}
    for k in kinds:
        rs = by.get(k, [])
        if not rs:
            continue
        st = {"n": len(rs), "h1": sum(1 for r in rs if r and r <= 1),
              "h3": sum(1 for r in rs if r and r <= 3),
              "mrr": sum(1.0 / r for r in rs if r)}
        per[k] = st
        for f in tot:
            tot[f] += st[f]
    na = sum(1 for x in noise if abstains(bm, mems, x)) * 100.0 / len(noise)
    ga = sum(1 for q in qs if abstains(bm, mems, q["query"])) * 100.0 / len(qs)
    return per, tot, na, ga, ranks


def main():
    mems = json.load(io.open(os.path.join(HERE, "corpus.json"), encoding="utf-8"))
    qs = [json.loads(l) for l in io.open(os.path.join(HERE, "memory-golden.jsonl"),
                                         encoding="utf-8") if l.strip()]
    noise = [l.strip() for l in io.open(os.path.join(HERE, "noise-queries.txt"), encoding="utf-8")
             if l.strip() and not l.strip().startswith("#")]
    kinds = ["explicit", "paraphrase", "prescriptive", "implicit"]

    global PATCHES
    PATCHES = load_patches()
    patched, hit_ids = apply_patches(mems)
    touched = set(hit_ids)

    out = []
    out.append("写入端干预价值验证｜语料 %d 条｜题 %d 道｜噪声 %d 条" % (len(mems), len(qs), len(noise)))
    out.append("补了 %d/%d 条记忆的 cue（只补未命中题的 gold，其余一字未动）"
               % (len(touched), len(PATCHES)))
    if len(touched) != len(PATCHES):
        miss = [n for n, _ in PATCHES if not any(n in m["cue"] for m in mems)]
        out.append("  !! 有定位子串没匹配上：%s" % miss)
    out.append("")

    per0, tot0, na0, ga0, r0 = evaluate(mems, qs, noise, kinds)
    per1, tot1, na1, ga1, r1 = evaluate(patched, qs, noise, kinds)

    hdr = "%-16s %9s %9s %9s %9s %8s %10s %7s" % (
        "配置", "explicit", "parap", "prescr", "implic", "总MRR", "噪声弃权", "误弃")
    out.append(hdr)
    out.append("-" * len(hdr))
    for label, per, tot, na, ga in (("补之前", per0, tot0, na0, ga0),
                                    ("补之后", per1, tot1, na1, ga1)):
        m = {k: (per[k]["mrr"] / per[k]["n"] if k in per else 0.0) for k in kinds}
        out.append("%-16s %9.3f %9.3f %9.3f %9.3f %8.3f %9.0f%% %6.0f%%" % (
            label, m["explicit"], m["paraphrase"], m["prescriptive"], m["implicit"],
            tot["mrr"] / tot["n"], na, ga))
    out.append("")
    out.append("总 MRR %.3f -> %.3f（%+.3f）｜噪声弃权 %.0f%% -> %.0f%%（%+.0fpp）｜误弃 %.0f%% -> %.0f%%" % (
        tot0["mrr"] / tot0["n"], tot1["mrr"] / tot1["n"],
        tot1["mrr"] / tot1["n"] - tot0["mrr"] / tot0["n"], na0, na1, na1 - na0, ga0, ga1))
    out.append("")

    # 被补的 12 题
    out.append("── 被补 gold 的题：名次变化 ──")
    saved = 0
    gold_of_patched = [q for q in qs if any(g in touched for g in q["gold"])]
    for q in gold_of_patched:
        a, b = r0.get(q["id"]), r1.get(q["id"])
        mark = ""
        if b and not a:
            mark = "  救回"
            saved += 1
        elif a and b and b < a:
            mark = "  变好"
        elif a and not b:
            mark = "  !! 变坏"
        out.append("  %-6s %-13s %6s -> %-6s%s  |  %s" % (
            q["id"], q["kind"], str(a), str(b), mark, q["query"][:26]))
    out.append("  救回 %d 道" % saved)
    out.append("")

    # 副作用：没被补 gold 的那些题
    out.append("── 副作用检查：gold 未被补的其余题 ──")
    worse = []
    for q in qs:
        if any(g in touched for g in q["gold"]):
            continue
        a, b = r0.get(q["id"]), r1.get(q["id"])
        if (a and not b) or (a and b and b > a):
            worse.append((q["id"], q["kind"], a, b))
    if worse:
        for qid, k, a, b in worse:
            out.append("  %-6s %-13s %6s -> %-6s  变差" % (qid, k, str(a), str(b)))
    else:
        out.append("  无一变差")
    out.append("")

    # 过拟合自查：补充文本与对应 query 的词元重叠
    out.append("── 过拟合自查：补充文本与对应 query 的词元重叠 ──")
    bmp = BM25(patched)
    id2q = {}
    for q in qs:
        for g in q["gold"]:
            if g in touched:
                id2q.setdefault(g, []).append(q)
    rows = []
    for mid, extra in hit_ids.items():
        etoks = set(tokenize_doc(extra))
        for q in id2q.get(mid, []):
            qtoks = set(tokenize_query(q["query"]))
            ov = etoks & qtoks
            strong = sorted((bmp.idf(t), t) for t in ov)[::-1][:4]
            rows.append((q["id"], len(ov), [t for _i, t in strong if _i >= 4.0]))
    rows.sort()
    for qid, n, strong in rows:
        out.append("  %-6s 重叠 %2d 个词元，其中判别性强的：%s" % (
            qid, n, ",".join(strong) if strong else "（无）"))
    avg = sum(r[1] for r in rows) / float(len(rows)) if rows else 0
    out.append("  平均重叠 %.1f 个词元——**这个数越高，本实验的过拟合嫌疑越大**" % avg)

    io.open(os.path.join(HERE, "writeside-probe.txt"), "w", encoding="utf-8").write("\n".join(out))
    print("OK -> writeside-probe.txt")


if __name__ == "__main__":
    main()
