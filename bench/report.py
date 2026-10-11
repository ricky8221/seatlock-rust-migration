#!/usr/bin/env python3
"""Builds a self-contained HTML report (no network, no installs) from the benchmark runs.

    bench/report.py [results-dir] [-o bench/results/REPORT.html]

Charts:
  1. Process memory over a whole run, Java (shipped), Java (tuned) and Rust on one axis, with
     the benchmark phases marked.
  2. The same Rust run on its own axis: at the scale of chart 1 it is a flat line near zero.
  3. Memory per phase (idle, sustained, peak, after peak), grouped by target.
  4. Latency at the same offered load (sustained phase): p50 / p95 / p99.
  5. Where the JVM's memory goes (Native Memory Tracking) next to Rust's whole process.
Every chart has hover details, and a table view repeats every number.

Runs flagged invalid by summarize.py (errors during the run) are left out, as in the
comparison table. The timeline charts plot each target's representative run: the valid run
whose peak memory is the median of that target's runs.
"""

from __future__ import annotations

import argparse
import json
import re
import statistics
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import summarize  # noqa: E402

TARGETS = [("java", "Java (as shipped)"), ("java-tuned", "Java (tuned)"), ("rust", "Rust")]


def representative(runs: list[dict]) -> dict:
    peaks = sorted(runs, key=lambda r: r["memory"]["rss_mib"]["peak"] or 0)
    return peaks[len(peaks) // 2]


def timeline(run_dir: Path) -> tuple[list[list[float]], dict]:
    samples = summarize.load_samples(run_dir)
    idle = [s for s in samples if s["phase"] == "idle"]
    t0 = idle[0]["elapsed"] if idle else 0.0
    points = [[round(s["elapsed"] - t0, 1), round(s["rss"], 2)] for s in samples if s["phase"] != "startup"]
    phases: dict[str, list[float]] = {}
    for s in samples:
        if s["phase"] in ("startup", "warmup", ""):
            continue
        x = s["elapsed"] - t0
        lo, hi = phases.get(s["phase"], [x, x])
        phases[s["phase"]] = [min(lo, x), max(hi, x)]
    return points, phases


def nmt_categories(path: Path) -> list[list]:
    """Committed MB per NMT category, largest first, small ones folded into 'Other'."""
    text = path.read_text()
    found = dict(
        (m.group(1).strip(), int(m.group(2)))
        for m in re.finditer(r"^-\s+([A-Za-z ]+?) \(reserved=\d+MB, committed=(\d+)MB", text, re.M)
    )
    named = {
        "Java heap (application objects)": found.get("Java Heap", 0),
        "Metaspace + class space": found.get("Metaspace", 0) + found.get("Class", 0),
        "JIT code cache": found.get("Code", 0),
        "Symbols": found.get("Symbol", 0),
        "Thread stacks": found.get("Thread", 0),
    }
    other = sum(v for k, v in found.items() if k not in ("Java Heap", "Metaspace", "Class", "Code", "Symbol", "Thread"))
    rows = sorted(([k, v] for k, v in named.items()), key=lambda kv: -kv[1])
    rows.append(["Other (GC, internal, arenas)", other])
    total = re.search(r"Total: reserved=\d+MB, committed=(\d+)MB", text)
    return rows, int(total.group(1)) if total else sum(v for _, v in rows)


def collect(results: Path) -> dict:
    by_target: dict[str, list[dict]] = {}
    dirs: dict[tuple[str, str], Path] = {}
    excluded = []
    for f in sorted(results.glob("*/*/memory.csv")):
        if not (f.parent / "meta.txt").exists():
            continue
        s = summarize.summarize_run(f.parent)
        if not s.get("valid", True):
            excluded.append(f"{s['target']}/{s['label']}: " + "; ".join(s["problems"]))
            continue
        by_target.setdefault(s["target"], []).append(s)
        dirs[(s["target"], s["label"])] = f.parent

    def med(target, *path):
        vals = [summarize.dig(s, *path) for s in by_target.get(target, [])]
        vals = [v for v in vals if v is not None]
        return round(statistics.median(vals), 2) if vals else None

    data = {"targets": [], "excluded": excluded, "phases": {}, "nmt": None}
    for key, label in TARGETS:
        runs = by_target.get(key)
        if not runs:
            continue
        rep = representative(runs)
        points, phases = timeline(dirs[(key, rep["label"])])
        if key == "rust" or not data["phases"]:
            data["phases"] = phases
        data["targets"].append(
            {
                "key": key,
                "label": label,
                "runs": len(runs),
                "rep": rep["label"],
                "points": points,
                "mem": {k: med(key, "memory", "rss_mib", k) for k in ("idle", "sustained_p50", "peak", "post_peak")},
                "lat": {k: med(key, "sustained", "latency_ms", k) for k in ("p50", "p95", "p99")},
                "threads": med(key, "memory", "threads", "peak"),
                "rps": med(key, "sustained", "throughput_rps"),
                "peak_rps": med(key, "peak", "throughput_rps"),
                "startup": med(key, "startup_seconds"),
            }
        )
    nmt_files = sorted(results.glob("java-nmt/*/nmt-after-load.txt"))
    if nmt_files:
        rows, total = nmt_categories(nmt_files[-1])
        data["nmt"] = {"rows": rows, "total": total}
    return data


PAGE = r"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>seatlock: Java vs Rust</title>
<style>
.viz-root {
  color-scheme: light;
  --page: #f9f9f7; --surface-1: #fcfcfb;
  --text-primary: #0b0b0b; --text-secondary: #52514e; --muted: #898781;
  --grid: #e1e0d9; --axis: #c3c2b7; --border: rgba(11,11,11,0.10); --band: rgba(11,11,11,0.035);
  --s-rust: #2a78d6; --s-tuned: #eb6834; --s-java: #1baf7a;
}
@media (prefers-color-scheme: dark) {
  :root:where(:not([data-theme="light"])) .viz-root {
    color-scheme: dark;
    --page: #0d0d0d; --surface-1: #1a1a19;
    --text-primary: #f0efec; --text-secondary: #c3c2b7; --muted: #898781;
    --grid: #2c2c2a; --axis: #383835; --border: rgba(255,255,255,0.10); --band: rgba(255,255,255,0.04);
    --s-rust: #3987e5; --s-tuned: #d95926; --s-java: #199e70;
  }
}
:root[data-theme="dark"] .viz-root {
  color-scheme: dark;
  --page: #0d0d0d; --surface-1: #1a1a19;
  --text-primary: #f0efec; --text-secondary: #c3c2b7; --muted: #898781;
  --grid: #2c2c2a; --axis: #383835; --border: rgba(255,255,255,0.10); --band: rgba(255,255,255,0.04);
  --s-rust: #3987e5; --s-tuned: #d95926; --s-java: #199e70;
}
html, body { margin: 0; background: var(--page); }
.viz-root { font: 14px/1.45 system-ui, -apple-system, "Segoe UI", sans-serif; color: var(--text-primary);
  background: var(--page); padding: 24px 16px 48px; }
.wrap { max-width: 880px; margin: 0 auto; }
h1 { font-size: 22px; margin: 0 0 4px; }
.sub { color: var(--text-secondary); margin: 0 0 20px; }
.tiles { display: grid; grid-template-columns: repeat(auto-fit, minmax(190px, 1fr)); gap: 12px; margin-bottom: 20px; }
.tile, .card { background: var(--surface-1); border: 1px solid var(--border); border-radius: 10px; }
.tile { padding: 14px 16px; }
.tile .k { color: var(--text-secondary); font-size: 13px; }
.tile .v { font-size: 26px; font-weight: 600; margin-top: 2px; }
.tile .d { color: var(--text-secondary); font-size: 13px; }
.card { padding: 16px 16px 8px; margin-bottom: 16px; }
.card h2 { font-size: 15px; margin: 0 0 2px; }
.card p.note { color: var(--text-secondary); margin: 0 0 8px; font-size: 13px; }
.legend { display: flex; flex-wrap: wrap; gap: 14px; font-size: 13px; color: var(--text-secondary); margin: 4px 0 6px; }
.legend i { display: inline-block; width: 12px; height: 12px; border-radius: 3px; margin-right: 6px; vertical-align: -1px; }
svg { display: block; width: 100%; height: auto; overflow: visible; }
svg text { fill: var(--muted); font-size: 11px; font-variant-numeric: tabular-nums; }
svg text.val { fill: var(--text-secondary); font-size: 11px; }
svg text.lab { fill: var(--text-primary); font-size: 12px; }
#tip { font: 12px/1.4 system-ui, -apple-system, "Segoe UI", sans-serif; position: fixed; pointer-events: none; z-index: 10; background: var(--surface-1); color: var(--text-primary);
  border: 1px solid var(--border); border-radius: 8px; padding: 8px 10px; font-size: 12px; box-shadow: 0 4px 16px rgba(0,0,0,.12);
  display: none; min-width: 140px; }
#tip .t { color: var(--text-secondary); margin-bottom: 4px; }
#tip .r { display: flex; justify-content: space-between; gap: 12px; }
#tip i { display: inline-block; width: 8px; height: 8px; border-radius: 2px; margin-right: 6px; }
table { border-collapse: collapse; width: 100%; font-size: 13px; font-variant-numeric: tabular-nums; }
th, td { text-align: right; padding: 6px 8px; border-bottom: 1px solid var(--grid); }
th:first-child, td:first-child { text-align: left; }
th { color: var(--text-secondary); font-weight: 600; }
.scroll { overflow-x: auto; }
.foot { color: var(--muted); font-size: 12px; margin-top: 12px; }
</style>
</head>
<body>
<div class="viz-root"><div class="wrap">
  <h1>seatlock: Java/Spring Boot vs Rust</h1>
  <p class="sub">Same API, same PostgreSQL, same k6 load, same container limits (2 CPU, 1 GiB). Memory is the resident set (VmRSS) of the service process, sampled every second. Medians across valid runs.</p>
  <div class="tiles" id="tiles"></div>
  <div class="card"><h2>Memory over a full benchmark run</h2>
    <p class="note">Idle, then 5 minutes at ~415 req/s, a cooldown, a flash-sale burst and a ramp to saturation, then idle again. Java keeps its peak memory afterwards.</p>
    <div class="legend" id="lg1"></div><div id="c1"></div></div>
  <div class="card"><h2>Rust alone, on its own scale</h2>
    <p class="note">At the scale above, Rust is a flat line near zero. Here is the same run zoomed in.</p>
    <div id="c2"></div></div>
  <div class="card"><h2>Memory by phase</h2>
    <div class="legend" id="lg3"></div><div id="c3"></div></div>
  <div class="card"><h2>Latency at the same offered load (sustained phase)</h2>
    <p class="note">Garbage-collection pauses show up in Java's p99; Rust has no collector.</p>
    <div class="legend" id="lg4"></div><div id="c4"></div></div>
  <div class="card" id="nmtcard"><h2>Where the JVM's memory goes</h2>
    <p class="note">Java as shipped, after 2 minutes of load (Native Memory Tracking, committed MB). Only the heap holds the service's data; the rest is runtime the JVM pays regardless. Rust's whole process is shown for scale.</p>
    <div id="c5"></div></div>
  <div class="card"><h2>All numbers</h2><div class="scroll" id="tbl"></div><div class="foot" id="foot"></div></div>
</div><div id="tip"></div></div>
<script>
const DATA = __DATA__;
const COLOR = { "java": "var(--s-java)", "java-tuned": "var(--s-tuned)", "rust": "var(--s-rust)" };
const T = DATA.targets, tip = document.getElementById("tip");
const NS = "http://www.w3.org/2000/svg";
const el = (n, a = {}, p) => { const e = document.createElementNS(NS, n); for (const k in a) e.setAttribute(k, a[k]); if (p) p.appendChild(e); return e; };
const fmt = (v, d = 1) => v == null ? "–" : (Math.abs(v) >= 100 ? Math.round(v).toLocaleString() : v.toFixed(d));
const by = k => T.find(t => t.key === k);
function showTip(ev, title, rows) {
  tip.innerHTML = `<div class="t">${title}</div>` + rows.map(r => `<div class="r"><span><i style="background:${r[0]}"></i>${r[1]}</span><b>${r[2]}</b></div>`).join("");
  tip.style.display = "block";
  const w = tip.offsetWidth, h = tip.offsetHeight;
  tip.style.left = Math.min(window.innerWidth - w - 8, ev.clientX + 14) + "px";
  tip.style.top = Math.min(window.innerHeight - h - 8, ev.clientY + 14) + "px";
}
const hideTip = () => tip.style.display = "none";
function legend(id, keys) {
  document.getElementById(id).innerHTML = keys.map(k => `<span><i style="background:${COLOR[k]}"></i>${by(k).label}</span>`).join("");
}
function niceMax(v) { const p = Math.pow(10, Math.floor(Math.log10(v))); for (const m of [1, 1.2, 1.5, 2, 2.5, 3, 4, 5, 6, 8, 10]) if (m * p >= v) return m * p; return v; }
const ticks = v => [4, 5, 3, 6].find(n => Number.isInteger(+(v / n).toFixed(6))) || 4;

// ---- tiles ----
(function () {
  const r = by("rust"), j = by("java-tuned") || by("java");
  if (!r || !j) return;
  const tiles = [
    ["Memory at idle", r.mem.idle, j.mem.idle, "MiB"],
    ["Memory under load", r.mem.sustained_p50, j.mem.sustained_p50, "MiB"],
    ["Memory at peak", r.mem.peak, j.mem.peak, "MiB"],
    ["p99 latency under load", r.lat.p99, j.lat.p99, "ms"],
  ];
  document.getElementById("tiles").innerHTML = tiles.map(([k, rv, jv, u]) =>
    `<div class="tile"><div class="k">${k}</div><div class="v">${fmt(rv)} ${u}</div><div class="d">vs ${fmt(jv)} ${u} for ${j.label} · ${Math.round((1 - rv / jv) * 100)}% lower</div></div>`).join("");
})();

// ---- line charts ----
function lineChart(id, keys, yMaxHint) {
  const W = 840, H = 300, m = { l: 44, r: 96, t: 22, b: 30 };
  const svg = el("svg", { viewBox: `0 0 ${W} ${H}`, role: "img" }, document.getElementById(id));
  const xs = keys.flatMap(k => by(k).points.map(p => p[0]));
  const x0 = Math.min(...xs), x1 = Math.max(...xs);
  const yMax = niceMax(yMaxHint || Math.max(...keys.flatMap(k => by(k).points.map(p => p[1]))) * 1.08);
  const X = v => m.l + (v - x0) / (x1 - x0) * (W - m.l - m.r), Y = v => H - m.b - v / yMax * (H - m.t - m.b);
  for (const [name, [a, b]] of Object.entries(DATA.phases)) {
    if (name === "cooldown") continue;
    el("rect", { x: X(a), y: m.t, width: Math.max(0, X(b) - X(a)), height: H - m.t - m.b, fill: "var(--band)" }, svg);
    el("text", { x: X(a) + 4, y: m.t - 8 }, svg).textContent = name.replace("_", " ");
  }
  const nt = ticks(yMax);
  for (let i = 0; i <= nt; i++) {
    const v = yMax * i / nt;
    el("line", { x1: m.l, x2: W - m.r, y1: Y(v), y2: Y(v), stroke: i ? "var(--grid)" : "var(--axis)", "stroke-width": 1 }, svg);
    el("text", { x: m.l - 6, y: Y(v) + 4, "text-anchor": "end" }, svg).textContent = fmt(v, 0);
  }
  el("text", { x: m.l - 6, y: m.t - 8, "text-anchor": "end" }, svg).textContent = "MiB";
  for (let s = Math.ceil(x0 / 120) * 120; s <= x1; s += 120)
    el("text", { x: X(s), y: H - 10, "text-anchor": "middle" }, svg).textContent = `${Math.round(s / 60)} min`;
  for (const k of keys) {
    const pts = by(k).points;
    el("path", { d: pts.map((p, i) => (i ? "L" : "M") + X(p[0]).toFixed(1) + " " + Y(p[1]).toFixed(1)).join(""),
      fill: "none", stroke: COLOR[k], "stroke-width": 2, "stroke-linejoin": "round", "stroke-linecap": "round" }, svg);
    const last = pts[pts.length - 1];
    el("circle", { cx: X(last[0]), cy: Y(last[1]), r: 4, fill: COLOR[k], stroke: "var(--surface-1)", "stroke-width": 2 }, svg);
    el("text", { class: "lab", x: X(last[0]) + 8, y: Y(last[1]) + 4 }, svg).textContent = `${fmt(last[1])} MiB`;
  }
  const cross = el("line", { y1: m.t, y2: H - m.b, stroke: "var(--axis)", "stroke-width": 1, visibility: "hidden" }, svg);
  const hit = el("rect", { x: m.l, y: m.t, width: W - m.l - m.r, height: H - m.t - m.b, fill: "transparent" }, svg);
  hit.addEventListener("mousemove", ev => {
    const r = svg.getBoundingClientRect(), xv = x0 + ((ev.clientX - r.left) * W / r.width - m.l) / (W - m.l - m.r) * (x1 - x0);
    cross.setAttribute("x1", X(xv)); cross.setAttribute("x2", X(xv)); cross.setAttribute("visibility", "visible");
    const rows = keys.map(k => { const pts = by(k).points; let best = pts[0];
      for (const p of pts) if (Math.abs(p[0] - xv) < Math.abs(best[0] - xv)) best = p;
      return [COLOR[k], by(k).label, fmt(best[1]) + " MiB"]; });
    const ph = Object.entries(DATA.phases).find(([, [a, b]]) => xv >= a && xv <= b);
    showTip(ev, `${Math.max(0, Math.round(xv))} s${ph ? " · " + ph[0].replace("_", " ") : ""}`, rows);
  });
  hit.addEventListener("mouseleave", () => { hideTip(); cross.setAttribute("visibility", "hidden"); });
}

// ---- grouped columns ----
function columns(id, groups, keys, value, unit) {
  const W = 840, H = 280, m = { l: 44, r: 12, t: 22, b: 34 };
  const svg = el("svg", { viewBox: `0 0 ${W} ${H}`, role: "img" }, document.getElementById(id));
  const yMax = niceMax(Math.max(...groups.flatMap(g => keys.map(k => value(by(k), g[0]) || 0))) * 1.1);
  const Y = v => H - m.b - v / yMax * (H - m.t - m.b), gw = (W - m.l - m.r) / groups.length, bw = 24, gap = 2;
  const nt = ticks(yMax);
  for (let i = 0; i <= nt; i++) {
    const v = yMax * i / nt;
    el("line", { x1: m.l, x2: W - m.r, y1: Y(v), y2: Y(v), stroke: i ? "var(--grid)" : "var(--axis)", "stroke-width": 1 }, svg);
    el("text", { x: m.l - 6, y: Y(v) + 4, "text-anchor": "end" }, svg).textContent = fmt(v, 0);
  }
  el("text", { x: m.l - 6, y: m.t - 8, "text-anchor": "end" }, svg).textContent = unit;
  groups.forEach(([g, label], gi) => {
    const cx = m.l + gw * gi + gw / 2, total = keys.length * bw + (keys.length - 1) * gap;
    el("text", { class: "lab", x: cx, y: H - 12, "text-anchor": "middle" }, svg).textContent = label;
    keys.forEach((k, ki) => {
      const v = value(by(k), g); if (v == null) return;
      const x = cx - total / 2 + ki * (bw + gap), y = Y(v), h = Math.max(1, H - m.b - y), r = Math.min(4, h);
      el("path", { d: `M${x} ${H - m.b}V${y + r}Q${x} ${y} ${x + r} ${y}H${x + bw - r}Q${x + bw} ${y} ${x + bw} ${y + r}V${H - m.b}Z`, fill: COLOR[k] }, svg);
      el("text", { class: "val", x: x + bw / 2, y: y - 5, "text-anchor": "middle" }, svg).textContent = fmt(v);
      const hit = el("rect", { x: x - 2, y: m.t, width: bw + 4, height: H - m.t - m.b, fill: "transparent" }, svg);
      hit.addEventListener("mousemove", ev => showTip(ev, label, [[COLOR[k], by(k).label, `${fmt(v)} ${unit}`]]));
      hit.addEventListener("mouseleave", hideTip);
    });
  });
}

// ---- NMT bars ----
function nmt() {
  if (!DATA.nmt || !by("rust")) { document.getElementById("nmtcard").style.display = "none"; return; }
  const rows = DATA.nmt.rows.map(([k, v]) => [k, v, "java"]).concat([["Rust: entire process (peak)", by("rust").mem.peak, "rust"]]);
  const W = 840, rowH = 30, m = { l: 230, r: 70, t: 6 }, H = m.t + rows.length * rowH + 6;
  const svg = el("svg", { viewBox: `0 0 ${W} ${H}`, role: "img" }, document.getElementById("c5"));
  const xMax = niceMax(Math.max(...rows.map(r => r[1])) * 1.05), X = v => v / xMax * (W - m.l - m.r);
  rows.forEach(([k, v, s], i) => {
    const y = m.t + i * rowH + (rowH - 18) / 2, w = Math.max(1, X(v)), r = Math.min(4, w);
    el("text", { class: "lab", x: m.l - 10, y: y + 13, "text-anchor": "end" }, svg).textContent = k;
    el("path", { d: `M${m.l} ${y}H${m.l + w - r}Q${m.l + w} ${y} ${m.l + w} ${y + r}V${y + 18 - r}Q${m.l + w} ${y + 18} ${m.l + w - r} ${y + 18}H${m.l}Z`, fill: COLOR[s] }, svg);
    el("text", { class: "val", x: m.l + w + 6, y: y + 13 }, svg).textContent = `${fmt(v)} MB`;
    const hit = el("rect", { x: 0, y: m.t + i * rowH, width: W, height: rowH, fill: "transparent" }, svg);
    hit.addEventListener("mousemove", ev => showTip(ev, k, [[COLOR[s], s === "rust" ? "Rust" : "Java (as shipped)", `${fmt(v)} MB`]]));
    hit.addEventListener("mouseleave", hideTip);
  });
}

// ---- table ----
function table() {
  const rows = [
    ["Runs", t => t.runs, 0], ["RSS idle (MiB)", t => t.mem.idle], ["RSS sustained p50 (MiB)", t => t.mem.sustained_p50],
    ["RSS peak (MiB)", t => t.mem.peak], ["RSS after peak (MiB)", t => t.mem.post_peak], ["Threads at peak", t => t.threads, 0],
    ["Startup to healthy (s)", t => t.startup, 0], ["Sustained p50 (ms)", t => t.lat.p50, 2], ["Sustained p95 (ms)", t => t.lat.p95, 2],
    ["Sustained p99 (ms)", t => t.lat.p99, 2], ["Sustained throughput (req/s)", t => t.rps], ["Peak throughput (req/s)", t => t.peak_rps],
  ];
  document.getElementById("tbl").innerHTML = `<table><tr><th>Metric</th>${T.map(t => `<th>${t.label}</th>`).join("")}</tr>` +
    rows.map(([n, f, d]) => `<tr><td>${n}</td>${T.map(t => `<td>${fmt(f(t), d ?? 1)}</td>`).join("")}</tr>`).join("") + "</table>";
  document.getElementById("foot").innerHTML = (DATA.excluded.length ? "Excluded (invalid) runs: " + DATA.excluded.join(" · ") + "<br>" : "") +
    "Timeline charts plot each target's representative run (median peak): " + T.map(t => `${t.label} ${t.rep}`).join(", ") + ".";
}

const keys = T.map(t => t.key);
legend("lg1", keys); lineChart("c1", keys);
if (by("rust")) lineChart("c2", ["rust"], by("rust").mem.peak * 1.15);
legend("lg3", keys); columns("c3", [["idle", "Idle"], ["sustained_p50", "Sustained (p50)"], ["peak", "Peak"], ["post_peak", "After peak"]], keys, (t, g) => t.mem[g], "MiB");
legend("lg4", keys); columns("c4", [["p50", "p50"], ["p95", "p95"], ["p99", "p99"]], keys, (t, g) => t.lat[g], "ms");
nmt(); table();
</script>
</body>
</html>
"""


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("results", nargs="?", default=str(Path(__file__).parent / "results"))
    ap.add_argument("-o", "--out", default=None)
    args = ap.parse_args()
    results = Path(args.results)
    data = collect(results)
    if not data["targets"]:
        print("No valid benchmark runs found.", file=sys.stderr)
        return 1
    out = Path(args.out) if args.out else results / "REPORT.html"
    out.write_text(PAGE.replace("__DATA__", json.dumps(data, separators=(",", ":"))))
    print(f"wrote {out} ({out.stat().st_size // 1024} KiB)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
