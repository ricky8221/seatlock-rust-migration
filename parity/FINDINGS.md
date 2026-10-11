# Findings log

Differences between the Java service and the Rust port, in the order the oracle exposed them.
"Probe" = found by hand with curl against the Java service before writing the Rust code;
"Diff" = found by `compare.py`.

| # | Found by | Behaviour in Java | Naive Rust result | Fix |
|---|---|---|---|---|
| 1 | Probe | `"rows":"2"` -> 2, `2.7` -> 2, `1e1` -> 10, missing/`null` int -> 0, `"name":5` -> `"5"` (Jackson coercion) | serde rejects the body: 400 "Failed to read request" where Java returns 201 or a field-level 400 | Parse into `serde_json::Value`, convert field by field with Jackson's rules (`input.rs`) |
| 2 | Probe | A missing `seatsPerRow` is 0, so the error is *validation* (`must be greater than or equal to 1`) | serde: unreadable body | Same as 1 |
| 3 | Probe | Validation lists *every* bad field | serde stops at the first | Validate after conversion, collect all |
| 4 | Probe | Path ids are parsed by `UUID.fromString`, which is lenient: `1-1-1-1-1` is a valid id (404, not 400) | `Uuid::parse_str`: 400 | `java_uuid_from_str` re-implements the JDK parser (`java.rs`) |
| 5 | Probe | Body ids go through Jackson's *strict* parser (36 chars or 24-char Base64) | n/a | Two parsers: lenient for paths, strict for bodies |
| 6 | Probe | "Seats do not belong to event X: [ffffffff-..., 00000000-...]": a `TreeSet<UUID>`, ordered by **signed** 64-bit halves | Rust and PostgreSQL order uuids unsigned: list reversed | `java_uuid_cmp` |
| 7 | Probe | `createdAt` in a POST response has 9 fraction digits (`.562261559Z`); the same value read back is `.562262Z` (PostgreSQL *rounds* to microseconds) | sqlx *truncates* to microseconds: off by 1 µs about half the time | `instant::pg_round` rounds like the server (`rint`, ties to even) before binding |
| 8 | Probe | Instants print as `ISO_INSTANT`: `.120Z`, never `.12Z`; fraction in groups of 3 | `time`'s RFC 3339 trims zeros | `instant::format` |
| 9 | Probe | `cancelledAt` is absent (not `null`) until set (`default-property-inclusion: non_null`) | `null` | `skip_serializing_if` |
| 10 | Probe | `name.strip()` uses `Character.isWhitespace` (NBSP is *not* whitespace); `@NotBlank` uses `String.trim()` (only chars <= U+0020). An em-space name passes validation and is stored as `""` | `str::trim` strips NBSP and em space | `java_strip`, `java_not_blank` |
| 11 | Probe | `@Size` counts UTF-16 units: an emoji is 2 | `str::len` counts bytes | `java_len` |
| 12 | Probe | Hold responses order seats by `String.compareTo` (UTF-16, `B` < `a`); the seat *listing* orders by the database collation | n/a | In-memory sort with UTF-16 compare; listing sorted in SQL |
| 13 | Probe | Unknown route: 404 `No static resource api/v1/nope.` (trailing slash stripped); 405 with `Allow`; `OPTIONS` answered 200 with `Allow: ...,OPTIONS` and an empty `Accept-Patch` | axum: empty 404/405 | Fallback handler + whole-router middleware |
| 14 | Probe | `415` detail echoes the media type; `text/plain` gains `;charset=UTF-8`; no Content-Type is reported as `application/octet-stream` | n/a | `input::require_json` |
| 15 | Probe | `Idempotency-Key` problems carry no `instance` (written by a servlet filter, not MVC) and use the reason phrase as title | n/a | `Problem::filter` |
| 16 | Diff | `startsAt: "2030-06-05t19:00z"` is rejected: `ISO_INSTANT` needs seconds | Accepted | Seconds mandatory |
| 17 | Diff | Repeated query param: an `int` gets the **first** value, a `String` gets them **joined with commas** | Joined for all | `Params::first` / `Params::joined` |
| 18 | Diff | Actuator answers with `application/vnd.spring-boot.actuator.v3+json` unless asked for `application/json` | `application/json` | `actuator_json` negotiation |
| 19 | Diff | Health body key order `{"status":..,"groups":..}` | `json!` sorts keys: `{"groups":..,"status":..}` | Struct with declared field order. Matters for stored idempotent responses, which are replayed byte for byte |
| 20 | Diff | OpenAPI document keys in springdoc order | Round-tripping through `serde_json::Value` sorts keys | Serve the captured text; substitute only `servers[0].url` |
| 21 | Diff | 405/OPTIONS bodies | Per-route middleware never saw axum's 405: axum rewrites `Allow` *after* route layers | Middleware moved around the whole router, with a route table for `Allow` and the metrics `uri` tag |
| 22 | Diff | `Allow` order changes between JVM restarts (hash set) | n/a | Accepted difference, compared as a set |
| 23 | Integration tests | Spring binds `SEATLOCK_TEST_HOOKS=1`-style flags leniently (`1`, `yes`, `on`) | `bool::from_str` accepts only `true`/`false`: the service exited at startup, and the tests saw only `UnresolvedAddressException` for its container | `config::flag`; `it/run.sh` now waits for the service and prints its logs if it never comes up |
