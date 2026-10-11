# seatlock-rust-migration

A Java 21 / Spring Boot 3.5 service, [seatlock](https://github.com/srujanmalakpata/seatlock)
(event seat holds with expiring holds, bookings and idempotent retries, on PostgreSQL via Spring
Data JPA), ported to Rust with full functional parity and a measured reduction in memory.

The original is pinned as a git submodule in `reference/` at
`f345bda22c9e11d7a5b2440d5416c3e1e899500f` and is never modified.

## Results

| | Java as shipped | Java tuned for memory | Rust | Rust vs tuned Java |
|---|---|---|---|---|
| RSS idle | 294.2 MiB | 275.7 MiB | **4.4 MiB** | −98% |
| RSS under sustained load (p50) | 383.3 MiB | 333.2 MiB | **7.8 MiB** | −98% |
| RSS peak | 457.3 MiB | 366.6 MiB | **18.9 MiB** | −95% |
| Sustained p99 latency | 30.7 ms | 44.3 ms | **5.6 ms** | −87% |
| Peak throughput | 1,887 req/s | 1,894 req/s | **1,902 req/s** | equal |

Medians of three runs per target, same container limits (2 CPUs, 1 GiB, no swap).

Parity:

- 1,638 of 1,638 exchanges identical to Java after normalization.
- 0 invariant violations in the concurrency races.
- 26 of the 27 upstream integration tests pass against Rust; the 27th is white-box and has a
  Rust twin test.

**[Write-up](docs/WRITEUP.md)** (approach, lessons, optimizations with MiB, improvements,
challenges, gotchas) · [Benchmark comparison](bench/results/COMPARISON.md) ·
[Report (PDF)](bench/results/REPORT.pdf) · [Interactive report](bench/results/REPORT.html) (open locally; GitHub shows its source) · [Parity findings](parity/FINDINGS.md)

![Resident memory through a benchmark run](docs/img/memory-timeline.png)

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

The Rust optimization experiments (each compared with the plain `rust` runs):

```bash
bench/run.sh rust-mimalloc run1                              # allocator builds (Cargo features)
bench/run.sh rust-jemalloc run1
VARIANT=arena2  MALLOC_ARENA_MAX=2     bench/run.sh rust run1   # env-only variants
VARIANT=pool5   DB_POOL_SIZE=5         bench/run.sh rust run1
VARIANT=1worker TOKIO_WORKER_THREADS=1 bench/run.sh rust run1   # the one kept: also run2, run3
```

### Why a run takes about 13 minutes

A run is a fixed timeline, not a fixed amount of work, so a faster service does not finish sooner:

| Phase | Duration | Why it is that long |
|---|---|---|
| Startup | ~5–30 s | Compose up, Flyway, wait for `/actuator/health` |
| Idle | 60 s | Lets startup allocations settle, so idle RSS is a floor rather than a startup spike |
| Sustained | 5 min | Long enough for heaps, caches, pools and allocator arenas to reach steady state |
| Cooldown | 30 s | Separates the two load phases in the memory timeline |
| Peak | ~3.8 min | A flash-sale burst, then a ramp to saturation |
| Post-peak | 60 s | Shows how much memory is returned after the spike |

k6 sends requests at a constant arrival rate (an open model), so latency and memory are compared
at the same offered load. That's why the phases can't end early.

The committed results are 16 runs:

| What | Runs | Time |
|---|---|---|
| Main comparison: `java`, `java-tuned`, `rust` × 3 | 9 | about 2 hours |
| Rust experiments: 5 screening runs, plus 2 more to confirm `1worker` | 7 | about 90 minutes |

The allocator builds also compile a new image the first time. On a laptop, wrap a batch in
`caffeinate -i` so the Mac doesn't sleep mid-run.

`IDLE_SECONDS`, `POST_PEAK_SECONDS`, `COOLDOWN_SECONDS` and `DURATION` shorten a run for a smoke
test (the peak phase has no knob). Results are only comparable between runs made with the same
settings, so all committed results use the defaults: a shortened run reads a few MiB higher
because the service has not settled.

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
