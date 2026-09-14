#!/usr/bin/env python3
"""
big_movies.py -- download the TMDB movies dataset, upload it to a database.

    python3 big_movies.py

Downloads the TMDB movies CSV (via Kaggle) and loads it into a database
called "tmdb".

Requires: pip3 install kaggle --break-system-packages
Requires: ~/.kaggle/kaggle.json (Kaggle API token, from kaggle.com account settings)
"""

import csv
import http.client
import json
import os
import subprocess
import sys
import time
import urllib.parse
import zipfile

# ---------------------------------------------------------------- CONFIG ---

KAGGLE_DATASET = "asaniczka/tmdb-movies-dataset-2023-930k-movies"
ARCHIVE        = "tmdb-movies-dataset-2023-930k-movies.zip"
DB_NAME        = "moviesbig"

SERVER   = "http://localhost:6006"
USERNAME = "admin"
PASSWORD = "secret"

BATCH_DOCS  = 50000
BATCH_BYTES = 512 * 1024 * 1024
MAX_DOCS    = 0                        # 0 = load everything
SHARD_COUNT = 4

# csv column -> (output name, kind, stemming, searchable, exact, weight min, weight max)
FIELDS = [
    ("title",                 "title",                "Text",    "english", True,  False, 90, 100),
    ("original_title",         "original_title",       "Text",    "english", True,  False, 85,  95),
    ("tagline",                "tagline",              "Text",    "english", True,  False, 70,  80),
    ("overview",                "overview",             "Text",    "english", True,  False, 40,  65),
    ("keywords",               "keywords",             "Text",    "english", True,  False, 30,  45),
    ("genres",                "genres",               "Text",    "english", True,  False, 20,  35),
    ("production_companies",   "production_companies", "Text",    "english", True,  False, 10,  20),
    ("production_countries",   "production_countries", "Text",    "english", True,  False,  5,  15),
    ("spoken_languages",       "spoken_languages",     "Text",    "english", True,  False,  5,  15),
    ("status",                "status",               "Text",    "",        True,  True,   3,   8),
    ("release_date",           "release_date",         "Text",    "",        True,  True,   3,   8),
    ("original_language",      "original_language",    "Text",    "",        True,  True,   3,   8),
    ("imdb_id",                "imdb_id",              "Text",    "",        True,  True,   1,   3),
    ("homepage",               "homepage",             "Text",    "",        True,  True,   1,   3),
    ("poster_path",            "poster_path",          "Text",    "",        True,  True,   1,   3),
    ("backdrop_path",          "backdrop_path",        "Text",    "",        True,  True,   1,   3),
    ("adult",                  "adult",                "Text",    "",        True,  True,   1,   3),
    ("runtime",                "runtime",              "Integer", "",        True,  True,  15,  25),
    ("budget",                 "budget",               "Float",   "",        True,  True,  10,  20),
    ("revenue",                "revenue",              "Float",   "",        True,  True,  15,  25),
    ("popularity",             "popularity",           "Float",   "",        True,  True,  25,  40),
    ("vote_average",           "vote_average",         "Float",   "",        True,  True,  20,  35),
    ("vote_count",             "vote_count",           "Float",   "",        True,  True,  15,  30),
]

# ---------------------------------------------------------------------------


def build_policy():
    out = [
        '[[fields]]\n'
        'name = "id"\n'
        'kind = "Id"\n'
        'searchable = true\n'
        'list = true\n'
        'exact = true\n'
        '[fields.weight]\nmin = 100\nmax = 100\n'
    ]
    for _, name, kind, stem, searchable, exact, lo, hi in FIELDS:
        out.append(
            '[[fields]]\n'
            f'name       = "{name}"\n'
            f'kind       = "{kind}"\n'
            f'searchable = {"true" if searchable else "false"}\n'
            'list       = true\n'
            + (f'stemming   = "{stem}"\n' if stem else '')
            + f'exact      = {"true" if exact else "false"}\n'
            + f'[fields.weight]\nmin = {lo}\nmax = {hi}\n'
        )
    return "\n".join(out)


def human(n):
    for unit in ("B", "KiB", "MiB", "GiB"):
        if abs(n) < 1024:
            return f"{n:.1f} {unit}"
        n /= 1024
    return f"{n:.1f} TiB"


def download_kaggle(dataset, dest):
    """Downloads a Kaggle dataset via the kaggle CLI. Kaggle's CLI itself
    handles resuming/skip-if-exists, so this is a thin wrapper."""
    if os.path.exists(dest):
        print(f"[info] {dest} already downloaded ({human(os.path.getsize(dest))})")
        return
    print(f"[info] downloading {dataset} via kaggle CLI")
    result = subprocess.run(
        ["kaggle", "datasets", "download", "-d", dataset],
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        sys.exit(f"[error] kaggle download failed:\n{result.stderr}")
    if not os.path.exists(dest):
        sys.exit(f"[error] expected {dest} after download, not found. "
                  f"Check the actual filename kaggle produced.")
    print(f"[info] downloaded {human(os.path.getsize(dest))}")


def unzip(archive, target_dir="."):
    print(f"[info] extracting {archive}")
    with zipfile.ZipFile(archive) as z:
        names = z.namelist()
        z.extractall(target_dir)
    csv_files = [n for n in names if n.lower().endswith(".csv")]
    if not csv_files:
        sys.exit(f"[error] no CSV found inside {archive}")
    return os.path.join(target_dir, csv_files[0])


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

    # def login(self, user, pw):
    #     status, text = self.call("POST", "/api/login",
    #                              json.dumps({"username": user, "password": pw}))
    #     if status != 200:
    #         sys.exit(f"[error] login failed (HTTP {status}): {text[:200]}")
    #     try:
    #         self.token = json.loads(text)["data"]["token"]
    #     except Exception:
    #         sys.exit(f"[error] no token in login response: {text[:200]}")


def main():
    download_kaggle(KAGGLE_DATASET, ARCHIVE)
    datafile = unzip(ARCHIVE)

    total_bytes = os.path.getsize(datafile)
    print(f"[info] source {datafile} ({human(total_bytes)})")

    srv = Server(SERVER)
   
    

    for method, path, body, label in (
        ("DELETE", f"/api/databases/{DB_NAME}/delete-database", b"", "delete"),
        ("POST",   f"/api/databases/{DB_NAME}/create-database",
                   json.dumps({"shard_count": SHARD_COUNT}).encode("utf-8"), "create"),
        ("POST",   f"/api/databases/{DB_NAME}/start-database",  b"", "start"),
    ):
        status, text = srv.call(method, path, body)
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
            print(text[:3000])
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

    with open(datafile, "r", encoding="utf-8", errors="replace", newline="") as fh:
        reader = csv.DictReader(fh)
        for row in reader:
            line_bytes = sum(len(str(v or "")) for v in row.values())
            read_bytes += line_bytes

            title = row.get("title") or ""
            release_date = row.get("release_date") or ""
            year = release_date[:4] if release_date else ""
            tmdb_id = row.get("id") or ""

            if tmdb_id:
                doc_id = f"{title} ({year}) [{tmdb_id}]" if year else f"{title} [{tmdb_id}]"
            elif year:
                doc_id = f"{title} ({year})"
            else:
                doc_id = title

            doc = {"id": doc_id}
            for src, name, _, _, _, _, _, _ in FIELDS:
                doc[name] = row.get(src) or ""

            batch.append(doc)
            batch_bytes += line_bytes
            if len(batch) >= BATCH_DOCS or batch_bytes >= BATCH_BYTES:
                flush()
            if MAX_DOCS and docs + len(batch) >= MAX_DOCS:
                break
    flush()
    sys.stderr.write("\n")

    elapsed = time.monotonic() - start
    print(f"[info] {docs:,} documents in {elapsed:.1f}s, {skipped:,} bad lines skipped")

    # print("[info] reindexing")
    # status, text = srv.call("POST", f"/api/databases/{DB_NAME}/reindex")
    # print(f"[info] reindex: HTTP {status} {text[:200]}")
    # print("[info] done")


if __name__ == "__main__":
    main()