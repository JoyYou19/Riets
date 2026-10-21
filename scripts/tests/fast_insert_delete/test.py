#!/usr/bin/env python3
"""Concurrent insert/delete black-box stress test.

Defaults: 1M docs, 4 shards, 32 workers, delete 60% of live docs.
Any failure (lost docs / count mismatch) -> dumps last commands + responses
to ./black_box.json and exits non-zero.
"""

import string
import argparse
import collections
import json
import random
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor, as_completed

import requests

BASE = ""
DB = ""
TOKEN = None

LOCK = threading.Lock()
# last insert/delete commands + responses
HISTORY = collections.deque(maxlen=20)

# good luck
POLICY = {
    "id": {"$type": "id"},
    "title":   {"$type": "text", "$exact": True,  "$weight": {"min": 80, "max": 95}},
    "body":    {"$type": "text", "$stemming": "english", "$weight": {"min": 35, "max": 55}},
    # floods token index
    "payload": {"$type": "text"},
    # random string, floods exact
    "blob":    {"$type": "text", "$exact": True},
    # repeated exact
    "tags":    [{"$type": "text", "$exact": True}],
    # repeated, floods tokens
    "attrs":   [{"$type": "text"}],
    "score":   {"$type": "float", "$searchable": True},             # numeric
    "count":   {"$type": "int",   "$searchable": True},             # numeric
    "is_active": {"$type": "bool"},                                 # bool
    "meta": [                                                       # nested struct (list of objects)
        {
            "k": {"$type": "text", "$exact": True},
            "v": {"$type": "text"},
            "n": {"$type": "int", "$searchable": True},
        }
    ],
}

_local = threading.local()


VOCAB_SIZE = 50_000
BODY_WORDS = (80, 200)       # random word counts per doc
PAYLOAD_WORDS = (120, 300)
BLOB_LEN = (24, 48)
TAGS_COUNT = (1, 5)
ATTRS_COUNT = (2, 8)
META_COUNT = (1, 4)

_rng = threading.local()


def rng():
    r = getattr(_rng, "r", None)
    if r is None:
        r = random.Random()
        _rng.r = r
    return r


def _token(r, lo=4, hi=12):
    return "".join(r.choices(string.ascii_lowercase, k=r.randint(lo, hi)))


# pre-generate a vocabulary once (thread-safe: read-only after this)
VOCAB = [_token(random.Random()) for _ in range(VOCAB_SIZE)]


def _words(r, n):
    return " ".join(r.choice(VOCAB) for _ in range(n))


def _rand_string(r):
    return "".join(r.choices(string.ascii_letters + string.digits, k=r.randint(*BLOB_LEN)))


def make_doc(x):
    r = rng()
    return {
        "id": x,
        "title": _words(r, r.randint(3, 8)),
        "body": _words(r, r.randint(*BODY_WORDS)),
        "payload": _words(r, r.randint(*PAYLOAD_WORDS)),
        "blob": _rand_string(r),
        "tags": [r.choice(VOCAB) for _ in range(r.randint(*TAGS_COUNT))],
        "attrs": [r.choice(VOCAB) for _ in range(r.randint(*ATTRS_COUNT))],
        "score": round(r.uniform(0.0, 1000.0), 3),
        "count": r.randint(0, 1_000_000),
        "is_active": r.random() < 0.5,
        "meta": [
            {"k": r.choice(VOCAB), "v": r.choice(
                VOCAB), "n": r.randint(0, 1000)}
            for _ in range(r.randint(*META_COUNT))
        ],
    }


def _session():
    s = getattr(_local, "s", None)
    if s is None:
        s = requests.Session()
        if TOKEN:
            s.headers["Authorization"] = "Bearer " + TOKEN
        _local.s = s
    return s


def _url(path):
    return f"{BASE}/api/databases/{DB}/{path}"


def _request(method, path, payload):
    url = _url(path)
    data = json.dumps(payload if payload is not None else {})
    headers = {"Content-Type": "application/json"}
    s = _session()
    if method == "POST":
        r = s.post(url, data=data, headers=headers, timeout=120)
    elif method == "DELETE":
        r = s.delete(url, data=data, headers=headers, timeout=120)
    else:
        r = s.get(url, timeout=120)
    try:
        body = r.json()
    except Exception:
        body = None
    return r.status_code, body


def record(op, payload, status, body):
    ids = [d["id"] for d in payload] if op == "insert" else payload
    with LOCK:
        HISTORY.append({
            "ts": round(time.time(), 3),
            "op": op,
            "count": len(ids),
            "ids_sample": (ids[:2] + ["..."] + ids[-2:]) if len(ids) > 5 else ids,
            "status": status,
            "response": body,
        })


def server_counts():
    _, body = _request("GET", "status", None)
    d = (body or {}).get("data", {}).get("indexed", {})
    return d.get("documents", 0), d.get("deleted", 0)


def dump_blackbox(reason, client_live):
    documents, deleted = server_counts()
    out = {
        "ts": round(time.time(), 3),
        "db": DB,
        "reason": reason,
        "client_live": client_live,
        "server_documents": documents,
        "server_deleted": deleted,
        "server_live": documents - deleted,
        "last_commands": list(HISTORY)[-3:],
    }
    with open("black_box.json", "w") as f:
        json.dump(out, f, indent=2, default=str)
    print(f"\n[FAIL] {reason} -> dumped ./black_box.json")


def recreate_db(shard_count):
    # delete if it exists (404 ok), retry "in use"
    for attempt in range(10):
        status, body = _request("DELETE", "delete-database", None)
        if status in (200, 404):
            break
        if status == 409:
            time.sleep(0.3 * (attempt + 1))
            continue
        raise RuntimeError(f"delete-database -> {status}: {str(body)[:200]}")
    status, body = _request("POST", "create-database",
                            {"shard_count": shard_count})
    if status not in (200, 201):
        raise RuntimeError(f"create-database -> {status}: {str(body)[:200]}")
    status, body = _request("POST", "set-policy", POLICY)
    if status != 200:
        raise RuntimeError(f"set-policy -> {status}: {str(body)[:200]}")


def insert_batch(ids):
    docs = [make_doc(x) for x in ids]
    status, body = _request("POST", "insert", docs)
    record("insert", docs, status, body)
    n = int((body or {}).get("data", {}).get("inserted", 0))
    return set(ids) if n == len(ids) else set()


def delete_batch(ids):
    status, body = _request("DELETE", "delete", ids)
    record("delete", ids, status, body)
    not_found = set()
    for f in (body or {}).get("data", {}).get("results", []):
        if f.get("code") == "not_found":
            not_found.add(f.get("id"))
    return set(ids) - not_found, not_found


def run_parallel(fn, batches, concurrency, desc):
    out, errors = [], []
    t0 = time.time()
    with ThreadPoolExecutor(max_workers=concurrency) as ex:
        futs = {ex.submit(fn, b): b for b in batches}
        for f in as_completed(futs):
            try:
                out.append(f.result())
            except Exception as e:
                errors.append(str(e)[:200])
    dt = time.time() - t0
    print(f"  [{desc}] {len(batches)} batches in {
          dt:.2f}s, {len(errors)} errors")
    for e in errors[:3]:
        print(f"      err: {e}")
    return out, errors


def main():
    global BASE, DB, TOKEN
    ap = argparse.ArgumentParser()
    ap.add_argument("--base-url", default="http://localhost:6006")
    ap.add_argument("--db", default="stress")
    ap.add_argument("--n", type=int, default=1_000_00)
    ap.add_argument("--concurrency", type=int, default=32)
    ap.add_argument("--batch", type=int, default=10000)
    ap.add_argument("--delete-fraction", type=float, default=0.6)
    ap.add_argument("--rounds", type=int, default=5)
    ap.add_argument("--round-size", type=int, default=2000)
    ap.add_argument("--shards", type=int, default=4)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--token", default=None)
    args = ap.parse_args()

    BASE = args.base_url.rstrip("/")
    DB = args.db
    TOKEN = args.token
    random.seed(args.seed)

    prefix = f"stress_{int(time.time() * 1000)}_{random.randrange(10**9)}_"

    print(f"== setup db '{DB}' ({args.shards} shards)")
    recreate_db(args.shards)

    ids = [f"{prefix}{i:07d}" for i in range(args.n)]
    live = set()
    lost = []  # ids the server said "not found" (the real bug signal)

    # ---- Phase 1: insert ----
    batches = [ids[i:i + args.batch] for i in range(0, len(ids), args.batch)]
    print(f"== INSERT {len(ids)} docs ({len(batches)} batches)")
    ins, _ = run_parallel(insert_batch, batches, args.concurrency, "insert")
    for ok in ins:
        live |= ok

    # ---- Phase 2: delete a fraction ----
    victims = random.sample(list(live), int(len(live) * args.delete_fraction))
    dbatches = [victims[i:i + args.batch]
                for i in range(0, len(victims), args.batch)]
    print(f"== DELETE {len(victims)} docs")
    dels, _ = run_parallel(delete_batch, dbatches, args.concurrency, "delete")
    for ok, nf in dels:
        live -= ok
        live -= nf
        lost += list(nf)

    # ---- Phase 3: mixed rounds ----
    next_id = args.n
    for r in range(args.rounds):
        new = [f"{prefix}{next_id + i:07d}" for i in range(args.round_size)]
        next_id += args.round_size
        to_del = random.sample(list(live), min(args.round_size, len(live)))
        nb = [new[i:i + args.batch] for i in range(0, len(new), args.batch)]
        db = [to_del[i:i + args.batch]
              for i in range(0, len(to_del), args.batch)]
        print(f"== MIX {r + 1}: ins {len(new)} / del {len(to_del)}")
        ins2, _ = run_parallel(insert_batch, nb, args.concurrency, "  ins")
        for ok in ins2:
            live |= ok
        del2, _ = run_parallel(delete_batch, db, args.concurrency, "  del")
        for ok, nf in del2:
            live -= ok
            live -= nf
            lost += list(nf)

    # ---- edge checks ----
    print("== edge checks")
    dup = random.sample(list(live), min(100, len(live)))
    _, body = _request("POST", "insert", [
                       {"id": x, "title": "dup"} for x in dup])
    dup_inserted = int((body or {}).get("data", {}).get("inserted", 0))
    print(f"  duplicate insert: {dup_inserted}/100 inserted (want 0)")

    ghost = [f"{prefix}ghost{i:07d}" for i in range(100)]
    _, body2 = _request("DELETE", "delete", ghost)
    ghost_deleted = int((body2 or {}).get("data", {}).get("deleted", 0))
    print(f"  ghost delete: {ghost_deleted}/100 deleted (want 0)")

    # ---- verify ----
    print("== verify")
    server_docs, server_deleted = server_counts()
    server_live = server_docs - server_deleted
    client_live = len(live)
    print(f"  client live      : {client_live}")
    print(f"  server live      : {server_live}  (documents {
          server_docs} - deleted {server_deleted})")
    print(f"  lost (not_found) : {len(lost)}")

    failures = []
    if lost:
        failures.append(f"{len(lost)} docs 'not found' on delete (lost)")
    if client_live != server_live:
        failures.append(f"count mismatch: client {
                        client_live} != server {server_live}")
    if dup_inserted != 0:
        failures.append(f"duplicate insert inserted {dup_inserted}/100")
    if ghost_deleted != 0:
        failures.append(f"ghost delete deleted {ghost_deleted}/100")

    if failures:
        dump_blackbox("; ".join(failures), client_live)
        print("\nRESULT: FAIL ❌  (" + "; ".join(failures) + ")")
        sys.exit(1)
    print("\nRESULT: PASS ✅")


if __name__ == "__main__":
    main()
