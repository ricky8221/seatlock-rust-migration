# Parity: proving the Rust service behaves like the Java one

The Java service at the pinned commit is the **oracle**. Nothing is compared against a spec or
against what the code "should" do: only against what Java actually returns.

| Script | What it does |
|---|---|
| `capture.py` | Drives one service through ~140 curated cases plus a seeded random walk, records every exchange (status, key headers, body) as JSONL |
| `compare.py` | Normalizes both captures (UUIDs by first appearance, "now" timestamps, set-valued lists) and classifies every remaining difference |
| `races.py` | Fires the same races at a service (50 holds on one seat, 20 confirms of one hold, release vs confirm, 20 retries with one Idempotency-Key, ...) and reports outcome counts plus database invariants |
| `run.sh` | All of the above with Docker: fresh databases, Java on :8090, Rust on :8091 |

```bash
parity/run.sh            # 1500 random steps, seed 42
parity/run.sh 300 7      # quicker
```

## Progressive results

| Run | Exchanges | Identical after normalization | Unexplained |
|---|---|---|---|
| First run, 200 random steps | 341 | 77 | 365 (cascading from 4 real differences) |
| After fixes 1-4 below | 341 | 320 | 41 |
| After fixes 5-7 | 441 | 435 | 6 (Allow header order) |
| `Allow` compared as a set | 441 | 441 | 0 |
| 1,500 random steps, seed 42 | 1,638 | 1,638 | 0 |

Races: the three single-winner races (50 holds on one seat, 20 confirms of one hold, 10 cancels
of one booking) always give identical counts. In the other three (overlapping holds, release vs
confirm, 20 retries with one Idempotency-Key), which request wins depends on thread scheduling and
varies between runs on both services: for example Java's release won 15 of 15 rounds in one run,
and Rust's confirm won 1 of 15 in another. What must hold, and did in every run, is the contract:
exactly one winner per round, no seat claimed twice, one hold per key, and zero invariant
violations in the database.

## Accepted differences

* **`Allow` header order.** Java builds it from a hash set; across JVM restarts the same route
  answered `GET, POST` and `POST, GET`. Rust sends sorted order; the compare checks the sets.
* **Repeated non-String query parameter that fails conversion** (`?size=x&size=1`): Java's detail
  contains `String[].toString()`, e.g. `[Ljava.lang.String;@3af0288c`, an identity hash that differs
  between two Java runs. Rust prints the same shape with its own hex.
* **Validation `errors` order**, and the key order *inside* each error object: Hibernate reports
  violations in no fixed order and `Map.of` randomizes per JVM. Compared as a set.

See FINDINGS.md for every difference found and how it was fixed.
