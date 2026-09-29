import json
import os
import shutil
import time
import math
import random

INPUT_FILE = "movies.json"
OUTPUT_DIR = "./movie_chunks"
CHUNK_SIZE = 10000  # 1000 movies per file


def flatten_value(value):
    """Convert any value to a string suitable for storage."""
    if isinstance(value, list):
        return " ".join(str(v) for v in value if v is not None)
    elif value is None:
        return ""
    else:
        return str(value)


def parse_cast(cast_value):
    """Convert cast entries into a list of dictionaries with name and surname."""
    if not isinstance(cast_value, list):
        if isinstance(cast_value, str) and cast_value.strip():
            cast_value = [cast_value]
        else:
            return []

    structured_cast = []
    for person in cast_value:
        if not person:
            continue
        parts = str(person).strip().split()
        if not parts:
            continue
        elif len(parts) == 1:
            name = parts[0]
            surname = ""
        else:
            name = " ".join(parts[:-1])
            surname = parts[-1]

        structured_cast.append({"name": name, "surname": surname})

    return structured_cast


def parse_genres(genres_value):
    """Convert genres into a clean array of strings."""
    if isinstance(genres_value, list):
        return [str(g).strip() for g in genres_value if g is not None and str(g).strip()]
    elif isinstance(genres_value, str) and genres_value.strip():
        # Handles comma-separated strings if your data uses them
        return [g.strip() for g in genres_value.split(",") if g.strip()]
    else:
        return []


def main():
    start_time = time.time()
    print("[INFO] Starting movie parser...")

    # Clear output directory if it exists, then recreate it
    if os.path.exists(OUTPUT_DIR):
        print(f"[INFO] Clearing existing directory: {OUTPUT_DIR} ...")
        shutil.rmtree(OUTPUT_DIR)

    os.makedirs(OUTPUT_DIR, exist_ok=True)

    print(f"[INFO] Reading {INPUT_FILE} ...")
    with open(INPUT_FILE, "r", encoding="utf-8") as f:
        movies = json.load(f)

    total_movies = len(movies)
    print(f"[INFO] Found {total_movies} movie entries.")

    docs = []
    for i, movie in enumerate(movies, start=1):
        doc = {}
        for key, value in movie.items():
            if key == "cast":
                doc[key] = parse_cast(value)
            elif key == "genres":
                doc[key] = parse_genres(value)
            else:
                doc[key] = flatten_value(value)

        # Add random float between 1.000 and 1000.000
        # doc["random_float"] = round(random.uniform(1.0, 1000.0), 3)

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
