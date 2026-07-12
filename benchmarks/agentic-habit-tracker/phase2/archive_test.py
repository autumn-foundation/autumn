#!/usr/bin/env python3
"""Visible Phase-2 acceptance tests for the habit-tracker benchmark.

Phase 2 adds "archived habits" (soft-delete/hide). See SPEC.md §4 and
prompts/phase2_maintenance.md. This suite exercises the archive endpoint and
the list-filtering behavior over HTTP only, so — like acceptance/contract_test.py
— the same file works against any framework that satisfies the contract.

Stdlib only (urllib/json/datetime/sys/os/time). No external dependencies.

Usage:
    BASE_URL=http://localhost:8080 python3 archive_test.py

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


def request(method, path, body=None, accept="application/json"):
    """Perform an HTTP request. `path` may be absolute or relative to BASE_URL.

    `accept` controls the Accept header: JSON endpoints keep the default, while
    the server-rendered HTML view is fetched with ``accept="text/html"`` so a
    content-negotiating server isn't wrongly asked for JSON on an HTML route.
    """
    url = path if path.startswith("http") else BASE_URL + path
    data = None
    headers = {"Accept": accept}
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


# ── helpers ──────────────────────────────────────────────────────────────────

def new_habit(name):
    """Create a habit; return (response, id)."""
    r = request("POST", "/api/habits", {"name": name})
    hid = r.json().get("id") if r.status == 201 else None
    return r, hid


def list_ids(path):
    """GET a habits listing and return the set of stringified ids (or None on error)."""
    r = request("GET", path)
    if r.status != 200:
        return None
    body = r.json()
    if not isinstance(body, list):
        return None
    return {str(h.get("id")) for h in body}


def contains(ids, hid):
    return ids is not None and str(hid) in ids


# ── tests ──────────────────────────────────────────────────────────────────────

def run():
    if not wait_for_server():
        print("RESULT: 0/1 passed")
        sys.exit(1)

    # create habit H -> should be active (archived false or absent from archived list)
    r, hid = new_habit("archive-me")
    check("POST /api/habits -> 201", r.status == 201, f"got {r.status}")
    check("created habit has id", hid is not None)
    if hid is None:
        # Denominator is intentionally fixed at 16: a failed setup must not
        # shrink the total and inflate the pass rate. Record every dependent
        # check as an explicit failure instead of exiting early and skipping them.
        for label in (
            "newly created habit is active (archived false/absent AND not in archived list)",
            "created habit present in default list",
            "POST /api/habits/{id}/archive -> 200/204",
            "archived habit absent from default list",
            "archived habit present in ?archived=true list",
            "GET archived habit -> 200",
            "archived habit still has current_streak",
            "archived habit still has history array",
            "POST archive unknown id -> 404",
            "POST second habit -> 201",
            "non-archived habit still present in default list after archiving another",
            "archived habit still absent from default list (regression)",
            "archived habit name absent from HTML GET / view",
            "active habit name present in HTML GET / view",
        ):
            check(label, False, "setup failed: no habit id")
        total = _PASSED + _FAILED
        print(f"RESULT: {_PASSED}/{total} passed")
        sys.exit(1)
    archived_field = r.json().get("archived")
    archived_ids = list_ids("/api/habits?archived=true")
    check(
        "newly created habit is active (archived false/absent AND not in archived list)",
        archived_field in (False, None) and not contains(archived_ids, hid),
        f"archived={archived_field!r}, in_archived_list={contains(archived_ids, hid)}",
    )

    # H appears in default list
    default_ids = list_ids("/api/habits")
    check("created habit present in default list", contains(default_ids, hid))

    # archive H -> 200 or 204 (SPEC allows either)
    r = request("POST", f"/api/habits/{hid}/archive")
    check("POST /api/habits/{id}/archive -> 200/204", r.status in (200, 204), f"got {r.status}")

    # after archive: default list does NOT contain H
    default_ids = list_ids("/api/habits")
    check("archived habit absent from default list", not contains(default_ids, hid))

    # after archive: ?archived=true DOES contain H
    archived_ids = list_ids("/api/habits?archived=true")
    check("archived habit present in ?archived=true list", contains(archived_ids, hid))

    # GET single still works for the archived habit, with streak + history
    r = request("GET", f"/api/habits/{hid}")
    check("GET archived habit -> 200", r.status == 200, f"got {r.status}")
    got = r.json() if r.status == 200 else {}
    check("archived habit still has current_streak", "current_streak" in got)
    check("archived habit still has history array", isinstance(got.get("history"), list))

    # archive unknown id -> 404. Use a real returned-ID shape: create a
    # throwaway habit, capture its id, DELETE it, then archive the now-deleted
    # id. SPEC allows opaque string/UUID ids, so a hard-coded numeric sentinel
    # could be rejected as a malformed route param (400) before the not-found
    # lookup happens.
    _, deleted_id = new_habit("throwaway-404")
    request("DELETE", f"/api/habits/{deleted_id}")
    r = request("POST", f"/api/habits/{deleted_id}/archive")
    check("POST archive unknown id -> 404", r.status == 404, f"got {r.status}")

    # regression: a second, non-archived habit still shows up in the default list
    r2, hid2 = new_habit("keep-me")
    check("POST second habit -> 201", r2.status == 201, f"got {r2.status}")
    default_ids = list_ids("/api/habits")
    check("non-archived habit still present in default list after archiving another",
          contains(default_ids, hid2))
    check("archived habit still absent from default list (regression)",
          not contains(default_ids, hid))

    # HTML view must list only non-archived habits (phase-2 brief / SPEC §4).
    # Create one habit to archive and one to keep active, both with unique,
    # HTML-greppable names, then assert the archived name is absent from the
    # server-rendered HTML body while the active name is present. Fail-closed:
    # if either habit can't be created or the archive doesn't take, record both
    # checks as failures rather than skipping (keeps the denominator at 16).
    HIDDEN_NAME = "ZzArchivedHidden-Q7"
    ACTIVE_NAME = "ZzActiveVisible-Q7"
    _, hidden_hid = new_habit(HIDDEN_NAME)
    _, active_hid = new_habit(ACTIVE_NAME)
    html_setup_ok = hidden_hid is not None and active_hid is not None
    if html_setup_ok:
        ar = request("POST", f"/api/habits/{hidden_hid}/archive")
        html_setup_ok = ar.status in (200, 204)
    if html_setup_ok:
        r = request("GET", "/", accept="text/html")
        html_ok = r.status == 200 and "text/html" in r.content_type().lower()
        body_text = r.body_bytes.decode("utf-8", "replace") if html_ok else ""
        check(
            "archived habit name absent from HTML GET / view",
            html_ok and HIDDEN_NAME not in body_text,
            f"status={r.status}, ct={r.content_type()!r}, present={HIDDEN_NAME in body_text}",
        )
        check(
            "active habit name present in HTML GET / view",
            html_ok and ACTIVE_NAME in body_text,
            f"status={r.status}, ct={r.content_type()!r}, present={ACTIVE_NAME in body_text}",
        )
    else:
        check("archived habit name absent from HTML GET / view", False,
              "setup failed: could not create/archive habits for HTML view check")
        check("active habit name present in HTML GET / view", False,
              "setup failed: could not create/archive habits for HTML view check")

    total = _PASSED + _FAILED
    print(f"RESULT: {_PASSED}/{total} passed")
    sys.exit(0 if _FAILED == 0 else 1)


if __name__ == "__main__":
    run()
