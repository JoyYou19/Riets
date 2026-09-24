import json
import random
import time
import subprocess
#
INPUT_FILE = "./corpus.jsonl"
BASE_URL = "http://localhost:6006"
DB_NAME = "fever"

# custom constant, tune to taste
BATCH_SIZE = 60000

USERNAME = "admin"
PASSWORD = "secret"

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


def count_lines(path):
    # cheap pass just to give upload progress a denominator; skip if the
    # file is huge enough that this itself takes too long
    total = 0
    with open(path, "r", encoding="utf-8") as f:
        for _ in f:
            total += 1
    return total


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

            # test-only: inject random numeric fields to exercise the columns
            doc["random_year"] = random.randint(1900, 2024)
            doc["random_float"] = round(random.uniform(0.0, 100.0), 4)

            batch.append(doc)
            if len(batch) >= batch_size:
                yield batch
                batch = []

    if batch:
        yield batch


def main():
    start_time = time.time()
    print("[INFO] Starting fever uploader...")

    print(f"[INFO] Logging in as '{USERNAME}'...")
    token = login(USERNAME, PASSWORD)
    print("[INFO] Login successful, token acquired.")

    # 1. delete if exists
    # print(f"[INFO] Deleting existing '{DB_NAME}' database if it exists...")
    # out, _ = curl_delete(
    #     f"{BASE_URL}/api/databases/{DB_NAME}/clear-database", token)
    # print(f"[INFO] {out}")

    # 2. create database
    print(f"[INFO] Creating database '{DB_NAME}'...")
    out, _ = curl_post(
        f"{BASE_URL}/api/databases/{DB_NAME}/create-database", "{\"shard_count\": 5}", token)
    print(f"[INFO] {out}")

    # 2b. start database
    print(f"[INFO] Starting database '{DB_NAME}'...")
    out, _ = curl_post(
        f"{BASE_URL}/api/databases/{DB_NAME}/start-database", "", token)
    print(f"[INFO] {out}")

    # 3. set policy
    print("[INFO] Setting policy...")
    out, _ = curl_post(
        f"{BASE_URL}/api/databases/{DB_NAME}/set-policy", POLICY, token)
    print(f"[INFO] {out}")

    # 4. count lines up front so progress has a denominator
    print(f"[INFO] Counting lines in {INPUT_FILE}...")
    total_lines = count_lines(INPUT_FILE)
    print(f"[INFO] {total_lines} line(s) found.")

    # 5. stream + upload in batches
    uploaded = 0
    for batch_no, batch in enumerate(batches_from_jsonl(INPUT_FILE, BATCH_SIZE), start=1):
        payload = json.dumps(batch, ensure_ascii=False)
        out, code = curl_post(
            f"{BASE_URL}/api/databases/{DB_NAME}/insert", payload, token)

        uploaded += len(batch)
        if code != 0:
            print(f"[ERROR] batch {
                  batch_no} failed to send (curl exit {code})")
            print(out)
        else:
            pct = (uploaded / total_lines * 100) if total_lines else 0
            print(f"[INFO] batch {batch_no}: {
                  uploaded}/{total_lines} ({pct:.1f}%) — {out}")

    # 6. reindex
    # print("[INFO] Reindexing...")
    # out, _ = curl_post(
    #     f"{BASE_URL}/api/databases/{DB_NAME}/reindex", "", token)
    # print(f"[INFO] {out}")

    duration = time.time() - start_time
    print(f"\n[INFO] Done in {duration:.2f}s.")


if __name__ == "__main__":
    main()
