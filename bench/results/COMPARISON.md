| Metric | java (n=3) | java-tuned (n=3) |
|---|---|---|
| RSS idle (MiB) | 290.4 | 274.1 |
| RSS sustained p50 (MiB) | 390 | 328.7 |
| RSS peak (MiB) | 468.6 | 365.2 |
| RSS after peak (MiB) | 467.7 | 363.2 |
| cgroup sustained p50 (MiB) | 376.3 | 317.9 |
| Threads at peak | 222 | 72 |
| Startup to healthy (s) | 4 | 5 |
| Sustained p50 (ms) | 1.2 | 1.2 |
| Sustained p95 (ms) | 3.6 | 3.7 |
| Sustained p99 (ms) | 40.1 | 76.8 |
| Sustained throughput (req/s) | 412.4 | 412.6 |
| Peak throughput (req/s) | 1,781.2 | 1,873.7 |
| Peak p99 (ms) | 673.9 | 118.3 |
| Peak dropped iterations | 22,125 | 3,389 |
| Unexpected statuses (all) | 0 | 0 |

Medians across runs. Memory = VmRSS of the service process (PID 1). Latency compared at the same offered load (sustained). Percent column compares Rust with the java-tuned baseline; negative is lower.
