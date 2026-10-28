"""Build `base` and `proto` checkouts in separate venvs and compare benchmarks.

Usage: python .bench/run.py <base_dir> <proto_dir> [rounds]
Writes a markdown table to $GITHUB_STEP_SUMMARY (or stdout).
"""
import json
import os
import platform
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
SCRIPTS = ["proto_bench.py", "lru_bench.py", "fifo_bench.py", "ttl_bench.py", "lfu_bench.py", "vttl_bench.py"]


def venv_python(venv: Path) -> Path:
    return venv / ("Scripts/python.exe" if os.name == "nt" else "bin/python")


def build(src: Path) -> Path:
    venv = src / ".venv-bench"
    subprocess.run(["uv", "venv", "-q", "--clear", "--python", "3.13", str(venv)], check=True)
    py = venv_python(venv)
    subprocess.run(["uv", "pip", "install", "-q", "--python", str(py), "maturin"], check=True)
    env = dict(os.environ, VIRTUAL_ENV=str(venv))
    maturin = venv / ("Scripts/maturin.exe" if os.name == "nt" else "bin/maturin")
    subprocess.run([str(maturin), "develop", "-q", "--release"], cwd=src, env=env, check=True)
    # Confirm this venv imports its own build, not the other checkout's.
    loc = subprocess.run([str(py), "-I", "-c", "import cachebox; print(cachebox.__file__)"],
                         capture_output=True, text=True, check=True).stdout.strip()
    assert Path(loc).resolve().is_relative_to(src.resolve()), loc
    return py


def main() -> None:
    base, proto = Path(sys.argv[1]), Path(sys.argv[2])
    rounds = int(sys.argv[3]) if len(sys.argv) > 3 else 3
    pys = {"main": build(base), "proto": build(proto)}
    best: dict[str, dict[str, int]] = {"main": {}, "proto": {}}
    for _ in range(rounds):
        for script in SCRIPTS:
            for name, py in pys.items():  # interleaved
                out = subprocess.run([str(py), "-I", str(HERE / script)],
                                     capture_output=True, text=True, check=True).stdout
                for k, v in json.loads(out).items():
                    best[name][k] = min(v, best[name].get(k, v))

    lines = [f"### {platform.system()} {platform.machine()}, best of {rounds} interleaved rounds, ns/op",
             "", "| benchmark | main | proto | proto/main |", "|---|---:|---:|---:|"]
    for k, m in best["main"].items():
        p = best["proto"][k]
        lines.append(f"| {k} | {m} | {p} | {p / m:.2f}x |")
    text = "\n".join(lines) + "\n"
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as f:
            f.write(text)
    print(text)
    (Path.cwd() / "bench.json").write_text(json.dumps(best))


if __name__ == "__main__":
    main()
