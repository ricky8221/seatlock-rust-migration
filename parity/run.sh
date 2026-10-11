#!/usr/bin/env bash
# Parity run: Java (the oracle) and Rust side by side on fresh databases, the same seeded request
# sequence against both, then a normalized diff and the concurrency races.
#
#   parity/run.sh [random-steps] [seed]        # defaults: 1500 steps, seed 42
#
# Results: parity/results/<timestamp>/{java,rust}.jsonl, compare.txt, races-*.txt
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"
STEPS="${1:-1500}"; SEED="${2:-42}"
OUT="parity/results/$(date -u +%Y%m%dT%H%M%SZ)"; mkdir -p "$OUT"

source "$ROOT/scripts/stack-guard.sh"
require_idle_stack

cleanup() {
  docker rm -f sl-parity-java sl-parity-rust >/dev/null 2>&1 || true
  docker compose down -v --remove-orphans >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup

docker compose up -d --wait postgres
for db in seats_java seats_rust; do
  docker exec sl-postgres psql -q -U seats -d seats -c "CREATE DATABASE $db"
  docker compose run --rm -T flyway -url=jdbc:postgresql://postgres:5432/$db -user=seats -password=seats migrate >/dev/null
done
docker compose --profile java --profile rust build java rust
docker compose --profile java run -d --no-deps --name sl-parity-java -p 127.0.0.1:8090:8080 \
  -e DB_URL=jdbc:postgresql://postgres:5432/seats_java java >/dev/null
docker compose --profile rust run -d --no-deps --name sl-parity-rust -p 127.0.0.1:8091:8080 \
  -e DB_URL=jdbc:postgresql://postgres:5432/seats_rust rust >/dev/null
for p in 8090 8091; do
  for _ in $(seq 120); do curl -fs -o /dev/null "localhost:$p/actuator/health" && break; sleep 0.5; done
done

# One shared start time for the "event starts mid-run" case keeps both request streams identical.
SOON=$(python3 -c 'import datetime as d; print((d.datetime.now(d.timezone.utc)+d.timedelta(seconds=10)).strftime("%Y-%m-%dT%H:%M:%SZ"))')
python3 parity/capture.py --base http://localhost:8090 --out "$OUT/java.jsonl" --random-steps "$STEPS" --seed "$SEED" --soon "$SOON" &
python3 parity/capture.py --base http://localhost:8091 --out "$OUT/rust.jsonl" --random-steps "$STEPS" --seed "$SEED" --soon "$SOON" &
wait

status=0
python3 parity/compare.py "$OUT/java.jsonl" "$OUT/rust.jsonl" --show 50 | tee "$OUT/compare.txt" || status=1
for svc in "java 8090 seats_java" "rust 8091 seats_rust"; do
  set -- $svc
  python3 parity/races.py --base "http://localhost:$2" > "$OUT/races-$1.txt" || status=1
  docker exec sl-postgres psql -U seats -d "$3" -tAc "SELECT 'violations', \
    (SELECT count(*) FROM seat WHERE status <> 'AVAILABLE' AND hold_id IS NULL) + \
    (SELECT count(*) FROM (SELECT hold_id FROM booking GROUP BY hold_id HAVING count(*) > 1) x)" >> "$OUT/races-$1.txt"
done
echo "== races (outcome counts; which request wins is scheduling, the counts are the contract)"
paste -d'\n' "$OUT/races-java.txt" "$OUT/races-rust.txt" | sed 's/^/  /'
echo "results in $OUT"
exit $status
