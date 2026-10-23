#!/usr/bin/env python3
"""Download a BEIR dataset and load it into Corelamo, the way load.py loads FEVER.

    python3 load_beir.py scifact            # 5,183 abstracts, 300 test queries: done in seconds
    python3 load_beir.py nq                 # 2.68M Wikipedia passages, 3,452 test queries
    python3 load_beir.py fiqa --shards 2    # any BEIR dataset name works

    # Climate-FEVER searches the FEVER corpus (plus 25 pages): add only those to the fever
    # database instead of loading 5.4M documents again
    python3 load_beir.py climate-fever --db fever --existing --skip-ids-from fever/corpus.jsonl
    python3 beir_eval.py --database fever --dataset-dir beir/climate-fever

The dataset is downloaded once from the BEIR mirror at TU Darmstadt and unpacked into
scripts/beir/<name>/ (corpus.jsonl, queries.jsonl, qrels/<split>.tsv); later runs reuse it.
The database gets the same policy as FEVER (title 40-80, text 20-30), with each document's
BEIR `_id` as its id, so beir_eval.py can match hits by id:

    python3 beir_eval.py --database scifact --dataset-dir beir/scifact

Nothing is deleted: if the database exists, create-database reports it and the inserts add to
it (documents whose id is already there are reported as failed). With --existing the database
is not created and its policy is left as it is. Only the standard library is used.
"""

import argparse
import json
import re
import shutil
import sys
import time
import urllib.error
import urllib.request
import zipfile
from collections import deque
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
BEIR_DIR = SCRIPT_DIR / "beir"
DATASET_URL = "https://public.ukp.informatik.tu-darmstadt.de/thakur/BEIR/datasets/{name}.zip"

# per dataset: shards, documents per insert request
DEFAULTS = {
    "scifact": (1, 5_000),
    "nfcorpus": (1, 5_000),
    "nq": (4, 50_000),
}
FALLBACK = (4, 50_000)
IN_FLIGHT = 12  # insert requests running at the same time

POLICY = """\
{
  "_id": {
    "$type": "id_auto",
    "$list": true,
    "$weight": { "min": 90, "max": 95 }
  },
  "title": {
    "$type": "text",
    "$list": true,
    "$exact": true,
    "$stemming": "english",
    "$weight": { "min": 40, "max": 80 }
  },
  "text": {
    "$type": "text",
    "$list": true,
    "$stemming": "english",
    "$weight": { "min": 20, "max": 30 }
  }
}
"""


def download(name: str) -> Path:
    """The unpacked dataset folder, downloading and unpacking it the first time."""
    folder = BEIR_DIR / name
    if (folder / "corpus.jsonl").exists() and (folder / "queries.jsonl").exists():
        print(f"[INFO] Using {folder}")
        return folder
    BEIR_DIR.mkdir(parents=True, exist_ok=True)
    archive = BEIR_DIR / f"{name}.zip"
    if not archive.exists():
        url = DATASET_URL.format(name=name)
        print(f"[INFO] Downloading {url}")
        partial = archive.with_suffix(".part")
        request = urllib.request.Request(url, headers={"User-Agent": "corelamo-beir-loader/1.0"})
        try:
            with urllib.request.urlopen(request) as response, open(partial, "wb") as out:
                total = int(response.headers.get("Content-Length") or 0)
                done = 0
                while chunk := response.read(1 << 20):
                    out.write(chunk)
                    done += len(chunk)
                    if total:
                        print(f"\r  {done >> 20} / {total >> 20} MB", end="", flush=True)
        except urllib.error.HTTPError as err:
            partial.unlink(missing_ok=True)
            sys.exit(f"[ERROR] {url}: {err.code} {err.reason} (is '{name}' a BEIR dataset name?)")
        print()
        partial.rename(archive)
    print(f"[INFO] Unpacking {archive.name}")
    with zipfile.ZipFile(archive) as zipped:
        zipped.extractall(BEIR_DIR)
    if not (folder / "corpus.jsonl").exists():
        sys.exit(f"[ERROR] {archive} did not unpack into {folder}")
    return folder


def post(base_url, path, body):
    """POST to the server and return the parsed JSON reply (or an error dict)."""
    data = body.encode("utf-8") if isinstance(body, str) else body
    request = urllib.request.Request(
        f"{base_url}{path}",
        data=data,
        method="POST",
        headers={"Accept": "application/json"},
    )
    try:
        with urllib.request.urlopen(request) as response:
            text = response.read().decode("utf-8")
    except urllib.error.HTTPError as e:
        text = e.read().decode("utf-8", errors="replace")
    except urllib.error.URLError as e:
        return {"error": str(e.reason)}

    try:
        return json.loads(text)
    except json.JSONDecodeError:
        return {"error": text}


ID_FIELD = re.compile(rb'"_id"\s*:\s*"((?:[^"\\]|\\.)*)"')


def doc_id(line):
    """The `_id` of a corpus line, read without parsing the whole document."""
    match = ID_FIELD.search(line)
    if match is None:
        return None
    raw = match.group(1)
    return json.loads(b'"' + raw + b'"') if b"\\" in raw else raw.decode("utf-8")


def ids_in(path):
    """Every document id in a corpus file."""
    print(f"[INFO] Reading the ids in {path}...")
    ids = set()
    with open(path, "rb") as f:
        for line in f:
            found = doc_id(line)
            if found is not None:
                ids.add(found)
    print(f"[INFO] {len(ids):,} ids to skip")
    return ids


def batches_from_jsonl(path, batch_size, skip_ids=None):
    """Yield (body_bytes, doc_count) without parsing any JSON: each line is already a document
    ({"_id", "title", "text", "metadata"}). Documents whose id is in `skip_ids` are left out."""
    batch = []
    with open(path, "rb") as f:
        for line_no, line in enumerate(f, start=1):
            line = line.strip()
            if not line:
                continue
            if not (line.startswith(b"{") and line.endswith(b"}")):
                print(f"[WARN] skipping line {line_no}: not a JSON object")
                continue
            if skip_ids and doc_id(line) in skip_ids:
                continue
            batch.append(line)
            if len(batch) >= batch_size:
                yield b"[" + b",".join(batch) + b"]", len(batch)
                batch = []
    if batch:
        yield b"[" + b",".join(batch) + b"]", len(batch)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("dataset", help="BEIR dataset name: scifact, nq, fiqa, nfcorpus, ...")
    parser.add_argument("--db", help="database name (default: the dataset name)")
    parser.add_argument("--url", default="http://localhost:6006")
    parser.add_argument("--shards", type=int, help="shard count (default: per dataset)")
    parser.add_argument("--batch-size", type=int, help="documents per insert request")
    parser.add_argument("--in-flight", type=int, default=IN_FLIGHT, help="insert requests at once")
    parser.add_argument("--existing", action="store_true",
                        help="add to an existing database: do not create it or change its policy")
    parser.add_argument("--skip-ids-from", metavar="CORPUS",
                        help="leave out documents whose id is in this corpus.jsonl (already loaded)")
    args = parser.parse_args()

    name = args.dataset.strip().lower()
    db = args.db or name
    shards, batch_size = DEFAULTS.get(name, FALLBACK)
    shards = args.shards or shards
    batch_size = args.batch_size or batch_size
    if shutil.disk_usage(SCRIPT_DIR).free < 2 << 30:
        print("[WARN] less than 2 GB free next to this script; nq needs about 1.5 GB unpacked")

    folder = download(name)
    corpus = folder / "corpus.jsonl"
    skip_ids = None
    if args.skip_ids_from:
        skip_path = Path(args.skip_ids_from)
        if not skip_path.is_absolute() and not skip_path.exists():
            skip_path = SCRIPT_DIR / skip_path
        if not skip_path.exists():
            sys.exit(f"[ERROR] {args.skip_ids_from} not found")
        skip_ids = ids_in(skip_path)
    start = time.time()

    if args.existing:
        print(f"[INFO] Adding to the existing database '{db}' (policy left as it is)...")
        print(post(args.url, f"/api/databases/{db}/start-database", "").get("title"))
    else:
        print(f"[INFO] Creating database '{db}' ({shards} shard(s))...")
        print(post(args.url, f"/api/databases/{db}/create-database", json.dumps({"shard_count": shards})).get("title"))

        print(f"[INFO] Starting database '{db}'...")
        print(post(args.url, f"/api/databases/{db}/start-database", "").get("title"))

        print("[INFO] Setting policy...")
        print(post(args.url, f"/api/databases/{db}/set-policy", POLICY).get("title"))

    sent = 0
    inserted = 0
    insert_path = f"/api/databases/{db}/insert"

    def report(batch_no, batch_len, reply):
        nonlocal inserted
        data = reply.get("data") or {}
        if "error" in reply or "inserted" not in data:
            print(f"[ERROR] batch {batch_no}: {reply}")
            return
        inserted += data["inserted"]
        failed = batch_len - data["inserted"]
        elapsed = time.time() - start
        rate = inserted / elapsed if elapsed > 0 else 0
        line = (f"[batch {batch_no}] inserted {inserted:,} / sent {sent:,}"
                f"  ({rate:,.0f} docs/s, {elapsed:.1f}s)")
        if failed:
            line += f"  — {failed:,} failed: {reply.get('title')}"
        print(line, flush=True)

    print(f"[INFO] Uploading {corpus} ({batch_size:,} docs per request, {args.in_flight} in flight)...")

    pending = deque()
    with ThreadPoolExecutor(max_workers=args.in_flight) as pool:
        for batch_no, (payload, count) in enumerate(batches_from_jsonl(corpus, batch_size, skip_ids), start=1):
            sent += count
            pending.append((batch_no, count, pool.submit(post, args.url, insert_path, payload)))
            if len(pending) >= args.in_flight:
                done_no, done_len, future = pending.popleft()
                report(done_no, done_len, future.result())
        for done_no, done_len, future in pending:
            report(done_no, done_len, future.result())

    duration = time.time() - start
    print(f"\n[INFO] Done: {inserted:,} of {sent:,} documents inserted in {duration:.2f}s.")
    if skip_ids is not None:
        print(f"[INFO] {len(skip_ids):,} documents were already loaded and left out.")
    splits = sorted(p.stem for p in (folder / "qrels").glob("*.tsv"))
    print(f"[INFO] Splits: {', '.join(splits)}. Evaluate with:")
    print(f"  python3 beir_eval.py --database {db} --dataset-dir {folder.relative_to(SCRIPT_DIR)}")


if __name__ == "__main__":
    main()