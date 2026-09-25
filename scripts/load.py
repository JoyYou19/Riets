import json
import random
import time
import urllib.error
import urllib.request
from collections import deque
from concurrent.futures import ThreadPoolExecutor

INPUT_FILE = "fever/corpus.jsonl"
BASE_URL = "http://localhost:6006"
DB_NAME = "fever"
SHARD_COUNT = 5

BATCH_SIZE = 50_000   # documents per insert request
IN_FLIGHT = 3         # insert requests running at the same time

POLICY = """\
[[fields]]
name = "_id"
kind = "IdAuto"
list = true
[fields.weight]
min = 90
max = 95

[[fields]]
name = "title"
kind = "Text"
list = true
exact = true
stemming = "english"
[fields.weight]
min = 40
max = 80

[[fields]]
name = "text"
kind = "Text"
list = true
stemming = "english"
[fields.weight]
min = 1
max = 100

[[fields]]
name = "random_year"
kind = "Integer"
searchable = true
list = true
[fields.weight]
min = 1
max = 50

[[fields]]
name = "random_float"
kind = "Float"
list = true
[fields.weight]
min = 1
max = 50
"""


def post(path, body):
    """POST to the server and return the parsed JSON reply (or an error dict)."""
    data = body.encode("utf-8") if isinstance(body, str) else body
    request = urllib.request.Request(
        f"{BASE_URL}{path}",
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


def batches_from_jsonl(path, batch_size):
    batch = []
    with open(path, "r", encoding="utf-8") as f:
        for line_no, line in enumerate(f, start=1):
            line = line.strip()
            if not line:
                continue
            try:
                doc = json.loads(line)
            except json.JSONDecodeError as e:
                print(f"[WARN] skipping malformed line {line_no}: {e}")
                continue

            # Test data for the numeric fields in the policy.
            doc["random_year"] = random.randint(1900, 2024)
            doc["random_float"] = round(random.uniform(0.0, 100.0), 4)

            batch.append(doc)
            if len(batch) >= batch_size:
                yield batch
                batch = []

    if batch:
        yield batch


def main():
    start = time.time()

    print(f"[INFO] Creating database '{DB_NAME}'...")
    print(post(f"/api/databases/{DB_NAME}/create-database",
               json.dumps({"shard_count": SHARD_COUNT})).get("title"))

    print(f"[INFO] Starting database '{DB_NAME}'...")
    print(post(f"/api/databases/{DB_NAME}/start-database", "").get("title"))

    print("[INFO] Setting policy...")
    print(post(f"/api/databases/{DB_NAME}/set-policy", POLICY).get("title"))

    sent = 0
    inserted = 0
    insert_path = f"/api/databases/{DB_NAME}/insert"

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
        print(line)

    print(f"[INFO] Uploading {INPUT_FILE} "
          f"({BATCH_SIZE:,} docs per request, {IN_FLIGHT} in flight)...")

    pending = deque()
    with ThreadPoolExecutor(max_workers=IN_FLIGHT) as pool:
        for batch_no, batch in enumerate(batches_from_jsonl(INPUT_FILE, BATCH_SIZE), start=1):
            payload = json.dumps(batch, ensure_ascii=False)
            sent += len(batch)
            pending.append((batch_no, len(batch), pool.submit(post, insert_path, payload)))

            if len(pending) >= IN_FLIGHT:
                done_no, done_len, future = pending.popleft()
                report(done_no, done_len, future.result())

        for done_no, done_len, future in pending:
            report(done_no, done_len, future.result())

    duration = time.time() - start
    print(f"\n[INFO] Done: {inserted:,} of {sent:,} documents inserted in {duration:.2f}s.")


if __name__ == "__main__":
    main()