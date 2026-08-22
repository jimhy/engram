# -*- coding: utf-8 -*-
import io, json, collections
BASE = os.path.dirname(os.path.abspath(__file__)) + "/"
d = json.load(io.open(BASE + "_raw_all.json", encoding="utf-8"))
keep = [m for m in d if m["status"] in ("active", "cold")]
out = [{
    "id": m["id"], "level": m["level"], "status": m["status"],
    "cue": m["cue"], "tags": m["tags"],
    "importance": m["importance"], "project": m["project"],
} for m in keep]
out.sort(key=lambda x: x["id"])
with io.open(BASE + "corpus.json", "w", encoding="utf-8") as f:
    json.dump(out, f, ensure_ascii=False, indent=1)
# 侧写文件，供人工出题时通读（避免中文走 stdout 管道）
with io.open(BASE + "_corpus_readable.txt", "w", encoding="utf-8") as f:
    for m in out:
        f.write(u"%s | %s | %s | imp=%.2f | proj=%s | tags=%s\n    %s\n" % (
            m["id"], m["level"], m["status"], m["importance"],
            m["project"], ",".join(m["tags"]), m["cue"]))
stat = collections.Counter((m["level"], m["status"]) for m in out)
with io.open(BASE + "_corpus_stats.txt", "w", encoding="utf-8") as f:
    f.write(u"total=%d\n" % len(out))
    for k in sorted(stat):
        f.write(u"%s %s: %d\n" % (k[0], k[1], stat[k]))
    f.write(u"with_tags=%d distinct_tags=%d\n" % (
        sum(1 for m in out if m["tags"]),
        len(set(t for m in out for t in m["tags"]))))
print("ok", len(out))
