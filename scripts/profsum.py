#!/usr/bin/env python3
"""Summarise a samply .json.gz (saved with -s, unsymbolicated): resolves addresses with addr2line, then self/inclusive per thread and crate."""
import gzip, json, sys, re, collections, subprocess, shutil
p = json.load(gzip.open(sys.argv[1])); top = int(sys.argv[2]) if len(sys.argv) > 2 else 20
libs = p['libs']
# 1. collect (lib, addr) pairs
pairs = collections.defaultdict(set)
for t in p['threads']:
    ft, fu, rt = t['frameTable'], t['funcTable'], t['resourceTable']
    for i in range(ft['length']):
        a = ft['address'][i]; f = ft['func'][i]
        if a is None or a < 0: continue
        r = fu['resource'][f]
        if r is None or r < 0: continue
        pairs[rt['lib'][r]].add(a)
# 2. symbolicate
sym = {}
a2l = shutil.which('addr2line')
for li, addrs in pairs.items():
    path = libs[li]['debugPath'] or libs[li]['path']; addrs = sorted(addrs)
    try:
        out = subprocess.run([a2l, '-f', '-C', '-e', path] + [hex(a) for a in addrs], capture_output=True, text=True, timeout=600).stdout.splitlines()
    except Exception as e:
        out = []
    names = out[0::2]
    for a, n in zip(addrs, names):
        n = re.sub(r'::h[0-9a-f]{16}$', '', n)
        sym[(li, a)] = n if n and n != '??' else f"{libs[li]['name']}+{hex(a)}"
def fname(t, frame):
    ft, fu, rt = t['frameTable'], t['funcTable'], t['resourceTable']
    a = ft['address'][frame]; f = ft['func'][frame]; r = fu['resource'][f]
    if a is None or a < 0 or r is None or r < 0: return t['stringTable'][fu['name'][f]] if 'stringTable' in t else '?'
    return sym.get((rt['lib'][r], a), '?')
def crate(f):
    m = re.match(r'^(?:<)?([A-Za-z0-9_]+)(?:::|\+)', f); return m.group(1) if m else f[:28]
total = sum(t['samples']['length'] for t in p['threads'])
print(f"samples={total} (rate as recorded)")
for t in sorted(p['threads'], key=lambda t: -t['samples']['length']):
    n = t['samples']['length']
    if n < max(20, total * 0.01): continue
    st = t['stackTable']; stacks = t['samples']['stack']
    memo = {}
    def chain(s):
        if s in memo: return memo[s]
        out = []; cur = s
        while cur is not None:
            out.append(fname(t, st['frame'][cur])); cur = st['prefix'][cur]
        memo[s] = out; return out
    selfc = collections.Counter(); incl = collections.Counter(); cr = collections.Counter()
    for s in stacks:
        if s is None: continue
        c = chain(s); selfc[c[0]] += 1; cr[crate(c[0])] += 1
        for f in dict.fromkeys(c): incl[f] += 1
    print(f"\n=== thread '{t['name']}' pid={t.get('pid')} samples={n} ({100*n/total:.0f}% of all) ===")
    print("-- self by crate/lib --")
    for k, c in cr.most_common(10): print(f"{100*c/n:6.1f}%  {k}")
    print("-- self --")
    for f, c in selfc.most_common(top): print(f"{100*c/n:6.1f}%  {f[:120]}")
    print("-- inclusive --")
    for f, c in incl.most_common(top): print(f"{100*c/n:6.1f}%  {f[:120]}")
