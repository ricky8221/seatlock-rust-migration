| Metric | java (n=3) | java-tuned (n=3) | rust (n=3) | rust vs java-tuned |
|---|---|---|---|---|
| RSS idle (MiB) | 294.2 | 275.7 | 4.4 | -98% |
| RSS sustained p50 (MiB) | 383.3 | 333.2 | 7.8 | -98% |
| RSS peak (MiB) | 457.3 | 366.6 | 18.9 | -95% |
| RSS after peak (MiB) | 457 | 364.6 | 16.7 | -95% |
| cgroup sustained p50 (MiB) | 370.1 | 376.2 | 6.4 | -98% |
| Threads at peak | 222 | 72 | 3 | -96% |
| Startup to healthy (s) | 4 | 5 | 0 | -100% |
| Sustained p50 (ms) | 1.2 | 1.2 | 1.0 | -20% |
| Sustained p95 (ms) | 3.4 | 3.4 | 3.2 | -4% |
| Sustained p99 (ms) | 30.7 | 44.3 | 5.6 | -87% |
| Sustained throughput (req/s) | 412.6 | 413 | 414.8 | +0% |
| Peak throughput (req/s) | 1,887.3 | 1,893.5 | 1,901.9 | +0% |
| Peak p99 (ms) | 263.6 | 101.6 | 97.7 | -4% |
| Peak dropped iterations | 3,350 | 1,558 | 1,566 | +1% |
| Unexpected statuses (all) | 0 | 0 | 0 | – |

Medians across runs. Memory = VmRSS of the service process (PID 1). Latency compared at the same offered load (sustained). Percent column compares Rust with the java-tuned baseline; negative is lower.

## Rust optimization experiments (delta vs the plain `rust` build)

| Metric | rust (baseline) | rust-jemalloc | rust-mimalloc | rust@1worker | rust@arena2 | rust@pool5 |
|---|---|---|---|---|---|---|
| RSS idle (MiB) | 4.4 | 24.8 (+20.4) | 12.8 (+8.4) | 4.4 (+0.0) | 6.3 (+1.9) | 4.4 (+0.0) |
| RSS sustained p50 (MiB) | 7.8 | 28 (+20.2) | 17 (+9.2) | 7.2 (-0.6) | 9.6 (+1.8) | 7.6 (-0.2) |
| RSS peak (MiB) | 18.9 | 43.3 (+24.4) | 27.2 (+8.3) | 16.4 (-2.5) | 21.7 (+2.8) | 21.1 (+2.2) |
| RSS after peak (MiB) | 16.7 | 40.5 (+23.8) | 24.2 (+7.5) | 13.2 (-3.5) | 20 (+3.3) | 17.6 (+0.9) |
| Threads at peak | 3 | 3 (+0.0) | 3 (+0.0) | 2 (-1.0) | 3 (+0.0) | 3 (+0.0) |
| Sustained p99 (ms) | 5.6 | 5.8 (+0.2) | 6.1 (+0.4) | 5.8 (+0.2) | 5.9 (+0.2) | 6.3 (+0.7) |
| Peak throughput (req/s) | 1,901.9 | 1,891.9 (-10.0) | 1,889.9 (-12.0) | 1,888.5 (-13.4) | 1,890.6 (-11.3) | 1,882.4 (-19.5) |
| Peak p99 (ms) | 97.7 | 98.3 (+0.6) | 96.7 (-1.0) | 97.6 (-0.0) | 98.1 (+0.4) | 101.7 (+4.0) |
