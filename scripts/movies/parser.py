import json
import os
import time
import math

INPUT_FILE = "movies.json"
OUTPUT_DIR = "./movie_chunks"
CHUNK_SIZE = 100  # 100 movies per file


def flatten_value(value):
    """Convert any value to a string suitable for storage."""
    if isinstance(value, list):
        return " ".join(str(v) for v in value if v is not None)
    elif value is None:
        return ""
    else:
        return str(value)


def main():
    start_time = time.time()
    print("[INFO] Starting movie parser...")

    os.makedirs(OUTPUT_DIR, exist_ok=True)

    print(f"[INFO] Reading {INPUT_FILE} ...")
    with open(INPUT_FILE, "r", encoding="utf-8") as f:
        movies = json.load(f)

    total_movies = len(movies)
    print(f"[INFO] Found {total_movies} movie entries.")

    docs = []
    for i, movie in enumerate(movies, start=1):
        doc = {"id": str(i)}
        for key, value in movie.items():
            doc[key] = flatten_value(value)
        docs.append(doc)

    num_chunks = math.ceil(total_movies / CHUNK_SIZE)
    for i in range(num_chunks):
        chunk = docs[i * CHUNK_SIZE:(i + 1) * CHUNK_SIZE]
        out_file = os.path.join(OUTPUT_DIR, f"movies_{i + 1:04d}.json")
        with open(out_file, "w", encoding="utf-8") as f:
            json.dump(chunk, f, indent=2, ensure_ascii=False)

    duration = time.time() - start_time
    print(
        f"[INFO] Finished. Created {num_chunks} chunk files in '{OUTPUT_DIR}/' in {duration:.2f}s.")
    print(
        f"[INFO] Fields kept: {sorted(set(k for m in docs for k in m.keys()))}")


if __name__ == "__main__":
    main()
