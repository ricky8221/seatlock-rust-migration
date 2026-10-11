#!/usr/bin/env python3
"""Concurrency equivalence: fires the same races at a service and checks the outcome counts and
database invariants, the HTTP-level twin of the reference suite's ConcurrencyIT.

    races.py --base http://localhost:8080 [--dsn "host=127.0.0.1 user=seats dbname=seats"]

Outcomes, not individual responses, are compared: which of 50 simultaneous requests wins is
scheduling, but "exactly one 201 and 49 409s" is the contract. Prints one JSON line per race so
two services' results can be diffed. With --dsn it also checks the invariants with psql.
"""

from __future__ import annotations

import argparse
import collections
import http.client
import json
import subprocess
import sys
import threading
from urllib.parse import urlsplit

BASE = None


def call(method, path, body=None, headers=None):
    u = urlsplit(BASE)
    conn = http.client.HTTPConnection(u.hostname, u.port, timeout=60)
    data = json.dumps(body).encode() if body is not None else None
    h = {"Content-Type": "application/json", **(headers or {})} if data is not None else dict(headers or {})
    conn.request(method, path, body=data, headers=h)
    r = conn.getresponse()
    text = r.read().decode()
    conn.close()
    return r.status, (json.loads(text) if text else None)


def burst(n, fn):
    """Runs fn(i) on n threads released at the same instant; returns the statuses."""
    gate = threading.Barrier(n)
    out = [None] * n

    def run(i):
        gate.wait()
        out[i] = fn(i)

    threads = [threading.Thread(target=run, args=(i,)) for i in range(n)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    return out


def event(rows, per_row, name):
    st, ev = call("POST", "/api/v1/events", {"name": name, "venue": "Race Hall", "startsAt": "2031-06-01T19:00:00Z",
                                            "sections": [{"name": "R", "rows": rows, "seatsPerRow": per_row, "priceCents": 1000}]})
    assert st == 201, (st, ev)
    _, seats = call("GET", f"/api/v1/events/{ev['id']}/seats?size=500")
    return ev["id"], [s["id"] for s in seats["items"]]


def hold(eid, ids, ref="racer", key=None):
    return call("POST", f"/api/v1/events/{eid}/holds", {"seatIds": ids, "customerRef": ref},
                {"Idempotency-Key": key} if key else None)


def tally(results):
    return dict(sorted(collections.Counter(st for st, _ in results).items()))


def main() -> int:
    global BASE
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--base", required=True)
    ap.add_argument("--dsn", help="libpq connection string for invariant checks via psql")
    args = ap.parse_args()
    BASE = args.base.rstrip("/")
    report = {}

    # 1. 50 holds on one seat: exactly one wins.
    eid, seats = event(1, 1, "one seat")
    r = burst(50, lambda i: hold(eid, [seats[0]], f"r{i}"))
    report["50 holds, 1 seat"] = tally(r)

    # 2. Overlapping multi-seat holds never share a seat.
    eid, seats = event(1, 6, "overlap")
    r = burst(30, lambda i: hold(eid, [seats[i % 6], seats[(i + 1) % 6], seats[(i + 2) % 6]], f"o{i}"))
    won = [b for st, b in r if st == 201]
    claimed = [s["id"] for b in won for s in b["seats"]]
    report["30 overlapping holds"] = {**tally(r), "seats claimed twice": len(claimed) - len(set(claimed))}

    # 3. 20 confirms of one hold: one booking.
    eid, seats = event(1, 2, "confirm race")
    _, h = hold(eid, seats)
    r = burst(20, lambda i: call("POST", f"/api/v1/holds/{h['id']}/confirm"))
    report["20 confirms, 1 hold"] = tally(r)

    # 4. Release racing confirm: exactly one of them succeeds.
    outcomes = collections.Counter()
    for _ in range(15):
        eid, seats = event(1, 1, "release vs confirm")
        _, h = hold(eid, seats)
        r = burst(2, lambda i: call("POST", f"/api/v1/holds/{h['id']}/confirm") if i == 0 else call("DELETE", f"/api/v1/holds/{h['id']}"))
        outcomes[f"confirm {r[0][0]} / release {r[1][0]}"] += 1
    report["release vs confirm x15"] = dict(sorted(outcomes.items()))

    # 5. 20 concurrent retries with one Idempotency-Key: one hold, the rest replay or wait.
    eid, seats = event(1, 1, "idempotent burst")
    r = burst(20, lambda i: hold(eid, [seats[0]], "idem", key="race-key"))
    ids = {b["id"] for st, b in r if st == 201}
    report["20 retries, 1 key"] = {**tally(r), "distinct holds": len(ids)}

    # 6. Cancel racing cancel.
    eid, seats = event(1, 1, "cancel race")
    _, h = hold(eid, seats)
    _, b = call("POST", f"/api/v1/holds/{h['id']}/confirm")
    r = burst(10, lambda i: call("POST", f"/api/v1/bookings/{b['id']}/cancel"))
    report["10 cancels, 1 booking"] = tally(r)

    for k, v in report.items():
        print(json.dumps({"race": k, "outcome": v}))

    if args.dsn:
        checks = {
            "seat HELD/BOOKED without a hold": "SELECT count(*) FROM seat WHERE status <> 'AVAILABLE' AND hold_id IS NULL",
            "seat held by a non-ACTIVE hold": "SELECT count(*) FROM seat s JOIN seat_hold h ON h.id = s.hold_id WHERE s.status = 'HELD' AND h.status <> 'ACTIVE'",
            "booked seat without a CONFIRMED booking": "SELECT count(*) FROM seat s LEFT JOIN booking b ON b.hold_id = s.hold_id AND b.status = 'CONFIRMED' WHERE s.status = 'BOOKED' AND b.id IS NULL",
            "holds with more than one booking": "SELECT count(*) FROM (SELECT hold_id FROM booking GROUP BY hold_id HAVING count(*) > 1) x",
            "CONFIRMED hold without a booking": "SELECT count(*) FROM seat_hold h LEFT JOIN booking b ON b.hold_id = h.id WHERE h.status = 'CONFIRMED' AND b.id IS NULL",
        }
        bad = 0
        for name, sql in checks.items():
            n = int(subprocess.check_output(["psql", args.dsn, "-tAc", sql]).decode().strip())
            bad += n
            print(json.dumps({"invariant": name, "violations": n}))
        return 1 if bad else 0
    return 0


if __name__ == "__main__":
    sys.exit(main())
