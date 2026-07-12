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
# SERVER_TODAY anchors every backdated streak date to the SERVER's clock rather
# than the evaluator's local date. It is established at runtime (see run()) by
# POSTing a default completion and reading the returned date, so a correct
# server is not failed by a timezone / midnight-boundary skew. The module-level
# value is only a fallback used before the probe runs.
SERVER_TODAY = datetime.date.today()

_PASSED = 0
_FAILED = 0


# ── HTTP helper ────────────────────────────────────────────────────────────────

class Resp:
    def __init__(self, status, headers, body_bytes):
        self.status = status
        self.headers = headers
        self.body_bytes = body_bytes

    def json(self):
        """Parse the body as JSON, returning None on any decode failure.

        A candidate can answer an otherwise-expected status with an empty or
        HTML body; raising here would crash the suite before it prints its
        RESULT line. Returning None lets callers fold a non-JSON body into a
        recorded check failure (they treat it as ``{}``).
        """
        try:
            return json.loads(self.body_bytes.decode("utf-8"))
        except (ValueError, UnicodeDecodeError):
            return None


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
    """ISO date string for the SERVER's today - delta days."""
    return (SERVER_TODAY - datetime.timedelta(days=delta)).isoformat()


def new_habit(name):
    r = request("POST", "/api/habits", {"name": name})
    if r.status != 201:
        return None
    return (r.json() or {}).get("id")


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
    return (r.json() or {}).get("current_streak")


# ── tests ──────────────────────────────────────────────────────────────────────

def run():
    if not wait_for_server():
        print("RESULT: 0/1 passed")
        sys.exit(1)

    # Establish the SERVER's today from a default completion (no date) so every
    # backdated streak date below is anchored to the server clock. This probe
    # habit is isolated from the scenarios and adds no checks, so the denominator
    # is unchanged.
    global SERVER_TODAY
    probe = new_habit("_server-date-probe")
    if probe is not None:
        r = request("POST", f"/api/habits/{probe}/complete", {})
        if r.status == 201:
            ds = (r.json() or {}).get("date")
            try:
                if ds:
                    SERVER_TODAY = datetime.date.fromisoformat(ds)
            except (ValueError, TypeError):
                pass

    # Denominator is intentionally fixed: each scenario contributes a constant
    # number of checks. When a setup step (habit creation) fails, the dependent
    # checks are recorded as explicit failures below instead of being skipped,
    # so a broken implementation can never shrink the total.

    # (a) gap before today: complete D-4, D-3, D-2; skip D-1 and today -> streak 0
    hid = new_habit("a-gap")
    check("(a) create habit", hid is not None)
    if hid is not None:
        complete(hid, 4)
        complete(hid, 3)
        complete(hid, 2)
        s = streak_of(hid)
        check("(a) streak 0 when last completion older than yesterday", s == 0, f"got {s}")
    else:
        for delta in (4, 3, 2):
            check(f"complete D-{delta} ({day(delta)}) -> 201", False, "setup failed: no habit id")
        check("(a) streak 0 when last completion older than yesterday", False,
              "setup failed: no habit id")

    # (b) out-of-order insertion: today, then D-2, then D-1 -> streak 3
    hid = new_habit("b-unordered")
    check("(b) create habit", hid is not None)
    if hid is not None:
        complete(hid, 0)   # insert today first
        complete(hid, 2)   # then D-2
        complete(hid, 1)   # then D-1
        s = streak_of(hid)
        check("(b) out-of-order backdated completions -> streak 3", s == 3, f"got {s}")
        # Regardless of insertion order, history must come back in strictly
        # DESCENDING date order (SPEC). Fetch the habit and compare its history
        # to the same dates sorted newest-first: [D, D-1, D-2].
        rb = request("GET", f"/api/habits/{hid}")
        hist = (rb.json() or {}).get("history") if rb.status == 200 else None
        expected_desc = [day(0), day(1), day(2)]
        check("(b) history in descending date order after out-of-order inserts",
              hist == expected_desc, f"got {hist}")
    else:
        for delta in (0, 2, 1):
            check(f"complete D-{delta} ({day(delta)}) -> 201", False, "setup failed: no habit id")
        check("(b) out-of-order backdated completions -> streak 3", False,
              "setup failed: no habit id")
        check("(b) history in descending date order after out-of-order inserts", False,
              "setup failed: no habit id")

    # (c) duplicate via explicit today's date after default-complete -> 409
    hid = new_habit("c-dup")
    check("(c) create habit", hid is not None)
    if hid is not None:
        r = request("POST", f"/api/habits/{hid}/complete", {})  # default = today
        check("(c) default complete -> 201", r.status == 201, f"got {r.status}")
        completed_date = (r.json() or {}).get("date") if r.status == 201 else day(0)
        r = request("POST", f"/api/habits/{hid}/complete", {"date": completed_date})  # explicit today
        check("(c) explicit-today duplicate -> 409", r.status == 409, f"got {r.status}")
    else:
        check("(c) default complete -> 201", False, "setup failed: no habit id")
        check("(c) explicit-today duplicate -> 409", False, "setup failed: no habit id")

    # (d) yesterday only -> streak 1 (yesterday counts as current)
    hid = new_habit("d-yesterday")
    check("(d) create habit", hid is not None)
    if hid is not None:
        complete(hid, 1)
        s = streak_of(hid)
        check("(d) yesterday-only -> streak 1", s == 1, f"got {s}")
    else:
        check(f"complete D-1 ({day(1)}) -> 201", False, "setup failed: no habit id")
        check("(d) yesterday-only -> streak 1", False, "setup failed: no habit id")

    # (e) D-3 only -> streak 0 (too old to be current)
    hid = new_habit("e-old")
    check("(e) create habit", hid is not None)
    if hid is not None:
        complete(hid, 3)
        s = streak_of(hid)
        check("(e) D-3-only -> streak 0", s == 0, f"got {s}")
    else:
        check(f"complete D-3 ({day(3)}) -> 201", False, "setup failed: no habit id")
        check("(e) D-3-only -> streak 0", False, "setup failed: no habit id")

    # (f) malformed date -> 4xx
    hid = new_habit("f-baddate")
    check("(f) create habit", hid is not None)
    if hid is not None:
        r = request("POST", f"/api/habits/{hid}/complete", {"date": "2026-13-40"})
        check("(f) malformed date -> 4xx", r.status in (400, 422), f"got {r.status}")
    else:
        check("(f) malformed date -> 4xx", False, "setup failed: no habit id")

    # (g) after DELETE, GET -> 404 (and completions gone with it)
    hid = new_habit("g-delete")
    check("(g) create habit", hid is not None)
    if hid is not None:
        complete(hid, 0)
        r = request("DELETE", f"/api/habits/{hid}")
        check("(g) DELETE -> 204", r.status == 204, f"got {r.status}")
        r = request("GET", f"/api/habits/{hid}")
        check("(g) GET after delete -> 404", r.status == 404, f"got {r.status}")
    else:
        check(f"complete D-0 ({day(0)}) -> 201", False, "setup failed: no habit id")
        check("(g) DELETE -> 204", False, "setup failed: no habit id")
        check("(g) GET after delete -> 404", False, "setup failed: no habit id")

    total = _PASSED + _FAILED
    print(f"RESULT: {_PASSED}/{total} passed")
    sys.exit(0 if _FAILED == 0 else 1)


if __name__ == "__main__":
    run()
