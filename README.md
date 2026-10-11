# seatlock-rust-migration

A Java 21 / Spring Boot 3.5 service, [seatlock](https://github.com/srujanmalakpata/seatlock)
(event seat holds with expiring holds, bookings and idempotent retries, on PostgreSQL via Spring
Data JPA), ported to Rust with full functional parity and a measured reduction in memory.

The original is pinned as a git submodule in `reference/` at
`f345bda22c9e11d7a5b2440d5416c3e1e899500f` and is never modified.

| Path | What it is |
|---|---|
| `reference/` | The original Java service at the pinned commit |
| `seatlock-rs/` | The Rust service (axum, sqlx, tokio) |
| `parity/` | Oracle-based equivalence tests: Java's responses are the source of truth |
| `it/` | The reference repository's own integration tests, run against either service |
| `bench/` | Memory sampler, k6 workloads, benchmark orchestration and results |
| `compose.yaml` | PostgreSQL, a one-shot Flyway migration, both services, k6 |
| `docs/WRITEUP.md` | Technical write-up |

## Reproduce from a clean checkout

Requirements: **Docker** (with Compose v2), **git**, **Python 3.9+** and **curl**. Nothing else:
both services, Maven, Flyway and k6 build or run in containers. On Docker Desktop give the VM at
least 6 CPUs and 6 GB of memory.

```bash
git clone --recurse-submodules https://github.com/ricky8221/seatlock-rust-migration.git
cd seatlock-rust-migration
```

`--recurse-submodules` matters: without it `reference/` is empty and the Java build fails. In an
existing clone run `git submodule update --init`.

### 1. Functional parity (about 3 minutes)

```bash
parity/run.sh
```

Starts Java and Rust side by side on fresh databases, sends both the same ~140 curated requests
plus a 1,500-step seeded random walk, diffs every response after normalization, then runs the
concurrency races. Expected tail:

```
exchanges compared: 1638, identical after normalization: 1638
accepted differences: 0, unexplained findings: 0
```

### 2. The reference integration tests (about 4 minutes each)

```bash
it/run.sh java   # the 27 upstream ITs against the original service (baseline)
it/run.sh rust   # the same tests against the Rust service
```

### 3. Benchmarks (about 13 minutes per run)

```bash
bench/run.sh java-tuned     # the JVM tuned for memory: the baseline Rust is compared against (see below)
bench/run.sh java           # the JVM as shipped
bench/run.sh rust
bench/summarize.py compare  # median of all runs per target -> bench/results/COMPARISON.md
bench/jvm-nmt.sh            # diagnostic: where the JVM's memory goes (Native Memory Tracking)
python3 bench/charts.py     # PNG charts for docs/WRITEUP.md -> docs/img/ (needs matplotlib)
```

Each run starts from an empty database and goes through startup, idle, sustained load (k6 at a
constant ~440 req/s for 5 minutes), cooldown, peak (a flash-sale burst on 200 seats, then a ramp
to saturation) and post-peak, sampling the service's resident memory every second. Run each target
three times; the comparison reports medians. Committed results are in `bench/results/`.

### Why there are two Java baselines

Rust is compared against the JVM **tuned for memory**, not only against the JVM as shipped.
The shipped settings let the heap grow to 75% of the container and Tomcat run 200 threads, so
comparing against them alone would overstate the gain: much of that memory is headroom that a
configuration change removes, no rewrite needed. The tuned variant (`java-tuned` in
`compose.yaml`) applies the changes a JVM team would reach for first:

| Flag | Why |
|---|---|
| `-XX:+UseSerialGC` | The smallest-footprint collector for a 2-CPU container |
| `-Xms64m -Xmx256m` | Caps heap headroom (the shipped image allows ~768 MB) |
| `-Xss512k` | Smaller thread stacks |
| `-XX:ReservedCodeCacheSize=64m`, `-XX:MaxMetaspaceSize=128m` | Caps the JIT code cache and class metadata |
| `SERVER_TOMCAT_THREADS_MAX=50` | 50 request threads instead of 200 |

Measuring both separates the two effects. Tuning recovers most of the JVM's *headroom* at peak,
but barely moves its idle floor: what remains is runtime (class metadata, JIT code, symbols,
thread stacks) that no flag removes, as `bench/jvm-nmt.sh` shows. Tuning also has a cost:
SerialGC's stop-the-world pauses raise tail latency. The migration case therefore rests on the
comparison against the *tuned* JVM, which is the percentage column in
`bench/results/COMPARISON.md`; the shipped JVM is reported alongside for context.

### 4. Rust unit and database tests (optional, needs Rust 1.85+)

```bash
cd seatlock-rs
cargo test                                   # unit tests
docker compose up -d --wait postgres flyway  # from the repo root, for the database tests
TEST_DATABASE_URL=postgres://seats:seats@localhost:5433/seats cargo test db_tests
```

## Running a service by hand

```bash
docker compose up -d postgres flyway
docker compose --profile rust up -d --build rust   # or: --profile java ... java
curl -s localhost:8080/actuator/health
reference/scripts/smoke-test.sh http://localhost:8080
docker compose --profile rust down -v
```

PostgreSQL is published on `localhost:5433` (not 5432, to avoid clashing with a local install).

## Configuration

The Rust service reads the same environment as the Java one: `DB_URL` (a JDBC URL), `DB_USER`,
`DB_PASSWORD`, `DB_POOL_SIZE` (10), `PORT` (8080), and the `seats.*` properties as
`SEATS_HOLD_TTL`, `SEATS_MAX_SEATS_PER_HOLD`, `SEATS_EXPIRY_BATCH_SIZE`,
`SEATS_EXPIRY_SWEEP_INTERVAL`, `SEATS_IDEMPOTENCY_RETENTION`, `SEATS_SCHEDULING_ENABLED`.
Rust-only: `TOKIO_WORKER_THREADS` (default: one per CPU), `RUST_LOG`, and
`SEATLOCK_TEST_HOOKS=1` for the integration tests (see `it/README.md`; never in production).

Benchmark knobs: `APP_CPUS`, `APP_MEM`, `IDLE_SECONDS`, `DURATION`, `BROWSE_RPS`,
`RESERVE_RPS`, `FLASH_RPS`, `PEAK_RPS`; `DEBUG=1` logs unexpected statuses from k6.
