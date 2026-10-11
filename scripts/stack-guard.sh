# Sourced by bench/run.sh, parity/run.sh and it/run.sh. All three use the same Compose project
# and each starts by tearing it down, so running two at once silently breaks the first one
# (a benchmark lost its database mid-run this way). Refuse to start while another run is active.
require_idle_stack() {
  local running
  running=$(docker ps -q --filter label=com.docker.compose.project=seatlock-bench 2>/dev/null || true)
  if [[ -n "$running" && "${FORCE:-}" != 1 ]]; then
    echo "Another run is using the seatlock-bench stack:" >&2
    docker ps --filter label=com.docker.compose.project=seatlock-bench --format '  {{.Names}}  ({{.Status}})' >&2
    echo "Wait for it to finish, or stop it with 'docker compose down -v' (FORCE=1 to override)." >&2
    exit 1
  fi
}
