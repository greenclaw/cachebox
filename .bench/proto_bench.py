"""Cache and RRCache hot paths, ns/op (min of 3). Prints JSON."""
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
ints = [rng.randrange(20_000) for _ in range(N)]
strs = [f"user:{k}" for k in ints]
res = {}

for name, keys in (("int", ints), ("str", strs)):
    def fill():
        c = cachebox.Cache(0)
        for k in keys:
            if c.get(k) is None:
                c[k] = k
    res[f"Cache get-or-set {name}"] = best(fill, N)

    full = cachebox.Cache(0, {k: k for k in keys})
    def hit():
        g = full.get
        for k in keys:
            g(k)
    res[f"Cache get hit {name}"] = best(hit, N)

    def contains_miss():
        for k in keys:
            (k, 1) in full
    res[f"Cache miss {name}"] = best(contains_miss, N)

big = cachebox.Cache(0, {i: i for i in range(100_000)})
res["Cache iterate keys 100k"] = best(lambda: sum(1 for _ in big.keys()), 100_000)
res["Cache iterate items 100k"] = best(lambda: sum(1 for _ in big.items()), 100_000)
res["Cache pop+insert"] = best(lambda: [big.__setitem__(i, big.pop(i)) for i in range(100_000)], 100_000)

for m in (1_000, 100_000):
    ks = [rng.randrange(4 * m) for _ in range(N)]
    def rr():
        c = cachebox.RRCache(m, {k: k for k in range(m)})
        for k in ks:
            if c.get(k) is None:
                c[k] = k
    res[f"RRCache insert/evict maxsize={m}"] = best(rr, N)

json.dump(res, sys.stdout)
