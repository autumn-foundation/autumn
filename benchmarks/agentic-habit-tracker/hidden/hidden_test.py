#!/usr/bin/env python3
"""Hidden acceptance tests for the habit-tracker benchmark.

Not shown to the building agent. Same framework-agnostic shape as
acceptance/contract_test.py: HTTP-only, stdlib only, BASE_URL from env.

Probes streak edge cases with concrete backdated dates computed relative to
today, so results are deterministic regardless of the calendar day the trial is
run.

Usage:
    BASE_URL=http://localhost:8080 python3 hidden_test.py
"""

import datetime
import json
import os
import sys
import time
import urllib.error
import urllib.request

BASE_URL = os.environ.get("BASE_URL", "http://localhost:8080").rstrip("/")
TODAY = datetime.date.today()

_PASSED = 0
_FAILED = 0


# ── HTTP helper ────────────────────────────────────────────────────────────────

class Resp:
    def __init__(self, status, headers, body_bytes):
        self.status = status
        self.headers = headers
        self.body_bytes = body_bytes

    def json(self):
        return json.loads(self.body_bytes.decode("utf-8"))


def request(method, path, body=None):
    url = path if path.startswith("http") else BASE_URL + path
    data = None
    headers = {"Accept": "application/json"}
    if body is not None:
        data = json.dumps(body).encode("utf-8")
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=15) as r:
            raw = r.read()
            hdrs = {k.lower(): v for k, v in r.headers.items()}
            return Resp(r.status, hdrs, raw)
    except urllib.error.HTTPError as e:
        raw = e.read()
        hdrs = {k.lower(): v for k, v in e.headers.items()} if e.headers else {}
        return Resp(e.code, hdrs, raw)


def check(name, condition, detail=""):
    global _PASSED, _FAILED
    if condition:
        _PASSED += 1
        print(f"PASS  {name}")
    else:
        _FAILED += 1
        suffix = f" -- {detail}" if detail else ""
        print(f"FAIL  {name}{suffix}")


def wait_for_server(timeout_s=30):
    deadline = time.time() + timeout_s
    last = None
    while time.time() < deadline:
        try:
            r = request("GET", "/api/habits")
            if r.status < 500:
                return True
        except Exception as e:
            last = e
        time.sleep(0.5)
    print(f"FAIL  server did not become ready within {timeout_s}s ({last})")
    return False


def day(delta):
    """ISO date string for TODAY - delta days."""
    return (TODAY - datetime.timedelta(days=delta)).isoformat()


def new_habit(name):
    r = request("POST", "/api/habits", {"name": name})
    if r.status != 201:
        return None
    return r.json().get("id")


def complete(hid, delta, expect=201, label=None):
    """Backdate a completion TODAY-delta days; assert the status."""
    d = day(delta)
    r = request("POST", f"/api/habits/{hid}/complete", {"date": d})
    lbl = label or f"complete D-{delta} ({d})"
    check(f"{lbl} -> {expect}", r.status == expect, f"got {r.status}")
    return r


def streak_of(hid):
    r = request("GET", f"/api/habits/{hid}")
    if r.status != 200:
        return None
    return r.json().get("current_streak")


# ── tests ──────────────────────────────────────────────────────────────────────

def run():
    if not wait_for_server():
        print("RESULT: 0/1 passed")
        sys.exit(1)

    # (a) gap before today: complete D-4, D-3, D-2; skip D-1 and today -> streak 0
    hid = new_habit("a-gap")
    check("(a) create habit", hid is not None)
    if hid is not None:
        complete(hid, 4)
        complete(hid, 3)
        complete(hid, 2)
        s = streak_of(hid)
        check("(a) streak 0 when last completion older than yesterday", s == 0, f"got {s}")

    # (b) out-of-order insertion: today, then D-2, then D-1 -> streak 3
    hid = new_habit("b-unordered")
    check("(b) create habit", hid is not None)
    if hid is not None:
        complete(hid, 0)   # insert today first
        complete(hid, 2)   # then D-2
        complete(hid, 1)   # then D-1
        s = streak_of(hid)
        check("(b) out-of-order backdated completions -> streak 3", s == 3, f"got {s}")

    # (c) duplicate via explicit today's date after default-complete -> 409
    hid = new_habit("c-dup")
    check("(c) create habit", hid is not None)
    if hid is not None:
        r = request("POST", f"/api/habits/{hid}/complete", {})  # default = today
        check("(c) default complete -> 201", r.status == 201, f"got {r.status}")
        r = request("POST", f"/api/habits/{hid}/complete", {"date": day(0)})  # explicit today
        check("(c) explicit-today duplicate -> 409", r.status == 409, f"got {r.status}")

    # (d) yesterday only -> streak 1 (yesterday counts as current)
    hid = new_habit("d-yesterday")
    check("(d) create habit", hid is not None)
    if hid is not None:
        complete(hid, 1)
        s = streak_of(hid)
        check("(d) yesterday-only -> streak 1", s == 1, f"got {s}")

    # (e) D-3 only -> streak 0 (too old to be current)
    hid = new_habit("e-old")
    check("(e) create habit", hid is not None)
    if hid is not None:
        complete(hid, 3)
        s = streak_of(hid)
        check("(e) D-3-only -> streak 0", s == 0, f"got {s}")

    # (f) malformed date -> 4xx
    hid = new_habit("f-baddate")
    check("(f) create habit", hid is not None)
    if hid is not None:
        r = request("POST", f"/api/habits/{hid}/complete", {"date": "2026-13-40"})
        check("(f) malformed date -> 4xx", r.status in (400, 422), f"got {r.status}")

    # (g) after DELETE, GET -> 404 (and completions gone with it)
    hid = new_habit("g-delete")
    check("(g) create habit", hid is not None)
    if hid is not None:
        complete(hid, 0)
        r = request("DELETE", f"/api/habits/{hid}")
        check("(g) DELETE -> 204", r.status == 204, f"got {r.status}")
        r = request("GET", f"/api/habits/{hid}")
        check("(g) GET after delete -> 404", r.status == 404, f"got {r.status}")

    total = _PASSED + _FAILED
    print(f"RESULT: {_PASSED}/{total} passed")
    sys.exit(0 if _FAILED == 0 else 1)


if __name__ == "__main__":
    run()
