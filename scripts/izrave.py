#!/usr/bin/env python3
"""
delete_random.py -- repeatedly collect real document ids via search and
delete them, looping until search finds nothing left (or a safety cap on
rounds is hit).

    python3 delete_random.py
"""

import http.client
import json
import sys
import time
import urllib.parse

# ---------------------------------------------------------------- CONFIG ---

DB_NAME = "movies"
SERVER = "http://localhost:6006"
USERNAME = "admin"
PASSWORD = "secret"

BATCH_SIZE = 10
COLLECT_QUERIES = [
    "century", "government", "film", "university", "war", "music",
    "species", "river", "population", "history", "school", "state",
    "born", "released", "known", "team", "company", "national",
    "book", "city", "world", "played", "began", "later", "american",
    "english", "system", "area", "north", "south",
]
HITS_PER_QUERY = 1000
MAX_ROUNDS = 200   # safety cap so a stuck loop doesn't run forever

# ---------------------------------------------------------------------------


class Server:
    def __init__(self, base):
        p = urllib.parse.urlsplit(base)
        self.https = p.scheme == "https"
        self.host, self.port = p.hostname, p.port or (
            443 if self.https else 80)
        self.conn, self.token = None, None

    def call(self, method, path, body=b""):
        if isinstance(body, str):
            body = body.encode("utf-8")
        headers = {"Accept": "application/json",
                   "Content-Type": "application/json",
                   "Content-Length": str(len(body))}
        if self.token:
            headers["X-Corelamo-Key"] = self.token

        for attempt in range(5):
            try:
                if self.conn is None:
                    cls = (http.client.HTTPSConnection if self.https
                           else http.client.HTTPConnection)
                    self.conn = cls(self.host, self.port, timeout=600)
                self.conn.request(method, path, body=body, headers=headers)
                resp = self.conn.getresponse()
                data = resp.read().decode("utf-8", "replace")
                if resp.status < 500:
                    return resp.status, data
                last = f"HTTP {resp.status}: {data[:200]}"
            except (http.client.HTTPException, OSError) as e:
                last = f"{type(e).__name__}: {e}"
                try:
                    self.conn.close()
                except Exception:
                    pass
                self.conn = None
            time.sleep(2 ** attempt)
        sys.exit(f"[error] {method} {path}: {last}")

    def login(self, user, pw):
        status, text = self.call("POST", "/api/login",
                                 json.dumps({"username": user, "password": pw}))
        if status != 200:
            sys.exit(f"[error] login failed (HTTP {status}): {text[:200]}")
        try:
            self.token = json.loads(text)["data"]["token"]
        except Exception:
            sys.exit(f"[error] no token in login response: {text[:200]}")


def get_document_count(srv):
    status, text = srv.call("GET", f"/api/databases/{DB_NAME}/status")
    if status != 200:
        sys.exit(f"[error] status failed (HTTP {status}): {text[:300]}")
    try:
        return json.loads(text)["data"]["indexed"]["documents"]
    except Exception:
        sys.exit(f"[error] could not read document count from status: {
                 text[:300]}")


def collect_ids(srv):
    """One pass: gathers whatever real ids search can currently surface."""
    ids = set()
    search = f"/api/databases/{DB_NAME}/search"
    for q in COLLECT_QUERIES:
        body = json.dumps({"query": q, "docs": HITS_PER_QUERY})
        status, text = srv.call("POST", search, body)
        if status != 200:
            print(f"[warn] search '{q}' failed (HTTP {status}): {text[:200]}")
            continue
        try:
            hits = json.loads(text)["data"]
        except Exception:
            print(f"[warn] could not parse search response for '{q}'")
            continue
        for hit in hits:
            ids.add(hit["id"])
    return list(ids)


def delete_ids(srv, ids):
    delete = f"/api/databases/{DB_NAME}/delete"
    deleted, failed = 0, 0
    for i in range(0, len(ids), BATCH_SIZE):
        chunk = ids[i:i + BATCH_SIZE]
        status, text = srv.call(
            "DELETE", delete, json.dumps(chunk).encode("utf-8"))
        if status >= 400:
            print(f"[warn] delete batch failed (HTTP {status}): {text[:300]}")
            failed += len(chunk)
        else:
            deleted += len(chunk)
    return deleted, failed


def main():
    srv = Server(SERVER)
    srv.login(USERNAME, PASSWORD)
    print("[info] logged in")

    total_start = get_document_count(srv)
    print(f"[info] {total_start:,} documents currently indexed")

    round_num = 0
    total_deleted = 0
    start = time.monotonic()

    while round_num < MAX_ROUNDS:
        round_num += 1
        ids = collect_ids(srv)

        if not ids:
            print(f"[info] round {round_num}: search found 0 ids — stopping")
            break

        deleted, failed = delete_ids(srv, ids)
        total_deleted += deleted
        remaining = get_document_count(srv)
        elapsed = time.monotonic() - start

        print(f"[info] round {round_num}: found {len(ids):,} ids, "
              f"deleted {deleted:,} (failed {failed:,}), "
              f"remaining {remaining:,}, elapsed {elapsed:.1f}s")

        if remaining == 0:
            print("[info] document count reached 0 — stopping")
            break

    else:
        print(f"[warn] hit MAX_ROUNDS={MAX_ROUNDS} without reaching 0 — "
              f"search sampling may be stuck finding the same leftover docs")

    final = get_document_count(srv)
    print(f"[info] total rounds: {
          round_num}, total deleted: {total_deleted:,}")
    print(f"[info] documents remaining: {final:,}")
    print("[info] done")


if __name__ == "__main__":
    main()
