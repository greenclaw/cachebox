"""VTTLCache hot paths, ns/op (min of 3). Prints JSON."""
import json
import random
import sys
import time

import cachebox


def best(fn, n, rep=3):
    t = 1e9
    for _ in range(rep):
        s = time.perf_counter()
        fn()
        t = min(t, time.perf_counter() - s)
    return round(t / n * 1e9)


rng = random.Random(1)
N = 200_000
res = {}
for m in (1_000, 100_000):
    ints = [rng.randrange(4 * m) for _ in range(N)]
    strs = [f"user:{k}" for k in ints]
    for name, keys in (("int", ints), ("str", strs)):
        def miss_heavy():
            c = cachebox.VTTLCache(m)
            for k in range(m):
                c.insert(k, k, 3600 + k % 97)
            for i, k in enumerate(keys):
                if c.get(k) is None:
                    c.insert(k, k, 3600 + i % 97)
        res[f"VTTL get-or-set {name} maxsize={m}"] = best(miss_heavy, N + m)

hot = [rng.randrange(1_000) for _ in range(N)]
full = cachebox.VTTLCache(5_000)
for k in range(1_000):
    full.insert(k, k, 3600)
def hits():
    g = full.get
    for k in hot:
        g(k)
res["VTTL get hit (1k hot keys)"] = best(hits, N)

def expiring():
    c = cachebox.VTTLCache(0)
    for k in range(50_000):
        c.insert(k, k, 0.000001)
res["VTTL insert with constant expiry"] = best(expiring, 50_000)
json.dump(res, sys.stdout)
