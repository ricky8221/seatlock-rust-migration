#!/usr/bin/env python3
"""Diffs two captures (oracle first) and classifies every difference.

    compare.py java.jsonl rust.jsonl [--allowlist allowlist.json] [--show 20]

Normalization, applied identically to both sides before comparing:

* UUIDs are renamed by first appearance (`<id1>`, `<id2>`, ...). Responses are walked with
  sorted keys so key order cannot change the numbering.
* "Now"-based timestamps (`createdAt`, `expiresAt`, `cancelledAt`) become `<now>`, after checking
  `expiresAt - createdAt` equals the hold TTL. Instants inside `detail` strings become `<ts>`.
* Bean-validation `errors` are compared as a set: Hibernate Validator reports them in no fixed
  order, and Java's `Map.of` gives each error object a per-JVM-random key order.
* Lists ordered *by UUID* (`seatIds` in a seat-unavailable problem, the bracketed lists in
  `detail`) are compared as sets, because the two runs generate different UUIDs. Their order is
  still checked: each capture must be sorted the way its source dictates (PostgreSQL's unsigned
  uuid order for seat-unavailable, Java's signed `UUID.compareTo` for "do not belong").
* The OpenAPI `servers` URL contains the port each service listens on.
* `Allow` is compared as a set of methods: Java's order changes from one JVM start to the next.

Every remaining difference is a finding. Findings listed in the allowlist (by step name and
kind) are reported as accepted, with the reason, and do not fail the run.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from datetime import datetime

UUID_RE = re.compile(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")
INSTANT_RE = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?Z")
NOW_KEYS = {"createdAt", "expiresAt", "cancelledAt"}
BRACKETS_RE = re.compile(r"\[([0-9a-fA-F, -]+)\]")
RENAMED_LIST_RE = re.compile(r"\[(<id\d+>(?:, <id\d+>)*)\]")


def parse_instant(s: str) -> tuple[datetime, int]:
    base, _, frac = s.rstrip("Z").partition(".")
    dt = datetime.strptime(base, "%Y-%m-%dT%H:%M:%S")
    return dt, int((frac + "000000000")[:9]) if frac else 0


class Normalizer:
    def __init__(self):
        self.ids: dict[str, str] = {}

    def see(self, o):
        """Assigns names to UUIDs in a deterministic traversal order."""
        if isinstance(o, str):
            for m in UUID_RE.findall(o):
                self.ids.setdefault(m.lower(), f"<id{len(self.ids) + 1}>")
        elif isinstance(o, list):
            for x in o:
                self.see(x)
        elif isinstance(o, dict):
            for k in sorted(o):
                self.see(o[k])

    def ren(self, s: str) -> str:
        return UUID_RE.sub(lambda m: self.ids.get(m.group(0).lower(), "<id?>"), s)

    def norm(self, o, key=None):
        if isinstance(o, str):
            if key in NOW_KEYS and INSTANT_RE.fullmatch(o):
                return "<now>"
            s = self.ren(o)
            if key == "detail":
                s = INSTANT_RE.sub("<ts>", s)
                s = RENAMED_LIST_RE.sub(lambda m: "[" + ", ".join(sorted(m.group(1).split(", "))) + "]", s)
            return s
        if isinstance(o, list):
            out = [self.norm(x) for x in o]
            if key == "errors":
                out = sorted(out, key=lambda e: json.dumps(e, sort_keys=True))
            if key == "seatIds":
                out = sorted(out)
            return out
        if isinstance(o, dict):
            return {k: self.norm(v, k) for k, v in o.items()}
        return o


def method_set(allow: str) -> set:
    return {m.strip() for m in allow.split(",") if m.strip()}


def key_orders(o, path="$"):
    """Yields (path, keys) for every object, skipping validation error objects."""
    if isinstance(o, dict):
        yield path, list(o.keys())
        for k, v in o.items():
            if k != "errors":
                yield from key_orders(v, f"{path}.{k}")
    elif isinstance(o, list):
        for i, v in enumerate(o):
            yield from key_orders(v, f"{path}[{i}]")


def java_signed_key(u: str):
    h = u.replace("-", "")
    msb, lsb = int(h[:16], 16), int(h[16:], 16)
    sign = lambda x: x - (1 << 64) if x >= 1 << 63 else x
    return sign(msb), sign(lsb)


def property_checks(rec, ttl_seconds: float):
    """Invariants each capture must satisfy on its own (raw values, before normalization)."""
    problems = []
    b = rec["body"]
    if isinstance(b, dict):
        if "createdAt" in b and "expiresAt" in b:
            (c, cn), (e, en) = parse_instant(b["createdAt"]), parse_instant(b["expiresAt"])
            delta = (e - c).total_seconds() + (en - cn) / 1e9
            if abs(delta - ttl_seconds) > 1e-6:
                problems.append(f"expiresAt - createdAt = {delta}s, expected {ttl_seconds}s")
        if b.get("type") == "urn:seatlock:problem:seat-unavailable" and "seatIds" in b:
            ids = [x.lower() for x in b["seatIds"]]
            if ids != sorted(ids):  # PostgreSQL ORDER BY id: unsigned byte order
                problems.append(f"seat-unavailable seatIds not in uuid order: {ids}")
        d = b.get("detail") or ""
        if "do not belong to event" in d:
            m = BRACKETS_RE.search(d)
            ids = m.group(1).split(", ") if m else []
            if ids != sorted(ids, key=java_signed_key):
                problems.append(f"missing-seat list not in Java UUID order: {ids}")
    return problems


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("oracle")
    ap.add_argument("candidate")
    ap.add_argument("--allowlist", default=None)
    ap.add_argument("--ttl", type=float, default=300.0)
    ap.add_argument("--show", type=int, default=25)
    args = ap.parse_args()

    allow = []
    if args.allowlist:
        with open(args.allowlist) as fh:
            allow = json.load(fh)

    def allowed(name, kind):
        for a in allow:
            if re.fullmatch(a["step"], name) and a["kind"] == kind:
                return a["reason"]
        return None

    a_recs = [json.loads(l) for l in open(args.oracle)]
    b_recs = [json.loads(l) for l in open(args.candidate)]
    na, nb = Normalizer(), Normalizer()
    findings, accepted = [], []
    stats = {"exchanges": 0, "identical": 0}

    for rec_list, label in ((a_recs, "oracle"), (b_recs, "candidate")):
        for r in rec_list:
            for p in property_checks(r, args.ttl):
                findings.append((r["i"], r["name"], f"property[{label}]", p))

    for ra, rb in zip(a_recs, b_recs):
        stats["exchanges"] += 1
        if ra["name"] != rb["name"] or ra["path"] != rb["path"]:
            findings.append((ra["i"], ra["name"], "desync", f"step {ra['name']} vs {rb['name']}"))
            break
        for n, r in ((na, ra), (nb, rb)):
            n.see(r["headers"].get("location", ""))
            n.see(r["body"])
        diffs = []
        if ra["status"] != rb["status"]:
            diffs.append(("status", f"{ra['status']} != {rb['status']}"))
        ha = {k: na.ren(v) for k, v in ra["headers"].items()}
        hb = {k: nb.ren(v) for k, v in rb["headers"].items()}
        for k in sorted(set(ha) | set(hb)):
            if ha.get(k) != hb.get(k):
                if k == "allow" and ha.get(k) and hb.get(k) and method_set(ha[k]) == method_set(hb[k]):
                    accepted.append((ra["i"], ra["name"], "header:allow",
                                     f"same methods, different order ({ha[k]!r} vs {hb[k]!r}): Java builds Allow "
                                     "from a hash set whose order changes between JVM runs"))
                    continue
                diffs.append((f"header:{k}", f"{ha.get(k)!r} != {hb.get(k)!r}"))
        ba, bb = ra["body"], rb["body"]
        if ra["name"] == "api docs" and isinstance(ba, dict) and isinstance(bb, dict):
            ba, bb = dict(ba, servers=None), dict(bb, servers=None)
        xa, xb = na.norm(ba), nb.norm(bb)
        if xa != xb:
            diffs.append(("body", f"\n    oracle:    {json.dumps(xa)[:600]}\n    candidate: {json.dumps(xb)[:600]}"))
        elif isinstance(ba, (dict, list)):
            oa, ob = dict(key_orders(ba)), dict(key_orders(bb))
            for path in oa:
                if path in ob and oa[path] != ob[path] and sorted(oa[path]) == sorted(ob[path]):
                    diffs.append(("key-order", f"{path}: {oa[path]} != {ob[path]}"))
                    break
        if not diffs:
            stats["identical"] += 1
        for kind, detail in diffs:
            reason = allowed(ra["name"], kind)
            (accepted if reason else findings).append((ra["i"], ra["name"], kind, detail if not reason else reason))

    if len(a_recs) != len(b_recs):
        findings.append((-1, "-", "length", f"{len(a_recs)} oracle vs {len(b_recs)} candidate exchanges"))

    print(f"exchanges compared: {stats['exchanges']}, identical after normalization: {stats['identical']}")
    print(f"accepted differences: {len(accepted)}, unexplained findings: {len(findings)}")
    for i, name, kind, detail in accepted[: args.show]:
        print(f"  ACCEPTED #{i} {name} [{kind}]: {detail}")
    for i, name, kind, detail in findings[: args.show]:
        print(f"  FINDING  #{i} {name} [{kind}]: {detail}")
    return 1 if findings else 0


if __name__ == "__main__":
    sys.exit(main())
