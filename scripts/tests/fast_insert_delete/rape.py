#!/usr/bin/env python3
"""Concurrent insert/delete stress test for Corelamo.

Phases:
  1. create a db + set a minimal TOML policy
  2. INSERT  N docs, in parallel, batched
  3. DELETE  a fraction of them, in parallel
  4. MIX     several rounds: parallel insert (fresh ids) + delete (live ids)
  5. VERIFY  server live count == client-side live set, plus no dup ids

The one invariant this all reduces to:
    live_on_server == live_we_think_we_have

Run:
  python3 stress_insert_delete.py --base-url http://localhost:6006 --db stress \
      --n 50000 --concurrency 64 --delete-fraction 0.4 --rounds 5
"""

import argparse
import json
import random
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor, as_completed

import requests

POLICY_TOML = """\
[[fields]]
name = "id"
kind = "Id"

[[fields]]
name = "title"
kind = "Text"

[[fields]]
name = "body"
kind = "Text"
stemming = "english"

[fields.weight]
min = 35
max = 55
"""

# --------------------------------------------------------------------------
# HTTP plumbing (one requests.Session per thread)
# --------------------------------------------------------------------------
_local = threading.local()
BASE = ""
TOKEN = None


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


def _post(path, payload, expect=(200, 201)):
    r = _session().post(_url(path), data=json.dumps(payload),
                        headers={"Content-Type": "application/json"}, timeout=60)
    if r.status_code not in expect:
        raise RuntimeError(f"{path} -> {r.status_code}: {r.text[:300]}")
    return r.json()


def _delete(path, payload):
    r = _session().delete(_url(path), data=json.dumps(payload),
                          headers={"Content-Type": "application/json"}, timeout=60)
    if r.status_code != 200:
        raise RuntimeError(f"{path} -> {r.status_code}: {r.text[:300]}")
    return r.json()


def _get(path):
    r = _session().get(_url(path), timeout=60)
    if r.status_code != 200:
        raise RuntimeError(f"{path} -> {r.status_code}: {r.text[:300]}")
    return r.json()


# --------------------------------------------------------------------------
# Setup / teardown
# --------------------------------------------------------------------------
def recreate_db(shard_count):
    # create (ignore "already exists"), then force a clean slate by deleting + recreating
    try:
        _post("create-database",
              {"shard_count": shard_count}, expect=(200, 201, 409, 400))
    except RuntimeError:
        pass
    # if it existed, drop it so every run starts empty
    try:
        _delete("delete-database", {})
    except RuntimeError:
        pass
    _post("create-database", {"shard_count": shard_count}, expect=(200, 201))
    _post("set-policy", POLICY_TOML)


# --------------------------------------------------------------------------
# Operations (each returns the ids that actually succeeded)
# --------------------------------------------------------------------------
def insert_ids(ids, batch_size=200):
    """Insert a list of ids (doc body derived from id). Returns set of inserted ids."""
    ok = set()
    for i in range(0, len(ids), batch_size):
        batch = ids[i:i + batch_size]
        docs = [{"id": x, "title": f"doc {x}", "body": f"body for {x} " * 10}
                for x in batch]
        resp = _post("insert", docs)
        n = int(resp.get("data", {}).get("inserted", 0))
        # with unique ids, inserted == len(batch); be safe and only trust what the server said
        if n == len(batch):
            ok.update(batch)
        else:
            # partial: we can't tell which ones failed from the count alone -> don't claim them
            pass
    return ok


def delete_ids(ids, batch_size=200):
    """Delete a list of ids. Returns set of ids the server reported deleted."""
    ok = set()
    for i in range(0, len(ids), batch_size):
        batch = ids[i:i + batch_size]
        resp = _delete("delete", batch)
        n = int(resp.get("data", {}).get("deleted", 0))
        if n == len(batch):
            ok.update(batch)
    return ok


def live_count_from_status():
    d = _get("status")["data"]["indexed"]
    return d["documents"], d["deleted"]


def search_page(docs=5000):
    return _post("search", {"query": "match_all", "docs": docs})


# --------------------------------------------------------------------------
# Parallel driver
# --------------------------------------------------------------------------
def run_parallel(fn, items, concurrency, desc):
    """Run fn(item) over items across `concurrency` threads; returns (ok_total, errors)."""
    ok = 0
    errors = []
    t0 = time.time()
    with ThreadPoolExecutor(max_workers=concurrency) as ex:
        futs = {ex.submit(fn, it): it for it in items}
        for f in as_completed(futs):
            try:
                ok += f.result()
            except Exception as e:  # network / 5xx / parse
                errors.append(str(e)[:200])
    dt = time.time() - t0
    rate = ok / dt if dt > 0 else 0.0
    print(f"  [{desc}] {ok} ok in {dt:.2f}s ({
          rate:,.0f}/s), {len(errors)} errors")
    for e in errors[:3]:
        print(f"      err: {e}")
    return ok, errors


# --------------------------------------------------------------------------
# Main
# --------------------------------------------------------------------------
def main():
    global BASE, DB, TOKEN
    ap = argparse.ArgumentParser()
    ap.add_argument("--base-url", default="http://localhost:6006")
    ap.add_argument("--db", default="stress")
    ap.add_argument("--n", type=int, default=50000, help="docs to insert")
    ap.add_argument("--concurrency", type=int, default=64)
    ap.add_argument("--batch", type=int, default=200,
                    help="docs per HTTP request")
    ap.add_argument("--delete-fraction", type=float, default=0.4)
    ap.add_argument("--rounds", type=int, default=5,
                    help="mixed insert+delete rounds")
    ap.add_argument("--round-size", type=int, default=2000)
    ap.add_argument("--shards", type=int, default=2)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--token", default=None)
    args = ap.parse_args()

    BASE = args.base_url.rstrip("/")
    DB = args.db
    TOKEN = args.token
    random.seed(args.seed)

    run = f"{int(time.time())}_{random.randrange(10**6)}"
    prefix = f"stress_{run}_"

    print(f"== setup db '{DB}' ({args.shards} shards)")
    recreate_db(args.shards)

    # ---- Phase 1: insert N unique ids in parallel ----
    ids = [f"{prefix}{i:07d}" for i in range(args.n)]
    batches = [ids[i:i + args.batch] for i in range(0, len(ids), args.batch)]
    print(f"== INSERT {len(ids)} docs ({len(batches)} requests)")
    inserted, errs = run_parallel(
        lambda b: len(insert_ids(b, args.batch)), batches, args.concurrency, "insert")
    live = set(ids)  # all unique, expect all inserted
    if inserted != len(ids):
        print(f"  !! expected {len(ids)} inserted, server says {inserted}")
        live = set()  # can't trust it; fail below
    else:
        live = set(ids)

    # ---- Phase 2: delete a fraction in parallel ----
    del_n = int(len(ids) * args.delete_fraction)
    victims = random.sample(ids, del_n)
    print(f"== DELETE {len(victims)} docs")
    del_batches = [victims[i:i + args.batch]
                   for i in range(0, len(victims), args.batch)]
    deleted, errs2 = run_parallel(
        lambda b: len(delete_ids(b, args.batch)), del_batches, args.concurrency, "delete")
    if deleted == len(victims):
        live -= set(victims)

    # ---- Phase 3: mixed rounds (insert fresh + delete live) ----
    next_id = args.n
    for r in range(args.rounds):
        new = [f"{prefix}{next_id + i:07d}" for i in range(args.round_size)]
        next_id += args.round_size
        still_live = list(live)
        to_del = random.sample(still_live, min(
            args.round_size, len(still_live)))

        nb = [new[i:i + args.batch] for i in range(0, len(new), args.batch)]
        db = [to_del[i:i + args.batch]
              for i in range(0, len(to_del), args.batch)]
        print(f"== MIX round {
              r + 1}: insert {len(new)} / delete {len(to_del)}")
        ins_ok, _ = run_parallel(lambda b: len(insert_ids(
            b, args.batch)), nb, args.concurrency, "  ins")
        del_ok, _ = run_parallel(lambda b: len(delete_ids(
            b, args.batch)), db, args.concurrency, "  del")
        live |= set(new) if ins_ok == len(new) else set()
        live -= set(to_del) if del_ok == len(to_del) else set()

    # ---- Phase 4: edge checks ----
    print("== edge checks")
    # duplicate insert must fail
    dup = random.sample(list(live), min(100, len(live)))
    r = _post("insert", [{"id": x, "title": "dup"} for x in dup])
    dup_inserted = int(r.get("data", {}).get("inserted", 0))
    print(f"  duplicate insert: server inserted {
          dup_inserted} of {len(dup)} (want 0)")

    # delete a non-existent id must report 0 deleted
    ghost = [f"{prefix}ghost{i:07d}" for i in range(100)]
    r2 = _delete("delete", ghost)
    ghost_deleted = int(r2.get("data", {}).get("deleted", 0))
    print(f"  ghost delete: server deleted {
          ghost_deleted} of {len(ghost)} (want 0)")

    # ---- VERIFY ----
    print("== verify")
    server_docs, server_deleted = live_count_from_status()
    server_live = server_docs - server_deleted
    client_live = len(live)

    # no duplicate ids in a match_all page
    page = search_page()
    got_ids = [h["id"] for h in page.get("data", [])]
    dup_count = len(got_ids) - len(set(got_ids))

    print(f"  client live set : {client_live}")
    print(f"  server live     : {server_live}  (documents {
          server_docs} - deleted {server_deleted})")
    print(f"  match_all total : {page['total_hits']['value']}")
    print(f"  duplicate ids in page: {dup_count}")
    print(f"  errors during run: insert={len(errs)} delete={len(errs2)}")

    ok = (server_live == client_live) and (dup_count == 0) and (
        dup_inserted == 0) and (ghost_deleted == 0)
    print("\nRESULT:", "PASS ✅" if ok else "FAIL ❌")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
