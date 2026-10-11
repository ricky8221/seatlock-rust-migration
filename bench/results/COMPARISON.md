| Metric | java (n=3) | java-tuned (n=3) | rust (n=3) | rust vs java-tuned |
|---|---|---|---|---|
| RSS idle (MiB) | 293.3 | 275.3 | 4.4 | -98% |
| RSS sustained p50 (MiB) | 384.5 | 330.5 | 7.8 | -98% |
| RSS peak (MiB) | 468.6 | 366.6 | 18.9 | -95% |
| RSS after peak (MiB) | 461.1 | 363.2 | 16.7 | -95% |
| cgroup sustained p50 (MiB) | 371.5 | 330.9 | 6.4 | -98% |
| Threads at peak | 222 | 72 | 3 | -96% |
| Startup to healthy (s) | 4 | 4 | 0 | -100% |
| Sustained p50 (ms) | 1.2 | 1.3 | 1.0 | -22% |
| Sustained p95 (ms) | 3.4 | 3.8 | 3.2 | -15% |
| Sustained p99 (ms) | 30.7 | 94.9 | 5.6 | -94% |
| Sustained throughput (req/s) | 412.6 | 411.6 | 414.8 | +1% |
| Peak throughput (req/s) | 1,887.3 | 1,884.7 | 1,901.9 | +1% |
| Peak p99 (ms) | 263.6 | 102.2 | 97.7 | -4% |
| Peak dropped iterations | 3,350 | 1,228 | 1,566 | +28% |
| Unexpected statuses (all) | 0 | 0 | 0 | – |

Medians across runs. Memory = VmRSS of the service process (PID 1). Latency compared at the same offered load (sustained). Percent column compares Rust with the java-tuned baseline; negative is lower.
