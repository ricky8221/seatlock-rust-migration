#!/usr/bin/env python3
"""Turns raw benchmark output into the numbers the write-up reports.

  summarize.py run <run-dir>         -> <run-dir>/summary.json (one run)
  summarize.py compare [results-dir] -> results-dir/COMPARISON.md (median of runs per target)

Memory definitions (MiB, from VmRSS of the service process):
  idle            median over the last 30 s of the idle phase
  sustained_p50   median over the sustained phase
  sustained_max   max over the sustained phase
  peak            max over the peak phase, also checked against VmHWM so a spike between two
                  samples is not missed
  post_peak       median over the last 30 s after peak: does memory come back down?

Latency/throughput come from the k6 summaries (sustained = fixed offered load, so latency is
comparable; peak = where each service saturates).
"""

from __future__ import annotations

import csv
import json
import statistics
import sys
from pathlib import Path

KIB_PER_MIB = 1024


def load_samples(run_dir: Path) -> list[dict]:
    rows = []
    with (run_dir / "memory.csv").open() as fh:
        for row in csv.DictReader(fh):
            if not row.get("vm_rss_kib"):
                continue
            rows.append(
                {
                    "elapsed": float(row["elapsed_s"]),
                    "phase": row["phase"],
                    "rss": int(row["vm_rss_kib"]) / KIB_PER_MIB,
                    "hwm": int(row["vm_hwm_kib"]) / KIB_PER_MIB if row["vm_hwm_kib"] else None,
                    "anon": int(row["rss_anon_kib"]) / KIB_PER_MIB if row["rss_anon_kib"] else None,
                    "threads": int(row["threads"]) if row["threads"] else None,
                    "cgroup": int(row["cgroup_current_bytes"]) / 2**20
                    if row["cgroup_current_bytes"]
                    else None,
                }
            )
    return rows


def tail_seconds(rows: list[dict], seconds: float) -> list[dict]:
    if not rows:
        return rows
    end = rows[-1]["elapsed"]
    return [r for r in rows if r["elapsed"] >= end - seconds]


def memory_stats(rows: list[dict]) -> dict:
    by_phase: dict[str, list[dict]] = {}
    for r in rows:
        by_phase.setdefault(r["phase"], []).append(r)

    def med(rs, key="rss"):
        vals = [r[key] for r in rs if r[key] is not None]
        return round(statistics.median(vals), 1) if vals else None

    def mx(rs, key="rss"):
        vals = [r[key] for r in rs if r[key] is not None]
        return round(max(vals), 1) if vals else None

    idle = tail_seconds(by_phase.get("idle", []), 30)
    sustained = by_phase.get("sustained", [])
    peak = by_phase.get("peak", [])
    post = tail_seconds(by_phase.get("post_peak", []), 30)

    peak_rss = mx(peak)
    peak_hwm = mx(peak, "hwm")
    return {
        "rss_mib": {
            "idle": med(idle),
            "sustained_p50": med(sustained),
            "sustained_max": mx(sustained),
            "peak": max(v for v in (peak_rss, peak_hwm) if v is not None) if peak else None,
            "post_peak": med(post),
        },
        "cgroup_mib": {
            "idle": med(idle, "cgroup"),
            "sustained_p50": med(sustained, "cgroup"),
            "peak": mx(peak, "cgroup"),
        },
        "threads": {"idle": med(idle, "threads"), "peak": mx(peak, "threads")},
        "samples": len(rows),
    }


def k6_stats(path: Path) -> dict | None:
    if not path.exists():
        return None
    data = json.loads(path.read_text())
    m = data["metrics"]

    def val(metric, key):
        return m.get(metric, {}).get("values", {}).get(key)

    dur = "http_req_duration"
    return {
        "latency_ms": {
            "p50": round(val(dur, "med"), 2) if val(dur, "med") is not None else None,
            "p95": round(val(dur, "p(95)"), 2) if val(dur, "p(95)") is not None else None,
            "p99": round(val(dur, "p(99)"), 2) if val(dur, "p(99)") is not None else None,
            "max": round(val(dur, "max"), 2) if val(dur, "max") is not None else None,
        },
        "throughput_rps": round(val("http_reqs", "rate") or 0, 1),
        "requests": val("http_reqs", "count"),
        "failed_rate": val("http_req_failed", "rate"),
        "dropped_iterations": val("dropped_iterations", "count") or 0,
        "unexpected_status": val("biz_unexpected_status", "count") or 0,
        "holds_placed": val("biz_holds_placed", "count") or 0,
        "hold_conflicts": val("biz_hold_conflicts", "count") or 0,
        "bookings_confirmed": val("biz_bookings_confirmed", "count") or 0,
    }


def summarize_run(run_dir: Path) -> dict:
    meta = {}
    meta_file = run_dir / "meta.txt"
    if meta_file.exists():
        for line in meta_file.read_text().splitlines():
            for part in line.split():
                if "=" in part and not part.startswith("-"):
                    k, v = part.split("=", 1)
                    meta.setdefault(k, v)
    summary = {
        "target": meta.get("target", run_dir.parent.name),
        "label": meta.get("label", run_dir.name),
        "startup_seconds": int(meta["startup_seconds"]) if "startup_seconds" in meta else None,
        "memory": memory_stats(load_samples(run_dir)),
        "sustained": k6_stats(run_dir / "sustained-summary.json"),
        "peak": k6_stats(run_dir / "peak-summary.json"),
    }
    (run_dir / "summary.json").write_text(json.dumps(summary, indent=2))
    return summary


def dig(d: dict | None, *keys):
    for k in keys:
        if d is None:
            return None
        d = d.get(k)
    return d


ROWS = [
    ("RSS idle (MiB)", ("memory", "rss_mib", "idle"), True),
    ("RSS sustained p50 (MiB)", ("memory", "rss_mib", "sustained_p50"), True),
    ("RSS peak (MiB)", ("memory", "rss_mib", "peak"), True),
    ("RSS after peak (MiB)", ("memory", "rss_mib", "post_peak"), True),
    ("cgroup sustained p50 (MiB)", ("memory", "cgroup_mib", "sustained_p50"), True),
    ("Threads at peak", ("memory", "threads", "peak"), True),
    ("Startup to healthy (s)", ("startup_seconds",), True),
    ("Sustained p50 (ms)", ("sustained", "latency_ms", "p50"), True),
    ("Sustained p95 (ms)", ("sustained", "latency_ms", "p95"), True),
    ("Sustained p99 (ms)", ("sustained", "latency_ms", "p99"), True),
    ("Sustained throughput (req/s)", ("sustained", "throughput_rps"), False),
    ("Peak throughput (req/s)", ("peak", "throughput_rps"), False),
    ("Peak p99 (ms)", ("peak", "latency_ms", "p99"), True),
    ("Peak dropped iterations", ("peak", "dropped_iterations"), True),
    ("Unexpected statuses (all)", None, True),
]


def compare(results_dir: Path) -> str:
    by_target: dict[str, list[dict]] = {}
    for f in sorted(results_dir.glob("*/*/summary.json")):
        s = json.loads(f.read_text())
        by_target.setdefault(s["target"], []).append(s)
    order = [t for t in ("java", "java-tuned", "rust") if t in by_target] + sorted(
        t for t in by_target if t not in ("java", "java-tuned", "rust")
    )
    if not order:
        return "No summary.json files found. Run bench/run.sh first.\n"

    def median_of(target, path):
        if path is None:
            vals = [
                (dig(s, "sustained", "unexpected_status") or 0) + (dig(s, "peak", "unexpected_status") or 0)
                for s in by_target[target]
            ]
        else:
            vals = [dig(s, *path) for s in by_target[target]]
        vals = [v for v in vals if v is not None]
        return statistics.median(vals) if vals else None

    def fmt(v):
        if v is None:
            return "–"
        return f"{v:,.1f}" if isinstance(v, float) and not v.is_integer() else f"{int(v):,}"

    has_rust = "rust" in by_target
    baseline = "java-tuned" if "java-tuned" in by_target else ("java" if "java" in by_target else None)
    header = ["Metric"] + [f"{t} (n={len(by_target[t])})" for t in order]
    if has_rust and baseline:
        header.append(f"rust vs {baseline}")
    lines = ["| " + " | ".join(header) + " |", "|" + "---|" * len(header)]
    for title, path, lower_is_better in ROWS:
        vals = [median_of(t, path) for t in order]
        row = [title] + [fmt(v) for v in vals]
        if has_rust and baseline:
            b, r = median_of(baseline, path), median_of("rust", path)
            if b not in (None, 0) and r is not None:
                pct = (r - b) / b * 100
                row.append(f"{pct:+.0f}%")
            else:
                row.append("–")
        lines.append("| " + " | ".join(row) + " |")
    note = (
        "\nMedians across runs. Memory = VmRSS of the service process (PID 1). "
        "Latency compared at the same offered load (sustained). "
        f"Percent column compares Rust with the {baseline} baseline; negative is lower.\n"
        if baseline
        else "\n"
    )
    return "\n".join(lines) + "\n" + note


def main(argv: list[str]) -> int:
    if len(argv) >= 2 and argv[0] == "run":
        print(json.dumps(summarize_run(Path(argv[1])), indent=2))
        return 0
    if argv and argv[0] == "compare":
        results = Path(argv[1]) if len(argv) > 1 else Path(__file__).parent / "results"
        table = compare(results)
        (results / "COMPARISON.md").write_text(table)
        print(table)
        return 0
    print(__doc__)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
