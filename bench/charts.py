#!/usr/bin/env python3
"""Static PNG charts for docs/WRITEUP.md, from the committed benchmark results.

    python3 bench/charts.py            # writes docs/img/*.png

Needs matplotlib. Colors match bench/report.py (validated palette, light mode).
"""
import csv
import json
import re
import statistics
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
RES = ROOT / "bench" / "results"
OUT = ROOT / "docs" / "img"

SURFACE = "#fcfcfb"
TEXT = "#0b0b0b"
TEXT2 = "#52514e"
MUTED = "#898781"
GRID = "#e1e0d9"
AXIS = "#c3c2b7"
BAND = "#f2f2ef"
NEUTRAL = "#b4b2a9"
COLOR = {"java": "#1baf7a", "java-tuned": "#eb6834", "rust": "#2a78d6"}
LABEL = {"java": "Java as shipped", "java-tuned": "Java tuned", "rust": "Rust"}
TARGETS = ["java", "java-tuned", "rust"]
PHASES = [("idle", "Idle"), ("sustained", "Sustained"), ("cooldown", "Cooldown"),
          ("peak", "Peak"), ("post_peak", "Post-peak")]

plt.rcParams.update({
    "font.family": ["Inter", "Helvetica Neue", "Lato", "DejaVu Sans"],
    "font.size": 10,
    "text.color": TEXT,
    "axes.edgecolor": AXIS,
    "axes.labelcolor": TEXT2,
    "axes.facecolor": SURFACE,
    "figure.facecolor": SURFACE,
    "xtick.color": TEXT2,
    "ytick.color": TEXT2,
    "xtick.major.size": 0,
    "ytick.major.size": 0,
    "axes.spines.top": False,
    "axes.spines.right": False,
    "axes.grid": False,
})


def summaries(target):
    out = []
    for d in sorted((RES / target).glob("*/summary.json")):
        s = json.loads(d.read_text())
        if s.get("valid", True):
            out.append((d.parent, s))
    return out


def median_metric(target, key):
    return statistics.median(s["memory"]["rss_mib"][key] for _, s in summaries(target))


def representative_run(target):
    """The run whose peak RSS is the median of the target's runs."""
    runs = summaries(target)
    med = statistics.median(s["memory"]["rss_mib"]["peak"] for _, s in runs)
    return min(runs, key=lambda r: abs(r[1]["memory"]["rss_mib"]["peak"] - med))[0]


def load_samples(run_dir):
    rows = list(csv.DictReader(open(run_dir / "memory.csv")))
    # Keep only the last segment, as summarize.py does.
    start = max(i for i, r in enumerate(rows) if float(r["elapsed_s"]) == 0.0)
    rows = rows[start:]
    return ([float(r["elapsed_s"]) / 60 for r in rows],
            [int(r["vm_rss_kib"]) / 1024 for r in rows],
            [r["phase"] for r in rows])


def title(fig, main, sub):
    fig.text(0.012, 0.965, main, fontsize=13, fontweight="bold", color=TEXT, va="top")
    fig.text(0.012, 0.905, sub, fontsize=9.5, color=TEXT2, va="top")


def ygrid(ax):
    ax.yaxis.grid(True, color=GRID, linewidth=0.8)
    ax.set_axisbelow(True)


def save(fig, name):
    OUT.mkdir(parents=True, exist_ok=True)
    fig.savefig(OUT / name, dpi=200)
    plt.close(fig)
    print("wrote", (OUT / name).relative_to(ROOT))


def phase_bands(ax, xs, phases, top):
    spans, cur, s0 = [], phases[0], xs[0]
    for x, p in zip(xs, phases):
        if p != cur:
            spans.append((cur, s0, x))
            cur, s0 = p, x
    spans.append((cur, s0, xs[-1]))
    names = dict(PHASES)
    for i, (p, a, b) in enumerate(spans):
        if i % 2 == 1:
            ax.axvspan(a, b, color=BAND, zorder=0, linewidth=0)
        if b - a > 0.6:
            ax.text((a + b) / 2, top, names.get(p, p), ha="center", va="bottom",
                    fontsize=8.5, color=MUTED)


def timeline():
    fig, ax = plt.subplots(figsize=(10, 4.6))
    fig.subplots_adjust(left=0.07, right=0.91, top=0.80, bottom=0.12)
    title(fig, "Resident memory through a benchmark run",
          "VmRSS of the service process, sampled every second. One representative run per "
          "target (the run with the median peak).")
    top = 0
    series = {}
    for t in TARGETS:
        xs, ys, ph = load_samples(representative_run(t))
        series[t] = (xs, ys, ph)
        top = max(top, max(ys))
    ymax = (int(top / 100) + 1) * 100
    xs, _, ph = series["rust"]
    phase_bands(ax, xs, ph, ymax * 1.005)
    xend = max(series["rust"][0])
    for t in TARGETS:
        x, y, _ = series[t]
        ax.plot(x, y, color=COLOR[t], linewidth=2, solid_capstyle="round", label=LABEL[t])
        yend = [v for u, v in zip(x, y) if u <= xend][-1]
        ax.annotate(f"{yend:.0f} MiB" if t != "rust" else f"{yend:.1f} MiB",
                    (xend, yend), xytext=(8, 0), textcoords="offset points",
                    va="center", fontsize=9.5, color=TEXT, annotation_clip=False)
    ax.set_ylim(0, ymax)
    ax.set_xlim(0, xend)
    ax.set_xlabel("minutes")
    ax.set_ylabel("MiB")
    ygrid(ax)
    ax.legend(loc="upper left", bbox_to_anchor=(0, 1.0), frameon=False, ncol=3, fontsize=9)
    save(fig, "memory-timeline.png")


def rust_zoom():
    fig, ax = plt.subplots(figsize=(10, 3.8))
    fig.subplots_adjust(left=0.07, right=0.86, top=0.76, bottom=0.14)
    title(fig, "Rust, zoomed in",
          "The same Rust run on its own scale: 4 MiB idle, under 8 MiB under sustained load, "
          "19 MiB at saturation.")
    xs, ys, ph = load_samples(representative_run("rust"))
    ymax = 25
    phase_bands(ax, xs, ph, ymax * 1.01)
    ax.plot(xs, ys, color=COLOR["rust"], linewidth=2)
    ax.annotate(f"Rust  {ys[-1]:.1f} MiB", (xs[-1], ys[-1]), xytext=(8, 0),
                textcoords="offset points", va="center", fontsize=9.5, color=TEXT)
    ax.set_ylim(0, ymax)
    ax.set_xlim(0, xs[-1])
    ax.set_xlabel("minutes")
    ax.set_ylabel("MiB")
    ygrid(ax)
    save(fig, "memory-rust-zoom.png")


def per_phase():
    keys = [("idle", "Idle"), ("sustained_p50", "Sustained (p50)"), ("peak", "Peak"),
            ("post_peak", "After peak")]
    fig, ax = plt.subplots(figsize=(10, 4.6))
    fig.subplots_adjust(left=0.07, right=0.98, top=0.80, bottom=0.10)
    title(fig, "Resident memory by phase",
          "Median of three runs per target. Same limits for every target: 2 CPUs, 1 GiB, no swap.")
    w = 0.26
    for i, t in enumerate(TARGETS):
        vals = [median_metric(t, k) for k, _ in keys]
        xs = [j + (i - 1) * w for j in range(len(keys))]
        ax.bar(xs, vals, width=w, color=COLOR[t], edgecolor=SURFACE, linewidth=2, label=LABEL[t])
        for x, v in zip(xs, vals):
            ax.text(x, v + 6, f"{v:.1f}" if v < 100 else f"{v:.0f}", ha="center", va="bottom",
                    fontsize=8.5, color=TEXT2)
    ax.set_xticks(range(len(keys)), [n for _, n in keys])
    ax.set_ylabel("MiB")
    ax.set_ylim(0, 520)
    ygrid(ax)
    ax.legend(loc="upper left", frameon=False, ncol=3, fontsize=9)
    save(fig, "memory-by-phase.png")


def nmt():
    files = sorted(RES.glob("java-nmt/*/nmt-after-load.txt"))
    text = files[-1].read_text()
    got = {m.group(1).strip(): int(m.group(2))
           for m in re.finditer(r"^-\s+(.+?) \(reserved=\d+MB, committed=(\d+)MB", text, re.M)}
    total = int(re.search(r"^Total: reserved=\d+MB, committed=(\d+)MB", text, re.M).group(1))
    parts = [("Java heap", got.get("Java Heap", 0)),
             ("Class metadata", got.get("Class", 0) + got.get("Metaspace", 0)),
             ("JIT code cache", got.get("Code", 0)),
             ("Symbols", got.get("Symbol", 0)),
             ("Thread stacks", got.get("Thread", 0))]
    parts.append(("Everything else", total - sum(v for _, v in parts)))
    rust_peak = median_metric("rust", "peak")

    rows = parts + [("Rust: whole process, at peak", rust_peak)]
    fig, ax = plt.subplots(figsize=(10, 4.4))
    fig.subplots_adjust(left=0.25, right=0.95, top=0.80, bottom=0.10)
    title(fig, f"Where the JVM's {total} MiB goes, against the whole Rust process",
          "Native Memory Tracking, committed memory, Java as shipped after sustained load. "
          f"Non-heap alone is {total - parts[0][1]} MiB.")
    ys = list(range(len(rows)))[::-1]
    for y, (name, v) in zip(ys, rows):
        c = COLOR["rust"] if name.startswith("Rust") else (COLOR["java"] if name == "Java heap" else NEUTRAL)
        ax.barh(y, v, height=0.62, color=c, edgecolor=SURFACE, linewidth=2)
        ax.text(v + 2, y, f"{v:.1f} MiB" if name.startswith("Rust") else f"{v:.0f} MiB", va="center",
                fontsize=9, color=TEXT2)
    ax.set_yticks(ys, [n for n, _ in rows])
    ax.tick_params(axis="y", colors=TEXT)
    ax.set_xlim(0, max(v for _, v in rows) * 1.18)
    ax.set_xlabel("MiB")
    ax.xaxis.grid(True, color=GRID, linewidth=0.8)
    ax.set_axisbelow(True)
    save(fig, "jvm-nmt-vs-rust.png")


def experiments():
    base_runs = [s["memory"]["rss_mib"]["peak"] for _, s in summaries("rust")]
    base = statistics.median(base_runs)
    variants = [("rust@1worker", "TOKIO_WORKER_THREADS=1"), ("rust@pool5", "DB_POOL_SIZE=5"),
                ("rust@arena2", "MALLOC_ARENA_MAX=2"), ("rust-mimalloc", "mimalloc"),
                ("rust-jemalloc", "jemalloc")]
    rows = []
    for t, name in variants:
        runs = summaries(t)
        if runs:
            v = statistics.median(s["memory"]["rss_mib"]["peak"] for _, s in runs)
            rows.append((f"{name}  (n={len(runs)})", v - base))
    fig, ax = plt.subplots(figsize=(10, 3.9))
    fig.subplots_adjust(left=0.27, right=0.95, top=0.76, bottom=0.13)
    title(fig, "Rust optimization experiments: change in peak RSS",
          f"Against the plain Rust build (peak {base:.1f} MiB, n={len(base_runs)}). "
          "Gray band: spread of the plain runs. Only the blue one is kept.")
    lo, hi = min(base_runs) - base, max(base_runs) - base
    ax.axvspan(min(lo, -0.15), max(hi, 0.15), color=BAND, zorder=0, linewidth=0)
    ax.axvline(0, color=AXIS, linewidth=1)
    ys = list(range(len(rows)))[::-1]
    for y, (name, d) in zip(ys, rows):
        c = COLOR["rust"] if d < lo else NEUTRAL
        ax.barh(y, d, height=0.6, color=c, edgecolor=SURFACE, linewidth=2)
        ax.text(d + (0.4 if d >= 0 else -0.4), y, f"{d:+.1f} MiB", va="center",
                ha="left" if d >= 0 else "right", fontsize=9, color=TEXT2)
    ax.set_yticks(ys, [n for n, _ in rows])
    ax.tick_params(axis="y", colors=TEXT)
    span = max(abs(d) for _, d in rows)
    ax.set_xlim(-6, span * 1.2)
    ax.set_xlabel("MiB vs plain Rust (negative is less memory)")
    save(fig, "rust-experiments.png")


if __name__ == "__main__":
    timeline()
    rust_zoom()
    per_phase()
    nmt()
    experiments()
