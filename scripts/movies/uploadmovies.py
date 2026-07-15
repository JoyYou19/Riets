import subprocess
import json
import time
import os
import glob

INPUT_DIR = "./movie_chunks"
BASE_URL = "http://localhost:6006"
DB_NAME = "movies"
MAX_CHUNKS = 0  # 0 = send all

POLICY = """\
[[fields]]
name = "title"
xpath = 0
index = "Id"
list = true

[fields.weight]
min = 90
max = 95


[[fields]]
name     = "year"
xpath    = 2
index    = "Text"
list   = true
stemming = ""

[fields.weight]
min = 1
max = 50

[[fields]]
name     = "cast"
xpath    = 3
index    = "Text"
list   = true
stemming = "english"

[fields.weight]
min = 1
max = 75

[[fields]]
name     = "genres"
xpath    = 4
index    = "Text"
list   = true
stemming = ""

[fields.weight]
min = 1
max = 60

[[fields]]
name     = "extract"
xpath    = 5
index    = "Text"
list   = true
stemming = "english"

[fields.weight]
min = 1
max = 75

[[fields]]
name     = "href"
xpath    = 6
index    = "None"
list   = true
stemming = ""

[fields.weight]
min = 0
max = 0

[[fields]]
name     = "thumbnail"
xpath    = 7
index    = "None"
list   = true
stemming = ""

[fields.weight]
min = 0
max = 0

[[fields]]
name     = "thumbnail_width"
xpath    = 8
index    = "None"
list   = true
stemming = ""

[fields.weight]
min = 0
max = 0

[[fields]]
name     = "thumbnail_height"
xpath    = 9
index    = "None"
list   = true
stemming = ""

[fields.weight]
min = 0
max = 0
"""


def curl_post(url, body):
    result = subprocess.run(
        ["curl", "-s", "-X", "POST", url,
         "-H", "Accept: application/json",
         "-d", body],
        capture_output=True,
        text=True,
    )
    return result.stdout.strip(), result.returncode


def curl_delete(url):
    result = subprocess.run(
        ["curl", "-s", "-X", "DELETE", url],
        capture_output=True,
        text=True,
    )
    return result.stdout.strip(), result.returncode


def main():
    start_time = time.time()
    print("[INFO] Starting movie uploader...")

    # 1. delete if exists
    print(f"[INFO] Deleting existing '{DB_NAME}' database if it exists...")
    out, _ = curl_delete(f"{BASE_URL}/api/databases/{DB_NAME}/delete-database")
    print(f"[INFO] {out}")

    # 2. create database
    print(f"[INFO] Creating database '{DB_NAME}'...")
    out, _ = curl_post(
        f"{BASE_URL}/api/databases/{DB_NAME}/create-database", "")
    print(f"[INFO] {out}")

    # 3. set policy — always TOML, no format suffix
    print("[INFO] Setting policy...")
    out, _ = curl_post(
        f"{BASE_URL}/api/databases/{DB_NAME}/policy",
        POLICY
    )
    print(f"[INFO] {out}")

    # 4. upload chunks
    files = sorted(glob.glob(os.path.join(INPUT_DIR, "movies_*.json")))
    if not files:
        print(
            f"[ERROR] No chunk files found in {INPUT_DIR}. Run parse_movies.py first.")
        return

    if MAX_CHUNKS > 0:
        files = files[:MAX_CHUNKS]

    print(f"[INFO] Uploading {len(files)} chunk(s)...")
    for idx, file in enumerate(files, start=1):
        with open(file, "r", encoding="utf-8") as f:
            chunk = json.load(f)

        payload = json.dumps(chunk, ensure_ascii=False)
        out, code = curl_post(
            f"{BASE_URL}/api/databases/{DB_NAME}/insert",
            payload
        )

        if code != 0:
            print(f"[ERROR] Failed to upload {file}")
            print(out)
        else:
            print(
                f"[INFO] ({idx}/{len(files)}) uploaded {len(chunk)} docs — {out}")

    # 5. reindex
    print("[INFO] Reindexing...")
    out, _ = curl_post(f"{BASE_URL}/api/databases/{DB_NAME}/reindex", "")
    print(f"[INFO] {out}")

    duration = time.time() - start_time
    print(f"\n[INFO] Done in {duration:.2f}s.")


if __name__ == "__main__":
    main()
