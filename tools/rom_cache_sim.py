#!/usr/bin/env python3
"""Simulate the ROM page cache on instruction traces from gdb_trace.py --step.

Replays ROM (08xxxxxx) instruction fetches through a set-associative LRU cache for
several cache sizes, page sizes, associativities and next-page prefetch, and reports
misses and an estimated SD stall time. Each trace is run cold, and all traces in
order as one warm sequence.

Usage: rom_cache_sim.py TRACE [TRACE ...]      (.log or .log.gz)
       e.g. rom_cache_sim.py docs/traces/emerald-step-1m-run*.log.gz

Model assumptions: only instruction fetches (no ROM data reads); an SD read costs
1.4 ms for up to 512 B plus 0.1 ms per extra 512 B; a prefetched next page costs
nothing extra when both pages fit in one 512 B block.
"""
import gzip
import sys

if len(sys.argv) < 2:
    sys.exit(__doc__)
NAMES = sys.argv[1:]


def load(f):
    op = gzip.open if f.endswith(".gz") else open
    with op(f, "rt") as fh:
        return [int(l.split()[1], 16) for l in fh if l.split()[1][:2] == "08"]


raw = [load(f) for f in NAMES]


def collapse(pcs,P):
    out=[];last=-1
    for pc in pcs:
        pg=pc//P
        if pg!=last: out.append(pg); last=pg
    return out
def sim(seqs,cap,P,ways,prefetch):
    """seqs: list of page sequences processed in order with a persistent cache. returns misses per seq."""
    sets=max(1,cap//(P*ways)); w=cap//(P*sets)
    tags=[[] for _ in range(sets)]
    res=[]
    def touch(pg,count):
        s=tags[pg%sets]
        if pg in s: s.remove(pg); s.append(pg); return 0
        if len(s)==w: s.pop(0)
        s.append(pg); return 1
    for seq in seqs:
        m=0
        for pg in seq:
            if touch(pg,True):
                m+=1
                if prefetch: touch(pg+1,False)   # prefetched page rides the same SD read
        res.append(m)
    return res
def cost(P,prefetch):
    n=max(P,512)*(2 if prefetch and P<512 else 1)  # prefetch of next page in same 512B block is free when P<=256
    blocks=max(1,(P*(2 if prefetch else 1)+511)//512)
    return 1.4+0.1*(blocks-1)
cache={}
rows=[]
for cap in (16,32,64,128):
  for P in (128,256,512,1024,2048):
    if P not in cache: cache[P]=[collapse(r,P) for r in raw]
    seqs=cache[P]
    for ways in (2,4,8,16):
      if cap*1024//(P*ways)<1: continue
      for pf in (0,1):
        cold = [sim([s], cap * 1024, P, ways, pf)[0] for s in seqs]
        warm=sim(seqs,cap*1024,P,ways,pf)
        c=cost(P,pf)
        rows.append((cap,P,ways,pf,cold,warm,c))
for cap in (16,32,64,128):
    print(f"\n== {cap} KB: top 6 by cold stall (sum of 3 runs) ==")
    print("page ways pf |  cold misses r1/r2/r3 | warm misses r1/r2/r3 | cold stall ms | warm stall ms")
    sel=sorted([r for r in rows if r[0]==cap],key=lambda r:sum(r[4])*r[6])[:6]
    for _,P,w,pf,cold,warm,c in sel:
        print(f"{P:5d} {w:3d}  {pf} | {cold[0]:6d} {cold[1]:5d} {cold[2]:5d} | {warm[0]:6d} {warm[1]:5d} {warm[2]:5d} | {sum(cold)*c:8.0f} | {sum(warm)*c:8.0f}")
