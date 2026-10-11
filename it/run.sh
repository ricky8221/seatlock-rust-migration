#!/usr/bin/env bash
# The reference repository's own integration tests (27 HTTP-level ITs), unmodified except for
# it/rust-target.patch, run against either service:
#
#   it/run.sh rust     # against the Rust port (default)
#   it/run.sh java     # against the original Java service, as upstream runs them (baseline)
#
# The patch adds a switch (-Dseatlock.target=rust) that sends HTTP to the external service and
# forwards the tests' white-box calls (move the clock, run the expiry job now) to its test-only
# /__test hooks. Maven runs in a container, so no local JDK is needed. Reports are copied to
# it/results/<target>/.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"
TARGET="${1:-rust}"
case "$TARGET" in rust|java) ;; *) echo "usage: it/run.sh [rust|java]" >&2; exit 2 ;; esac
MAVEN_IMAGE="maven:3.9.11-eclipse-temurin-21"
NETWORK="seatlock-bench_default"

source "$ROOT/scripts/stack-guard.sh"
require_idle_stack

cleanup() {
  docker rm -f sl-it-rust >/dev/null 2>&1 || true
  docker compose down -v --remove-orphans >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup

# Fresh copy of the pinned reference with the patch applied.
rm -rf it/build && mkdir -p it/build
tar -C reference --exclude=.git --exclude=target -cf - . | tar -C it/build -xf -
git apply --directory=it/build it/rust-target.patch

MVN_ARGS=(-B -q verify -Dtest=NONE -Dsurefire.failIfNoSpecifiedTests=false -Djacoco.skip=true)
DOCKER_ARGS=(--rm -v "$ROOT/it/build:/w" -v "$HOME/.m2:/root/.m2" -w /w)

if [[ "$TARGET" == rust ]]; then
  docker compose up -d --wait postgres
  docker exec sl-postgres psql -q -U seats -d seats -c "CREATE DATABASE seats_it"
  docker compose run --rm -T flyway -url=jdbc:postgresql://postgres:5432/seats_it -user=seats -password=seats migrate >/dev/null
  docker compose --profile rust build rust
  # Same settings the base IT class gives the Java context: jobs off (tests drive them), 2-minute TTL.
  docker compose --profile rust run -d --no-deps --name sl-it-rust \
    -e DB_URL=jdbc:postgresql://postgres:5432/seats_it \
    -e SEATLOCK_TEST_HOOKS=1 -e SEATS_SCHEDULING_ENABLED=false -e SEATS_HOLD_TTL=PT2M rust >/dev/null
  # Fail fast, with the service's own logs, if it does not come up (instead of 27 DNS errors).
  for i in $(seq 60); do
    if docker run --rm --network "$NETWORK" curlimages/curl:8.10.1 -fs -o /dev/null \
         -X POST http://sl-it-rust:8080/__test/clock -d '{"advanceSeconds":0}' 2>/dev/null; then
      break
    fi
    if [[ "$(docker inspect -f '{{.State.Running}}' sl-it-rust 2>/dev/null)" != true || $i == 60 ]]; then
      echo "Rust service did not start with test hooks enabled:" >&2
      docker logs sl-it-rust >&2 || true
      exit 1
    fi
    sleep 1
  done
  docker run "${DOCKER_ARGS[@]}" --network "$NETWORK" "$MAVEN_IMAGE" \
    mvn "${MVN_ARGS[@]}" -Dseatlock.target=rust -Dseatlock.baseUrl=http://sl-it-rust:8080 \
      -Dseatlock.jdbcUrl=jdbc:postgresql://postgres:5432/seats_it && status=0 || status=$?
else
  # Upstream mode: Testcontainers starts PostgreSQL through the host's Docker socket.
  docker run "${DOCKER_ARGS[@]}" -v /var/run/docker.sock:/var/run/docker.sock \
    -e TESTCONTAINERS_HOST_OVERRIDE=host.docker.internal -e TESTCONTAINERS_RYUK_DISABLED=true \
    --add-host host.docker.internal:host-gateway "$MAVEN_IMAGE" \
    mvn "${MVN_ARGS[@]}" && status=0 || status=$?
fi

mkdir -p "it/results/$TARGET"
cp it/build/target/failsafe-reports/*.txt "it/results/$TARGET/" 2>/dev/null || true
echo "== $TARGET: integration test summary"
python3 - "$TARGET" <<'PY'
import glob, re, sys
tot = {"run": 0, "fail": 0, "err": 0, "skip": 0}
for f in sorted(glob.glob(f"it/results/{sys.argv[1]}/*.txt")):
    t = open(f).read()
    m = re.search(r"Tests run: (\d+), Failures: (\d+), Errors: (\d+), Skipped: (\d+)", t)
    if m:
        r, fa, e, s = map(int, m.groups())
        for k, v in zip(tot, (r, fa, e, s)):
            tot[k] += v
        print(f"  {f.split('/')[-1][:-4]:45} run {r:2}  failed {fa}  errors {e}  skipped {s}")
print(f"  TOTAL run {tot['run']}  failed {tot['fail']}  errors {tot['err']}  skipped {tot['skip']}")
PY
exit $status
