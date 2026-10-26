# usage: thr.py <seconds> : CPU (cores) per thread-name group of the cobalt process over a window, plus totals
import os, sys, time, re, collections
pid = max((int(p) for p in os.popen("pgrep -x " + os.environ.get("PROC", "cobalt")).read().split()), key=lambda p: len(os.listdir(f"/proc/{p}/task")))
hz = os.sysconf("SC_CLK_TCK")
def snap():
    d = {}
    for t in os.listdir(f"/proc/{pid}/task"):
        try:
            s = open(f"/proc/{pid}/task/{t}/stat").read()
        except OSError:
            continue
        name = s[s.index("(")+1:s.rindex(")")]
        f = s[s.rindex(")")+2:].split()
        d[t] = (name, int(f[11]) + int(f[12]))
    return d
T = float(sys.argv[1]) if len(sys.argv) > 1 else 30
a = snap(); time.sleep(T); b = snap()
g = collections.Counter(); n = collections.Counter()
for t, (name, ticks) in b.items():
    key = re.sub(r"\d+", "#", name)
    g[key] += ticks - a.get(t, (name, 0))[1]; n[key] += 1
tot = sum(g.values())
rss = [l for l in open(f"/proc/{pid}/status") if l.startswith(("VmRSS", "VmHWM"))]
print(f"total_cores={tot/hz/T:.2f} threads={len(b)} " + " ".join(x.split()[0] + x.split()[1] for x in rss))
for k, v in g.most_common(14):
    print(f"  {v/hz/T:5.2f}  {k} (x{n[k]})")
top = sorted(((v[1] - a.get(t, (v[0], 0))[1], t, v[0]) for t, v in b.items()), reverse=True)[:8]
print("  top threads: " + ", ".join(f"{nm}[{t}]={d/hz/T:.2f}" for d, t, nm in top))
