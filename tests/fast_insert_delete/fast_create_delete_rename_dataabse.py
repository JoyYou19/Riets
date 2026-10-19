import random
from concurrent.futures import ThreadPoolExecutor
import requests

BASE_URL = "http://localhost:6006/api/databases"

# Use a small shared pool of names to force maximum collisions and race conditions
SESSION_ID = random.randint(1000, 9999)
NAMES = [f"testdb_{SESSION_ID}_{i}" for i in range(3)]


def api_create(name):
    url = f"{BASE_URL}/{name}/create-database"
    try:
        res = requests.post(url, timeout=1)
        print(
            f"[CREATE] {
                name} -> Status: {res.status_code} | Body: {res.text.strip()}"
        )
    except Exception as e:
        print(f"[CREATE ERR] {name}: {e}")


def api_delete(name):
    url = f"{BASE_URL}/{name}/delete-database"
    try:
        res = requests.delete(url, timeout=1)
        print(
            f"[DELETE] {
                name} -> Status: {res.status_code} | Body: {res.text.strip()}"
        )
    except Exception as e:
        print(f"[DELETE ERR] {name}: {e}")


def api_start(name):
    url = f"{BASE_URL}/{name}/start-database"
    try:
        res = requests.post(url, timeout=1)
        print(
            f"[START] {
                name} -> Status: {res.status_code} | Body: {res.text.strip()}"
        )
    except Exception as e:
        print(f"[START ERR] {name}: {e}")


def api_stop(name):
    url = f"{BASE_URL}/{name}/stop-database"
    try:
        res = requests.post(url, timeout=1)
        print(
            f"[STOP] {
                name} -> Status: {res.status_code} | Body: {res.text.strip()}"
        )
    except Exception as e:
        print(f"[STOP ERR] {name}: {e}")


def api_rename(old_name, new_name):
    url = f"{BASE_URL}/{old_name}/rename-database"
    try:
        res = requests.post(url, json={"name": new_name}, timeout=1)
        print(
            f"[RENAME] {
                old_name} -> {new_name} -> Status: {res.status_code} | Body: {res.text.strip()}"
        )
    except Exception as e:
        print(f"[RENAME ERR] {old_name} -> {new_name}: {e}")


def chaotic_attack(_):
    # Pick a malicious/edge-case sequence designed to break business logic
    strategy = random.choice([
        "double_create_race",
        "rapid_start_stop",
        "illegal_rename",
        "delete_and_resurrect",
        "spam_create",
    ])

    name = random.choice(NAMES)
    alt_name = f"{name}_alt"

    if strategy == "double_create_race":
        # Hammer the same creation endpoint concurrently
        api_create(name)
        api_create(name)

    elif strategy == "rapid_start_stop":
        # Flip states instantly without checking if it worked
        api_start(name)
        api_stop(name)
        api_start(name)

    elif strategy == "illegal_rename":
        # Try to rename a database while it's running (violates your rule)
        api_create(name)
        api_start(name)
        api_rename(name, alt_name)

    elif strategy == "delete_and_resurrect":
        # Delete and immediately try to start it
        api_delete(name)
        api_start(name)

    elif strategy == "spam_create":
        # Rapid-fire creation spam
        for _ in range(5):
            api_create(name)


def main():
    print(
        "--- LAUNCHING MAXIMUM CONCURRENCY FUZZER (ZERO SLEEP, MULTITHREADED) ---"
    )

    # 30 concurrent workers blasting requests instantly
    with ThreadPoolExecutor(max_workers=30) as executor:
        # Execute 300 total rapid-fire chaos iterations
        executor.map(chaotic_attack, range(300))

    print(
        "--- Fuzzing Finished. Check your server/backend logs for panics or 500 errors. ---"
    )


if __name__ == "__main__":
    main()
