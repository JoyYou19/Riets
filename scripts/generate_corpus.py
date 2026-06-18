#!/usr/bin/env python3

import argparse
import json
import random
from collections import deque
from pathlib import Path

COMMON = [
    "database", "search", "engine", "storage", "index", "query", "segment",
    "rust", "distributed", "cache", "document", "ranking", "posting",
    "token", "schema", "cluster", "replica", "memory", "disk", "snapshot",
]

FILLER = [
    "system", "service", "data", "node", "request", "response", "field",
    "record", "value", "worker", "thread", "batch", "merge", "flush",
    "reader", "writer", "latency", "throughput", "recovery", "payload",
]

TOPICS = [
    "database indexing",
    "search ranking",
    "distributed storage",
    "query execution",
    "segment compaction",
    "disk persistence",
    "memory tables",
    "token analysis",
]


def build_term_pool(rng: random.Random, max_term: int) -> deque[str]:
    terms = []
    for i in range(1, max_term + 1):
        terms.extend([f"term{i}"] * i)

    rng.shuffle(terms)
    return deque(terms)


def sentence(
    rng: random.Random,
    term_pool: deque[str],
    min_words: int = 8,
    max_words: int = 22,
) -> str:
    words = []

    for _ in range(rng.randint(min_words, max_words)):
        if rng.random() < 0.35:
            words.append(rng.choice(COMMON))
        else:
            words.append(rng.choice(FILLER))

    if term_pool:
        words.append(term_pool.popleft())

    return " ".join(words).capitalize() + "."


def paragraph(
    rng: random.Random,
    term_pool: deque[str],
    min_sentences: int = 4,
    max_sentences: int = 12,
) -> str:
    return " ".join(
        sentence(rng, term_pool)
        for _ in range(rng.randint(min_sentences, max_sentences))
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--docs", type=int, default=10_000)
    parser.add_argument(
        "--out",
        type=Path,
        default=Path("crates/core-testkit/fixtures/generated_10k.jsonl"),
    )
    parser.add_argument("--seed", type=int, default=42)
    parser.add_argument("--max-term", type=int, default=999)
    args = parser.parse_args()

    rng = random.Random(args.seed)
    term_pool = build_term_pool(rng, args.max_term)

    args.out.parent.mkdir(parents=True, exist_ok=True)

    with args.out.open("w", encoding="utf-8") as f:
        for doc_id in range(1, args.docs + 1):
            topic = rng.choice(TOPICS)
            title = f"{topic.title()} Document {doc_id}"
            body = paragraph(rng, term_pool)

            row = {
                "id": doc_id,
                "title": title,
                "body": body,
            }

            f.write(json.dumps(row, ensure_ascii=False) + "\n")

    if term_pool:
        raise RuntimeError(
            f"Not enough generated sentences to place all terms. "
            f"{len(term_pool)} term occurrences were left unused. "
            f"Increase --docs or lower --max-term."
        )


if __name__ == "__main__":
    main()
