import json
import time
import os
import glob
import subprocess
from collections import deque
from concurrent.futures import ThreadPoolExecutor

INPUT_DIR = "./movie_chunks"
BASE_URL = "http://localhost:6006"
DB_NAME = "movies"
MAX_CHUNKS = 0  # 0 = send all
SHARD_COUNT = 4
IN_FLIGHT = 1   # insert requests running at the same time

POLICY = """\
[[fields]]
name = "id"
kind = "IdAuto"

[[fields]]
name = "title"
kind = "Text"
exact = true
[fields.weight]
min = 90
max = 100

[[fields]]
name = "year"
kind = "Integer"
searchable = true
[fields.weight]
min = 1
max = 40

[[fields]]
name = "cast"
kind = "Struct"

[[fields.subfields]]
name = "name"
kind = "Text"
exact = true
[fields.subfields.weight]
min = 1
max = 60

[[fields.subfields]]
name = "surname"
kind = "Text"
exact = true
[fields.subfields.weight]
min = 1
max = 60

[[fields]]
name = "genres"
kind = "Text"
repeated = true
exact = true
[fields.weight]
min = 1
max = 40

[[fields]]
name = "extract"
kind = "Text"
stemming = "english"
[fields.weight]
min = 1
max = 70

[[fields]]
name = "href"
kind = "Text"
exact = true
[fields.weight]
min = 0
max = 0

[[fields]]
name = "thumbnail"
kind = "None"

[[fields]]
name = "thumbnail_width"
kind = "Integer"
[fields.weight]
min = 0
max = 0

[[fields]]
name = "thumbnail_height"
kind = "Integer"
[fields.weight]
min = 0
max = 0
"""


def curl_post(url, body):
    """POST text or bytes with curl. Returns (stdout, exit code)."""
    data = body.encode("utf-8") if isinstance(body, str) else body
    result = subprocess.run(
        ["curl", "-s", "-X", "POST", url,
         "-H", "Accept: application/json",
         "--data-binary", "@-"],
        input=data,
        capture_output=True,
    )
    return result.stdout.decode("utf-8", errors="replace").strip(), result.returncode


def main():
    start_time = time.time()
    print("[INFO] Starting movie uploader...")

    print(f"[INFO] Creating database '{DB_NAME}'...")
    out, _ = curl_post(
        f"{BASE_URL}/api/databases/{DB_NAME}/create-database",
        json.dumps({"shard_count": SHARD_COUNT}))
    print(f"[INFO] {out}")

    print(f"[INFO] Starting database '{DB_NAME}'...")
    out, _ = curl_post(
        f"{BASE_URL}/api/databases/{DB_NAME}/start-database", "")
    print(f"[INFO] {out}")

    print("[INFO] Setting policy...")
    out, _ = curl_post(
        f"{BASE_URL}/api/databases/{DB_NAME}/set-policy", POLICY)
    print(f"[INFO] {out}")

    files = sorted(glob.glob(os.path.join(INPUT_DIR, "movies_*.json")))
    if not files:
        print(f"[ERROR] No chunk files found in {INPUT_DIR}.")
        return

    if MAX_CHUNKS > 0:
        files = files[:MAX_CHUNKS]

    insert_url = f"{BASE_URL}/api/databases/{DB_NAME}/insert"
    inserted = 0
    done = 0

    def report(file, out, code):
        nonlocal inserted, done
        done += 1
        name = os.path.basename(file)
        if code != 0:
            print(
                f"[ERROR] ({done}/{len(files)}) failed to upload {name} (curl exit {code})")
            print(out)
            return
        try:
            reply = json.loads(out)
        except json.JSONDecodeError:
            print(
                f"[ERROR] ({done}/{len(files)}) {name}: unreadable reply: {out[:300]}")
            return
        got = (reply.get("data") or {}).get("inserted")
        if got is None:
            print(
                f"[ERROR] ({done}/{len(files)}) {name}: {reply.get('title') or out[:300]}")
            return
        inserted += got
        elapsed = time.time() - start_time
        print(f"[INFO] ({done}/{len(files)}) {name}: {reply.get('title')}  "
              f"total inserted {inserted:,}  ({elapsed:.1f}s)", flush=True)

    print(f"[INFO] Uploading {len(files)} chunk(s), {IN_FLIGHT} at a time...")
    pending = deque()
    with ThreadPoolExecutor(max_workers=IN_FLIGHT) as pool:
        for file in files:
            # Chunk files are already JSON arrays: send the bytes as they are.
            with open(file, "rb") as f:
                payload = f.read()
            pending.append((file, pool.submit(curl_post, insert_url, payload)))

            if len(pending) >= IN_FLIGHT:
                done_file, future = pending.popleft()
                report(done_file, *future.result())

        for done_file, future in pending:
            report(done_file, *future.result())

    duration = time.time() - start_time
    print(f"\n[INFO] Done: {
          inserted:,} documents inserted in {duration:.2f}s.")


if __name__ == "__main__":
    main()
