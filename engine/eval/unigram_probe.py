# -*- coding: utf-8 -*-
"""给中文分词补 unigram（单字）作低权重词元，量它能救回多少「假硬鸿沟」。

诊断（diagnose.py）发现：12 道未命中题 100% 是「硬鸿沟」——query 与 gold 没有
判别性的共同词元。但逐题看，这里面混着两种完全不同的病：

  真鸿沟   「我传的路径怎么老是找不到文件」 vs 「反斜杠…被当成驱动器相对路径」
           用词根本不同，只能靠改写入端（让 cue 带上现象侧措辞）解决。

  假鸿沟   「准备发一个新版本」 vs 「发版铁律·适配器仓库必须先推」
           `发版` 这个词明明就在里面，但二元 ngram 切出来是
           准备/备发/发一/一个/个新/新版/版本  vs  发版/版铁/铁律
           **零交集**——词边界没对齐，bigram 全部错位。这是分词粒度问题，
           不用动记忆就能治：补上单字，两边就能在 `发`/`版` 上对上。

做法：在现有 bigram 之外，对每个 CJK 字额外产出一个 unigram 词元（打 `u:` 前缀
独立参与 df 统计），BM25 打分时该项乘以权重 w_uni < 1——单字判别力弱，
不能与 bigram 同权，否则噪声会淹没信号。

要同时盯三个指标，任何一个塌了这条路就不成立：
  1. paraphrase / prescriptive 是否真的提升（目标）
  2. explicit 是否被损害（不能为了救长尾把原本对的搞坏）
  3. 噪声弃权率是否下降（单字更容易蹭上，弃权判据可能被冲垮）
"""

import io
import json
import math
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from score import is_cjk, segment_field, doc_text, percentile  # noqa: E402

UNI = "u:"
LIMIT = 10


def seg_ext(field, with_uni):
    """在 segment_field 的结果上，可选地追加每个 CJK 字的 unigram（带 u: 前缀）。"""
    out = list(segment_field(field))
    if not with_uni:
        return out
    for ch in field:
        if is_cjk(ch):
            out.append(UNI + ch.lower())
    return out


def tok_doc(text, with_uni):
    out = []
    for f in text.split():
        out.extend(seg_ext(f, with_uni))
    return out


def tok_query(q, with_uni):
    seen = []
    for f in q.split():
        for t in seg_ext(f, with_uni):
            if t and t not in seen:
                seen.append(t)
    return seen


class BM25U(object):
    """BM25，unigram 词元按 w_uni 折算权重。"""

    K1 = 1.2
    B = 0.75

    def __init__(self, mems, with_uni, w_uni=1.0):
        self.w_uni = w_uni
        self.docs = [tok_doc(doc_text(m), with_uni) for m in mems]
        self.dl = [len(d) for d in self.docs]
        self.N = len(self.docs)
        self.avgdl = sum(self.dl) / float(self.N) if self.N else 0.0
        self.tf = []
        df = {}
        for d in self.docs:
            c = {}
            for t in d:
                c[t] = c.get(t, 0) + 1
            self.tf.append(c)
            for t in c:
                df[t] = df.get(t, 0) + 1
        self.df = df

    def idf(self, t):
        n = self.df.get(t, 0)
        return math.log(1.0 + (self.N - n + 0.5) / (n + 0.5))

    def idf_max(self):
        if self.N == 0:
            return 1.0
        return math.log(1.0 + (self.N - 1 + 0.5) / 1.5)

    def score(self, i, toks):
        if not toks or self.N == 0 or self.avgdl <= 0:
            return 0.0
        tf, dl = self.tf[i], self.dl[i]
        tot = 0.0
        for t in toks:
            f = tf.get(t, 0)
            if f == 0:
                continue
            denom = f + self.K1 * (1.0 - self.B + self.B * dl / self.avgdl)
            v = self.idf(t) * (f * (self.K1 + 1.0)) / denom
            if t.startswith(UNI):
                v *= self.w_uni
            tot += v
        return tot

    def query_info(self, toks):
        if not toks:
            return 0.0
        s = 0.0
        for t in toks:
            v = self.idf(t)
            if t.startswith(UNI):
                v *= self.w_uni
            s += v
        return s / self.idf_max()

    def rank(self, mems, query, with_uni, limit=LIMIT):
        toks = tok_query(query, with_uni)
        sc = [(i, self.score(i, toks)) for i in range(len(mems))]
        sc = [(i, s) for i, s in sc if s > 0]
        sc.sort(key=lambda x: -x[1])
        return sc[:limit], toks

    def top1(self, mems, query, with_uni):
        sc, toks = self.rank(mems, query, with_uni, limit=1)
        return (sc[0][1] if sc else 0.0), toks


def first_rank(ranked, mems, gold):
    g = set(gold)
    for pos, (i, _s) in enumerate(ranked, start=1):
        if mems[i]["id"] in g:
            return pos
    return None


def run(mems, qs, noise, with_uni, w_uni, kinds, abst_info, abst_hit):
    bm = BM25U(mems, with_uni, w_uni)
    by = {}
    for q in qs:
        r, _ = bm.rank(mems, q["query"], with_uni)
        by.setdefault(q["kind"], []).append(first_rank(r, mems, q["gold"]))
    tot = {"n": 0, "h1": 0, "h3": 0, "mrr": 0.0}
    per = {}
    for k in kinds:
        rs = by.get(k, [])
        if not rs:
            continue
        st = {"n": len(rs),
              "h1": sum(1 for r in rs if r and r <= 1),
              "h3": sum(1 for r in rs if r and r <= 3),
              "mrr": sum(1.0 / r for r in rs if r)}
        per[k] = st
        for f in tot:
            tot[f] += st[f]

    # 弃权率：沿用 Rust 侧同款两条闸
    def abstains(text):
        best, toks = bm.top1(mems, text, with_uni)
        if not toks or best <= 0:
            return True
        if bm.N < 80:
            return False
        if bm.query_info(toks) < abst_info:
            return True
        return (best / bm.idf_max()) < abst_hit

    n_noise = sum(1 for x in noise if abstains(x))
    n_wrong = sum(1 for q in qs if abstains(q["query"]))
    return per, tot, n_noise * 100.0 / len(noise), n_wrong * 100.0 / len(qs)


def main():
    mems = json.load(io.open(os.path.join(HERE, "corpus.json"), encoding="utf-8"))
    qs = [json.loads(l) for l in io.open(os.path.join(HERE, "memory-golden.jsonl"),
                                         encoding="utf-8") if l.strip()]
    noise = [l.strip() for l in io.open(os.path.join(HERE, "noise-queries.txt"), encoding="utf-8")
             if l.strip() and not l.strip().startswith("#")]
    kinds = ["explicit", "paraphrase", "prescriptive", "implicit"]
    A_INFO, A_HIT = 2.05, 1.20

    out = []
    out.append("unigram 补充实验｜语料 %d 条｜题 %d 道｜噪声 %d 条" % (len(mems), len(qs), len(noise)))
    out.append("弃权判据沿用 Rust 侧参数：query_info>=%.2f 且 hit>=%.2f，语料 <80 不做统计弃权"
               % (A_INFO, A_HIT))
    out.append("")
    hdr = "%-14s %8s %8s %8s %8s %8s %10s %8s" % (
        "配置", "explicit", "parap", "prescr", "implic", "总MRR", "噪声弃权", "误弃")
    out.append(hdr)
    out.append("-" * len(hdr))

    rows = []
    base = None
    for label, wu, wuni in [("bigram(现状)", False, 1.0)] + \
                           [("+uni w=%.2f" % w, True, w) for w in (0.15, 0.25, 0.35, 0.5, 0.75, 1.0)]:
        per, tot, na, ga = run(mems, qs, noise, wu, wuni, kinds, A_INFO, A_HIT)
        mrrs = {k: (per[k]["mrr"] / per[k]["n"] if k in per else 0.0) for k in kinds}
        total_mrr = tot["mrr"] / tot["n"]
        rows.append((label, mrrs, total_mrr, na, ga, per))
        if base is None:
            base = (mrrs, total_mrr, na, ga, per)
        out.append("%-14s %8.3f %8.3f %8.3f %8.3f %8.3f %9.0f%% %7.0f%%" % (
            label, mrrs["explicit"], mrrs["paraphrase"], mrrs["prescriptive"],
            mrrs["implicit"], total_mrr, na, ga))

    out.append("")
    b_mrrs, b_tot, b_na, b_ga, b_per = base
    out.append("── 相对现状的变化 ──")
    for label, mrrs, total_mrr, na, ga, per in rows[1:]:
        out.append("%-14s 总MRR %+.3f | explicit %+.3f | parap %+.3f | prescr %+.3f | 噪声弃权 %+.0fpp | 误弃 %+.0fpp" % (
            label, total_mrr - b_tot, mrrs["explicit"] - b_mrrs["explicit"],
            mrrs["paraphrase"] - b_mrrs["paraphrase"],
            mrrs["prescriptive"] - b_mrrs["prescriptive"], na - b_na, ga - b_ga))

    # 挑一个综合最好的：总 MRR 最高，且 explicit 不劣化、误弃不增
    ok = [r for r in rows[1:]
          if r[1]["explicit"] >= b_mrrs["explicit"] - 1e-9 and r[4] <= b_ga + 1e-9]
    out.append("")
    if ok:
        best = max(ok, key=lambda r: r[2])
        out.append("满足「explicit 不劣化 且 误弃不增」的最优配置：%s" % best[0])
        out.append("  总 MRR %.3f -> %.3f（%+.3f）" % (b_tot, best[2], best[2] - b_tot))
        out.append("  噪声弃权 %.0f%% -> %.0f%%（%+.0fpp）" % (b_na, best[3], best[3] - b_na))
        verdict = "值得做" if (best[2] - b_tot) >= 0.02 else "提升太小，不值得"
        out.append("  结论：%s" % verdict)
    else:
        out.append("没有任何权重同时满足「explicit 不劣化 且 误弃不增」——unigram 这条路不成立。")

    # 逐题：原本未命中的 12 题救回了几道
    out.append("")
    out.append("── 原未命中的 12 题，在最优 unigram 配置下的名次 ──")
    if ok:
        best = max(ok, key=lambda r: r[2])
        wu = float(best[0].split("=")[1]) if "=" in best[0] else 1.0
        bmu = BM25U(mems, True, wu)
        bm0 = BM25U(mems, False, 1.0)
        miss_ids = ["P-01", "P-02", "P-05", "P-06", "P-07", "P-08",
                    "P-14", "P-16", "P-18", "R-03", "R-07", "I-01"]
        saved = 0
        for q in qs:
            if q["id"] not in miss_ids:
                continue
            r0, _ = bm0.rank(mems, q["query"], False)
            r1, _ = bmu.rank(mems, q["query"], True)
            a = first_rank(r0, mems, q["gold"])
            b = first_rank(r1, mems, q["gold"])
            if b and not a:
                saved += 1
            out.append("  %-6s %-13s %6s -> %-6s %s  |  %s" % (
                q["id"], q["kind"], str(a), str(b),
                "救回" if (b and not a) else "", q["query"][:26]))
        out.append("  共救回 %d/12 道" % saved)

    io.open(os.path.join(HERE, "unigram-probe.txt"), "w", encoding="utf-8").write("\n".join(out))
    print("OK -> unigram-probe.txt")


if __name__ == "__main__":
    main()
