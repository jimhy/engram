# -*- coding: utf-8 -*-
"""E1 补做：量一量向量检索到底值不值那 186MB。

背景：Phase 1 的 BM25 是零依赖拿到的。要不要再上向量（engram-kb sidecar：
195MB 二进制 + 95MB 模型，外加 redb 锁 / 备份导出丢向量 / 部分平台装不了
这一串运维代价），本来就该用同一套 golden set 量了再决定，而不是拍脑袋。

做法：
1. 把 corpus.json 的 261 条记忆各写成一个以 mem id 命名的 .md 灌进**独立**知识库
   （用 --db 指定临时目录，绝不碰项目现有的 kb 库）；
2. 用同一套 50 道题跑 kb search，命中的 doc_path 反解回 mem id；
3. 与 BM25 / 现状词法三档对拍，**分 kind 报告**。

判据（照方案里定的）：
- hybrid 相对 BM25 的 hit@3 提升 >= 20pp  -> 值得做，立项 Phase 3
- 提升不到这个数                          -> 砍掉向量，省下全部工程代价
- 且 explicit 类不得低于纯 BM25（不能为了语义把字面匹配搞坏）
"""

import io
import json
import os
import shutil
import tempfile
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from score import BM25, tokenize_query, rank_current, rank_bm25  # noqa: E402

# kb sidecar 与工作目录都可用环境变量覆盖，别把本机绝对路径写死进仓库。
KB = os.environ.get(
    "ENGRAM_KB_BIN",
    os.path.join(os.path.expanduser("~"), ".engram", "bin",
                 "engram-kb-windows-x86_64.exe").replace("\\", "/"))
WORK = os.environ.get("ENGRAM_VECPROBE_DIR",
                      os.path.join(tempfile.gettempdir(), "engram-vecprobe").replace("\\", "/"))
DOCS = os.path.join(WORK, "docs")
KBDIR = os.path.join(WORK, "kb")
LIMIT = 10


def build_docs(mems):
    """每条记忆一个 .md，文件名 = mem id（纯 ASCII，便于反解）。"""
    if os.path.isdir(DOCS):
        shutil.rmtree(DOCS)
    os.makedirs(DOCS)
    for m in mems:
        body = m["cue"]
        tags = m.get("tags") or []
        if tags:
            body += "\n\n标签：" + " ".join(tags)
        io.open(os.path.join(DOCS, m["id"] + ".md"), "w", encoding="utf-8").write(body)
    return len(mems)


def ingest():
    if os.path.isdir(KBDIR):
        shutil.rmtree(KBDIR)
    os.makedirs(KBDIR)
    p = subprocess.run([KB, "ingest", "--db", KBDIR, DOCS, "--json"],
                       capture_output=True)
    out = p.stdout.decode("utf-8", "replace")
    err = p.stderr.decode("utf-8", "replace")
    return p.returncode, out[-600:], err[-600:]


def kb_search(query, limit=LIMIT):
    """返回按名次排列、去重后的 mem id 列表。"""
    p = subprocess.run([KB, "search", "--db", KBDIR, "--query", query,
                        "--limit", str(limit * 3), "--json"], capture_output=True)
    if p.returncode != 0:
        return []
    try:
        data = json.loads(p.stdout.decode("utf-8"))
    except Exception:
        return []
    hits = data.get("hits") if isinstance(data, dict) else data
    if not isinstance(hits, list):
        return []
    seen = []
    for h in hits:
        dp = h.get("doc_path") or h.get("path") or ""
        mid = os.path.basename(dp)
        if mid.endswith(".md"):
            mid = mid[:-3]
        # 同一条记忆可能被切成多块都命中，只取它最靠前的那次
        if mid and mid not in seen:
            seen.append(mid)
    return seen[:limit]


def first_rank(ids, gold):
    g = set(gold)
    for i, mid in enumerate(ids, start=1):
        if mid in g:
            return i
    return None


def tally(name, ranks_by_kind, kinds):
    lines = []
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
        lines.append("%-12s %-13s %4d %7s %7s %7s %8.3f" % (
            name, k, st["n"], "%d/%d" % (st["h1"], st["n"]),
            "%d/%d" % (st["h3"], st["n"]), "%d/%d" % (st["h8"], st["n"]),
            st["mrr"] / st["n"]))
    lines.append("%-12s %-13s %4d %7s %7s %7s %8.3f" % (
        name, "** 合计 **", tot["n"], "%d/%d" % (tot["h1"], tot["n"]),
        "%d/%d" % (tot["h3"], tot["n"]), "%d/%d" % (tot["h8"], tot["n"]),
        tot["mrr"] / tot["n"]))
    return lines, tot


def main():
    mems = json.load(io.open(os.path.join(HERE, "corpus.json"), encoding="utf-8"))
    qs = [json.loads(l) for l in io.open(os.path.join(HERE, "memory-golden.jsonl"),
                                         encoding="utf-8") if l.strip()]
    kinds = ["explicit", "paraphrase", "prescriptive", "implicit"]
    log = []

    if "--reuse" not in sys.argv:
        n = build_docs(mems)
        log.append("写出 %d 个文档 -> %s" % (n, DOCS))
        code, out, err = ingest()
        log.append("ingest 退出码 %d" % code)
        if out.strip():
            log.append("  stdout: " + out.strip()[-300:])
        if err.strip():
            log.append("  stderr: " + err.strip()[-300:])
        if code != 0:
            io.open(os.path.join(HERE, "vector-probe.txt"), "w",
                    encoding="utf-8").write("\n".join(log))
            print("ingest 失败，见 vector-probe.txt")
            return 1

    bm = BM25(mems)
    ranks = {"现状词法": {}, "BM25": {}, "kb 向量+BM25": {}}
    per_q = []
    for q in qs:
        k = q["kind"]
        r_cur = first_rank([i for i, _ in rank_current(mems, q["query"], LIMIT)], q["gold"])
        r_bm = first_rank([i for i, _ in rank_bm25(bm, mems, q["query"], LIMIT)], q["gold"])
        r_kb = first_rank(kb_search(q["query"]), q["gold"])
        ranks["现状词法"].setdefault(k, []).append(r_cur)
        ranks["BM25"].setdefault(k, []).append(r_bm)
        ranks["kb 向量+BM25"].setdefault(k, []).append(r_kb)
        per_q.append((q["id"], k, r_cur, r_bm, r_kb))

    out = []
    out.append("向量检索价值验证（E1）｜语料 %d 条｜题 %d 道｜limit=%d" % (len(mems), len(qs), LIMIT))
    out.append("独立知识库：%s（不碰项目现有 kb）" % KBDIR)
    out.extend(log)
    out.append("")
    out.append("%-12s %-13s %4s %7s %7s %7s %8s" % (
        "检索器", "kind", "n", "hit@1", "hit@3", "hit@8", "MRR"))
    out.append("-" * 68)
    tots = {}
    for name in ("现状词法", "BM25", "kb 向量+BM25"):
        lines, tot = tally(name, ranks[name], kinds)
        out.extend(lines)
        out.append("")
        tots[name] = tot

    # 判据核算
    def h3(t):
        return t["h3"] * 100.0 / t["n"] if t["n"] else 0.0

    def mrr(t):
        return t["mrr"] / t["n"] if t["n"] else 0.0

    d_h3 = h3(tots["kb 向量+BM25"]) - h3(tots["BM25"])
    out.append("── 判据核算 ──")
    out.append("BM25 hit@3 = %.0f%%   向量 hybrid hit@3 = %.0f%%   差 %+.0f pp（立项线 +20pp）"
               % (h3(tots["BM25"]), h3(tots["kb 向量+BM25"]), d_h3))
    out.append("BM25 MRR  = %.3f   向量 hybrid MRR  = %.3f   差 %+.3f"
               % (mrr(tots["BM25"]), mrr(tots["kb 向量+BM25"]),
                  mrr(tots["kb 向量+BM25"]) - mrr(tots["BM25"])))
    ex_bm = ranks["BM25"].get("explicit", [])
    ex_kb = ranks["kb 向量+BM25"].get("explicit", [])
    ok_ex = sum(1 for r in ex_kb if r and r <= 3) >= sum(1 for r in ex_bm if r and r <= 3)
    out.append("explicit 不得劣化：BM25 hit@3=%d/%d，向量 hit@3=%d/%d -> %s" % (
        sum(1 for r in ex_bm if r and r <= 3), len(ex_bm),
        sum(1 for r in ex_kb if r and r <= 3), len(ex_kb),
        "通过" if ok_ex else "**未通过**"))
    out.append("")
    out.append("结论：%s" % ("值得立项 Phase 3" if (d_h3 >= 20 and ok_ex)
                           else "**不值得**——收益达不到立项线，砍掉向量，省下全部工程代价"))

    out.append("")
    out.append("── per-question 名次（None = 未命中）──")
    out.append("%-6s %-13s %8s %8s %10s" % ("题", "kind", "现状", "BM25", "向量hybrid"))
    for qid, k, a, b, c in per_q:
        mark = ""
        if b and c and c < b:
            mark = "  <- 向量更好"
        elif b and not c:
            mark = "  <- 向量丢了"
        elif c and not b:
            mark = "  <- 向量捡回"
        out.append("%-6s %-13s %8s %8s %10s%s" % (qid, k, str(a), str(b), str(c), mark))

    io.open(os.path.join(HERE, "vector-probe.txt"), "w", encoding="utf-8").write("\n".join(out))
    print("OK -> vector-probe.txt")
    return 0


if __name__ == "__main__":
    sys.exit(main())
