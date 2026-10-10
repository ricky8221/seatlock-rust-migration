#!/usr/bin/env bash
# One full benchmark run for one target, from an empty database:
#
#   startup -> warmup (1 request) -> idle -> sustained (k6) -> cooldown -> peak (k6) -> post_peak
#
# Memory is sampled every second for the whole run; each sample is tagged with its phase.
#
#   bench/run.sh <java|java-tuned|rust> [run-label]
#
# Env overrides: IDLE_SECONDS (60), COOLDOWN_SECONDS (30), POST_PEAK_SECONDS (60),
# plus the k6 knobs documented in bench/k6/*.js (DURATION, BROWSE_RPS, RESERVE_RPS, FLASH_RPS,
# PEAK_RPS) and APP_CPUS / APP_MEM from compose.yaml.
set -euo pipefail

TARGET="${1:?usage: bench/run.sh <java|java-tuned|rust> [run-label]}"
case "$TARGET" in java|java-tuned|rust) ;; *) echo "unknown target: $TARGET" >&2; exit 2 ;; esac

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
LABEL="${2:-$(date -u +%Y%m%dT%H%M%SZ)}"
RUN_DIR="bench/results/${TARGET}/${LABEL}"
mkdir -p "$RUN_DIR"
chmod a+rwx "$RUN_DIR"   # the k6 container (non-root) writes its summaries here
PHASE_FILE="$RUN_DIR/phase"
CONTAINER="sl-${TARGET}"

IDLE_SECONDS="${IDLE_SECONDS:-60}"
COOLDOWN_SECONDS="${COOLDOWN_SECONDS:-30}"
POST_PEAK_SECONDS="${POST_PEAK_SECONDS:-60}"

phase() { echo "$1" > "$PHASE_FILE"; echo "== $(date -u +%T) phase: $1"; }

SAMPLER_PID=""
cleanup() {
  [[ -n "$SAMPLER_PID" ]] && kill "$SAMPLER_PID" 2>/dev/null || true
  docker logs "$CONTAINER" > "$RUN_DIR/app.log" 2>&1 || true
  docker compose --profile "$TARGET" down -v --remove-orphans >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "== run: target=$TARGET label=$LABEL dir=$RUN_DIR"
docker compose --profile java --profile java-tuned --profile java-nmt --profile rust \
  down -v --remove-orphans >/dev/null 2>&1 || true

# Record exactly what was measured.
{
  echo "target=$TARGET"
  echo "label=$LABEL"
  echo "started_utc=$(date -u +%FT%TZ)"
  echo "repo_commit=$(git rev-parse HEAD 2>/dev/null || echo uncommitted)"
  echo "reference_commit=$(git -C reference rev-parse HEAD)"
  echo "app_cpus=${APP_CPUS:-2} app_mem=${APP_MEM:-1g} db_pool=${DB_POOL_SIZE:-10}"
  echo "docker_arch=$(docker info --format '{{.Architecture}}') docker_cpus=$(docker info --format '{{.NCPU}}') docker_mem=$(docker info --format '{{.MemTotal}}')"
  docker compose --profile "$TARGET" config | sed -n "/^  ${TARGET}:/,/^  [a-z]/p"
} > "$RUN_DIR/meta.txt"

phase startup
docker compose up -d postgres flyway
docker compose --profile "$TARGET" up -d --build "$TARGET"

python3 bench/sampler.py --container "$CONTAINER" --out "$RUN_DIR/memory.csv" \
  --phase-file "$PHASE_FILE" &
SAMPLER_PID=$!

START=$(date +%s)
# -s without -S: while the JVM boots, Docker Desktop accepts the connection and closes it
# (curl 52 "Empty reply"); those expected failures are not worth printing.
until curl -fs -o /dev/null http://127.0.0.1:8080/actuator/health; do
  sleep 0.2
  if (( $(date +%s) - START > 180 )); then echo "service not healthy after 180s" >&2; exit 1; fi
done
echo "startup_seconds=$(( $(date +%s) - START ))" >> "$RUN_DIR/meta.txt"

phase warmup
curl -fsS -o /dev/null http://127.0.0.1:8080/api/v1/events?size=1

phase idle
sleep "$IDLE_SECONDS"

k6() {
  docker compose --profile load run --rm \
    -e BASE_URL="http://${TARGET}:8080" \
    -e SUMMARY_OUT="/results/${TARGET}/${LABEL}/$1-summary.json" \
    -e DURATION -e BROWSE_RPS -e RESERVE_RPS -e FLASH_RPS -e PEAK_RPS -e DEBUG \
    k6 run --quiet "/scripts/$1.js"
}

phase sustained
k6 sustained | tee "$RUN_DIR/sustained-k6.txt" || echo "k6 sustained reported threshold failures"

phase cooldown
sleep "$COOLDOWN_SECONDS"

phase peak
k6 peak | tee "$RUN_DIR/peak-k6.txt" || echo "k6 peak reported threshold failures"

phase post_peak
sleep "$POST_PEAK_SECONDS"

kill "$SAMPLER_PID" 2>/dev/null || true
wait "$SAMPLER_PID" 2>/dev/null || true
SAMPLER_PID=""

python3 bench/summarize.py run "$RUN_DIR"
