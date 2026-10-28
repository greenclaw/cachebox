"""LFUCache hot paths, ns/op (min of 3). Prints JSON."""
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
            c = cachebox.LFUCache(m, {k: k for k in range(m)})
            for k in keys:
                if c.get(k) is None:
                    c[k] = k
        res[f"LFU get-or-set {name} maxsize={m}"] = best(miss_heavy, N)

hot = [rng.randrange(1_000) for _ in range(N)]
full = cachebox.LFUCache(5_000, {k: k for k in range(1_000)})
def hits():
    g = full.get
    for k in hot:
        g(k)
res["LFU get hit (1k hot keys)"] = best(hits, N)

big = cachebox.LFUCache(0, {i: i for i in range(100_000)})
res["LFU iterate keys 100k"] = best(lambda: sum(1 for _ in big.keys()), 100_000)
json.dump(res, sys.stdout)
