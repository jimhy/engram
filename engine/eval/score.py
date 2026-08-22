# -*- coding: utf-8 -*-
"""engram 记忆检索评测：现状词法打分 vs BM25 对拍。

分词部分**逐字复刻** engine/src/commands.rs 的 is_cjk / segment_field / tokenize_query。
两套分词一旦漂移，这里量出来的结论就迁移不到 Rust 实现上——那是最隐蔽的坑，
所以下面每个函数都标注了它对应的 Rust 源码位置，改任何一边都要同步改另一边。

用法：
    python score.py                    # 跑对拍，输出分 kind 报告
    python score.py --abstain          # 额外跑弃权阈值标定（需 noise-queries.txt）
    python score.py --detail           # 输出 per-question 明细
"""

import io
import json
import math
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))

# ────────────────────────── 分词（对应 commands.rs） ──────────────────────────


def is_cjk(ch):
    """对应 commands.rs 的 `fn is_cjk(c: char) -> bool`。

    注意中文标点（，。：「」）**不在**这些区间里，它们会被归进「非 CJK 段」，
    再因为不含字母数字而被 segment_field 丢弃。
    """
    c = ord(ch)
    return (
        0x3040 <= c <= 0x30FF      # 日文平假名 / 片假名
        or 0x3400 <= c <= 0x4DBF   # CJK 扩展 A
        or 0x4E00 <= c <= 0x9FFF   # CJK 基本区
        or 0xF900 <= c <= 0xFAFF   # CJK 兼容表意文字
        or 0x20000 <= c <= 0x2A6DF  # CJK 扩展 B
    )


def segment_field(field):
    """对应 commands.rs 的 `fn segment_field(field: &str) -> Vec<String>`。

    把一个**不含空白**的片段切成词元：先按「是否 is_cjk」切成交替的连续段，再分别处理——
    CJK 段长度 >= 2 出相邻二元 ngram；长度 = 1 取该字本身；
    非 CJK 段原样保留，但不含任何字母数字的纯标点段直接丢弃。

    返回已小写化、**未去重**（去重在 tokenize_query 里做，BM25 侧则要保留词频）。
    """
    chars = list(field)
    out = []
    i = 0
    n = len(chars)
    while i < n:
        cjk = is_cjk(chars[i])
        start = i
        while i < n and is_cjk(chars[i]) == cjk:
            i += 1
        seg = chars[start:i]
        if cjk:
            if len(seg) < 2:
                out.append("".join(seg).lower())
            else:
                for k in range(len(seg) - 1):
                    out.append("".join(seg[k:k + 2]).lower())
        elif any(c.isalnum() for c in seg):
            out.append("".join(seg).lower())
    return out


def tokenize_query(query):
    """对应 commands.rs 的 `pub fn tokenize_query(query: &str) -> Vec<String>`。

    按空白切片段，片段内过 segment_field，再按首次出现序去重。
    """
    seen = []
    for field in query.split():
        for tok in segment_field(field):
            if tok and tok not in seen:
                seen.append(tok)
    return seen


def tokenize_doc(text):
    """文档侧分词：与 query 侧同款规则，但**保留重复**（BM25 要词频）。

    现状的 score_query 对文档根本不分词（直接子串匹配），所以这个函数是 BM25 独有的。
    """
    out = []
    for field in text.split():
        out.extend(segment_field(field))
    return out


def doc_text(mem):
    """一条记忆参与检索的文本：cue + tags，与现状 score_query 的 haystack 口径一致。"""
    text = mem.get("cue", "")
    tags = mem.get("tags") or []
    if tags:
        text = text + " " + " ".join(tags)
    return text


# ────────────────────────── 现状打分（对应 commands.rs score_query） ──────────────────────────


def score_query(mem, tokens):
    """对应 commands.rs 的 `pub fn score_query(m: &Memory, tokens: &[String]) -> f64`。

    命中的**不同**词元数 ÷ query 词元总数。子串匹配、对文档不分词。
    """
    if not tokens:
        return 0.0
    haystack = mem.get("cue", "").lower()
    tags = mem.get("tags") or []
    if tags:
        haystack = haystack + " " + " ".join(tags).lower()
    hits = sum(1 for t in tokens if t in haystack)
    return hits / float(len(tokens))


# ────────────────────────── BM25 ──────────────────────────

BM25_K1 = 1.2
BM25_B = 0.75


class BM25(object):
    """Okapi BM25。

        score(D,Q) = sum_t IDF(t) * ( f(t,D)*(k1+1) ) / ( f(t,D) + k1*(1 - b + b*|D|/avgdl) )
        IDF(t)     = ln( 1 + (N - n(t) + 0.5) / (n(t) + 0.5) )

    IDF 用的是这个 +0.5 平滑形式，恒为正（不会像经典 Robertson-Sparck-Jones 那样
    在 n(t) > N/2 时翻负）。这一点对弃权判据很重要：分数没有负数干扰，零点才有意义。
    """

    def __init__(self, mems):
        self.mems = mems
        self.docs = [tokenize_doc(doc_text(m)) for m in mems]
        self.dl = [len(d) for d in self.docs]
        self.N = len(self.docs)
        self.avgdl = (sum(self.dl) / float(self.N)) if self.N else 0.0
        self.tf = []
        df = {}
        for d in self.docs:
            counter = {}
            for t in d:
                counter[t] = counter.get(t, 0) + 1
            self.tf.append(counter)
            for t in counter:
                df[t] = df.get(t, 0) + 1
        self.df = df

    def idf(self, token):
        n = self.df.get(token, 0)
        return math.log(1.0 + (self.N - n + 0.5) / (n + 0.5))

    def score(self, idx, tokens):
        if not tokens or self.N == 0 or self.avgdl <= 0.0:
            return 0.0
        tf = self.tf[idx]
        dl = self.dl[idx]
        total = 0.0
        for t in tokens:
            f = tf.get(t, 0)
            if f == 0:
                continue
            denom = f + BM25_K1 * (1.0 - BM25_B + BM25_B * dl / self.avgdl)
            total += self.idf(t) * (f * (BM25_K1 + 1.0)) / denom
        return total

    def hit_tokens(self, idx, tokens):
        """该文档命中了哪些 query 词元，各自 IDF 多少——弃权判据与归因都要用。"""
        tf = self.tf[idx]
        out = []
        for t in tokens:
            if tf.get(t, 0) > 0:
                out.append((t, self.idf(t)))
        out.sort(key=lambda x: -x[1])
        return out


# ────────────────────────── 检索器 ──────────────────────────


def rank_current(mems, query, limit=10):
    """现状：score_query 降序，丢 score <= 0。"""
    tokens = tokenize_query(query)
    scored = []
    for i, m in enumerate(mems):
        s = score_query(m, tokens)
        if s > 0.0:
            scored.append((s, i))
    scored.sort(key=lambda x: -x[0])
    return [(mems[i]["id"], s) for s, i in scored[:limit]]


def rank_bm25(bm, mems, query, limit=10):
    tokens = tokenize_query(query)
    scored = []
    for i in range(len(mems)):
        s = bm.score(i, tokens)
        if s > 0.0:
            scored.append((s, i))
    scored.sort(key=lambda x: -x[0])
    return [(mems[i]["id"], s) for s, i in scored[:limit]]


# ────────────────────────── 评测 ──────────────────────────


def first_hit_rank(ranked, gold):
    """gold 中任一条首次出现的名次（1-based），没命中返回 None。"""
    gold = set(gold)
    for pos, (mid, _s) in enumerate(ranked, start=1):
        if mid in gold:
            return pos
    return None


def evaluate(name, ranker, questions, detail=False):
    by_kind = {}
    rows = []
    for q in questions:
        ranked = ranker(q["query"])
        r = first_hit_rank(ranked, q["gold"])
        k = q["kind"]
        st = by_kind.setdefault(k, {"n": 0, "h1": 0, "h3": 0, "h8": 0, "mrr": 0.0})
        st["n"] += 1
        if r is not None:
            if r <= 1:
                st["h1"] += 1
            if r <= 3:
                st["h3"] += 1
            if r <= 8:
                st["h8"] += 1
            st["mrr"] += 1.0 / r
        rows.append((q["id"], k, r, ranked[:3]))
    return {"name": name, "by_kind": by_kind, "rows": rows}


def fmt_report(res_list, kinds):
    lines = []
    head = "%-14s %-13s %6s %6s %6s %6s %8s" % ("检索器", "kind", "n", "hit@1", "hit@3", "hit@8", "MRR")
    lines.append(head)
    lines.append("-" * len(head))
    for res in res_list:
        tot = {"n": 0, "h1": 0, "h3": 0, "h8": 0, "mrr": 0.0}
        for k in kinds:
            st = res["by_kind"].get(k)
            if not st:
                continue
            for f in tot:
                tot[f] += st[f]
            lines.append("%-14s %-13s %6d %6s %6s %6s %8.3f" % (
                res["name"], k, st["n"],
                "%d/%d" % (st["h1"], st["n"]),
                "%d/%d" % (st["h3"], st["n"]),
                "%d/%d" % (st["h8"], st["n"]),
                st["mrr"] / st["n"] if st["n"] else 0.0,
            ))
        lines.append("%-14s %-13s %6d %6s %6s %6s %8.3f" % (
            res["name"], "** 合计 **", tot["n"],
            "%d/%d" % (tot["h1"], tot["n"]),
            "%d/%d" % (tot["h3"], tot["n"]),
            "%d/%d" % (tot["h8"], tot["n"]),
            tot["mrr"] / tot["n"] if tot["n"] else 0.0,
        ))
        lines.append("")
    return "\n".join(lines)


def percentile(sorted_vals, p):
    if not sorted_vals:
        return 0.0
    k = (len(sorted_vals) - 1) * (p / 100.0)
    lo = int(math.floor(k))
    hi = int(math.ceil(k))
    if lo == hi:
        return sorted_vals[lo]
    return sorted_vals[lo] * (hi - k) + sorted_vals[hi] * (k - lo)


def abstain_calibration(bm, mems, questions, noise_queries):
    """E3：标定弃权阈值。

    比较「golden set 真答案的 BM25 分数」与「纯噪声 query 的最高分」两个分布，
    看它们之间有没有可分的间隔。
    """
    gold_top = []      # 每题命中 gold 的那条的分数（没命中就跳过）
    gold_best = []     # 每题的 top1 分数（无论对错）
    for q in questions:
        tokens = tokenize_query(q["query"])
        gset = set(q["gold"])
        best = 0.0
        gbest = 0.0
        for i, m in enumerate(mems):
            s = bm.score(i, tokens)
            if s > best:
                best = s
            if m["id"] in gset and s > gbest:
                gbest = s
        gold_best.append(best)
        if gbest > 0.0:
            gold_top.append(gbest)

    noise_top = []
    noise_detail = []
    for nq in noise_queries:
        tokens = tokenize_query(nq)
        best = 0.0
        besti = -1
        for i in range(len(mems)):
            s = bm.score(i, tokens)
            if s > best:
                best = s
                besti = i
        noise_top.append(best)
        noise_detail.append((nq, best, mems[besti]["id"] if besti >= 0 else None))

    gold_top.sort()
    noise_top.sort()
    return {
        "gold_top": gold_top,
        "gold_best": sorted(gold_best),
        "noise_top": noise_top,
        "noise_detail": noise_detail,
        "gold_p10": percentile(gold_top, 10),
        "gold_p25": percentile(gold_top, 25),
        "noise_p90": percentile(noise_top, 90),
        "noise_max": noise_top[-1] if noise_top else 0.0,
    }


def main():
    detail = "--detail" in sys.argv
    do_abstain = "--abstain" in sys.argv

    mems = json.load(io.open(os.path.join(HERE, "corpus.json"), encoding="utf-8"))
    qs = []
    gpath = os.path.join(HERE, "memory-golden.jsonl")
    for line in io.open(gpath, encoding="utf-8"):
        line = line.strip()
        if line and not line.startswith("//"):
            qs.append(json.loads(line))

    # gold id 存在性自查——出题时写错 id 会让整把尺子失真且不报错
    ids = set(m["id"] for m in mems)
    bad = []
    for q in qs:
        for g in q["gold"]:
            if g not in ids:
                bad.append((q["id"], g))
    if bad:
        out = ["gold id 不存在于语料（出题错误，必须先修）："]
        for qid, g in bad:
            out.append("  %s -> %s" % (qid, g))
        io.open(os.path.join(HERE, "report.txt"), "w", encoding="utf-8").write("\n".join(out))
        print("FAIL: %d bad gold ids, see report.txt" % len(bad))
        return 1

    bm = BM25(mems)
    kinds = ["explicit", "paraphrase", "prescriptive", "implicit"]

    res_cur = evaluate("现状词法", lambda q: rank_current(mems, q), qs, detail)
    res_bm = evaluate("BM25", lambda q: rank_bm25(bm, mems, q), qs, detail)

    lines = []
    lines.append("语料 %d 条 | 题 %d 道 | avgdl=%.1f 词元 | k1=%.1f b=%.2f"
                 % (len(mems), len(qs), bm.avgdl, BM25_K1, BM25_B))
    lines.append("")
    lines.append(fmt_report([res_cur, res_bm], kinds))

    if detail:
        lines.append("")
        lines.append("── per-question 明细（名次，None=未命中）──")
        lines.append("%-6s %-13s %8s %8s" % ("题", "kind", "现状", "BM25"))
        cur_map = dict((r[0], r[2]) for r in res_cur["rows"])
        bm_map = dict((r[0], r[2]) for r in res_bm["rows"])
        for q in qs:
            lines.append("%-6s %-13s %8s %8s" % (
                q["id"], q["kind"], str(cur_map.get(q["id"])), str(bm_map.get(q["id"]))))

    if do_abstain:
        npath = os.path.join(HERE, "noise-queries.txt")
        noise = [l.strip() for l in io.open(npath, encoding="utf-8")
                 if l.strip() and not l.strip().startswith("#")]
        cal = abstain_calibration(bm, mems, qs, noise)
        lines.append("")
        lines.append("── E3 弃权阈值标定（BM25 标度）──")
        lines.append("真答案分数   n=%d  min=%.3f  P10=%.3f  P25=%.3f  median=%.3f  max=%.3f" % (
            len(cal["gold_top"]), cal["gold_top"][0], cal["gold_p10"], cal["gold_p25"],
            percentile(cal["gold_top"], 50), cal["gold_top"][-1]))
        lines.append("噪声 top1   n=%d  min=%.3f  median=%.3f  P90=%.3f  max=%.3f" % (
            len(cal["noise_top"]), cal["noise_top"][0], percentile(cal["noise_top"], 50),
            cal["noise_p90"], cal["noise_max"]))
        gap = cal["gold_p10"] / cal["noise_p90"] if cal["noise_p90"] > 0 else float("inf")
        lines.append("间隔比 (真答案P10 / 噪声P90) = %.2f 倍   %s" % (
            gap, "可设绝对下限" if gap > 1.5 else "重叠严重，绝对阈值不可靠，须用结构判据"))
        lines.append("")
        lines.append("噪声 query 命中最高分明细（前 15）：")
        for nq, s, mid in sorted(cal["noise_detail"], key=lambda x: -x[1])[:15]:
            lines.append("  %.3f  %-28s  %s" % (s, nq[:28], mid))

    text = "\n".join(lines)
    io.open(os.path.join(HERE, "report.txt"), "w", encoding="utf-8").write(text)
    print("OK -> report.txt (%d lines)" % len(lines))
    return 0


if __name__ == "__main__":
    sys.exit(main())
