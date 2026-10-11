# seatlock: Java/Spring Boot → Rust migration

Reference service: [srujanmalakpata/seatlock](https://github.com/srujanmalakpata/seatlock) at
`f345bda22c9e11d7a5b2440d5416c3e1e899500f`, pinned as the `reference/` submodule and never modified.
Reproduction steps are in the [README](../README.md); the interactive benchmark report is
[`bench/results/REPORT.html`](../bench/results/REPORT.html).

## Summary

| | Java as shipped | Java tuned for memory | Rust | Rust vs tuned Java |
|---|---|---|---|---|
| RSS idle | 294.2 MiB | 275.7 MiB | **4.4 MiB** | −98% |
| RSS under sustained load (p50) | 383.3 MiB | 333.2 MiB | **7.8 MiB** | −98% |
| RSS peak | 457.3 MiB | 366.6 MiB | **18.9 MiB** | −95% |
| RSS after peak | 457.0 MiB | 364.6 MiB | **16.7 MiB** | −95% |
| Threads at peak | 222 | 72 | **3** | −96% |
| Sustained p99 latency | 30.7 ms (11.9–84.7) | 44.3 ms (19.6–94.9) | **5.6 ms (5.3–6.0)** | −87% |
| Peak throughput | 1,887 req/s | 1,894 req/s | **1,902 req/s** | equal |
| Unexpected statuses | 0 | 0 | 0 | |

Medians of three runs per target, ranges in brackets. Same container limits for every target
(2 CPUs, 1 GiB, no swap) on the same machine (Docker Desktop, aarch64, the architecture of Graviton).

Functional parity:

- **Oracle diff:** 1,638 of 1,638 request/response exchanges identical after normalization
  (~140 curated cases plus a 1,500-step seeded random walk).
- **Concurrency races:** six races, identical outcomes, 0 invariant violations on either service.
- **Upstream integration tests:** 26 of the 27 pass against Rust unchanged. The 27th is
  white-box (it holds a JPA persistence context open across an HTTP call) and has a Rust twin test.

## Why this service

The brief asks for ≥5 JSON endpoints, a relational database through JPA/JDBC and at least two
of validation, caching, scheduling, messaging and security filters. seatlock has:

- 11 JSON endpoints
- PostgreSQL through Spring Data JPA with Flyway migrations
- Bean Validation
- two `@Scheduled` jobs (hold expiry, idempotency-record cleanup)
- an idempotency servlet filter
- optimistic locking
- Actuator and Prometheus endpoints
- 27 HTTP-level integration tests

That last point mattered most: an existing test suite is an independent check on the port that
I didn't write.

It is also the kind of service where a rewrite pays off: thin request handling over a database.
In a service like that, the JVM's fixed runtime cost (class metadata, JIT code, thread stacks,
GC headroom) is most of the resident memory, rather than application data. A service that holds
a large in-memory cache would see a much smaller relative gain, because the data costs the same
in either language.

---

## 1. Approach

**Decide whether the memory is structural before rewriting anything.** I measured the Java
service twice: as shipped, and tuned with the flags a JVM team would reach for first (SerialGC,
`-Xmx256m`, smaller stacks, capped code cache and metaspace, 50 Tomcat threads). Native Memory
Tracking then showed where the remaining memory goes:

| JVM area (shipped, after load) | Committed |
|---|---|
| Java heap | 134 MB |
| Class metadata (metaspace + class space) | 99 MB |
| JIT code cache | 71 MB |
| Symbols | 32 MB |
| Thread stacks | 19 MB |
| Other | 25 MB |
| **Total** | **380 MB** (VmRSS 409 MB) |

Even with the heap set to zero, the JVM commits about 246 MB of non-heap memory for this service.
Tuning took 91 MiB off the peak but only 18.5 MiB off idle. What remains is runtime that no flag
removes, and that is what justifies a rewrite. Without that floor, tuning would have been the
cheaper answer.

**Build the oracle before writing the port.** Java is the source of truth:

- I probed the running Java service with curl for edge cases first. That found 15 of the 23
  behavioral differences before any Rust existed.
- `parity/` then runs both services side by side on fresh databases and diffs every response:
  status, key headers and body.
- Normalization covers only what is legitimately different: generated UUIDs (renamed by first
  appearance), "now" timestamps, set-valued lists, and the `Allow` header's order.
- The fixed gate was zero unexplained diffs.

**Port the behavior, not the framework.** The Rust service is axum + tokio + sqlx with
hand-written SQL that mirrors what Hibernate emits, including:

- optimistic locking: `version = version + 1 WHERE version = $n`
- Hibernate's flush order: inserts first, then updates sorted by entity name and Java UUID order
- the exact JSON coercion and validation messages

There is no ORM, no reflection and no per-request allocation of an entity graph.

**Order of work:**

1. Pin the reference service and stand up a compose environment where every target gets the
   same limits and a freshly migrated database.
2. Measure the Java baselines.
3. Probe Java's edge-case behavior.
4. Port module by module.
5. Run the oracle diff until clean.
6. Run the upstream ITs against Rust.
7. Benchmark.
8. Screen the optimization experiments, then confirm the winner.

## 2. Lessons learnt

1. **Parity work is mostly framework semantics, not business logic.** Almost none of the 23
   findings were about seat-hold rules. They were about:
   - Jackson coercing `"rows":"2"` to 2
   - `String.trim()` vs `strip()` disagreeing on what whitespace is
   - `@Size` counting UTF-16 units
   - PostgreSQL rounding timestamps where the Rust driver truncates
   - Spring binding a repeated query parameter differently for `int` and `String`

   Rewrites underestimate this because none of it is visible in the application's own code.
2. **Probe before porting.** Each difference found by curl up front cost one line in a table.
   Each one found by the diff later cost a debugging session, because one real difference
   cascades into hundreds of diffs. My first 200-step random walk showed 365 diffs from 4 real
   causes.
3. **The tuned JVM is the honest baseline.** Against the shipped JVM, Rust saves 438 MiB at
   peak. Against the tuned JVM it saves 348 MiB. The second number is the one a reviewer can trust.
4. **Measure the noise before claiming an optimization.** The three plain Rust runs agree within
   0.3 MiB at idle and 0.2 MiB at peak, but vary by 2.3 MiB after peak. Without those three
   runs, one lucky run of a variant would have looked like a win.
5. **JVM tail latency is not repeatable run to run.** The tuned JVM's sustained p99 ranged from
   19.6 to 94.9 ms across three identical runs (SerialGC pauses). A single run would have
   given any number in that range.
6. **Benchmarks need isolation guards, not discipline.** One Java run overlapped with a parity
   run that tore down the shared Postgres and produced 30,808 failed requests. The harness now:
   - refuses to start while the benchmark stack is up
   - wipes a run's directory before re-using its label
   - excludes runs with failures from the comparison, listing them

## 3. Areas of optimization (each with measured MiB)

What the migration removed, measured against the tuned JVM, then the Rust-side experiments.

**The migration itself**

| Area | Before (tuned JVM) | After (Rust) | Impact |
|---|---|---|---|
| Runtime floor (idle RSS): no class metadata, JIT code cache or symbol table | 275.7 MiB | 4.4 MiB | **−271 MiB** |
| Steady state (sustained p50): exact allocation instead of GC headroom | 333.2 MiB | 7.8 MiB | **−325 MiB** |
| Peak: no heap growth to absorb the burst | 366.6 MiB | 18.9 MiB | **−348 MiB** |
| Threads: async runtime instead of thread-per-request | 72 threads | 3 threads | 19 MB of thread stacks in the shipped JVM (NMT) → 2 workers + main |
| Memory returned after the peak | 2.0 MiB returned | 2.2 MiB returned | both keep most of their peak |

**Rust-side experiments** (vs the plain build; noise band from the three plain runs)

| Experiment | Runs | Peak RSS | Sustained RSS | Latency / throughput | Verdict |
|---|---|---|---|---|---|
| `TOKIO_WORKER_THREADS=1` | 3 | **−2.5 MiB** (16.4 vs 18.7–18.9) | **−0.6 MiB** (7.2 vs 7.6–7.8) | unchanged | **Kept as a deployment option** |
| `DB_POOL_SIZE=5` (from 10) | 1 | +2.2 MiB | −0.2 MiB | dropped requests 4,212 vs 1,297–2,619 | Rejected |
| `MALLOC_ARENA_MAX=2` | 1 | +2.8 MiB | +1.8 MiB | unchanged | Rejected |
| mimalloc | 1 | +8.3 MiB | +9.2 MiB | unchanged | Rejected |
| jemalloc | 1 | +24.4 MiB | +20.2 MiB | unchanged | Rejected |

Why `1worker` isn't the default: one worker kept up here because the peak is bound by Postgres,
not by the service's CPU. A CPU-heavier endpoint would lose throughput. It belongs on small
tasks (≤1 vCPU), where the second worker has no core to run on anyway.

At this footprint the default glibc allocator is the smallest of the three. I haven't verified
why. My hypotheses:

- jemalloc sizes its arenas from the host's CPU count (10) rather than the container's 2.
- mimalloc reserves memory in large segments.

The usual advice to switch allocators to cut memory comes from heap sizes where fragmentation
dominates. That isn't the case at 8 MiB.

**On the Java side, for comparison**

| Change | Impact |
|---|---|
| JVM as shipped → tuned flags | peak −91 MiB, idle −18.5 MiB, threads 222 → 72; sustained p99 not improved (30.7 → 44.3 ms median) |

## 4. Improvements (ranked by expected memory benefit and effort)

Not done in this submission; ordered by what I'd do next.

1. **Size the deployment to the measured footprint.** This is the largest saving and costs no
   code. Rust peaks at 19 MiB inside a 1 GiB limit. On ECS Fargate the smallest task size
   (0.25 vCPU / 512 MB) is far more than enough, against a JVM that needs its 1 GiB. On EC2 or
   ECS-on-EC2, bin-packing gains even more.
2. **Use `TOKIO_WORKER_THREADS=1` on ≤1 vCPU tasks.** Measured −2.5 MiB at peak. It's a config
   change, but it should be tied to the task's CPU size, not hard-coded.
3. **Bound peak memory with a concurrency limit.** RSS grows from 7.8 to 18.9 MiB at saturation:
   in-flight requests and their buffers. A `tower` concurrency limit with load shedding (503 +
   `Retry-After`) would cap the peak at a chosen point. Not measured; it trades excess load
   for memory, so the limit needs choosing from the throughput curve.
4. **Return memory after the peak.** Rust keeps about 2 MiB less than its peak afterwards (glibc
   retains freed pages). A periodic `malloc_trim(0)` when idle should return most of the rest.
   That's a few MiB at most, and a small change.
5. **Shrink idempotency-record handling.** Stored responses are replayed byte for byte from
   Postgres. Large bodies are read fully into memory. Streaming them would only matter with
   large responses, which this API doesn't have today.

## 5. Challenges

- **Byte-for-byte idempotent replay.** The Java filter stores the exact response bytes and
  replays them. A Rust response that differed only in JSON key order would differ forever on
  replay. This forced exact key order everywhere: structs with declared field order, and the
  OpenAPI document served from its captured text rather than re-serialized.
- **Running Java's integration tests against a Rust process.** Several tests call Spring beans
  directly (run the expiry job, expire one hold at a given instant) and drive a mutable clock.
  The fix was test-only hooks in the Rust service (`SEATLOCK_TEST_HOOKS=1`) plus a test-side
  patch with `@Primary` beans that forward over HTTP. One test still can't cross the boundary:
  it holds a JPA persistence context across an HTTP call. Its Rust twin forces the same
  interleaving with an explicit transaction.
- **Reproducing Spring MVC's edge behavior in axum.** Examples:
  - 405 with an `Allow` header
  - `OPTIONS` answered with an empty `Accept-Patch`
  - "No static resource" 404s
  - Actuator's vendor media type

  axum rewrites `Allow` after per-route middleware has run, so the problem rendering had to move
  around the whole router, driven by a route table that also feeds the metrics `uri` tag.
- **A fair benchmark.** Making the comparison fair took these decisions:
  - Flyway runs once, outside both services, so neither pays for migrations.
  - Every run starts from an empty database.
  - k6 uses an open model (constant arrival rate), so both services face the same offered load
    and a slow service can't lower its own load.
  - Memory is VmRSS of the service process sampled every second, tagged by phase.

## 6. Gotchas

From the [findings log](../parity/FINDINGS.md) (23 entries) and the benchmark harness:

- **Timestamps:**
  - PostgreSQL rounds timestamps to microseconds (ties to even); sqlx truncates. That makes a
    1 µs difference on about half of all writes, visible when a POST response is compared with a
    later GET.
  - `ISO_INSTANT` always prints the fraction in groups of three (`.120Z`, never `.12Z`) and
    requires seconds on input.
- **UUIDs:**
  - `UUID.fromString` is lenient: `1-1-1-1-1` is a valid id.
  - Jackson's body parser is strict. The service needs both parsers.
  - Java sorts UUIDs as signed 64-bit halves; Rust and PostgreSQL sort them unsigned. Error
    messages list ids from a `TreeSet`, so the order is part of the output.
- **Strings:** `@NotBlank` uses `trim()` (chars ≤ U+0020) but the service stores `strip()`
  (Unicode whitespace). A name made of em spaces passes validation and is stored as `""`.
- **`json!` sorts object keys.** Spring's Actuator doesn't. That's harmless until a response is
  stored and replayed.
- **Spring's lenient booleans.** `SEATLOCK_TEST_HOOKS=1` is `true` to Spring and an error to
  `bool::from_str`. The service exited at startup, and the tests only saw a DNS error for its
  container.
- **`Allow` order changes between JVM restarts** (hash-set order). The oracle compares it as a set.
- **Benchmark harness:**
  - An append-mode memory log silently mixed two runs' samples under one label.
  - jemalloc and mimalloc both increased RSS here (see section 3).

---

## Benchmark methodology and fairness

- **Limits:** one service at a time, 2 CPUs / 1 GiB / no swap for every target, k6 in its own
  container with its own CPU limit.
- **Phases:** startup → idle (60 s) → sustained (constant ~440 req/s mixed browse/hold/confirm
  traffic, 5 min) → cooldown → peak (a flash-sale burst on 200 seats, then a ramp to
  saturation) → post-peak (60 s).
- **Memory:** VmRSS of PID 1 every second. The cgroup's memory figure is recorded alongside it.
- **Runs:** three per target, medians reported, invalid runs excluded automatically.
- **Two Java baselines:** shipped and tuned. Rust is judged against the tuned one.
- **Peak saturation:** all three targets saturate at about 1,900 req/s with similar peak p99
  (tuned Java 94–102 ms, Rust 97–99 ms). That ceiling is shared, most likely Postgres on the
  same machine, so peak throughput shows "equal", not Rust's own limit. The sustained phase is
  the like-for-like latency comparison.
- **Variance:** experiments are screened with one run and confirmed with three.
- **Known limitations:**
  - Docker Desktop on a laptop, not a cloud instance.
  - Postgres shares the host with the service under test.
  - Startup time is measured in whole seconds.

## How I'd ship it

The oracle carries over to production as a shadow deploy:

1. Mirror a share of real traffic to the Rust service.
2. Diff its responses with the same normalization `compare.py` uses.
3. Promote when the unexplained diff rate stays at zero.

The shadow must not write to the production database: point it at a copy, or mirror only reads,
because both services would otherwise apply the same hold twice. The schema is unchanged and
Flyway stays the owner of migrations, so rollback is a routing change. For a fleet of such
migrations, I'd codify the findings table as a reusable checklist: Jackson coercions, JDK string
semantics, timestamp precision, Spring MVC error shapes. These recur in every Spring service.

## AI assistance disclosure

I built this with Claude (Anthropic) as a coding assistant. What each of us did:

- **Me:**
  - chose the project and the overall plan
  - made the decisions recorded here (tuned-JVM baseline, which experiments to run, which
    optimization to keep)
  - ran every test and benchmark on my own machine
  - reviewed the results
- **Claude:**
  - wrote most of the Rust code, the parity, IT and benchmark tooling, and drafts of this document
  - analyzed results with me

Every number in this document comes from the committed results in `bench/results/`,
`parity/results/` and `it/results/`.
