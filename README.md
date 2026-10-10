# seatlock-migration

Migration of [seatlock](https://github.com/srujanmalakpata/seatlock) (Java 21, Spring Boot 3.5,
Spring Data JPA, PostgreSQL) to Rust, with functional parity and a measured reduction in memory.
The original service is pinned as a git submodule in `reference/` at `f345bda`.

| Path | What it is |
|---|---|
| `reference/` | The original Java service, unmodified, at the pinned commit |
| `seatlock-rs/` | The Rust service |
| `compose.yaml` | PostgreSQL, a one-shot Flyway migration, the Java and Rust services, and k6 |
| `bench/` | Memory sampler, k6 load scripts, run orchestration, result summaries |
| `parity/` | Differential replay harness (Java as oracle) |
| `docs/WRITEUP.md` | Technical write-up |

## Prerequisites

Docker with Compose v2, Python 3, curl, git. On Docker Desktop give the VM at least 6 CPUs and
6 GB of memory: the service under test (2 CPUs, 1 GiB), PostgreSQL and k6 run side by side.

## Quick start

```bash
git clone --recurse-submodules <this repo> && cd seatlock-migration
# or, in an existing clone: git submodule update --init

docker compose up -d postgres flyway              # schema, from reference/ migrations
docker compose --profile java up -d --build java  # Java service on :8080
curl -s localhost:8080/actuator/health
reference/scripts/smoke-test.sh http://localhost:8080
docker compose --profile java down -v
```

Swap `java` for `rust` to run the Rust service on the same port.

## Benchmarks

```bash
bench/run.sh java          # Java as shipped (G1, MaxRAMPercentage=75)
bench/run.sh java-tuned    # Java tuned for memory: the baseline Rust is compared against
bench/run.sh rust
bench/summarize.py compare # median of all runs per target -> bench/results/COMPARISON.md
bench/jvm-nmt.sh           # diagnostic: JVM Native Memory Tracking breakdown (not a benchmark)
```

Each run starts from an empty database and goes through these phases, sampling memory every
second: startup, one warm-up request, **idle** (60 s), **sustained** (k6, constant arrival rate,
about 440 req/s for 5 min), cooldown, **peak** (k6: a flash-sale burst on 200 seats, then a ramp
to saturation), and post-peak. Run each target three times; the comparison reports medians.

Results land in `bench/results/<target>/<label>/`: `memory.csv`, the k6 summaries, the app log,
`meta.txt` (exact flags, limits and commits) and `summary.json`.

Tunables: `APP_CPUS`, `APP_MEM`, `DB_POOL_SIZE`, `IDLE_SECONDS`, `DURATION`, `BROWSE_RPS`,
`RESERVE_RPS`, `FLASH_RPS`, `PEAK_RPS`. Set `DEBUG=1` to log unexpected HTTP statuses from k6.

### What is measured

- **Memory:** VmRSS of the service process (PID 1) is the headline number; VmHWM catches spikes
  between samples; the container cgroup's `memory.current` is recorded alongside it.
- **Latency:** p50/p95/p99 at the same offered load (sustained phase), so the services are compared
  under identical demand. Peak shows where each one saturates.
- **Equal work:** both services use a 10-connection pool, neither runs migrations (the `flyway`
  container applies the reference SQL once), and k6 counts holds, conflicts and bookings so a run
  can show both services did the same business work.

## Building the Rust service locally

```bash
cd seatlock-rs && cargo build --release && cargo test
```

Commit the generated `Cargo.lock`; the Docker build uses it when present.
