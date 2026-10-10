# Parity harness (phase 2)

Differential replay: the same request sequences go to the Java service (the oracle) and the Rust
service, each on a fresh database; responses are normalized (UUIDs mapped by first appearance,
timestamps compared by relationship, validation errors compared as sets) and diffed. Accepted
differences live in `allowlist.yaml`, each with a justification. Scale: 10 -> 100 -> 1,000+.
