import json
import time
import os
import glob
import subprocess

INPUT_DIR = "./movie_chunks"
BASE_URL = "http://localhost:6006"
DB_NAME = "movies"
MAX_CHUNKS = 0  # 0 = send all

USERNAME = "admin"
PASSWORD = "secret"

POLICY = """\
[[fields]]
name = "id"
index = "IdAuto"
list = true
[fields.weight]
min = 90
max = 95


[[fields]]
name = "title"
index = "Text"
list = true
[fields.weight]
min = 90
max = 95

[[fields]]
name     = "year"
index    = "Text"
list   = true
stemming = ""
[fields.weight]
min = 1
max = 50

[[fields]]
name     = "cast"
index    = "Text"
list   = true
stemming = "english"
[fields.weight]
min = 1
max = 75

[[fields]]
name     = "genres"
index    = "Text"
list   = true
stemming = ""
[fields.weight]
min = 1
max = 60

[[fields]]
name     = "extract"
index    = "Text"
list   = true
stemming = "english"
[fields.weight]
min = 1
max = 75

[[fields]]
name     = "href"
index    = "None"
list   = true
stemming = ""
[fields.weight]
min = 0
max = 0

[[fields]]
name     = "thumbnail"
index    = "None"
list   = true
stemming = ""
[fields.weight]
min = 0
max = 0

[[fields]]
name     = "thumbnail_width"
index    = "None"
list   = true
stemming = ""
[fields.weight]
min = 0
max = 0

[[fields]]
name     = "thumbnail_height"
index    = "None"
list   = true
stemming = ""
[fields.weight]
min = 0
max = 0
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


def curl_put(url, body, token):
    result = subprocess.run(
        ["curl", "-s", "-X", "PUT", url,
         "-H", "Accept: application/json",
         "-H", f"X-Corelamo-Key: {token}",
         "--data-binary", "@-"],
        input=body,
        capture_output=True,
        text=True,
    )
    return result.stdout.strip(), result.returncode


def curl_delete(url, token):
    result = subprocess.run(
        ["curl", "-s", "-X", "DELETE", url,
         "-H", f"X-Corelamo-Key: {token}"],
        capture_output=True,
        text=True,
    )
    return result.stdout.strip(), result.returncode


def main():
    start_time = time.time()
    print("[INFO] Starting movie uploader...")

    # 0. log in first — every request below needs the token
    print(f"[INFO] Logging in as '{USERNAME}'...")
    token = login(USERNAME, PASSWORD)

    print("[INFO] Login successful, token acquired.")

    # 1. delete if exists
    # print(f"[INFO] Deleting existing '{DB_NAME}' database if it exists...")
    # out, _ = curl_delete(
    #    f"{BASE_URL}/api/databases/{DB_NAME}/clear-database", token)
    # print(f"[INFO] {out}")

    # 2. create database
    print(f"[INFO] Creating database '{DB_NAME}'...")
    out, _ = curl_post(
        f"{BASE_URL}/api/databases/{DB_NAME}/create-database", "", token)
    print(f"[INFO] {out}")
    # 2b. start database
    print(f"[INFO] Starting database '{DB_NAME}'...")
    out, _ = curl_post(
        f"{BASE_URL}/api/databases/{DB_NAME}/start-database", "", token)
    print(f"[INFO] {out}")
    # 3. set policy — always TOML, no format suffix
    print("[INFO] Setting policy...")
    out, _ = curl_post(
        f"{BASE_URL}/api/databases/{DB_NAME}/set-policy", POLICY, token)
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
            f"{BASE_URL}/api/databases/{DB_NAME}/insert", payload, token)
        if code != 0:
            print(f"[ERROR] Failed to upload {file}")
            print(out)
        else:
            print(
                f"[INFO] ({idx}/{len(files)}) uploaded {len(chunk)} docs — {out}")

    # 5. reindex
    # print("[INFO] Reindexing...")
    # out, _ = curl_post(
    #     f"{BASE_URL}/api/databases/{DB_NAME}/reindex", "", token)
    # print(f"[INFO] {out}")

    duration = time.time() - start_time
    print(f"\n[INFO] Done in {duration:.2f}s.")


if __name__ == "__main__":
    main()
