import json
import time
import os
import glob
import subprocess

INPUT_DIR = "./movie_chunks"
BASE_URL = "http://localhost:6006"
DB_NAME = "movies"
MAX_CHUNKS = 0  # 0 = send all
SHARD_COUNT = 4
USERNAME = "admin"
PASSWORD = "secret"

POLICY = """\
[[fields]]
name = "id"
kind = "IdAuto"
list = true
[fields.weight]
min = 90
max = 95

[[fields]]
name = "title"
kind = "Text"
list = true
[fields.weight]
min = 90
max = 95

[[fields]]
name = "year"
kind = "Integer"
searchable = true
list = true
[fields.weight]
min = 1
max = 50

[[fields]]
name = "cast"
kind = "Text"
list = true
stemming = "english"
[fields.weight]
min = 1
max = 75

[[fields]]
name = "genres"
kind = "Text"
list = true
[fields.weight]
min = 1
max = 60

[[fields]]
name = "extract"
kind = "Text"
list = true
stemming = "english"
[fields.weight]
min = 1
max = 75

[[fields]]
name = "href"
kind = "None"
list = true
[fields.weight]
min = 0
max = 0

[[fields]]
name = "thumbnail"
kind = "None"
list = true
[fields.weight]
min = 0
max = 0

[[fields]]
name = "thumbnail_width"
kind = "None"
list = true
[fields.weight]
min = 0
max = 0

[[fields]]
name = "thumbnail_height"
kind = "None"
list = true
[fields.weight]
min = 0
max = 0

[[fields]]
name = "random_float"
kind = "Float"
searchable = true
list = true
[fields.weight]
min = 1
max = 50
"""


def login(username, password):
    body = json.dumps({"username": username, "password": password})
    result = subprocess.run(
        ["curl", "-s", "-X", "POST", f"{BASE_URL}/api/login",
         "-H", "Accept: application/json",
         "-H", "Content-Type: application/json",
         "-d", body],
        capture_output=True,
        text=True,
    )
    print(result.stdout)
    return json.loads(result.stdout)["data"]["token"]


def curl_post(url, body, token):
    result = subprocess.run(
        ["curl", "-s", "-X", "POST", url,
         "-H", "Accept: application/json",
         "-H", f"X-Corelamo-Key: {token}",
         "--data-binary", "@-"],
        input=body,
        capture_output=True,
        text=True,
    )
    return result.stdout.strip(), result.returncode


def main():
    start_time = time.time()
    print("[INFO] Starting movie uploader...")

    print(f"[INFO] Logging in as '{USERNAME}'...")
    token = login(USERNAME, PASSWORD)
    print("[INFO] Login successful, token acquired.")

    print(f"[INFO] Creating database '{DB_NAME}'...")
    out, _ = curl_post(
        f"{BASE_URL}/api/databases/{DB_NAME}/create-database",
        json.dumps({"shard_count": SHARD_COUNT}), token)
    print(f"[INFO] {out}")

    print(f"[INFO] Starting database '{DB_NAME}'...")
    out, _ = curl_post(
        f"{BASE_URL}/api/databases/{DB_NAME}/start-database", "", token)
    print(f"[INFO] {out}")

    print("[INFO] Setting policy...")
    out, _ = curl_post(
        f"{BASE_URL}/api/databases/{DB_NAME}/set-policy", POLICY, token)
    print(f"[INFO] {out}")

    files = sorted(glob.glob(os.path.join(INPUT_DIR, "movies_*.json")))
    if not files:
        print(f"[ERROR] No chunk files found in {INPUT_DIR}.")
        return

    if MAX_CHUNKS > 0:
        files = files[:MAX_CHUNKS]

    print(f"[INFO] Uploading {len(files)} chunk(s)...")
    for idx, file in enumerate(files, start=1):
        with open(file, "r", encoding="utf-8") as f:
            chunk = json.load(f)
        payload = json.dumps(chunk, ensure_ascii=False)
        out, code = curl_post(
            f"{BASE_URL}/api/databases/{DB_NAME}/insert", payload, token)
        if code != 0:
            print(f"[ERROR] Failed to upload {file}")
            print(out)
        else:
            print(
                f"[INFO] ({idx}/{len(files)}) uploaded {len(chunk)} docs — {out}")

    duration = time.time() - start_time
    print(f"\n[INFO] Done in {duration:.2f}s.")


if __name__ == "__main__":
    main()
