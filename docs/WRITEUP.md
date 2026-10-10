# seatlock: Java/Spring Boot -> Rust migration

Reference service: https://github.com/srujanmalakpata/seatlock at
`f345bda22c9e11d7a5b2440d5416c3e1e899500f` (pinned as the `reference/` submodule).

## Why this service
<!-- 11 JSON endpoints, PostgreSQL via Spring Data JPA, validation + 2 @Scheduled jobs + a servlet
filter, 27 HTTP integration tests, thin CRUD where fixed JVM cost dominates RSS. -->

## Results
<!-- paste bench/results/COMPARISON.md -->

## 1. Approach
## 2. Lessons learnt
## 3. Areas of optimization (each with measured MiB)
## 4. Improvements (ranked by expected memory benefit and effort)
## 5. Challenges
## 6. Gotchas

## Benchmark methodology and fairness
<!-- equal CPU/mem limits, fresh DB per run, Flyway run once outside both apps, JVM as shipped AND
tuned, Rust compared against the better JVM, median of 3, VmRSS of PID 1 as primary metric. -->

## AI assistance disclosure
