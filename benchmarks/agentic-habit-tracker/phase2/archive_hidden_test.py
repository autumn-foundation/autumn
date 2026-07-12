#!/usr/bin/env python3
"""Hidden Phase-2 acceptance tests for the habit-tracker benchmark.

Not shown to the building agent. Same framework-agnostic shape as
acceptance/contract_test.py and hidden/hidden_test.py: HTTP-only, stdlib only,
BASE_URL from env. Probes the archive edge cases (SPEC.md §4,
prompts/phase2_maintenance.md): default-active, archive idempotency, the
optional explicit `?archived=false` filter, and streak correctness for an
archived habit. Dates are computed relative to the server's returned date so
results are deterministic on any calendar day.

Stdlib only (urllib/json/datetime/sys/os/time). No external dependencies.

Usage:
    BASE_URL=http://localhost:8080 python3 archive_hidden_test.py

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


# ── helpers ──────────────────────────────────────────────────────────────────

def new_habit(name):
    r = request("POST", "/api/habits", {"name": name})
    return r, (r.json().get("id") if r.status == 201 else None)


def id_list(path):
    """GET a listing and return the list of stringified ids (preserving dups), or None."""
    r = request("GET", path)
    if r.status != 200:
        return None
    body = r.json()
    if not isinstance(body, list):
        return None
    return [str(h.get("id")) for h in body]


def contains(ids, hid):
    return ids is not None and str(hid) in ids


# ── tests ──────────────────────────────────────────────────────────────────────

def run():
    if not wait_for_server():
        print("RESULT: 0/1 passed")
        sys.exit(1)

    # (a) default-active: a newly created habit is active — present in the default
    #     list and absent from ?archived=true.
    _, active_hid = new_habit("h-default-active")
    check("(a) create habit", active_hid is not None)
    if active_hid is not None:
        check("(a) new habit present in default list",
              contains(id_list("/api/habits"), active_hid))
        check("(a) new habit absent from ?archived=true",
              not contains(id_list("/api/habits?archived=true"), active_hid))

    # (b) archive idempotency: POST archive twice -> both 200/204; H appears
    #     exactly once in ?archived=true and never in the default list.
    _, hid = new_habit("h-idempotent")
    check("(b) create habit", hid is not None)
    if hid is not None:
        r1 = request("POST", f"/api/habits/{hid}/archive")
        check("(b) first archive -> 200/204", r1.status in (200, 204), f"got {r1.status}")
        r2 = request("POST", f"/api/habits/{hid}/archive")
        check("(b) second archive (idempotent) -> 200/204", r2.status in (200, 204),
              f"got {r2.status}")
        archived = id_list("/api/habits?archived=true")
        count = archived.count(str(hid)) if archived is not None else -1
        check("(b) archived habit appears exactly once in ?archived=true", count == 1,
              f"count={count}")
        check("(b) archived habit never in default list",
              not contains(id_list("/api/habits"), hid))

    # (c) explicit ?archived=false mirrors the default (active only). SPEC §4 says
    #     this param MAY be treated the same as no query, so explicit support is
    #     optional: when the endpoint returns a 200 JSON list we require it to
    #     behave like the default (an active witness present, the archived habit
    #     absent); otherwise we only require it not to be a server error (don't
    #     over-specify beyond SPEC). `active_hid` is still active; `hid` is archived.
    if active_hid is not None and hid is not None:
        r = request("GET", "/api/habits?archived=false")
        if r.status == 200 and isinstance(r.json(), list):
            ids = [str(h.get("id")) for h in r.json()]
            ok = str(active_hid) in ids and str(hid) not in ids
            check("(c) ?archived=false returns only active habits", ok,
                  f"active_present={str(active_hid) in ids}, archived_present={str(hid) in ids}")
        else:
            check("(c) ?archived=false optional param handled without server error",
                  r.status < 500, f"got {r.status}")

    # (d) an archived habit still computes its streak correctly. Complete today
    #     (default date, so we use the server-returned date) and yesterday, then
    #     archive, then GET -> current_streak == 2 with both dates in history.
    _, h2 = new_habit("h-archived-streak")
    check("(d) create habit", h2 is not None)
    if h2 is not None:
        rc = request("POST", f"/api/habits/{h2}/complete", {})  # default = today
        check("(d) complete today (default) -> 201", rc.status == 201, f"got {rc.status}")
        today_str = rc.json().get("date") if rc.status == 201 else None
        if today_str:
            yesterday = datetime.date.fromisoformat(today_str) - datetime.timedelta(days=1)
            yesterday_str = yesterday.isoformat()
            rc2 = request("POST", f"/api/habits/{h2}/complete", {"date": yesterday_str})
            check("(d) complete yesterday (backdated) -> 201", rc2.status == 201,
                  f"got {rc2.status}")
            ra = request("POST", f"/api/habits/{h2}/archive")
            check("(d) archive habit -> 200/204", ra.status in (200, 204), f"got {ra.status}")
            r = request("GET", f"/api/habits/{h2}")
            check("(d) GET archived habit -> 200", r.status == 200, f"got {r.status}")
            got = r.json() if r.status == 200 else {}
            check("(d) archived habit current_streak == 2", got.get("current_streak") == 2,
                  f"got {got.get('current_streak')}")
            hist = got.get("history") if isinstance(got.get("history"), list) else []
            check("(d) archived habit history contains both completed dates",
                  today_str in hist and yesterday_str in hist,
                  f"history={hist}")

    total = _PASSED + _FAILED
    print(f"RESULT: {_PASSED}/{total} passed")
    sys.exit(0 if _FAILED == 0 else 1)


if __name__ == "__main__":
    run()
