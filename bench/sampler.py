#!/usr/bin/env python3
"""Samples one container's memory once per interval and appends rows to a CSV.

Two views of memory, because they answer different questions:

* Process (PID 1) from /proc/1/status: VmRSS (resident set, the headline number), VmHWM (peak
  RSS since start, catches spikes between samples), RssAnon (heap-like memory), RssFile (mapped
  binaries/jars), Threads.
* Container cgroup: memory.current and the `anon` line of memory.stat. This is what an ECS/K8s
  memory limit enforces, and it includes page cache the process does not own.

PID 1 is the service itself in both images (the Java entrypoint uses `exec`), so the short-lived
`sh`/`cat` started by `docker exec` never shows up in VmRSS. It does show up briefly in the cgroup
numbers (well under 1 MiB), so VmRSS is the primary metric.

The phase column comes from a text file that bench/run.sh rewrites at each phase boundary.

Usage: sampler.py --container sl-java --out results/x/memory.csv --phase-file results/x/phase
"""

from __future__ import annotations

import argparse
import csv
import signal
import subprocess
import sys
import time
from pathlib import Path

FIELDS = [
    "ts",
    "elapsed_s",
    "phase",
    "vm_rss_kib",
    "vm_hwm_kib",
    "rss_anon_kib",
    "rss_file_kib",
    "threads",
    "cgroup_current_bytes",
    "cgroup_anon_bytes",
]

# One exec per sample keeps the two views consistent with each other. cgroup v2 first, v1 fallback.
PROBE = (
    "grep -E '^(VmRSS|VmHWM|RssAnon|RssFile|Threads):' /proc/1/status; "
    "echo cgroup_current $(cat /sys/fs/cgroup/memory.current 2>/dev/null "
    "|| cat /sys/fs/cgroup/memory/memory.usage_in_bytes 2>/dev/null); "
    "echo cgroup_anon $(grep -E '^(anon|rss) ' /sys/fs/cgroup/memory.stat 2>/dev/null "
    "|| grep -E '^rss ' /sys/fs/cgroup/memory/memory.stat 2>/dev/null)"
)


def parse_probe(text: str) -> dict[str, int | None]:
    """Parses the probe's output into integer fields; missing values become None."""
    out: dict[str, int | None] = {k: None for k in FIELDS[3:]}
    status_keys = {
        "VmRSS": "vm_rss_kib",
        "VmHWM": "vm_hwm_kib",
        "RssAnon": "rss_anon_kib",
        "RssFile": "rss_file_kib",
        "Threads": "threads",
    }
    for line in text.splitlines():
        parts = line.replace(":", " ").split()
        if not parts:
            continue
        key = parts[0]
        if key in status_keys and len(parts) >= 2:
            out[status_keys[key]] = int(parts[1])  # /proc reports kB (KiB)
        elif key == "cgroup_current" and len(parts) >= 2:
            out["cgroup_current_bytes"] = int(parts[1])
        elif key == "cgroup_anon" and len(parts) >= 2:
            out["cgroup_anon_bytes"] = int(parts[-1])  # "cgroup_anon anon 12345"
    return out


def probe(container: str) -> dict[str, int | None] | None:
    try:
        result = subprocess.run(
            ["docker", "exec", container, "sh", "-c", PROBE],
            capture_output=True,
            text=True,
            timeout=5,
        )
    except subprocess.TimeoutExpired:
        return None
    if result.returncode != 0 and not result.stdout:
        return None
    return parse_probe(result.stdout)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--container", required=True)
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument("--phase-file", type=Path)
    ap.add_argument("--interval", type=float, default=1.0)
    args = ap.parse_args()

    running = True

    def stop(*_: object) -> None:
        nonlocal running
        running = False

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)

    args.out.parent.mkdir(parents=True, exist_ok=True)
    new_file = not args.out.exists()
    start = time.time()
    misses = 0
    with args.out.open("a", newline="") as fh:
        writer = csv.DictWriter(fh, fieldnames=FIELDS)
        if new_file:
            writer.writeheader()
        while running:
            tick = time.time()
            sample = probe(args.container)
            if sample is None:
                misses += 1
                if misses >= 10:
                    print(f"sampler: {args.container} unreachable 10x, stopping", file=sys.stderr)
                    return 1
            else:
                misses = 0
                phase = ""
                if args.phase_file and args.phase_file.exists():
                    phase = args.phase_file.read_text().strip()
                writer.writerow(
                    {
                        "ts": f"{tick:.3f}",
                        "elapsed_s": f"{tick - start:.1f}",
                        "phase": phase,
                        **sample,
                    }
                )
                fh.flush()
            time.sleep(max(0.0, args.interval - (time.time() - tick)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
