#!/usr/bin/env python3
"""Aggregate `perf script` output (one sample = header line + indented frames + blank): self/inclusive per function, per thread, crate split."""
import sys, re, collections
top = int(sys.argv[2]) if len(sys.argv) > 2 else 20
def crate(f):
    m = re.match(r'^(?:<)?([A-Za-z0-9_]+)::', f); return m.group(1) if m else f.split('(')[0][:28]
def clean(sym):
    sym = re.sub(r'\+0x[0-9a-f]+$', '', sym)
    return sym or '?'
threads = collections.defaultdict(list); cur = None; frames = []
for line in open(sys.argv[1], errors='replace'):
    if not line.strip():
        if cur is not None and frames: threads[cur].append(frames)
        cur = None; frames = []; continue
    if not line.startswith((' ', '\t')):
        m = re.match(r'(\S.*?)\s+(\d+)(?:/(\d+))?\s', line)
        cur = f"{m.group(1)} {m.group(3) or m.group(2)}" if m else line.split()[0]; frames = []; continue
    m = re.match(r'\s+[0-9a-f]+\s+(.*?)\s+\((.*)\)\s*$', line)
    if m: frames.append(clean(m.group(1)))
if cur is not None and frames: threads[cur].append(frames)
total = sum(len(v) for v in threads.values())
print(f"samples={total} threads={len(threads)}")
allself = collections.Counter()
for t, stacks in sorted(threads.items(), key=lambda kv: -len(kv[1])):
    n = len(stacks)
    if n < total * 0.01: continue
    selfc = collections.Counter(s[0] for s in stacks)
    incl = collections.Counter(f for s in stacks for f in dict.fromkeys(s))
    cr = collections.Counter(crate(s[0]) for s in stacks)
    print(f"\n=== {t}: {n} samples ({100*n/total:.0f}% of all) ===")
    print("-- self by crate --")
    for k, c in cr.most_common(10): print(f"{100*c/n:6.1f}%  {k}")
    print("-- self --")
    for f, c in selfc.most_common(top): print(f"{100*c/n:6.1f}%  {f[:110]}")
    print("-- inclusive --")
    for f, c in incl.most_common(top): print(f"{100*c/n:6.1f}%  {f[:110]}")
    allself.update(selfc)
