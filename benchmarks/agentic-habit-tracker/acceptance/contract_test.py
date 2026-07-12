#!/usr/bin/env python3
"""Visible acceptance tests for the habit-tracker benchmark.

Framework-agnostic: talks to a running HTTP server over HTTP only, so the same
file works against Autumn, Django, Rails, Spring Boot, Phoenix, Loco, ...

Stdlib only (urllib/json/datetime/sys/os). No external dependencies.

Usage:
    BASE_URL=http://localhost:8080 python3 contract_test.py

Exits non-zero if any check fails.
"""

import datetime
import json
import os
import sys
import time
import urllib.error
import urllib.request

BASE_URL = os.environ.get("BASE_URL", "http://localhost:8080").rstrip("/")

_PASSED = 0
_FAILED = 0


# ── HTTP helper ────────────────────────────────────────────────────────────────

class Resp:
    def __init__(self, status, headers, body_bytes):
        self.status = status
        self.headers = headers  # dict, lowercased keys
        self.body_bytes = body_bytes

    def json(self):
        return json.loads(self.body_bytes.decode("utf-8"))

    def content_type(self):
        return self.headers.get("content-type", "")


def request(method, path, body=None):
    """Perform an HTTP request. `path` may be absolute or relative to BASE_URL."""
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


# ── assertions ─────────────────────────────────────────────────────────────────

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
    """Retry connecting until the server answers or the timeout elapses."""
    deadline = time.time() + timeout_s
    last = None
    while time.time() < deadline:
        try:
            r = request("GET", "/api/habits")
            if r.status < 500:
                return True
        except Exception as e:  # connection refused, etc.
            last = e
        time.sleep(0.5)
    print(f"FAIL  server did not become ready within {timeout_s}s ({last})")
    return False


def iso(d):
    return d.isoformat()


# ── tests ──────────────────────────────────────────────────────────────────────

def run():
    if not wait_for_server():
        print("RESULT: 0/1 passed")
        sys.exit(1)

    # create
    r = request("POST", "/api/habits", {"name": "Read", "description": "20 pages"})
    check("POST /api/habits -> 201", r.status == 201, f"got {r.status}")
    habit = r.json() if r.status == 201 else {}
    check("created habit has id", "id" in habit)
    check("created habit echoes name", habit.get("name") == "Read")
    check("created habit has created_at", "created_at" in habit)
    hid = habit.get("id")

    # validation: empty name
    r = request("POST", "/api/habits", {"name": ""})
    check("POST empty name -> 4xx", r.status in (400, 422), f"got {r.status}")

    # validation: missing name
    r = request("POST", "/api/habits", {"description": "no name"})
    check("POST missing name -> 4xx", r.status in (400, 422), f"got {r.status}")

    # list
    r = request("GET", "/api/habits")
    check("GET /api/habits -> 200", r.status == 200, f"got {r.status}")
    listed = r.json() if r.status == 200 else None
    check("GET /api/habits returns array", isinstance(listed, list))
    check(
        "created habit present in list",
        isinstance(listed, list) and any(str(h.get("id")) == str(hid) for h in listed),
    )

    # get single
    r = request("GET", f"/api/habits/{hid}")
    check("GET /api/habits/{id} -> 200", r.status == 200, f"got {r.status}")
    got = r.json() if r.status == 200 else {}
    check("GET single has current_streak", "current_streak" in got)
    check("GET single has history array", isinstance(got.get("history"), list))

    # unknown id -> 404. Use a real returned-ID shape: create a throwaway habit,
    # capture its id, DELETE it, then look up the now-deleted id. SPEC allows
    # opaque string/UUID ids, so a hard-coded numeric sentinel could be rejected
    # as a malformed route param (400) before any not-found lookup happens.
    r = request("POST", "/api/habits", {"name": "throwaway-404"})
    deleted_id = r.json().get("id") if r.status == 201 else None
    request("DELETE", f"/api/habits/{deleted_id}")
    r = request("GET", f"/api/habits/{deleted_id}")
    check("GET unknown id -> 404", r.status == 404, f"got {r.status}")

    # update
    r = request("PUT", f"/api/habits/{hid}", {"name": "Read more", "description": "30 pages"})
    check("PUT /api/habits/{id} -> 200", r.status == 200, f"got {r.status}")
    updated = r.json() if r.status == 200 else {}
    check("PUT updates name", updated.get("name") == "Read more")

    # complete today (default date)
    r = request("POST", f"/api/habits/{hid}/complete", {})
    check("POST complete (default date) -> 201", r.status == 201, f"got {r.status}")
    comp = r.json() if r.status == 201 else {}
    check("complete returns date", "date" in comp)
    check("complete returns current_streak", "current_streak" in comp)

    # duplicate same-day -> 409 (reuse the server-returned completion date so
    # we collide on the same day even across a midnight boundary)
    completed_date = comp.get("date") if isinstance(comp, dict) and "date" in comp else iso(datetime.date.today())
    r = request("POST", f"/api/habits/{hid}/complete", {"date": completed_date})
    check("POST duplicate same-day -> 409", r.status == 409, f"got {r.status}")

    # invalid date -> 4xx
    r = request("POST", f"/api/habits/{hid}/complete", {"date": "2026-13-40"})
    check("POST invalid date -> 4xx", r.status in (400, 422), f"got {r.status}")

    # complete on unknown habit -> 404 (reuse the deleted real-shaped id above)
    r = request("POST", f"/api/habits/{deleted_id}/complete", {})
    check("POST complete unknown habit -> 404", r.status == 404, f"got {r.status}")

    # basic streak: 3 consecutive backdated days ending today
    r = request("POST", "/api/habits", {"name": "Streaker"})
    check("POST streak habit -> 201", r.status == 201, f"got {r.status}")
    sid = r.json().get("id") if r.status == 201 else None
    today = datetime.date.today()
    for delta in (2, 1, 0):  # T-2, T-1, T
        d = today - datetime.timedelta(days=delta)
        rr = request("POST", f"/api/habits/{sid}/complete", {"date": iso(d)})
        check(f"complete {iso(d)} -> 201", rr.status == 201, f"got {rr.status}")
    r = request("GET", f"/api/habits/{sid}")
    streak = r.json().get("current_streak") if r.status == 200 else None
    check("streak of 3 consecutive days == 3", streak == 3, f"got {streak}")
    hist = r.json().get("history") if r.status == 200 else None
    expected_hist = [iso(today - datetime.timedelta(days=d)) for d in (0, 1, 2)]
    check("history is 3 dates descending", hist == expected_hist, f"got {hist}")

    # delete
    r = request("DELETE", f"/api/habits/{hid}")
    check("DELETE /api/habits/{id} -> 204", r.status == 204, f"got {r.status}")
    r = request("GET", f"/api/habits/{hid}")
    check("GET after delete -> 404", r.status == 404, f"got {r.status}")

    # HTML view
    r = request("GET", "/")
    check("GET / -> 200", r.status == 200, f"got {r.status}")
    check(
        "GET / is text/html",
        "text/html" in r.content_type().lower(),
        f"content-type={r.content_type()!r}",
    )

    total = _PASSED + _FAILED
    print(f"RESULT: {_PASSED}/{total} passed")
    sys.exit(0 if _FAILED == 0 else 1)


if __name__ == "__main__":
    run()
