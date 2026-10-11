#!/usr/bin/env python3
"""Drives one service through a fixed, seeded request sequence and records every exchange.

The Java service is the oracle: run this against it and against the Rust port with the same
seed, then diff the two captures with compare.py. Nothing here knows which service it talks to.

    capture.py --base http://localhost:8080 --out java.jsonl --random-steps 1000 --seed 7

Steps reference earlier responses through a small template language: "{ev.B}" is a value saved
under that name, "{seat:B:3}" is the 4th seat (seat-map order) of the event saved as "B". The
random phase picks *indices* from a seeded RNG, never raw ids, so both services receive
equivalent requests even though their generated UUIDs differ.

Standard library only, so it runs anywhere Python 3.9+ does.
"""

from __future__ import annotations

import argparse
import http.client
import json
import random
import re
import sys
import time
from datetime import datetime, timedelta, timezone
from urllib.parse import urlsplit

JSON = {"Content-Type": "application/json"}
SOON = None  # --soon: one shared start time, so both services see byte-identical requests
KEEP_HEADERS = ("content-type", "location", "allow", "idempotent-replayed", "retry-after", "accept-patch")


class Session:
    def __init__(self, base: str, out):
        u = urlsplit(base)
        self.host, self.port = u.hostname, u.port or 80
        self.out = out
        self.vars: dict[str, str] = {}
        self.seats: dict[str, list[str]] = {}  # event alias -> seat ids in seat-map order
        self.n = 0

    # -- templating ------------------------------------------------------------------------
    def fill(self, s: str) -> str:
        def sub(m):
            key = m.group(1)
            if key.startswith("seat:"):
                _, alias, idx = key.split(":")
                return self.seats[alias][int(idx)]
            return self.vars[key]

        return re.sub(r"\{([a-zA-Z0-9_.:]+)\}", sub, s)

    def fill_obj(self, o):
        if isinstance(o, str):
            return self.fill(o)
        if isinstance(o, list):
            return [self.fill_obj(x) for x in o]
        if isinstance(o, dict):
            return {k: self.fill_obj(v) for k, v in o.items()}
        return o

    # -- one exchange ----------------------------------------------------------------------
    def call(self, name, method, path, body=None, headers=None, raw=None, save=None, record=True):
        path_t = path
        path = self.fill(path)
        hdrs = dict(headers or {})
        if raw is not None:
            data = raw if isinstance(raw, bytes) else raw.encode()
        elif body is not None:
            data = json.dumps(self.fill_obj(body)).encode()
            hdrs = {**JSON, **hdrs}
        else:
            data = None
        conn = http.client.HTTPConnection(self.host, self.port, timeout=30)
        conn.putrequest(method, path, skip_accept_encoding=True)
        for k, v in hdrs.items():
            conn.putheader(k, v)
        if data is not None:
            conn.putheader("Content-Length", str(len(data)))
        conn.endheaders(data)
        resp = conn.getresponse()
        payload = resp.read()
        conn.close()
        text = payload.decode("utf-8", "replace")
        try:
            parsed = json.loads(text) if text else None
            kind = "json"
        except ValueError:
            parsed, kind = text, "text"
        rec = {
            "i": self.n,
            "name": name,
            "method": method,
            "path": path_t,
            "status": resp.status,
            "headers": {k: resp.getheader(k) for k in KEEP_HEADERS if resp.getheader(k) is not None},
            "kind": kind,
            "body": parsed,
            # Raw key order matters for byte parity; json.loads keeps it in the dict.
        }
        self.n += 1
        if record:
            self.out.write(json.dumps(rec) + "\n")
        if save and resp.status < 300 and isinstance(parsed, dict):
            for var, field in save.items():
                self.vars[var] = parsed[field]
        return resp.status, parsed

    def load_seats(self, alias: str):
        ids = []
        page = 0
        while True:
            st, p = self.call("load seats", "GET", f"/api/v1/events/{{{alias}}}/seats?size=500&page={page}", record=False)
            assert st == 200, (st, p)
            ids += [s["id"] for s in p["items"]]
            page += 1
            if page >= p["totalPages"]:
                break
        self.seats[alias] = ids


def iso(dt: datetime) -> str:
    return dt.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def event_body(name, sections, starts="2030-06-01T19:00:00Z", venue="Main Hall"):
    return {"name": name, "venue": venue, "startsAt": starts, "sections": sections}


def sec(name, rows, per_row, price):
    return {"name": name, "rows": rows, "seatsPerRow": per_row, "priceCents": price}


# ---- curated cases -------------------------------------------------------------------------

def curated(s: Session):
    c = s.call
    c("health", "GET", "/actuator/health")
    c("liveness", "GET", "/actuator/health/liveness")
    c("readiness", "GET", "/actuator/health/readiness")

    # Events: mixed-case sections expose collation-vs-ordinal ordering differences.
    c("create A", "POST", "/api/v1/events", event_body("  Jazz Night  ", [sec("B", 2, 3, 5000), sec("a", 1, 2, 2500)], "2030-06-01T19:00:00.120Z"), save={"A": "id"})
    c("create B", "POST", "/api/v1/events", event_body("Big", [sec("FLOOR", 2, 10, 9500), sec("BALCONY", 1, 10, 4500)], "2030-06-02T19:00:00Z"), save={"B": "id"})
    c("create NBSP", "POST", "/api/v1/events", event_body(" nbsp ", [sec("S", 1, 1, 1)], "2030-06-03T19:00:00Z"))
    c("create offset", "POST", "/api/v1/events", event_body("Offset", [sec("S", 1, 1, 1)], "2030-06-04T19:00:00+02:00"))
    c("create epoch", "POST", "/api/v1/events", event_body("Epoch", [sec("S", 1, 1, 1)], 1906743600.5))
    c("create lower t/z", "POST", "/api/v1/events", event_body("Lower", [sec("S", 1, 1, 1)], "2030-06-05t19:00z"))
    c("create coerced", "POST", "/api/v1/events", {"name": 5, "venue": True, "startsAt": "2030-06-06T19:00:00Z", "sections": [{"name": "S", "rows": "2", "seatsPerRow": 2.7, "priceCents": 1e1}]})
    for label, raw in [
        ("malformed", "{bad json"), ("array body", "[]"), ("null body", "null"), ("empty body", ""),
        ("rows bool", '{"name":"N","venue":"V","startsAt":"2030-01-01T00:00:00Z","sections":[{"name":"S","rows":true,"seatsPerRow":1,"priceCents":1}]}'),
        ("rows overflow", '{"name":"N","venue":"V","startsAt":"2030-01-01T00:00:00Z","sections":[{"name":"S","rows":99999999999,"seatsPerRow":1,"priceCents":1}]}'),
        ("no offset", '{"name":"N","venue":"V","startsAt":"2030-01-01T00:00:00","sections":[{"name":"S","rows":1,"seatsPerRow":1,"priceCents":1}]}'),
        ("10 fraction digits", '{"name":"N","venue":"V","startsAt":"2030-01-01T00:00:00.1234567891Z","sections":[{"name":"S","rows":1,"seatsPerRow":1,"priceCents":1}]}'),
        ("name array", '{"name":["x"],"venue":"V","startsAt":"2030-01-01T00:00:00Z","sections":[{"name":"S","rows":1,"seatsPerRow":1,"priceCents":1}]}'),
    ]:
        c(f"create {label}", "POST", "/api/v1/events", raw=raw, headers=JSON)
    c("create invalid fields", "POST", "/api/v1/events", {"name": "", "venue": "x" * 201, "startsAt": "2020-01-01T00:00:00Z", "sections": [{"name": "b c", "rows": 0, "priceCents": -1}, None]})
    c("create rows null", "POST", "/api/v1/events", event_body("N", [{"name": "S", "rows": None, "seatsPerRow": 1, "priceCents": 1}]))
    c("create missing all", "POST", "/api/v1/events", {})
    c("create em-space name", "POST", "/api/v1/events", event_body(" ", [sec("S", 1, 1, 1)], "2030-06-07T19:00:00Z"))
    c("create emoji 100", "POST", "/api/v1/events", event_body("\U0001F600" * 101, [sec("S", 1, 1, 1)]))
    c("create too many sections", "POST", "/api/v1/events", event_body("N", [sec(f"S{i}", 1, 1, 1) for i in range(21)]))
    c("create dup section", "POST", "/api/v1/events", event_body("N", [sec("S", 1, 1, 1), sec("S", 1, 1, 1)]))
    c("create too many seats", "POST", "/api/v1/events", event_body("N", [sec("S", 200, 51, 1)]))
    c("create price max", "POST", "/api/v1/events", event_body("N", [sec("S", 1, 1, 100000001)]))
    c("create text/plain", "POST", "/api/v1/events", raw="{}", headers={"Content-Type": "text/plain"})
    c("create no content type", "POST", "/api/v1/events", raw="{}")
    c("create json charset", "POST", "/api/v1/events", raw=json.dumps(event_body("Charset", [sec("S", 1, 1, 1)], "2030-06-08T19:00:00Z")), headers={"Content-Type": "application/json;charset=UTF-8"})
    s.load_seats("A")
    s.load_seats("B")

    c("get A", "GET", "/api/v1/events/{A}")
    c("get unknown", "GET", "/api/v1/events/00000000-0000-0000-0000-000000000000")
    c("get lenient uuid", "GET", "/api/v1/events/1-1-1-1-1")
    c("get upper uuid", "GET", "/api/v1/events/0000000A-0000-0000-0000-000000000000")
    c("get bad uuid", "GET", "/api/v1/events/not-a-uuid")
    c("get padded uuid", "GET", "/api/v1/events/%201-1-1-1-1%20")
    for q in ["", "?size=2", "?size=2&page=1", "?page=-1&size=1000", "?page=x", "?page=0x1&size=2", "?page=%201%20&size=2", "?size=", "?page=1&page=2", "?size=0"]:
        c(f"list events {q}", "GET", f"/api/v1/events{q}")
    for q in ["", "?size=4", "?size=4&page=1", "?status=HELD", "?status=AVAILABLE&size=2", "?status=held", "?status=", "?section=a", "?section=B&size=1&page=2", "?size=501", "?page=100001"]:
        c(f"seats A {q}", "GET", f"/api/v1/events/{{A}}/seats{q}")
    c("seats unknown event", "GET", "/api/v1/events/00000000-0000-0000-0000-000000000000/seats")
    c("availability A", "GET", "/api/v1/events/{A}/availability")
    c("availability unknown", "GET", "/api/v1/events/00000000-0000-0000-0000-000000000000/availability")

    # Holds.
    hold = lambda name, alias, idx, ref="cust-1", **kw: c(name, "POST", f"/api/v1/events/{{{alias}}}/holds", {"seatIds": [f"{{seat:{alias}:{i}}}" for i in idx], "customerRef": ref}, **kw)
    hold("hold A 6,0", "A", [6, 0], save={"h1": "id"})
    hold("hold A conflict 0,1", "A", [0, 1], ref="cust-2")
    hold("hold A dup", "A", [2, 2])
    hold("hold B 11 seats", "B", list(range(11)))
    c("hold A foreign + unknown", "POST", "/api/v1/events/{A}/holds", {"seatIds": ["{seat:A:3}", "{seat:B:0}", "00000000-0000-0000-0000-000000000001", "ffffffff-0000-0000-0000-000000000001"], "customerRef": "c"})
    c("hold empty + bad ref", "POST", "/api/v1/events/{A}/holds", {"seatIds": [], "customerRef": "bad ref!"})
    c("hold nulls", "POST", "/api/v1/events/{A}/holds", {"seatIds": [None, "{seat:A:3}"], "customerRef": None})
    c("hold lenient body uuid", "POST", "/api/v1/events/{A}/holds", {"seatIds": ["1-1-1-1-1"], "customerRef": "c"})
    c("hold base64 uuid", "POST", "/api/v1/events/{A}/holds", {"seatIds": ["AAAAAAAAAAAAAAAAAAAAAQ=="], "customerRef": "c"})
    c("hold ref 65", "POST", "/api/v1/events/{A}/holds", {"seatIds": ["{seat:A:3}"], "customerRef": "x" * 65})
    c("hold unknown event", "POST", "/api/v1/events/00000000-0000-0000-0000-000000000009/holds", {"seatIds": ["{seat:A:3}"], "customerRef": "c"})
    c("hold bad event id", "POST", "/api/v1/events/nope/holds", raw="{", headers={"Content-Type": "text/plain"})
    c("hold missing body", "POST", "/api/v1/events/{A}/holds", headers=JSON)
    c("get hold", "GET", "/api/v1/holds/{h1}")
    c("confirm h1", "POST", "/api/v1/holds/{h1}/confirm", save={"b1": "id"})
    c("confirm h1 again", "POST", "/api/v1/holds/{h1}/confirm")
    c("release confirmed", "DELETE", "/api/v1/holds/{h1}")
    c("get hold confirmed", "GET", "/api/v1/holds/{h1}")
    c("get booking", "GET", "/api/v1/bookings/{b1}")
    c("availability A after", "GET", "/api/v1/events/{A}/availability")
    c("cancel b1", "POST", "/api/v1/bookings/{b1}/cancel")
    c("cancel b1 again", "POST", "/api/v1/bookings/{b1}/cancel")
    c("get booking cancelled", "GET", "/api/v1/bookings/{b1}")
    hold("hold A 1", "A", [1], save={"h2": "id"})
    c("release h2", "DELETE", "/api/v1/holds/{h2}")
    c("release h2 again", "DELETE", "/api/v1/holds/{h2}")
    c("confirm released", "POST", "/api/v1/holds/{h2}/confirm")
    hold("hold A 1 again", "A", [1], save={"h3": "id"})
    c("old hold shows reused seat", "GET", "/api/v1/holds/{h2}")
    for p in ["/api/v1/holds/00000000-0000-0000-0000-000000000009", "/api/v1/holds/00000000-0000-0000-0000-000000000009/confirm",
              "/api/v1/bookings/00000000-0000-0000-0000-000000000009", "/api/v1/bookings/00000000-0000-0000-0000-000000000009/cancel"]:
        c(f"unknown {p}", "POST" if p.endswith(("confirm", "cancel")) else "GET", p)
    c("delete unknown hold", "DELETE", "/api/v1/holds/00000000-0000-0000-0000-000000000009")
    c("confirm bad uuid", "POST", "/api/v1/holds/xyz/confirm")

    # Idempotency-Key.
    k = lambda key: {"Idempotency-Key": key}
    hold("idem hold", "B", [12], headers=k("k-hold"), save={"h4": "id"})
    hold("idem hold replay", "B", [12], headers=k("k-hold"))
    hold("idem hold other body", "B", [13], headers=k("k-hold"))
    c("idem confirm", "POST", "/api/v1/holds/{h4}/confirm", headers=k("k-confirm"))
    c("idem confirm replay", "POST", "/api/v1/holds/{h4}/confirm", headers=k("k-confirm"))
    c("idem confirm other path", "POST", "/api/v1/holds/{h3}/confirm", headers=k("k-confirm"))
    c("idem 4xx stored", "POST", "/api/v1/events/{A}/holds", {"seatIds": []}, headers=k("k-400"))
    c("idem 4xx replay", "POST", "/api/v1/events/{A}/holds", {"seatIds": []}, headers=k("k-400"))
    c("idem 404 route", "POST", "/api/v1/nope", raw="{}", headers=k("k-404"))
    c("idem 404 route replay", "POST", "/api/v1/nope", raw="{}", headers=k("k-404"))
    c("idem blank key", "POST", "/api/v1/events", raw="{}", headers={**JSON, "Idempotency-Key": " "})
    c("idem long key", "POST", "/api/v1/events", raw="{}", headers={**JSON, "Idempotency-Key": "k" * 256})
    c("idem 255 key", "POST", "/api/v1/events", raw="{}", headers={**JSON, "Idempotency-Key": "k" * 255})
    c("idem big body", "POST", "/api/v1/events", raw='{"a":"' + "x" * 70000 + '"}', headers={**JSON, "Idempotency-Key": "k-big"})
    c("idem on GET ignored", "GET", "/api/v1/events/{A}", headers=k("k-get"))
    c("idem on DELETE ignored", "DELETE", "/api/v1/holds/{h3}", headers=k("k-del"))
    c("idem create event", "POST", "/api/v1/events", event_body("Idem", [sec("S", 1, 1, 1)], "2030-06-09T19:00:00Z"), headers=k("k-ev"))
    c("idem create event replay", "POST", "/api/v1/events", event_body("Idem", [sec("S", 1, 1, 1)], "2030-06-09T19:00:00Z"), headers=k("k-ev"))

    # Framework behaviour.
    for m, p in [("PUT", "/api/v1/events"), ("DELETE", "/api/v1/events"), ("PATCH", "/api/v1/holds/{h3}"), ("POST", "/api/v1/holds/{h3}"),
                 ("GET", "/api/v1/holds/{h3}/confirm"), ("GET", "/api/v1/events/{A}/holds"), ("PUT", "/api/v1/bookings/{b1}")]:
        c(f"405 {m} {p}", m, p)
    for p in ["/api/v1/events", "/api/v1/holds/{h3}", "/api/v1/events/{A}/holds", "/api/v1/bookings/{b1}/cancel"]:
        c(f"OPTIONS {p}", "OPTIONS", p)
    for p in ["/api/v1/nope", "/api/v1/events/", "/nope/deeper/", "/api/v1/events/{A}/seats/extra"]:
        c(f"404 {p}", "GET", p)
    c("HEAD events", "HEAD", "/api/v1/events")

    # Event that starts in a few seconds: hold now, then try everything after it starts.
    starts = SOON or iso(datetime.now(timezone.utc) + timedelta(seconds=4))
    c("create soon", "POST", "/api/v1/events", event_body("Soon", [sec("S", 1, 4, 100)], starts), save={"SOON": "id"})
    s.load_seats("SOON")
    hold("hold soon", "SOON", [0], save={"hs": "id"})
    hold("hold soon 2", "SOON", [1], save={"hs2": "id"})
    c("confirm soon 2", "POST", "/api/v1/holds/{hs2}/confirm", save={"bs2": "id"})
    wait = (datetime.strptime(starts, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc) - datetime.now(timezone.utc)).total_seconds()
    time.sleep(max(0.0, wait) + 1)
    hold("hold after start", "SOON", [2])
    c("confirm after start", "POST", "/api/v1/holds/{hs}/confirm")
    c("cancel after start", "POST", "/api/v1/bookings/{bs2}/cancel")
    c("release after start", "DELETE", "/api/v1/holds/{hs}")

    c("api docs", "GET", "/v3/api-docs")


# ---- random walk ---------------------------------------------------------------------------

def random_walk(s: Session, steps: int, seed: int):
    rng = random.Random(seed)
    events: list[str] = []
    holds: list[str] = []  # vars of placed holds
    bookings: list[str] = []
    base = datetime(2031, 1, 1, tzinfo=timezone.utc)

    def new_event():
        n = len(events)
        alias = f"R{n}"
        sections = [sec(name, rng.randint(1, 4), rng.randint(1, 6), rng.choice([0, 100, 2500, 99999]))
                    for name in rng.sample(["A", "b", "C", "d", "Z9", "_x", "-y"], rng.randint(1, 3))]
        st, _ = s.call("rw create", "POST", "/api/v1/events",
                       event_body(f"Random {n}", sections, iso(base + timedelta(minutes=n))), save={alias: "id"})
        if st == 201:
            events.append(alias)
            s.load_seats(alias)

    for _ in range(3):
        new_event()
    for step in range(steps):
        r = rng.random()
        ev = rng.choice(events)
        nseats = len(s.seats[ev])
        key = {"Idempotency-Key": f"rk-{rng.randint(0, 40)}"} if rng.random() < 0.15 else None
        if r < 0.05:
            new_event()
        elif r < 0.15:
            s.call("rw availability", "GET", f"/api/v1/events/{{{ev}}}/availability")
        elif r < 0.25:
            size = rng.choice([1, 2, 5, 50])
            st = rng.choice(["", "&status=AVAILABLE", "&status=HELD", "&status=BOOKED"])
            s.call("rw seats", "GET", f"/api/v1/events/{{{ev}}}/seats?size={size}&page={rng.randint(0, 3)}{st}")
        elif r < 0.30:
            s.call("rw list events", "GET", f"/api/v1/events?size={rng.randint(1, 5)}&page={rng.randint(0, 3)}")
        elif r < 0.60:
            k = rng.randint(1, min(4, nseats))
            idx = rng.sample(range(nseats), k)
            if rng.random() < 0.05:
                idx.append(idx[0])  # duplicate
            var = f"rh{step}"
            st, _ = s.call("rw hold", "POST", f"/api/v1/events/{{{ev}}}/holds",
                           {"seatIds": [f"{{seat:{ev}:{i}}}" for i in idx], "customerRef": f"c{rng.randint(1, 9)}"},
                           headers=key, save={var: "id"})
            if st == 201 and var in s.vars:
                holds.append(var)
        elif r < 0.75 and holds:
            h = rng.choice(holds)
            var = f"rb{step}"
            st, _ = s.call("rw confirm", "POST", f"/api/v1/holds/{{{h}}}/confirm", headers=key, save={var: "id"})
            if st == 201 and var in s.vars:
                bookings.append(var)
        elif r < 0.85 and holds:
            s.call("rw release", "DELETE", f"/api/v1/holds/{{{rng.choice(holds)}}}")
        elif r < 0.92 and bookings:
            s.call("rw cancel", "POST", f"/api/v1/bookings/{{{rng.choice(bookings)}}}/cancel", headers=key)
        elif r < 0.96 and holds:
            s.call("rw get hold", "GET", f"/api/v1/holds/{{{rng.choice(holds)}}}")
        elif bookings:
            s.call("rw get booking", "GET", f"/api/v1/bookings/{{{rng.choice(bookings)}}}")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--base", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--seed", type=int, default=7)
    ap.add_argument("--random-steps", type=int, default=1000)
    ap.add_argument("--no-curated", action="store_true")
    ap.add_argument("--soon", help="startsAt for the event-starts-mid-run case (default: now + 4s)")
    args = ap.parse_args()
    global SOON
    SOON = args.soon
    with open(args.out, "w") as out:
        s = Session(args.base, out)
        if not args.no_curated:
            curated(s)
        random_walk(s, args.random_steps, args.seed)
        print(f"{s.n} exchanges recorded to {args.out}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
