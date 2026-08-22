# -*- coding: utf-8 -*-
"""用**真实 Rust 二进制**复跑 golden set，验证 Python 离线复刻的结论确实迁移到了实现上。

score.py 里的 BM25 与分词是照着 Rust 源码逐字复刻的，但「复刻得对不对」本身需要证明——
两套分词一旦漂移，离线量出来的 MRR 就是空头支票。这里绕开所有复刻代码，
直接调 `engram recall --json`，用它真实返回的排序算指标。

同时对拍 `--lexical-legacy`（旧打分器），给出 Rust 侧的 现状 vs BM25 提升。
"""

import io
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ENGINE = os.path.dirname(HERE)
BIN = os.path.join(ENGINE, "target", "release", "engram.exe")
# 库路径：默认取用户级公共库；项目库用 ENGRAM_PROJECT_DB 指定（形如 name=path）。
# 两者都可用环境变量覆盖，别把本机绝对路径写死进仓库。
GENERAL = os.environ.get(
    "ENGRAM_GENERAL_DB",
    os.path.join(os.path.expanduser("~"), ".engram", "general.redb").replace("\\", "/"))
PROJECT = os.environ.get("ENGRAM_PROJECT_DB", "")
LIMIT = 10


def run_recall(query, legacy=False):
    """返回 (命中 id 列表, 是否弃权)。"""
    cmd = [BIN, "recall", "--general-db", GENERAL,
           "--query", query, "--limit", str(LIMIT), "--json", "--now", "1787408000"]
    if PROJECT:
        cmd[4:4] = ["--project-db", PROJECT]
    if legacy:
        cmd.append("--lexical-legacy")
    p = subprocess.run(cmd, capture_output=True)
    if p.returncode != 0:
        raise SystemExit("recall 失败: %s" % p.stderr.decode("utf-8", "replace")[:300])
    rows = json.loads(p.stdout.decode("utf-8"))
    ids = [r["id"] for r in rows if r.get("id")]
    abstained = any(r.get("abstained") for r in rows)
    return ids, abstained


def mrr_of(rank):
    return 1.0 / rank if rank else 0.0


def evaluate(questions, legacy):
    by_kind = {}
    for q in questions:
        ids, abstained = run_recall(q["query"], legacy)
        # 弃权 = 不返回任何结果，等价于全部未命中
        if abstained:
            ids = []
        gold = set(q["gold"])
        rank = None
        for pos, mid in enumerate(ids, start=1):
            if mid in gold:
                rank = pos
                break
        st = by_kind.setdefault(q["kind"], {"n": 0, "h1": 0, "h3": 0, "h8": 0, "mrr": 0.0, "abs": 0})
        st["n"] += 1
        if abstained:
            st["abs"] += 1
        if rank:
            if rank <= 1:
                st["h1"] += 1
            if rank <= 3:
                st["h3"] += 1
            if rank <= 8:
                st["h8"] += 1
            st["mrr"] += mrr_of(rank)
    return by_kind


def fmt(name, by_kind, kinds):
    lines = []
    tot = {"n": 0, "h1": 0, "h3": 0, "h8": 0, "mrr": 0.0, "abs": 0}
    for k in kinds:
        st = by_kind.get(k)
        if not st:
            continue
        for f in tot:
            tot[f] += st[f]
        lines.append("%-10s %-13s %4d %7s %7s %7s %8.3f %6d" % (
            name, k, st["n"], "%d/%d" % (st["h1"], st["n"]),
            "%d/%d" % (st["h3"], st["n"]), "%d/%d" % (st["h8"], st["n"]),
            st["mrr"] / st["n"], st["abs"]))
    lines.append("%-10s %-13s %4d %7s %7s %7s %8.3f %6d" % (
        name, "** 合计 **", tot["n"], "%d/%d" % (tot["h1"], tot["n"]),
        "%d/%d" % (tot["h3"], tot["n"]), "%d/%d" % (tot["h8"], tot["n"]),
        tot["mrr"] / tot["n"], tot["abs"]))
    return lines, tot["mrr"] / tot["n"] if tot["n"] else 0.0


def main():
    if not os.path.exists(BIN):
        raise SystemExit("找不到 release 二进制：%s（先 cargo build --release）" % BIN)
    qs = [json.loads(l) for l in io.open(os.path.join(HERE, "memory-golden.jsonl"), encoding="utf-8") if l.strip()]
    noise = [l.strip() for l in io.open(os.path.join(HERE, "noise-queries.txt"), encoding="utf-8")
             if l.strip() and not l.strip().startswith("#")]
    kinds = ["explicit", "paraphrase", "prescriptive", "implicit"]

    out = []
    out.append("用真实 Rust 二进制跑（%s）" % os.path.basename(BIN))
    out.append("库：公共库 + engram 项目库 | golden %d 题 | 噪声 %d 条 | limit=%d" % (len(qs), len(noise), LIMIT))
    out.append("")
    out.append("%-10s %-13s %4s %7s %7s %7s %8s %6s" % (
        "检索器", "kind", "n", "hit@1", "hit@3", "hit@8", "MRR", "弃权"))
    out.append("-" * 70)

    lg, mrr_legacy = fmt("legacy", evaluate(qs, True), kinds)
    out.extend(lg)
    out.append("")
    bm, mrr_bm25 = fmt("BM25", evaluate(qs, False), kinds)
    out.extend(bm)
    out.append("")
    out.append("总 MRR：legacy %.3f -> BM25 %.3f （%+.3f，%+.1f%%）" % (
        mrr_legacy, mrr_bm25, mrr_bm25 - mrr_legacy,
        (mrr_bm25 / mrr_legacy - 1) * 100 if mrr_legacy else 0.0))

    # 噪声弃权率
    out.append("")
    out.append("── 噪声 query 弃权率（%d 条）──" % len(noise))
    for label, legacy in (("legacy", True), ("BM25", False)):
        n_abs = 0
        leaked = []
        for nq in noise:
            ids, ab = run_recall(nq, legacy)
            if ab or not ids:
                n_abs += 1
            else:
                leaked.append(nq)
        out.append("%-8s 弃权 %d/%d = %.0f%%" % (label, n_abs, len(noise), n_abs * 100.0 / len(noise)))
        if not legacy and leaked:
            out.append("         漏网：" + " / ".join(x[:14] for x in leaked[:10]))

    text = "\n".join(out)
    io.open(os.path.join(HERE, "rust-verify.txt"), "w", encoding="utf-8").write(text)
    print("OK -> rust-verify.txt")


if __name__ == "__main__":
    main()
