#!/usr/bin/env python3
"""Write-path test suite for Corelamo: insert, upsert, replace, partial replace and delete.

Every case runs on a fresh multi-shard database, so each request is split across shards and
the merged response (counts, HTTP status, and the index/id/code of every failure) is checked
end to end. Documents are read back through /retrieve after every write.

    python3 write_paths_test.py                       # all cases, 4 shards
    python3 write_paths_test.py --shards 1            # same cases on a single shard
    python3 write_paths_test.py --only partial delete # cases whose name starts with these
    python3 write_paths_test.py --keep                # do not delete the test databases
    python3 write_paths_test.py --quiet               # hide the HTTP requests and responses
    python3 write_paths_test.py --body-limit 0        # print full bodies, no shortening

Standard library only. The server must be running (default http://localhost:6006).
"""

from __future__ import annotations

import argparse
import json
import sys
import time
import traceback
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor

BASE_URL = "http://localhost:6006"
TOKEN = ""
SHARDS = 4
KEEP = False
SHOW_HTTP = True
BODY_LIMIT = 2000  # characters of each request/response body to print, 0 = everything


def shorten(text: str) -> str:
    if BODY_LIMIT and len(text) > BODY_LIMIT:
        return text[:BODY_LIMIT] + f" ... ({len(text) - BODY_LIMIT} more characters)"
    return text


def show_exchange(method: str, path: str, body, status: int, parsed: dict, ms: float) -> None:
    if not SHOW_HTTP:
        return
    print(f"      --> {method} {path}")
    if body is not None:
        print("          " + shorten(json.dumps(body, ensure_ascii=False)))
    print(f"      <-- {status} ({ms:.1f} ms)")
    pretty = json.dumps(parsed, indent=2, ensure_ascii=False)
    print("          " + shorten(pretty).replace("\n", "\n          "))


class CaseFailed(Exception):
    pass


# ---------------------------------------------------------------------------------------------
# HTTP
# ---------------------------------------------------------------------------------------------


def request(method: str, path: str, body=None) -> tuple[int, dict]:
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(BASE_URL + path, data=data, method=method)
    req.add_header("Content-Type", "application/json")
    if TOKEN:
        req.add_header("X-Corelamo-Key", TOKEN)
    start = time.perf_counter()
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            raw = resp.read()
            status = resp.status
    except urllib.error.HTTPError as err:
        raw = err.read()
        status = err.code
    try:
        parsed = json.loads(raw) if raw else {}
    except json.JSONDecodeError:
        parsed = {"raw": raw.decode(errors="replace")}
    show_exchange(method, path, body, status, parsed, (time.perf_counter() - start) * 1000)
    return status, parsed


def login(username: str, password: str) -> str:
    status, body = request("POST", "/api/login", {"username": username, "password": password})
    if status != 200:
        sys.exit(f"login failed ({status}): {body}")
    return body["data"]["token"]


# ---------------------------------------------------------------------------------------------
# Database helpers
# ---------------------------------------------------------------------------------------------


class Db:
    def __init__(self, name: str):
        self.name = name

    def path(self, action: str) -> str:
        return f"/api/databases/{self.name}/{action}"

    def create(self) -> None:
        request("DELETE", self.path("delete-database"))
        status, body = request("POST", self.path("create-database"), {"shard_count": SHARDS})
        expect(status in (200, 201), f"create-database returned {status}: {body}")
        # creation may finish starting shards asynchronously
        for _ in range(50):
            status, body = request("GET", self.path("status"))
            if status == 200:
                return
            time.sleep(0.1)
        raise CaseFailed(f"database {self.name} never became ready: {status} {body}")

    def drop(self) -> None:
        if not KEEP:
            request("DELETE", self.path("delete-database"))

    def write(self, action: str, body, method: str = "POST") -> tuple[int, dict]:
        return request(method, self.path(action), body)

    def insert(self, docs):
        return self.write("insert", docs)

    def upsert(self, docs):
        return self.write("upsert", docs)

    def replace(self, docs):
        return self.write("replace", docs)

    def partial(self, items):
        return self.write("partial-replace", [{"id": i, "patch": p} for i, p in items])

    def delete(self, ids):
        return self.write("delete", ids, method="DELETE")

    def get_all(self, ids: list[str]) -> dict[str, dict]:
        status, body = request("POST", self.path("retrieve"), ids)
        expect(status == 200, f"retrieve returned {status}: {body}")
        return {d["id"]: d["data"] for d in body["data"]["documents"]}

    def get(self, doc_id: str) -> dict | None:
        return self.get_all([doc_id]).get(doc_id)

    def restart(self) -> None:
        status, body = request("POST", self.path("restart-database"))
        expect(status == 200, f"restart-database returned {status}: {body}")


def doc(doc_id: str, title: str, year: int = 2000) -> dict:
    return {"id": doc_id, "title": title, "body": f"body of {doc_id}", "year": year}


def ids(prefix: str, count: int) -> list[str]:
    # enough distinct ids that a batch lands on every shard with overwhelming probability
    return [f"{prefix}-{n}" for n in range(count)]


# ---------------------------------------------------------------------------------------------
# Assertions
# ---------------------------------------------------------------------------------------------


def expect(condition: bool, message: str) -> None:
    if not condition:
        raise CaseFailed(message)


def expect_status(status: int, wanted: int, body: dict, what: str) -> None:
    expect(status == wanted, f"{what}: expected HTTP {wanted}, got {status}: {body}")


def data(body: dict) -> dict:
    return body.get("data") or {}


def failures(body: dict) -> list[dict]:
    return data(body).get("results") or []


def expect_count(body: dict, label: str, wanted: int) -> None:
    got = data(body).get(label)
    expect(got == wanted, f"'{label}' expected {wanted}, got {got}: {body}")


def expect_failures(body: dict, wanted: list[tuple[int, str, str]]) -> None:
    """wanted: (request index, id, code) for every failure, in any order."""
    got = sorted((f.get("index"), f.get("id"), f.get("code")) for f in failures(body))
    want = sorted(wanted)
    expect(got == want, f"failures differ\n  expected {want}\n  got      {got}")


def expect_title(db: Db, doc_id: str, title: str | None) -> None:
    stored = db.get(doc_id)
    got = None if stored is None else stored.get("title")
    if title is None:
        expect(stored is None, f"{doc_id} should not exist, found {stored}")
    else:
        expect(got == title, f"{doc_id}: title expected {title!r}, got {got!r}")


# ---------------------------------------------------------------------------------------------
# Cases
# ---------------------------------------------------------------------------------------------


def insert_stores_every_document(db: Db) -> None:
    keys = ids("ins", 40)
    status, body = db.insert([doc(k, "first") for k in keys])
    expect_status(status, 200, body, "insert")
    expect_count(body, "inserted", 40)
    stored = db.get_all(keys)
    missing = [k for k in keys if stored.get(k, {}).get("title") != "first"]
    expect(not missing, f"documents not stored: {missing}")


def insert_duplicates_report_request_position(db: Db) -> None:
    keys = ids("dup", 12)
    db.insert([doc(k, "first") for k in keys[:6]])
    # interleave new and existing ids so positions differ from any per-shard position
    order = [keys[6], keys[0], keys[7], keys[1], keys[8], keys[2],
             keys[9], keys[3], keys[10], keys[4], keys[11], keys[5]]
    status, body = db.insert([doc(k, "second") for k in order])
    expect_status(status, 207, body, "insert with duplicates")
    expect_count(body, "inserted", 6)
    expect_failures(body, [(i, order[i], "duplicate_primary_id") for i in range(1, 12, 2)])
    for k in keys[:6]:
        expect_title(db, k, "first")
    for k in keys[6:]:
        expect_title(db, k, "second")


def insert_duplicate_inside_one_request(db: Db) -> None:
    status, body = db.insert([doc("same", "a"), doc("other", "b"), doc("same", "c")])
    expect(status in (200, 207), f"unexpected status {status}: {body}")
    expect_count(body, "inserted", 2)
    expect_failures(body, [(2, "same", "duplicate_primary_id")])
    expect_title(db, "same", "a")


def insert_invalid_documents_report_position(db: Db) -> None:
    batch = [doc("ok-0", "x"), {"id": "bad-year", "title": "t", "year": "abc"},
             doc("ok-1", "y"), {"title": "no id at all", "year": 1}]
    status, body = db.insert(batch)
    expect_status(status, 207, body, "insert with invalid documents")
    got = {f["index"]: f["code"] for f in failures(body)}
    expect(got.get(1) == "invalid_field", f"index 1 should be invalid_field, got {got}")
    # the default policy has an auto id, so a missing id may be accepted; it must not shift
    # the other positions either way
    expect(0 not in got and 2 not in got, f"valid documents reported as failed: {got}")
    expect_title(db, "bad-year", None)
    expect_title(db, "ok-1", "y")


def insert_all_failed_is_conflict(db: Db) -> None:
    db.insert([doc("c-0", "x"), doc("c-1", "y")])
    status, body = db.insert([doc("c-0", "z"), doc("c-1", "z")])
    expect_status(status, 409, body, "insert where every document exists")
    expect_count(body, "inserted", 0)


def upsert_creates_and_overwrites(db: Db) -> None:
    keys = ids("ups", 20)
    db.insert([doc(k, "old") for k in keys[:10]])
    status, body = db.upsert([doc(k, "new") for k in keys])
    expect_status(status, 200, body, "upsert")
    expect_count(body, "upserted", 20)
    stored = db.get_all(keys)
    wrong = [k for k in keys if stored.get(k, {}).get("title") != "new"]
    expect(not wrong, f"not upserted: {wrong}")


def upsert_drops_fields_missing_from_new_version(db: Db) -> None:
    db.insert([{"id": "u", "title": "t", "body": "will go", "year": 1999}])
    db.upsert([{"id": "u", "title": "t2"}])
    stored = db.get("u")
    expect(stored == {"id": "u", "title": "t2"}, f"upsert must replace whole document: {stored}")


def replace_existing_and_report_missing(db: Db) -> None:
    keys = ids("rep", 12)
    db.insert([doc(k, "old") for k in keys[:6]])
    order = [keys[6], keys[0], keys[7], keys[1], keys[2], keys[8],
             keys[9], keys[3], keys[4], keys[10], keys[5], keys[11]]
    status, body = db.replace([doc(k, "replaced") for k in order])
    expect_status(status, 207, body, "replace with missing ids")
    expect_count(body, "replaced", 6)
    missing_positions = [i for i, k in enumerate(order) if k in keys[6:]]
    expect_failures(body, [(i, order[i], "not_found") for i in missing_positions])
    for k in keys[:6]:
        expect_title(db, k, "replaced")
    for k in keys[6:]:
        expect_title(db, k, None)


def replace_all_missing_is_not_found(db: Db) -> None:
    status, body = db.replace([doc("ghost-0", "x"), doc("ghost-1", "y")])
    expect_status(status, 404, body, "replace where nothing exists")
    expect_count(body, "replaced", 0)


def partial_changes_only_patched_fields(db: Db) -> None:
    db.insert([doc("p", "keep me")])
    status, body = db.partial([("p", {"body": "patched"})])
    expect_status(status, 200, body, "partial replace")
    expect_count(body, "partially replaced", 1)
    stored = db.get("p")
    expect(stored == {"id": "p", "title": "keep me", "body": "patched", "year": 2000},
           f"unexpected document after patch: {stored}")


def partial_null_removes_field(db: Db) -> None:
    db.insert([doc("p", "t")])
    db.partial([("p", {"body": None})])
    stored = db.get("p")
    expect("body" not in stored, f"body should be removed: {stored}")
    expect(stored.get("title") == "t", f"title changed: {stored}")


def partial_nested_merge(db: Db) -> None:
    db.insert([{"id": "n", "title": "t", "meta": {"a": 1, "b": {"c": 2, "d": 3}}}])
    db.partial([("n", {"meta": {"b": {"c": 20}, "e": 5}})])
    stored = db.get("n")
    expect(stored.get("meta") == {"a": 1, "b": {"c": 20, "d": 3}, "e": 5},
           f"nested merge wrong: {stored}")


def partial_repeated_id_applies_in_order(db: Db) -> None:
    db.insert([doc("r", "start")])
    status, body = db.partial([("r", {"title": "first", "body": "one"}),
                               ("r", {"title": "second"})])
    expect(status == 200, f"partial replace returned {status}: {body}")
    stored = db.get("r")
    expect(stored.get("title") == "second" and stored.get("body") == "one",
           f"patches not applied in request order: {stored}")


def partial_failures_report_position(db: Db) -> None:
    keys = ids("pf", 8)
    db.insert([doc(k, "old") for k in keys[:4]])
    items = [
        (keys[4], {"title": "x"}),            # 0 missing
        (keys[0], {"year": "not a number"}),  # 1 invalid for the policy
        (keys[1], {"title": "ok"}),           # 2 fine
        (keys[5], {"title": "y"}),            # 3 missing
        (keys[2], {"title": "ok"}),           # 4 fine
        (keys[6], {"title": "z"}),            # 5 missing
        (keys[3], {"title": "ok"}),           # 6 fine
    ]
    status, body = db.partial(items)
    expect_status(status, 207, body, "partial replace with failures")
    expect_count(body, "partially replaced", 3)
    expect_failures(body, [(0, keys[4], "not_found"), (1, keys[0], "invalid_field"),
                           (3, keys[5], "not_found"), (5, keys[6], "not_found")])
    expect(db.get(keys[0]).get("year") == 2000, "invalid patch changed the stored document")
    for k in keys[1:4]:
        expect_title(db, k, "ok")
    for k in keys[4:7]:
        expect_title(db, k, None)


def partial_all_missing_is_not_found(db: Db) -> None:
    status, body = db.partial([("ghost", {"title": "x"})])
    expect_status(status, 404, body, "partial replace where nothing exists")


def partial_empty_is_bad_request(db: Db) -> None:
    status, body = db.write("partial-replace", [])
    expect_status(status, 400, body, "partial replace with no items")


def partial_concurrent_writers_lose_nothing(db: Db) -> None:
    writers = 32
    db.insert([doc("shared", "start")])

    def one(n: int):
        return db.partial([("shared", {f"w{n}": n})])

    with ThreadPoolExecutor(max_workers=writers) as pool:
        results = list(pool.map(one, range(writers)))
    bad = [(s, b) for s, b in results if s != 200]
    expect(not bad, f"some concurrent patches failed: {bad[:3]}")
    stored = db.get("shared")
    lost = [n for n in range(writers) if stored.get(f"w{n}") != n]
    expect(not lost, f"patches lost by concurrent writers: {lost}")
    expect(stored.get("title") == "start", f"title changed: {stored}")


def delete_existing_and_report_missing(db: Db) -> None:
    keys = ids("del", 12)
    db.insert([doc(k, "x") for k in keys[:6]])
    order = [keys[0], keys[6], keys[1], keys[7], keys[8], keys[2],
             keys[3], keys[9], keys[4], keys[10], keys[11], keys[5]]
    status, body = db.delete(order)
    expect_status(status, 207, body, "delete with missing ids")
    expect_count(body, "deleted", 6)
    missing_positions = [i for i, k in enumerate(order) if k in keys[6:]]
    expect_failures(body, [(i, order[i], "not_found") for i in missing_positions])
    stored = db.get_all(keys)
    expect(not stored, f"documents still present after delete: {list(stored)}")


def delete_then_insert_again(db: Db) -> None:
    db.insert([doc("again", "one")])
    db.delete(["again"])
    status, body = db.insert([doc("again", "two")])
    expect_status(status, 200, body, "insert after delete")
    expect_title(db, "again", "two")


def delete_all_missing_is_not_found(db: Db) -> None:
    status, body = db.delete(["ghost-0", "ghost-1"])
    expect_status(status, 404, body, "delete where nothing exists")


def every_write_survives_restart(db: Db) -> None:
    keys = ids("rs", 5)
    db.insert([doc(k, "inserted") for k in keys])
    db.upsert([doc(keys[0], "upserted")])
    db.replace([doc(keys[1], "replaced")])
    db.partial([(keys[2], {"title": "patched"})])
    db.delete([keys[3]])
    db.restart()
    expect_title(db, keys[0], "upserted")
    expect_title(db, keys[1], "replaced")
    expect_title(db, keys[2], "patched")
    expect_title(db, keys[3], None)
    expect_title(db, keys[4], "inserted")


def writes_visible_to_search_after_change(db: Db) -> None:
    db.insert([doc("s", "aardvarkzz")])
    db.partial([("s", {"title": "quokkazz"})])

    def hits(word: str) -> list:
        status, body = request("POST", db.path("search"), {"query": word})
        expect(status == 200, f"search returned {status}: {body}")
        found = body.get("data") or []
        if isinstance(found, dict):
            found = found.get("hits") or found.get("documents") or []
        return [h.get("id") for h in found]

    # give the indexer a moment if indexing is asynchronous
    for _ in range(50):
        if "s" in hits("quokkazz"):
            break
        time.sleep(0.1)
    expect("s" in hits("quokkazz"), "patched title is not searchable")
    expect("s" not in hits("aardvarkzz"), "old title is still searchable after patch")


CASES = [
    insert_stores_every_document,
    insert_duplicates_report_request_position,
    insert_duplicate_inside_one_request,
    insert_invalid_documents_report_position,
    insert_all_failed_is_conflict,
    upsert_creates_and_overwrites,
    upsert_drops_fields_missing_from_new_version,
    replace_existing_and_report_missing,
    replace_all_missing_is_not_found,
    partial_changes_only_patched_fields,
    partial_null_removes_field,
    partial_nested_merge,
    partial_repeated_id_applies_in_order,
    partial_failures_report_position,
    partial_all_missing_is_not_found,
    partial_empty_is_bad_request,
    partial_concurrent_writers_lose_nothing,
    delete_existing_and_report_missing,
    delete_then_insert_again,
    delete_all_missing_is_not_found,
    every_write_survives_restart,
    writes_visible_to_search_after_change,
]


# ---------------------------------------------------------------------------------------------
# Runner
# ---------------------------------------------------------------------------------------------


def main() -> int:
    global BASE_URL, TOKEN, SHARDS, KEEP, SHOW_HTTP, BODY_LIMIT
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--url", default=BASE_URL)
    parser.add_argument("--user", help="log in as this user (auth is off by default)")
    parser.add_argument("--password", default="")
    parser.add_argument("--shards", type=int, default=SHARDS)
    parser.add_argument("--only", nargs="*", default=[])
    parser.add_argument("--keep", action="store_true")
    parser.add_argument("--quiet", action="store_true", help="do not print HTTP requests and responses")
    parser.add_argument("--body-limit", type=int, default=BODY_LIMIT,
                        help="max characters per printed body, 0 = no limit")
    args = parser.parse_args()

    BASE_URL, SHARDS, KEEP = args.url.rstrip("/"), args.shards, args.keep
    SHOW_HTTP, BODY_LIMIT = not args.quiet, args.body_limit
    if args.user:
        TOKEN = login(args.user, args.password)

    selected = [c for c in CASES
                if not args.only or any(c.__name__.startswith(p) for p in args.only)]
    passed, failed = 0, []
    for number, case in enumerate(selected):
        # database names are limited to 30 characters
        db = Db(f"wp{number:02d}_{case.__name__}"[:30])
        start = time.perf_counter()
        if SHOW_HTTP:
            print(f"\n=== {case.__name__}  (database {db.name})")
        try:
            db.create()
            case(db)
            passed += 1
            print(f"PASS  {case.__name__}  ({time.perf_counter() - start:.2f}s)")
        except CaseFailed as err:
            failed.append(case.__name__)
            print(f"FAIL  {case.__name__}\n      {err}")
        except Exception:
            failed.append(case.__name__)
            print(f"ERROR {case.__name__}")
            print("      " + traceback.format_exc().replace("\n", "\n      "))
        finally:
            db.drop()

    print(f"\n{passed} passed, {len(failed)} failed, {SHARDS} shard(s)")
    for name in failed:
        print(f"  - {name}")
    if TOKEN:
        request("POST", "/api/logout")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())