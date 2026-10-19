import json
import os
import random
import subprocess
import sys
import time

INPUT_FILE = "../../fever/corpus.jsonl"
BASE_URL = "http://localhost:6006"
DB_NAME = "fever"

# custom constant, tune to taste
BATCH_SIZE = 60000

# prefer environment variables; fall back to the old defaults
USERNAME = os.environ.get("CORELAMO_USER", "admin")
PASSWORD = os.environ.get("CORELAMO_PASS", "secret")

POLICY = """\
{
  "_id": {
    "$type": "id",
    "$weight": { "min": 90, "max": 95 }
  },
  "title": {
    "$type": "text",
    "$exact": true,
    "$stemming": "english",
    "$weight": { "min": 40, "max": 80 }
  },
  "text": {
    "$type": "text",
    "$stemming": "english",
    "$weight": { "min": 1, "max": 100 }
  },
  "random_year": {
    "$type": "integer",
    "$searchable": true,
    "$weight": { "min": 1, "max": 50 }
  },
  "random_float": {
    "$type": "float",
    "$weight": { "min": 1, "max": 50 }
  }
}"""


def login(username, password):
    body = json.dumps({"username": username, "password": password})
    result = subprocess.run(
        ["curl", "-sS", "-X", "POST", f"{BASE_URL}/api/login",
         "-H", "Accept: application/json",
         "-H", "Content-Type: application/json",
         "-d", body],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        sys.exit(f"[FATAL] login request failed: {result.stderr.strip()}")
    try:
        return json.loads(result.stdout)["data"]["token"]
    except (json.JSONDecodeError, KeyError, TypeError):
        # don't echo the whole response on success (it contains the token),
        # but on failure it's what you need to see
        sys.exit(f"[FATAL] login failed, server said: {result.stdout.strip()}")


def curl_post(url, body, token):
    """POST a JSON body. Returns (response_text, http_status, curl_exit_code)."""
    result = subprocess.run(
        ["curl", "-sS", "-X", "POST", url,
         "-H", "Accept: application/json",
         "-H", "Content-Type: application/json",
         "-H", f"X-Corelamo-Key: {token}",
         "-w", "\n%{http_code}",
         "--data-binary", "@-"],
        input=body,
        capture_output=True,
        text=True,
    )
    out = result.stdout
    text, _, status = out.rpartition("\n")
    try:
        status = int(status)
    except ValueError:
        status = 0
    if result.returncode != 0 and result.stderr:
        text = (text + "\n" + result.stderr).strip()
    return text.strip(), status, result.returncode


def ok(status, code):
    return code == 0 and 200 <= status < 300


def count_lines(path):
    # cheap pass just to give upload progress a denominator
    total = 0
    with open(path, "r", encoding="utf-8") as f:
        for line in f:
            if line.strip():
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


def step(label, url, body, token, fatal=True):
    print(f"[INFO] {label}...")
    out, status, code = curl_post(url, body, token)
    print(f"[INFO] HTTP {status}: {out}")
    if not ok(status, code):
        msg = f"[{'FATAL' if fatal else 'WARN'}] {
            label} failed (HTTP {status}, curl exit {code})"
        if fatal:
            sys.exit(msg)
        print(msg)
    return out


def main():
    start_time = time.time()
    print("[INFO] Starting fever uploader...")

    print(f"[INFO] Logging in as '{USERNAME}'...")
    token = login(USERNAME, PASSWORD)
    print("[INFO] Login successful, token acquired.")

    db = f"{BASE_URL}/api/databases/{DB_NAME}"

    # non-fatal: may fail simply because the database already exists
    step(f"Creating database '{DB_NAME}'",
         f"{db}/create-database", json.dumps({"shard_count": 5}), token,
         fatal=False)

    step(f"Starting database '{DB_NAME}'",
         f"{db}/start-database", "", token, fatal=False)

    step("Setting policy", f"{db}/set-policy", POLICY, token)

    print(f"[INFO] Counting lines in {INPUT_FILE}...")
    total_lines = count_lines(INPUT_FILE)
    print(f"[INFO] {total_lines} non-empty line(s) found.")

    uploaded = 0
    failed = 0
    for batch_no, batch in enumerate(batches_from_jsonl(INPUT_FILE, BATCH_SIZE), start=1):
        payload = json.dumps(batch, ensure_ascii=False)
        out, status, code = curl_post(f"{db}/insert", payload, token)

        if not ok(status, code):
            failed += len(batch)
            print(f"[ERROR] batch {batch_no} failed (HTTP {
                  status}, curl exit {code})")
            print(out)
            continue

        uploaded += len(batch)
        pct = (uploaded / total_lines * 100) if total_lines else 0
        print(f"[INFO] batch {batch_no}: {
              uploaded}/{total_lines} ({pct:.1f}%) - {out}")

    duration = time.time() - start_time
    print(f"\n[INFO] Done in {duration:.2f}s. Uploaded {
          uploaded}, failed {failed}.")
    if failed:
        sys.exit(1)


if __name__ == "__main__":
    main()
