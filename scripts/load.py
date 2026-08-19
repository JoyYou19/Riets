#!/usr/bin/env python3
"""
load.py -- download one dataset, upload it to one database.

    python3 load.py

Downloads FEVER (5.42M Wikipedia passages, ~1 GB zip) and loads it into a
database called "fever". Resumes the download if interrupted.

To use a different dataset, change the CONFIG block below.
"""

import http.client
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import zipfile

# ---------------------------------------------------------------- CONFIG ---

URL      = "https://public.ukp.informatik.tu-darmstadt.de/thakur/BEIR/datasets/fever.zip"
ARCHIVE  = "fever.zip"
DATAFILE = "fever/corpus.jsonl"        # jsonl inside the zip; set ARCHIVE = None if not zipped
DB_NAME  = "fever"

SERVER   = "http://localhost:6006"
USERNAME = "admin"
PASSWORD = "secret"

BATCH_DOCS  = 10000
BATCH_BYTES = 8 * 1024 * 1024
MAX_DOCS    = 0                        # 0 = load everything

# source field -> (output name, index type, stemming, weight min, weight max)
FIELDS = [
    ("title", "title",  "Text", "english", 90, 95),
    ("text",  "text",   "Text", "english",  1, 85),
    ("_id",   "doc_id", "None", "",         0,  0),
]

# ---------------------------------------------------------------------------


def build_policy():
    """xpath is positional: FIELDS order defines it. id goes last."""
    out = [
        '[[fields]]\n'
        'name = "id"\n'
        f'xpath = {len(FIELDS)}\n'
        'index = "IdAutoIncrement"\n'
        'list = true\n'
        '[fields.weight]\nmin = 90\nmax = 95\n'
    ]
    for pos, (_, name, index, stem, lo, hi) in enumerate(FIELDS):
        out.append(
            '[[fields]]\n'
            f'name     = "{name}"\n'
            f'xpath    = {pos}\n'
            f'index    = "{index}"\n'
            'list     = true\n'
            f'stemming = "{stem}"\n'
            f'[fields.weight]\nmin = {lo}\nmax = {hi}\n'
        )
    return "\n".join(out)


def human(n):
    for unit in ("B", "KiB", "MiB", "GiB"):
        if abs(n) < 1024:
            return f"{n:.1f} {unit}"
        n /= 1024
    return f"{n:.1f} TiB"


def download(url, dest):
    """Resumable download."""
    try:
        req = urllib.request.Request(url, method="HEAD")
        with urllib.request.urlopen(req, timeout=60) as r:
            expected = int(r.headers.get("Content-Length") or 0)
    except Exception:
        expected = 0

    have = os.path.getsize(dest) if os.path.exists(dest) else 0
    if expected and have == expected:
        print(f"[info] {dest} already downloaded ({human(have)})")
        return
    if not expected and have:
        print(f"[info] {dest} exists ({human(have)}), keeping it")
        return

    for attempt in range(5):
        headers, mode = {}, "wb"
        if have:
            headers["Range"] = f"bytes={have}-"
            mode = "ab"
            print(f"[info] resuming at {human(have)}")
        try:
            with urllib.request.urlopen(
                    urllib.request.Request(url, headers=headers), timeout=120) as r:
                if have and r.status == 200:
                    have, mode = 0, "wb"          # server ignored Range
                start, last = time.monotonic(), 0.0
                with open(dest, mode) as fh:
                    while True:
                        block = r.read(512 * 1024)
                        if not block:
                            break
                        fh.write(block)
                        have += len(block)
                        now = time.monotonic()
                        if now - last > 1.0:
                            last = now
                            rate = have / max(now - start, 1e-9)
                            pct = f"{100.0 * have / expected:5.1f}%" if expected else "  ?  "
                            sys.stderr.write(f"\r[down] {pct}  {human(have)}  {human(rate)}/s   ")
                            sys.stderr.flush()
            sys.stderr.write("\n")
            print(f"[info] downloaded {human(os.path.getsize(dest))}")
            return
        except (urllib.error.URLError, OSError) as e:
            sys.stderr.write("\n")
            print(f"[warn] attempt {attempt + 1} failed: {e}")
            have = os.path.getsize(dest) if os.path.exists(dest) else 0
            time.sleep(2 ** attempt)

    sys.exit(f"[error] could not download {url}")


def unzip(archive, member):
    if os.path.exists(member):
        print(f"[info] {member} already extracted")
        return
    print(f"[info] extracting {archive}")
    with zipfile.ZipFile(archive) as z:
        z.extractall(".")
    if not os.path.exists(member):
        sys.exit(f"[error] {member} not found inside {archive}")


class Server:
    def __init__(self, base):
        p = urllib.parse.urlsplit(base)
        self.https = p.scheme == "https"
        self.host, self.port = p.hostname, p.port or (443 if self.https else 80)
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


def main():
    if ARCHIVE:
        download(URL, ARCHIVE)
        unzip(ARCHIVE, DATAFILE)
    else:
        download(URL, DATAFILE)

    total_bytes = os.path.getsize(DATAFILE)
    print(f"[info] source {DATAFILE} ({human(total_bytes)})")

    srv = Server(SERVER)
    srv.login(USERNAME, PASSWORD)
    print("[info] logged in")

    for method, path, label in (
        ("DELETE", f"/api/databases/{DB_NAME}/delete-database", "delete"),
        ("POST",   f"/api/databases/{DB_NAME}/create-database", "create"),
        ("POST",   f"/api/databases/{DB_NAME}/start-database",  "start"),
    ):
        status, text = srv.call(method, path)
        print(f"[info] {label}: HTTP {status} {text[:120]}")

    status, text = srv.call("POST", f"/api/databases/{DB_NAME}/set-policy", build_policy())
    if status >= 400:
        sys.exit(f"[error] set-policy failed (HTTP {status}): {text[:300]}")
    print(f"[info] policy set: HTTP {status}")

    insert = f"/api/databases/{DB_NAME}/insert"
    batch, batch_bytes, read_bytes = [], 0, 0
    docs, skipped, start = 0, 0, time.monotonic()

    def flush():
        nonlocal batch, batch_bytes, docs
        if not batch:
            return
        status, text = srv.call("POST", insert,
                                json.dumps(batch, ensure_ascii=False).encode("utf-8"))
        if status >= 400:
            sys.stderr.write("\n")
            sys.exit(f"[error] insert failed (HTTP {status}): {text[:300]}")
        docs += len(batch)
        batch, batch_bytes = [], 0
        elapsed = time.monotonic() - start
        pct = 100.0 * read_bytes / total_bytes
        rate = docs / max(elapsed, 1e-9)
        eta = (total_bytes - read_bytes) / max(read_bytes / max(elapsed, 1e-9), 1e-9)
        sys.stderr.write(f"\r[load] {pct:5.1f}%  {docs:,} docs  "
                         f"{rate:,.0f} docs/s  eta {int(eta // 60)}m{int(eta % 60):02d}s   ")
        sys.stderr.flush()

    with open(DATAFILE, "r", encoding="utf-8", errors="replace") as fh:
        for line in fh:
            read_bytes += len(line.encode("utf-8"))
            line = line.strip()
            if not line:
                continue
            try:
                rec = json.loads(line)
            except ValueError:
                skipped += 1
                continue

            doc = {}
            for src, name, _, _, _, _ in FIELDS:
                doc[name] = rec.get(src)

            batch.append(doc)
            batch_bytes += len(line)
            if len(batch) >= BATCH_DOCS or batch_bytes >= BATCH_BYTES:
                flush()
            if MAX_DOCS and docs + len(batch) >= MAX_DOCS:
                break
    flush()
    sys.stderr.write("\n")

    elapsed = time.monotonic() - start
    print(f"[info] {docs:,} documents in {elapsed:.1f}s, {skipped:,} bad lines skipped")

    print("[info] reindexing")
    status, text = srv.call("POST", f"/api/databases/{DB_NAME}/reindex")
    print(f"[info] reindex: HTTP {status} {text[:200]}")
    print("[info] done")


if __name__ == "__main__":
    main()
