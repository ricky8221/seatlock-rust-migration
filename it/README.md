# The reference integration tests, run against Rust

The upstream repository ships 27 HTTP-level integration tests (`*IT.java`, Spring Boot +
Testcontainers). The challenge requires them to pass against the Rust service, so they run
here **as written**, plus one small patch:

```bash
it/run.sh java   # baseline: the original Java service, exactly as upstream runs them
it/run.sh rust   # the same tests against the Rust port
```

Maven runs in a container (`maven:3.9.11-eclipse-temurin-21`), so no local JDK is needed.

## What `rust-target.patch` changes (about 230 lines, all in `src/test`)

| File | Change |
|---|---|
| `support/RustTarget.java` (new) | `-Dseatlock.target=rust` switch, base URL, and a client for the service's `/__test` hooks |
| `it/AbstractPostgresIT.java` | In Rust mode: no Testcontainers (use the service's database) and `TestRestTemplate` rooted at the Rust service |
| `support/MutableClock.java` | `set`/`advance` are mirrored into the service's clock (`POST /__test/clock`) |
| `it/RustTargetBeans.java` (new) | `@Primary` stand-ins for the three beans tests call directly: `HoldExpiryJob.sweep()`, `IdempotencyCleanupJob.purgeExpired()`, `ReservationService.expireHold(id, at)`; each forwards to a hook |
| `it/ScheduledExpiryIT.java` | In Rust mode, turns on the *Rust* service's scheduler (`POST /__test/scheduling`) and keeps the Java context's off, so only Rust can make the test pass |
| `it/ConcurrencyIT.java` | Skips `expiryWorkingFromAStaleHoldIsRejectedByTheHoldVersion` in Rust mode (see below) |

In Java mode the patched suite behaves exactly like the original, which `it/run.sh java` proves.

The Java test context still boots in Rust mode, but only to supply `JdbcTemplate` (tests assert
on database state) and the beans above. Its scheduling is off and nothing calls its HTTP port.

## The one test that cannot cross the HTTP boundary

`expiryWorkingFromAStaleHoldIsRejectedByTheHoldVersion` opens a JPA transaction inside the Java
service, loads a hold into the persistence context, confirms the hold over HTTP, then resumes the
expiry with the stale copy. That interleaving cannot be driven from outside a process. Its
Rust twin, `service::db_tests::expiry_working_from_a_stale_hold_is_rejected_by_the_hold_version`,
forces the same interleaving with an explicit transaction:

```bash
cd seatlock-rs && TEST_DATABASE_URL=postgres://seats:seats@localhost:5433/seats cargo test db_tests
```

## Test hooks

`SEATLOCK_TEST_HOOKS=1` (never in production) makes the clock manual and exposes:

| Hook | Java equivalent |
|---|---|
| `POST /__test/clock` `{"instant":…}` or `{"advanceSeconds":…}` | `MutableClock.set` / `advance` |
| `POST /__test/jobs/expire` | `HoldExpiryJob.sweep()` |
| `POST /__test/jobs/idempotency-cleanup` | `IdempotencyCleanupJob.purgeExpired()` |
| `POST /__test/holds/{id}/expire?at=…` | `ReservationService.expireHold(id, at)` |
| `POST /__test/scheduling` `{"enabled":true,"intervalMillis":200}` | `seats.scheduling.enabled` + `seats.expiry-sweep-interval` |
