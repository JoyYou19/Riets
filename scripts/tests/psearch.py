#!/usr/bin/env python3
"""
Search accuracy harness for a sharded Corelamo database.

The corpus is built so the correct ranking is known before running anything:
titles vary in length and term frequency in controlled steps, so BM25 has a
single defensible answer for each query. If the order comes back wrong, the
bug is in scoring, merging, or paging, not in the test data.

Every request and every response is printed. The PASS/FAIL line is a summary
on top of the output, not a replacement for it.

    python3 search_check.py --all              # create + load + check
    python3 search_check.py --create           # create the database only
    python3 search_check.py --load             # insert the corpus only
    python3 search_check.py --check            # run the queries only
    python3 search_check.py --check --json     # same, plus raw JSON bodies
    python3 search_check.py --corpus           # print the corpus, send nothing
    python3 search_check.py --query "matrix"   # one ad-hoc query, full output
"""

import argparse
import json
import random
import sys
import time
import urllib.error
import urllib.request

DEFAULT_BASE = "http://localhost:6006"
DEFAULT_DB = "movies"

RULE = "-" * 78

# ---------------------------------------------------------------- corpus

# Same term, title grows one token at a time. BM25 length normalisation means
# the shortest title should score highest for a single-term query.
LENGTH_DOCS = [
    ("len-01", "matrix"),
    ("len-02", "matrix reloaded"),
    ("len-03", "matrix reloaded revolutions"),
    ("len-04", "matrix reloaded revolutions resurrections deluxe"),
    ("len-05", "matrix reloaded revolutions resurrections deluxe collectors anniversary edition"),
]

# Term repeated, everything else held constant. Higher term frequency wins.
FREQ_DOCS = [
    ("tf-01", "phoenix rising from ashes"),
    ("tf-02", "phoenix phoenix rising from ashes"),
    ("tf-03", "phoenix phoenix phoenix rising from ashes"),
]

# One term that appears nowhere else: precision check.
UNIQUE_DOCS = [
    ("uniq-01", "zoetrope"),
]

# Two-term query: docs with both terms must outrank docs with one.
MULTI_DOCS = [
    ("multi-01", "blade runner"),
    ("multi-02", "blade of the immortal"),
    ("multi-03", "cannonball runner"),
    ("multi-04", "blade runner sequel"),
]

# Filler so IDF has a real collection to work against. None of these words
# appear in any of the groups above.
NOISE_WORDS = [
    "night", "city", "silent", "echo", "harbor", "winter", "letter", "garden",
    "iron", "paper", "amber", "vessel", "orbit", "field", "signal", "quiet",
    "hollow", "north", "glass", "ember", "tunnel", "marble", "coast", "lantern",
]
NOISE_COUNT = 60

TITLES = {}  # id -> title, filled by build_corpus, used when printing hits


def build_corpus():
    docs = []
    for group in (LENGTH_DOCS, FREQ_DOCS, UNIQUE_DOCS, MULTI_DOCS):
        docs.extend({"id": doc_id, "title": title} for doc_id, title in group)

    rng = random.Random(1337)  # fixed seed: same corpus every run
    for i in range(NOISE_COUNT):
        n_words = rng.randint(1, 6)
        title = " ".join(rng.sample(NOISE_WORDS, n_words))
        docs.append({"id": f"noise-{i:03d}", "title": title})

    for doc in docs:
        TITLES[doc["id"]] = doc["title"]
    return docs


def print_corpus():
    docs = build_corpus()
    groups = [
        ("length normalisation", "len-"),
        ("term frequency", "tf-"),
        ("unique term", "uniq-"),
        ("multi-term", "multi-"),
        ("noise", "noise-"),
    ]
    for label, prefix in groups:
        rows = [d for d in docs if d["id"].startswith(prefix)]
        print(f"\n{label} ({len(rows)} docs)")
        print(RULE)
        for d in rows[:8]:
            print(f"  {d['id']:<12} {
                  len(d['title'].split()):>2} tok  {d['title']}")
        if len(rows) > 8:
            print(f"  ... and {len(rows) - 8} more")
    print(f"\ntotal: {len(docs)} documents")


# ---------------------------------------------------------------- http

def post(url, payload, timeout=60):
    body = b"" if payload is None else json.dumps(payload).encode()
    req = urllib.request.Request(
        url, data=body, method="POST",
        headers={"Content-Type": "application/json"},
    )
    started = time.time()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, resp.read().decode(), time.time() - started
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(), time.time() - started
    except urllib.error.URLError as e:
        print(f"\ncannot reach {url}: {e.reason}", file=sys.stderr)
        sys.exit(1)


def pretty(raw):
    try:
        return json.dumps(json.loads(raw), indent=2)
    except json.JSONDecodeError:
        return raw


def show_exchange(label, url, payload, status, raw, elapsed, dump_json):
    print(f"\n{label}")
    print(RULE)
    print(f"POST {url}")
    if payload is not None:
        body = json.dumps(payload)
        print(f"body {body if len(body) <= 200 else body[:200] + ' ...'}")
    print(f"HTTP {status}  {elapsed * 1000:.0f} ms")
    if dump_json:
        print(pretty(raw))
    else:
        flat = " ".join(raw.split())
        print(f"resp {flat if len(flat) <= 300 else flat[:300] + ' ...'}")


def extract_hits(node, out=None):
    """Pull hit objects out of a response without assuming its exact shape."""
    if out is None:
        out = []
    if isinstance(node, dict):
        for key in ("id", "external_id", "_id"):
            if isinstance(node.get(key), str):
                out.append({"id": node[key], "fields": node})
                return out
        for value in node.values():
            extract_hits(value, out)
    elif isinstance(node, list):
        for value in node:
            extract_hits(value, out)
    return out


def render_hits(hits):
    if not hits:
        print("      (no hits)")
        return
    print(f"      {'#':>2}  {'id':<12} {'score':>10}  title")
    for rank, hit in enumerate(hits, 1):
        fields = hit["fields"]
        score = fields.get("score")
        score_txt = f"{score:.6f}" if isinstance(score, (int, float)) else "-"
        title = fields.get("title") or TITLES.get(hit["id"], "")
        print(f"      {rank:>2}  {hit['id']:<12} {score_txt:>10}  {title}")


# ---------------------------------------------------------------- steps

def create_db(base, db, shards, dump_json):
    url = f"{base}/api/databases/{db}/create-database"
    payload = {"shards": shards} if shards else None
    status, raw, elapsed = post(url, payload)
    show_exchange("CREATE DATABASE", url, payload,
                  status, raw, elapsed, dump_json)
    return status < 400


def load_corpus(base, db, batch_size, dump_json):
    url = f"{base}/api/databases/{db}/insert"
    docs = build_corpus()
    sent = 0
    for start in range(0, len(docs), batch_size):
        batch = docs[start:start + batch_size]
        status, raw, elapsed = post(url, batch)
        label = f"INSERT batch {start // batch_size + 1} ({len(batch)} docs)"
        show_exchange(label, url, batch, status, raw, elapsed, dump_json)
        if status >= 400:
            print(f"\nstopped after {sent} documents")
            return False
        sent += len(batch)
    print(f"\ninserted {sent} documents in batches of {batch_size}")
    return True


def search(base, db, query, docs=10, offset=0):
    url = f"{base}/api/databases/{db}/search"
    payload = {"query": query, "docs": docs, "offset": offset}
    status, raw, elapsed = post(url, payload)
    if status >= 400:
        return None, raw, status, elapsed, f"HTTP {status}"
    try:
        parsed = json.loads(raw)
    except json.JSONDecodeError:
        return None, raw, status, elapsed, "response was not JSON"
    return extract_hits(parsed), raw, status, elapsed, None


# ---------------------------------------------------------------- checks

def check_exact_order(got, expected):
    if got == expected:
        return True, ""
    return False, f"expected order {expected}, got {got}"


def check_prefix_set(got, expected_set, n):
    if set(got[:n]) == set(expected_set):
        return True, ""
    return False, f"expected top {n} to be {sorted(expected_set)}, got {got[:n]}"


def check_empty(got, _=None):
    if not got:
        return True, ""
    return False, f"expected no hits, got {got}"


LEN_ORDER = ["len-01", "len-02", "len-03", "len-04", "len-05"]

CHECKS = [
    {
        "name": "unique term returns exactly one doc",
        "query": "zoetrope",
        "params": {"docs": 10, "offset": 0},
        "fn": lambda got: check_exact_order(got, ["uniq-01"]),
        "why": "precision: a term in one document must not drag in neighbours",
    },
    {
        "name": "length normalisation ranks short titles first",
        "query": "matrix",
        "params": {"docs": 10, "offset": 0},
        "fn": lambda got: check_exact_order(got, LEN_ORDER),
        "why": "BM25 divides by document length; failure here means length norm is off",
    },
    {
        "name": "term frequency ranks repeats first",
        "query": "phoenix",
        "params": {"docs": 10, "offset": 0},
        "fn": lambda got: check_exact_order(got, ["tf-03", "tf-02", "tf-01"]),
        "why": "more occurrences of the term must score higher",
    },
    {
        "name": "two-term query prefers docs matching both",
        "query": "blade runner",
        "params": {"docs": 10, "offset": 0},
        "fn": lambda got: check_prefix_set(got, {"multi-01", "multi-04"}, 2),
        "why": "both-term docs must outrank single-term docs",
    },
    {
        "name": "analyzer lowercases the query",
        "query": "MATRIX",
        "params": {"docs": 10, "offset": 0},
        "fn": lambda got: check_exact_order(got, LEN_ORDER),
        "why": "same results as the lowercase query, or the analyzer is not applied",
    },
    {
        "name": "absent term returns nothing",
        "query": "kaleidoscope",
        "params": {"docs": 10, "offset": 0},
        "fn": check_empty,
        "why": "no false positives on an unseen term",
    },
    {
        "name": "paging past offset keeps global order",
        "query": "matrix",
        "params": {"docs": 2, "offset": 1},
        "fn": lambda got: check_exact_order(got, ["len-02", "len-03"]),
        "why": "each shard must return offset+limit hits so the merge can page correctly",
    },
    {
        "name": "limit truncates after merging",
        "query": "matrix",
        "params": {"docs": 3, "offset": 0},
        "fn": lambda got: check_exact_order(got, LEN_ORDER[:3]),
        "why": "truncation happens after the cross-shard merge, not before",
    },
]


def run_one_query(base, db, query, params, dump_json):
    hits, raw, status, elapsed, err = search(base, db, query, **params)
    print(f"\nquery {query!r}  docs={
          params['docs']} offset={params['offset']}")
    print(RULE)
    print(f"HTTP {status}  {elapsed * 1000:.0f} ms  {len(hits)
          if hits is not None else 0} hit(s)")
    if err:
        print(f"      {err}")
        print(pretty(raw))
        return None
    render_hits(hits)
    if dump_json:
        print("      raw:")
        for line in pretty(raw).splitlines():
            print(f"      {line}")
    return hits


def run_checks(base, db, dump_json):
    passed = failed = 0
    verdicts = []
    for check in CHECKS:
        hits = run_one_query(
            base, db, check["query"], check["params"], dump_json)
        if hits is None:
            print("      FAIL  request did not return usable results")
            verdicts.append(("FAIL", check["name"]))
            failed += 1
            continue
        ok, detail = check["fn"]([h["id"] for h in hits])
        if ok:
            print(f"      PASS  {check['name']}")
            verdicts.append(("PASS", check["name"]))
            passed += 1
        else:
            print(f"      FAIL  {check['name']}")
            print(f"            {detail}")
            print(f"            {check['why']}")
            verdicts.append(("FAIL", check["name"]))
            failed += 1

    print(f"\n{RULE}\nsummary\n{RULE}")
    for verdict, name in verdicts:
        print(f"  {verdict}  {name}")
    print(f"\n{passed} passed, {failed} failed")
    return failed == 0


# ---------------------------------------------------------------- main

def main():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--base", default=DEFAULT_BASE)
    ap.add_argument("--db", default=DEFAULT_DB)
    ap.add_argument("--shards", type=int, default=None,
                    help="sent in the create body; ignored if the endpoint hardcodes it")
    ap.add_argument("--batch-size", type=int, default=25)
    ap.add_argument("--create", action="store_true")
    ap.add_argument("--load", action="store_true")
    ap.add_argument("--check", action="store_true")
    ap.add_argument("--all", action="store_true")
    ap.add_argument("--corpus", action="store_true",
                    help="print the corpus and exit without sending anything")
    ap.add_argument("--query", metavar="TEXT", help="run one ad-hoc query")
    ap.add_argument("--docs", type=int, default=10, help="limit for --query")
    ap.add_argument("--offset", type=int, default=0, help="offset for --query")
    ap.add_argument("--json", action="store_true",
                    help="print full JSON bodies instead of one-line summaries")
    args = ap.parse_args()

    if args.corpus:
        print_corpus()
        return 0

    if args.query:
        build_corpus()  # fills TITLES so hits print with their titles
        hits = run_one_query(args.base, args.db, args.query,
                             {"docs": args.docs, "offset": args.offset}, True)
        return 0 if hits is not None else 1

    do_create = args.create or args.all
    do_load = args.load or args.all
    do_check = args.check or args.all
    if not (do_create or do_load or do_check):
        ap.print_help()
        return 2

    build_corpus()  # fills TITLES even when only checking

    if do_create and not create_db(args.base, args.db, args.shards, args.json):
        print("\ncontinuing anyway; the database may already exist")
    if do_load and not load_corpus(args.base, args.db, args.batch_size, args.json):
        return 1
    if do_check:
        return 0 if run_checks(args.base, args.db, args.json) else 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
