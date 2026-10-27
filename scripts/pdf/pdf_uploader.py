#!/usr/bin/env python3
"""Upload the slide JSON files to corelamo and wait until their text is embedded.

Every slide becomes one document:
    _id           "<deck>_slide<NN>"            e.g. 01_SE-intro_lv_slide17
    deck          deck name without .pdf        exact text, for filters
    slide_number  integer                       for filters / sort
    title         slide title                   word search
    content       title + all text blocks       word search + "$vector": embedded
    page_image    path of the slide picture     stored only

Usage:
    python3 upload_slides.py ~/Desktop/slaidi_json
    python3 upload_slides.py ~/Desktop/slaidi_json --db slaidi --shards 1 --no-wait

Login comes from CORELAMO_USER / CORELAMO_PASS (default admin / secret), the server from
CORELAMO_URL (default http://localhost:6006).
"""

import argparse
import glob
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request

BASE_URL = os.environ.get("CORELAMO_URL", "http://localhost:6006")
USERNAME = os.environ.get("CORELAMO_USER", "admin")
PASSWORD = os.environ.get("CORELAMO_PASS", "secret")

INSERT_BATCH = 200
POLL_SECONDS = 10

# No "$stemming": the slides are Latvian and corelamo has no Latvian stemmer; stemming them
# with the English one would damage the words. The vector field covers meaning instead.
POLICY = {
    "_id": {"$type": "id", "$weight": {"min": 90, "max": 95}},
    "deck": {"$type": "text", "$exact": True, "$weight": {"min": 1, "max": 20}},
    "slide_number": {"$type": "integer", "$searchable": True, "$weight": {"min": 1, "max": 10}},
    "title": {"$type": "text", "$exact": True, "$weight": {"min": 40, "max": 80}},
    "content": {"$type": "text", "$vector": True, "$weight": {"min": 1, "max": 100}},
}

BULLET_LINE = re.compile(r"^[•\-–▪◦●]\s*$")
# a line that is only a number or a marker: the numbered circles and arrows of slide diagrams
NOISE_LINE = re.compile(r"^(\d{1,2}[.)]?|[↺→←↑↓►▶•\-–▪◦●])$")
SENTENCE_END = (".", "!", "?", ":", ";", "…")


def request(method, path, token=None, body=None, timeout=300):
    """JSON request. Returns (http_status, parsed body or raw text)."""
    data = None if body is None else json.dumps(body, ensure_ascii=False).encode("utf-8")
    req = urllib.request.Request(f"{BASE_URL}{path}", data=data, method=method)
    req.add_header("Accept", "application/json")
    if data is not None:
        req.add_header("Content-Type", "application/json")
    if token:
        req.add_header("X-Corelamo-Key", token)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            status, raw = resp.status, resp.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        status, raw = e.code, e.read().decode("utf-8", "replace")
    except urllib.error.URLError as e:
        sys.exit(f"[FATAL] cannot reach {BASE_URL}: {e.reason}")
    try:
        return status, json.loads(raw)
    except json.JSONDecodeError:
        return status, raw


def login():
    status, body = request("POST", "/api/login", body={"username": USERNAME, "password": PASSWORD})
    try:
        return body["data"]["token"]
    except (KeyError, TypeError):
        # a server started with auth = "false" accepts requests without a key
        print(f"[WARN] login failed (HTTP {status}), continuing without a key: {body}")
        return None


def step(label, path, token, body=None, fatal=True):
    status, out = request("POST", path, token, body)
    good = 200 <= status < 300
    print(f"[{'OK' if good else 'FATAL' if fatal else 'WARN'}] {label} (HTTP {status}): {out}")
    if not good and fatal:
        sys.exit(1)


def clean_block(text):
    """One block as flowing text: the PDF's visual line breaks are joined with spaces (a word
    split with a hyphen is joined back), lone numbers and markers are dropped, and runs of
    whitespace collapse to one space."""
    out = ""
    for line in text.splitlines():
        line = " ".join(line.split())
        if not line or BULLET_LINE.match(line) or NOISE_LINE.match(line):
            continue
        line = line.lstrip("•▪◦●↺→► ").strip()
        if not line:
            continue
        if not out:
            out = line
        elif out.endswith("-") and line[:1].islower():
            out = out[:-1] + line
        elif line[:1].isupper() and not out.endswith(SENTENCE_END + ("—", "–", "(", "/", ",")):
            # a wrapped line continues in lowercase; a capital starts the next item or sentence
            out = f"{out}. {line}"
        else:
            out = f"{out} {line}"
    return out


def join_blocks(parts):
    """Blocks become sentences of one paragraph: a block that does not end like a sentence
    gets a full stop, so headings and list items do not run into the next block."""
    merged = []
    for part in parts:
        # a lowercase tail like "perspective)" is the end of a box the PDF split into two
        # blocks: it belongs to the earliest block still waiting for its ")"
        if part[:1].islower() and part.count(")") > part.count("("):
            owner = next((i for i, p in enumerate(merged) if p.count("(") > p.count(")")), None)
            if owner is not None:
                merged[owner] = f"{merged[owner]} {part}"
                continue
        merged.append(part)
    return " ".join(p if p.endswith(SENTENCE_END) else f"{p}." for p in merged)


def slide_document(path):
    with open(path, encoding="utf-8") as f:
        slide = json.load(f)
    deck = os.path.splitext(slide.get("deck") or "")[0]
    number = int(slide.get("slide_number") or 0)
    title = (slide.get("title") or "").strip()

    parts = [title] if title else []
    for block in slide.get("text_blocks") or []:
        text = clean_block(block.get("text") or "")
        # the title is usually also the first block; keep it once
        if text and text != title:
            parts.append(text)
    content = join_blocks(parts)

    doc = {
        "_id": f"{deck}_slide{number:02d}",
        "deck": deck,
        "slide_number": number,
        "title": title,
        "content": content,
    }
    if slide.get("page_image"):
        doc["page_image"] = slide["page_image"]
    return doc


def load_documents(folder):
    files = sorted(glob.glob(os.path.join(folder, "*.json")))
    if not files:
        sys.exit(f"[FATAL] no .json files in {folder}")
    docs, skipped = [], 0
    for path in files:
        try:
            doc = slide_document(path)
        except (OSError, ValueError, TypeError) as e:
            print(f"[WARN] skipping {os.path.basename(path)}: {e}")
            skipped += 1
            continue
        if not doc["content"]:
            # nothing to search or embed (picture-only slide)
            skipped += 1
            continue
        docs.append(doc)
    print(f"[INFO] {len(docs)} slides with text, {skipped} skipped, from {len(files)} files")
    return docs


def find_embedding(stats):
    """The embedding block of the status response, wherever the handler nests it."""
    if isinstance(stats, dict):
        for key, value in stats.items():
            if key == "embedding" and isinstance(value, dict):
                return value
            found = find_embedding(value)
            if found is not None:
                return found
    elif isinstance(stats, list):
        for value in stats:
            found = find_embedding(value)
            if found is not None:
                return found
    return None


def wait_for_embedding(db, token, expected):
    """Waits until the server reports at least `expected` documents to embed, all done.

    Right after an upload the slides are still in memory: they reach disk segments after ~2 s
    of no writes, and the worker re-counts segments only once a minute. Until then the server
    can report "complete" for 0 documents, so a status alone is not trusted.
    """
    print(f"[INFO] waiting for {expected} slides to be embedded "
          "(Ctrl+C stops waiting, embedding continues on the server)")
    started = time.time()
    last = None
    while True:
        status, stats = request("GET", f"{db}/status", token, timeout=60)
        embedding = find_embedding(stats) if 200 <= status < 300 else None
        if embedding is None:
            print(f"[WARN] no embedding info in status (HTTP {status}); is the field marked $vector?")
            return
        state = str(embedding.get("status", "")).lower()
        progress = embedding.get("progress")
        line = f"status={state} progress={progress}% " + " ".join(
            f"{k}={v}" for k, v in embedding.items() if k not in ("status", "progress")
        )
        if line != last:
            print(f"[{time.time() - started:6.0f}s] {line}")
            last = line
        total = int(embedding.get("vectors_total") or 0)
        done = int(embedding.get("vectors_done") or 0)
        if embedding.get("error"):
            print(f"[WARN] server reports an embedding error: {embedding['error']}")
        if state == "disabled":
            print("[WARN] embedding is disabled for this database")
            return
        if total >= expected and done >= total:
            print(f"[DONE] {done} slides embedded in {time.time() - started:.0f}s")
            return
        time.sleep(POLL_SECONDS)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("folder", help="folder with the *_slideNN.json files")
    parser.add_argument("--db", default="slaidi", help="database name (default: slaidi)")
    parser.add_argument("--shards", type=int, default=1, help="shards for a new database (default: 1)")
    parser.add_argument("--no-wait", action="store_true", help="do not wait for embedding to finish")
    args = parser.parse_args()

    docs = load_documents(os.path.expanduser(args.folder))
    token = login()
    db = f"/api/databases/{args.db}"

    # both may fail harmlessly when the database already exists / already runs
    step(f"create database '{args.db}'", f"{db}/create-database", token, {"shard_count": args.shards}, fatal=False)
    step(f"start database '{args.db}'", f"{db}/start-database", token, fatal=False)
    step("set policy", f"{db}/set-policy", token, POLICY)

    started = time.time()
    failed = 0
    for start in range(0, len(docs), INSERT_BATCH):
        batch = docs[start:start + INSERT_BATCH]
        # upsert: running the script again replaces the slides instead of failing on duplicates
        status, out = request("POST", f"{db}/upsert", token, batch)
        if not 200 <= status < 300:
            failed += len(batch)
            print(f"[ERROR] slides {start}-{start + len(batch) - 1} failed (HTTP {status}): {out}")
            continue
        print(f"[INFO] uploaded {start + len(batch)}/{len(docs)}")
    print(f"[INFO] upload finished in {time.time() - started:.1f}s, {failed} failed")

    if not args.no_wait and failed < len(docs):
        wait_for_embedding(db, token, len(docs) - failed)
    if failed:
        sys.exit(1)


if __name__ == "__main__":
    main()