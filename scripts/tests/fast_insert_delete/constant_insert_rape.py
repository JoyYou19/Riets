#!/usr/bin/env python3
"""Async insert hammer — 5 workers, each fires requests without blocking."""

import argparse
import asyncio
import json
import random
import string
import sys
import threading
import time

import aiohttp

BASE = ""
DB = "stress"
TOKEN = None
STOP = threading.Event()

POLICY = {
    "id": {"$type": "id"},
    "title":   {"$type": "text", "$exact": True,  "$weight": {"min": 80, "max": 95}},
    "body":    {"$type": "text", "$stemming": "english", "$weight": {"min": 35, "max": 55}},
    "payload": {"$type": "text"},
    "blob":    {"$type": "text", "$exact": True},
    "tags":    [{"$type": "text", "$exact": True}],
    "attrs":   [{"$type": "text"}],
    "score":   {"$type": "float", "$searchable": True},
    "count":   {"$type": "int",   "$searchable": True},
    "is_active": {"$type": "bool"},
    "meta": [
        {"k": {"$type": "text", "$exact": True},
         "v": {"$type": "text"},
         "n": {"$type": "int", "$searchable": True}}
    ],
}

VOCAB_SIZE = 50_000
BODY_WORDS = (80, 200)
PAYLOAD_WORDS = (120, 300)
BLOB_LEN = (24, 48)

_rng = threading.local()
VOCAB = []
_id_lock = threading.Lock()
_id_next = 0


def rng():
    r = getattr(_rng, "r", None)
    if r is None:
        r = random.Random()
        _rng.r = r
    return r


def _token(r, lo=4, hi=12):
    return "".join(r.choices(string.ascii_lowercase, k=r.randint(lo, hi)))


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
        "tags": [r.choice(VOCAB) for _ in range(r.randint(1, 5))],
        "attrs": [r.choice(VOCAB) for _ in range(r.randint(2, 8))],
        "score": round(r.uniform(0.0, 1000.0), 3),
        "count": r.randint(0, 1_000_000),
        "is_active": r.random() < 0.5,
        "meta": [{"k": r.choice(VOCAB), "v": r.choice(VOCAB), "n": r.randint(0, 1000)}
                 for _ in range(r.randint(1, 4))],
    }


def next_id(wid):
    global _id_next
    with _id_lock:
        n = _id_next
        _id_next += 1
    return f"h{wid}_{n:014d}"


def make_batch(wid, batch):
    return [make_doc(next_id(wid)) for _ in range(batch)]


async def setup(session, db_url, shards):
    headers = {"Authorization": "Bearer " + TOKEN} if TOKEN else {}
    # delete (404 ok)
    for _ in range(10):
        async with session.delete(f"{db_url}/delete-database", headers=headers) as r:
            if r.status in (200, 404):
                break
            if r.status == 409:
                await asyncio.sleep(0.3)
                continue
            raise RuntimeError(f"delete-database -> {r.status}")
    async with session.post(f"{db_url}/create-database", json={"shard_count": shards}, headers=headers) as r:
        if r.status not in (200, 201):
            raise RuntimeError(f"create-database -> {r.status}")
    async with session.post(f"{db_url}/set-policy", json=POLICY, headers=headers) as r:
        if r.status != 200:
            raise RuntimeError(f"set-policy -> {r.status}: {await r.text()}")


async def worker(wid, session, db_url, batch, inflight, stats):
    insert_url = f"{db_url}/insert"
    headers = {"Authorization": "Bearer " + TOKEN} if TOKEN else {}
    loop = asyncio.get_event_loop()
    sem = asyncio.Semaphore(inflight)
    running = set()

    async def one():
        async with sem:
            docs = await loop.run_in_executor(None, make_batch, wid, batch)
            try:
                async with session.post(insert_url, json=docs, headers=headers) as r:
                    body = await r.json()
                    n = int((body or {}).get("data", {}).get("inserted", 0))
                    stats["inserted"] += n
            except Exception as e:
                stats["errors"] += 1

    while not STOP.is_set():
        while len(running) < inflight and not STOP.is_set():
            t = asyncio.ensure_future(one())
            running.add(t)
            t.add_done_callback(running.discard)
        await asyncio.sleep(0.05)

    if running:
        await asyncio.gather(*running, return_exceptions=True)


async def reporter(stats):
    t0 = time.time()
    last = stats["inserted"]
    while not STOP.is_set():
        await asyncio.sleep(2)
        now = time.time()
        cur = stats["inserted"]
        print(f"  total {cur}  (+{(cur - last) / (now - t0):,.0f}/s)")
        last, t0 = cur, now


async def amain(args):
    global BASE, DB, TOKEN
    BASE = args.base_url.rstrip("/")
    DB = args.db
    TOKEN = args.token
    VOCAB[:] = [_token(random.Random()) for _ in range(VOCAB_SIZE)]

    db_url = f"{BASE}/api/databases/{DB}"
    stats = {"inserted": 0, "errors": 0}

    conn = aiohttp.TCPConnector(limit=0)  # unlimited connections
    async with aiohttp.ClientSession(connector=conn) as session:
        print(f"== setup db '{DB}' ({args.shards} shards)")
        await setup(session, db_url, args.shards)

        print(f"== {args.workers} workers x {args.batch} docs x {
              args.inflight} in-flight. Ctrl+C to stop.")
        tasks = [asyncio.create_task(worker(i, session, db_url, args.batch, args.inflight, stats))
                 for i in range(args.workers)]
        rep = asyncio.create_task(reporter(stats))
        try:
            await asyncio.gather(*tasks)
        except KeyboardInterrupt:
            STOP.set()
            await asyncio.gather(*tasks, return_exceptions=True)
        rep.cancel()

    print(f"\n== done: {stats['inserted']} inserted, {stats['errors']} errors")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base-url", default="http://localhost:6006")
    ap.add_argument("--db", default="stress")
    ap.add_argument("--workers", type=int, default=5)
    ap.add_argument("--batch", type=int, default=100)
    ap.add_argument("--inflight", type=int, default=50,
                    help="concurrent requests per worker")
    ap.add_argument("--shards", type=int, default=4)
    ap.add_argument("--token", default=None)
    args = ap.parse_args()
    try:
        asyncio.run(amain(args))
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
