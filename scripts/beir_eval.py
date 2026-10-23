#!/usr/bin/env python3
"""BEIR-style relevance evaluation of Corelamo on FEVER.

scripts/fever holds the BEIR version of FEVER (corpus.jsonl with 5,416,568 documents,
queries.jsonl, qrels/{train,dev,test}.tsv), so the numbers this prints are comparable with
published results. The BEIR paper (Thakur et al., 2021, table 2) reports nDCG@10 on the FEVER
test split:

    BM25 0.753   ColBERT 0.771   BM25 + cross-encoder reranking 0.819

Every query is searched as plain text, the way a user types it, and the hits are matched to the
judged documents by title (a BEIR id is its title with "_" for spaces). Printed:

    nDCG@10     the BEIR headline metric (binary relevance, log2 discount, as pytrec_eval)
    Recall@100  share of the relevant documents in the top 100: what a reranker could still fix
    MRR@10      1 / rank of the first relevant document
    Hit@1/@10   a relevant document first / in the top 10
    latency     the server's own time_taken, p50 / p95 / max

    python3 beir_eval.py --database fever --save beir-base.json                 # all 6,666 test queries
    python3 beir_eval.py --database fever --limit 1000                          # a fixed sample, faster
    python3 beir_eval.py --database fever --split train --limit 2000            # tune on train, not test
    # ...change the ranking, restart the server, then:
    python3 beir_eval.py --database fever --save beir-new.json --baseline beir-base.json

Tune constants on the train or dev split and report test once at the end, or the test numbers
stop meaning anything. Only the standard library is used.
"""

from __future__ import annotations

import argparse
import json
import math
import random
import re
import sys
import time
import unicodedata
import urllib.error
import urllib.request
from collections import defaultdict
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
PUBLISHED = {"BM25": 0.753, "ColBERT": 0.771, "BM25 + cross-encoder": 0.819}


def default_fever_dir() -> Path:
    for candidate in (SCRIPT_DIR / "fever", SCRIPT_DIR.parent / "fever"):
        if (candidate / "queries.jsonl").exists():
            return candidate
    return SCRIPT_DIR.parent / "fever"


def normalize_title(title: str) -> str:
    for code, char in (("-LRB-", "("), ("-RRB-", ")"), ("-LSB-", "["), ("-RSB-", "]"), ("-COLON-", ":")):
        title = title.replace(code, char)
    return unicodedata.normalize("NFC", title.replace("_", " ")).strip().casefold()


def load_split(fever_dir: Path, split: str) -> tuple[dict[str, str], dict[str, set[str]]]:
    qrels: dict[str, set[str]] = defaultdict(set)
    with open(fever_dir / "qrels" / f"{split}.tsv", encoding="utf-8") as handle:
        next(handle, None)
        for line in handle:
            parts = line.rstrip("\n").split("\t")
            if len(parts) >= 3 and int(parts[2]) > 0:
                qrels[parts[0]].add(normalize_title(parts[1]))
    queries: dict[str, str] = {}
    with open(fever_dir / "queries.jsonl", encoding="utf-8") as handle:
        for line in handle:
            entry = json.loads(line)
            if entry["_id"] in qrels:
                queries[entry["_id"]] = entry["text"]
    return queries, qrels


# ---------------------------------------------------------------- server


def hit_titles(response: object, field: str) -> list[str]:
    """Every `field` value nested below the top level, in response order
    (the top level's own `title` is the "N hit(s)" summary)."""
    found: list[str] = []

    def walk(node: object, depth: int) -> None:
        if isinstance(node, dict):
            for key, value in node.items():
                if key == field and depth > 0 and isinstance(value, str):
                    found.append(value)
                else:
                    walk(value, depth + 1)
        elif isinstance(node, list):
            for item in node:
                walk(item, depth + 1)

    walk(response, 0)
    return found


TIME_TAKEN = re.compile(r"^\s*([\d.]+)\s*(ns|µs|us|ms|s)\s*$")
UNIT_MS = {"ns": 1e-6, "µs": 1e-3, "us": 1e-3, "ms": 1.0, "s": 1000.0}


def server_ms(response: object) -> float | None:
    if isinstance(response, dict) and isinstance(response.get("time_taken"), str):
        match = TIME_TAKEN.match(response["time_taken"])
        if match:
            return float(match.group(1)) * UNIT_MS[match.group(2)]
    return None


class Client:
    def __init__(self, args: argparse.Namespace):
        self.url = f"{args.url.rstrip('/')}/api/databases/{args.database}/search"
        self.timeout = args.timeout
        self.field = args.title_field
        self.body: dict = {"docs": args.docs, "return_fields": {args.title_field: True}}
        if args.match_mode:
            self.body["match_mode"] = args.match_mode
        if args.semantic is not None:
            self.body["semantic"] = args.semantic == "on"
        if args.search_fields:
            self.body["search_fields"] = args.search_fields

    def search(self, query: str) -> tuple[list[str], float | None]:
        body = dict(self.body, query=query)
        request = urllib.request.Request(
            self.url, data=json.dumps(body).encode(),
            headers={"Content-Type": "application/json"}, method="POST",
        )
        with urllib.request.urlopen(request, timeout=self.timeout) as response:
            payload = json.load(response)
        return hit_titles(payload, self.field), server_ms(payload)


# ---------------------------------------------------------------- metrics


def relevant_ranks(titles: list[str], relevant: set[str]) -> list[int]:
    """1-based ranks of the relevant documents in the results, each document counted once."""
    seen: set[str] = set()
    ranks = []
    for position, title in enumerate(titles, start=1):
        key = normalize_title(title)
        if key in relevant and key not in seen:
            seen.add(key)
            ranks.append(position)
    return ranks


def ndcg_at(ranks: list[int], relevant_count: int, k: int) -> float:
    dcg = sum(1.0 / math.log2(rank + 1) for rank in ranks if rank <= k)
    ideal = sum(1.0 / math.log2(rank + 1) for rank in range(1, min(relevant_count, k) + 1))
    return dcg / ideal if ideal else 0.0


def score(record: dict) -> None:
    ranks, count = record["ranks"], record["relevant"]
    record["ndcg10"] = ndcg_at(ranks, count, 10)
    record["recall100"] = sum(1 for rank in ranks if rank <= 100) / count if count else 0.0
    record["mrr10"] = 1.0 / ranks[0] if ranks and ranks[0] <= 10 else 0.0
    record["hit1"] = 1.0 if ranks and ranks[0] == 1 else 0.0
    record["hit10"] = 1.0 if ranks and ranks[0] <= 10 else 0.0


def percentile(values: list[float], fraction: float) -> float:
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(fraction * len(ordered)))] if ordered else 0.0


def mean(records: list[dict], key: str) -> float:
    return sum(r[key] for r in records) / len(records) if records else 0.0


# ---------------------------------------------------------------- run


def run_one(client: Client, qid: str, text: str, relevant: set[str]) -> dict:
    record = {"qid": qid, "query": text, "relevant": len(relevant)}
    try:
        titles, took = client.search(text)
        record["ranks"] = relevant_ranks(titles, relevant)
        record["server_ms"] = took
        record["returned"] = len(titles)
        record["top"] = titles[:3]
        score(record)
    except (urllib.error.URLError, TimeoutError, ValueError) as err:
        record["error"] = str(err)
    return record


def report(results: list[dict], split: str, show_misses: int, qrels: dict[str, set[str]]) -> None:
    ok = [r for r in results if "error" not in r]
    errors = len(results) - len(ok)
    empty = sum(1 for r in ok if r["returned"] == 0)
    print(f"\n{len(results)} {split} queries, {errors} errors, {empty} with no results\n")
    print(f"  nDCG@10     {mean(ok, 'ndcg10'):.4f}")
    print(f"  Recall@100  {mean(ok, 'recall100'):.4f}")
    print(f"  MRR@10      {mean(ok, 'mrr10'):.4f}")
    print(f"  Hit@1       {mean(ok, 'hit1'):.4f}")
    print(f"  Hit@10      {mean(ok, 'hit10'):.4f}")
    times = [r["server_ms"] for r in ok if r.get("server_ms") is not None]
    if times:
        print(f"  latency     p50 {percentile(times, 0.5):.2f} ms, p95 {percentile(times, 0.95):.2f} ms, "
              f"max {max(times):.2f} ms")
    if split == "test":
        print("\npublished FEVER test nDCG@10 (BEIR, table 2): "
              + ", ".join(f"{name} {value:.3f}" for name, value in PUBLISHED.items()))
        if len(results) < 6666:
            print(f"  (this run is a sample of {len(results)} of 6,666 queries: compare loosely)")

    misses = [r for r in ok if r["hit10"] == 0.0]
    if misses and show_misses:
        print(f"\nno relevant document in the top 10 (showing {min(show_misses, len(misses))} "
              f"of {len(misses)}):")
        for r in misses[:show_misses]:
            want = sorted(qrels[r["qid"]])[0]
            found = f"rank {r['ranks'][0]}" if r["ranks"] else "not in results"
            print(f"  {r['query']!r}\n      want {want!r} ({found}), got {r['top']}")


def compare(current: list[dict], baseline: list[dict], show: int) -> None:
    before = {r["qid"]: r for r in baseline if "error" not in r}
    pairs = [(r, before[r["qid"]]) for r in current if "error" not in r and r["qid"] in before]
    if not pairs:
        print("\nvs baseline: no queries in common")
        return
    print(f"\nvs baseline ({len(pairs)} queries in common):")
    for key in ("ndcg10", "recall100", "mrr10", "hit1", "hit10"):
        new = sum(r[key] for r, _ in pairs) / len(pairs)
        old = sum(o[key] for _, o in pairs) / len(pairs)
        print(f"  {key:<10} {old:.4f} -> {new:.4f}  ({new - old:+.4f})")
    better = sorted((p for p in pairs if p[0]["ndcg10"] > p[1]["ndcg10"] + 1e-9),
                    key=lambda p: p[1]["ndcg10"] - p[0]["ndcg10"])
    worse = sorted((p for p in pairs if p[0]["ndcg10"] < p[1]["ndcg10"] - 1e-9),
                   key=lambda p: p[0]["ndcg10"] - p[1]["ndcg10"])
    print(f"  {len(better)} queries improved, {len(worse)} regressed")
    for title, items in (("regressed", worse), ("improved", better)):
        if items and show:
            print(f"\n{title} (showing {min(show, len(items))}):")
            for new, old in items[:show]:
                old_rank = old["ranks"][0] if old["ranks"] else None
                new_rank = new["ranks"][0] if new["ranks"] else None
                print(f"  {new['query']!r}: first relevant at {old_rank} -> {new_rank}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--url", default="http://127.0.0.1:6006")
    parser.add_argument("--database", default="fever")
    parser.add_argument("--fever-dir", default=str(default_fever_dir()))
    parser.add_argument("--split", default="test", choices=["test", "dev", "train"])
    parser.add_argument("--limit", type=int, default=0, help="a fixed random sample of N queries (0 = all)")
    parser.add_argument("--seed", type=int, default=20261023, help="picks the sample; keep it fixed between runs")
    parser.add_argument("--docs", type=int, default=100, help="results read per query (Recall@100 needs 100)")
    parser.add_argument("--workers", type=int, default=1,
                        help="queries in flight; above 1 is faster but latency then includes contention")
    parser.add_argument("--match-mode", choices=["crossfields", "bestfields"], help="default: the server's")
    parser.add_argument("--semantic", choices=["on", "off"], help="vector fusion; default: the server's automatic")
    parser.add_argument("--search-fields", nargs="+", help="default: the server's")
    parser.add_argument("--title-field", default="title")
    parser.add_argument("--timeout", type=float, default=60.0)
    parser.add_argument("--save", help="write every query's result to this JSON file")
    parser.add_argument("--baseline", help="a saved JSON file to compare with")
    parser.add_argument("--misses", type=int, default=15, help="misses to print")
    parser.add_argument("--show", type=int, default=15, help="changes vs baseline to print")
    args = parser.parse_args()

    queries, qrels = load_split(Path(args.fever_dir), args.split)
    ids = sorted(queries)
    if args.limit:
        random.Random(args.seed).shuffle(ids)
        ids = sorted(ids[: args.limit])
    client = Client(args)

    started = time.monotonic()
    results: list[dict] = []
    with ThreadPoolExecutor(max_workers=max(1, args.workers)) as pool:
        futures = [pool.submit(run_one, client, qid, queries[qid], qrels[qid]) for qid in ids]
        for index, future in enumerate(futures, start=1):
            results.append(future.result())
            if index % 100 == 0 or index == len(futures):
                print(f"\r  {index}/{len(futures)} queries", end="", file=sys.stderr)
    print(f"\n  done in {time.monotonic() - started:.1f}s", file=sys.stderr)

    if args.save:
        out = Path(args.save)
        out.parent.mkdir(parents=True, exist_ok=True)
        settings = {k: getattr(args, k) for k in ("split", "limit", "seed", "docs", "match_mode", "semantic")}
        out.write_text(json.dumps({"settings": settings, "results": results}, indent=1), encoding="utf-8")
        print(f"saved {out}", file=sys.stderr)

    report(results, args.split, args.misses, qrels)
    if args.baseline:
        compare(results, json.loads(Path(args.baseline).read_text(encoding="utf-8"))["results"], args.show)


if __name__ == "__main__":
    main()