#!/usr/bin/env bash
# Diagnostic, not a benchmark: where does the JVM's RSS go?
#
# Boots the Java service on a JDK image with Native Memory Tracking, takes an NMT summary at idle,
# drives a short sustained load, and takes another. The output (heap vs metaspace vs code cache vs
# threads vs GC vs other native) backs the "JVM overhead is a structural floor" argument in the
# write-up. NMT itself costs a few MiB, which is why these numbers are never used as the baseline.
#
#   bench/jvm-nmt.sh [load-seconds]
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"
LOAD_SECONDS="${1:-120}"
OUT="bench/results/java-nmt/$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$OUT"; chmod a+rwx "$OUT"
trap 'docker compose --profile java-nmt down -v --remove-orphans >/dev/null 2>&1 || true' EXIT

docker compose --profile java --profile java-tuned --profile java-nmt --profile rust down -v --remove-orphans >/dev/null 2>&1 || true
docker compose up -d postgres flyway
docker compose --profile java-nmt up -d --build java-nmt
until curl -fsS -o /dev/null http://127.0.0.1:8080/actuator/health; do sleep 0.5; done
curl -fsS -o /dev/null "http://127.0.0.1:8080/api/v1/events?size=1"
sleep 30

nmt() { docker exec sl-java-nmt jcmd 1 VM.native_memory summary scale=MB > "$OUT/nmt-$1.txt"; \
        docker exec sl-java-nmt grep -E '^(VmRSS|Threads):' /proc/1/status >> "$OUT/nmt-$1.txt"; }
nmt idle
docker exec sl-java-nmt jcmd 1 VM.native_memory baseline >/dev/null

docker compose --profile load run --rm -e BASE_URL=http://java-nmt:8080 -e DURATION="${LOAD_SECONDS}s" \
  -e SUMMARY_OUT="/results/java-nmt/$(basename "$OUT")/sustained-summary.json" \
  k6 run --quiet /scripts/sustained.js >/dev/null || true
nmt after-load
docker exec sl-java-nmt jcmd 1 VM.native_memory summary.diff scale=MB > "$OUT/nmt-diff.txt"

echo "NMT summaries in $OUT"
grep -E '^(Total|-\s+(Java Heap|Class|Thread|Code|GC|Internal|Other|Metaspace|Symbol|Arena))|VmRSS' "$OUT/nmt-after-load.txt" || true
