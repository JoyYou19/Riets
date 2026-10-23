#!/usr/bin/env python3
"""BEIR-style relevance evaluation of Corelamo.

Works on any BEIR dataset folder (corpus.jsonl, queries.jsonl, qrels/{split}.tsv): scripts/fever,
or the ones load_beir.py downloads into scripts/beir/<name>. The numbers are comparable with
published results; the BEIR paper (Thakur et al., 2021, table 2) reports these BM25 nDCG@10
scores on the test splits:

    fever 0.753   climate-fever 0.213   scifact 0.665   nq 0.329   hotpotqa 0.603
    fiqa 0.236    trec-covid 0.656      nfcorpus 0.325   quora 0.789

Every query is searched as plain text, the way a user types it. Hits are matched to the judged
documents by id (`--match id`, the default: the BEIR `_id` each document was loaded with comes
back as the hit's id) or by title (`--match title`, how runs of the earlier version matched;
only meaningful for FEVER, where every title is unique). Graded judgements (trec-covid,
nfcorpus: 1 = relevant, 2 = highly relevant) count with their grade, as pytrec_eval does.
Printed:

    nDCG@10     the BEIR headline metric
    Recall@100  share of the relevant documents in the top 100: what a reranker could still fix
    MRR@10      1 / rank of the first relevant document
    Hit@1/@10   a relevant document first / in the top 10
    latency     the server's own time_taken, p50 / p95 / max

    python3 beir_eval.py --database fever --save fever-base.json                       # FEVER test
    python3 beir_eval.py --database scifact --dataset-dir beir/scifact --save scifact-base.json
    python3 beir_eval.py --database nq --dataset-dir beir/nq --limit 1000
    python3 beir_eval.py --database fever --split train --limit 2000                   # tune on train
    # ...change the ranking, restart the server, then:
    python3 beir_eval.py --database fever --save fever-new.json --baseline fever-base.json

Tune constants on a train or dev split and report test once at the end, or the test numbers
stop meaning anything (nq has only a test split: use a fixed --limit sample for tuning and the
full split once). Only the standard library is used.
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
PUBLISHED_BM25 = {
    "fever": 0.753, "climate-fever": 0.213, "scifact": 0.665, "nq": 0.329, "hotpotqa": 0.603,
    "fiqa": 0.236, "trec-covid": 0.656, "nfcorpus": 0.325, "quora": 0.789, "dbpedia-entity": 0.313,
    "scidocs": 0.158, "arguana": 0.315, "webis-touche2020": 0.367, "msmarco": 0.228,
}


def default_dataset_dir() -> Path:
    for candidate in (SCRIPT_DIR / "fever", SCRIPT_DIR.parent / "fever"):
        if (candidate / "queries.jsonl").exists():
            return candidate
    return SCRIPT_DIR.parent / "fever"


def normalize_title(title: str) -> str:
    for code, char in (("-LRB-", "("), ("-RRB-", ")"), ("-LSB-", "["), ("-RSB-", "]"), ("-COLON-", ":")):
        title = title.replace(code, char)
    return unicodedata.normalize("NFC", title.replace("_", " ")).strip().casefold()


def load_split(dataset_dir: Path, split: str, match: str) -> tuple[dict[str, str], dict[str, dict[str, int]]]:
    """Queries with at least one relevant document, and per query: document key -> grade."""
    qrels_file = dataset_dir / "qrels" / f"{split}.tsv"
    if not qrels_file.exists():
        available = sorted(p.stem for p in (dataset_dir / "qrels").glob("*.tsv"))
        sys.exit(f"{qrels_file} not found; splits in {dataset_dir}: {', '.join(available) or 'none'}")
    qrels: dict[str, dict[str, int]] = defaultdict(dict)
    with open(qrels_file, encoding="utf-8") as handle:
        next(handle, None)
        for line in handle:
            parts = line.rstrip("\n").split("\t")
            if len(parts) < 3 or int(parts[2]) <= 0:
                continue
            key = parts[1] if match == "id" else normalize_title(parts[1])
            qrels[parts[0]][key] = max(qrels[parts[0]].get(key, 0), int(parts[2]))
    queries: dict[str, str] = {}
    with open(dataset_dir / "queries.jsonl", encoding="utf-8") as handle:
        for line in handle:
            entry = json.loads(line)
            if entry["_id"] in qrels:
                queries[entry["_id"]] = entry["text"]
    return queries, qrels


# ---------------------------------------------------------------- server


def hit_list(response: object) -> list[dict]:
    """The hits of a search response: the list whose entries carry an id and a score."""
    if isinstance(response, list):
        if response and all(isinstance(item, dict) and "id" in item and "score" in item for item in response):
            return response
        for item in response:
            found = hit_list(item)
            if found:
                return found
    elif isinstance(response, dict):
        for value in response.values():
            found = hit_list(value)
            if found:
                return found
    return []


def hit_keys(response: object, match: str, title_field: str) -> list[str]:
    """Each hit's id, or its title (normalized), in response order."""
    keys = []
    for hit in hit_list(response):
        if match == "id":
            keys.append(str(hit["id"]))
        else:
            data = hit.get("data")
            title = data.get(title_field) if isinstance(data, dict) else None
            keys.append(normalize_title(title) if isinstance(title, str) else "")
    return keys


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
        self.match = args.match
        self.title_field = args.title_field
        self.body: dict = {"docs": args.docs, "return_fields": {args.title_field: True}}
        if args.match_mode:
            self.body["match_mode"] = args.match_mode
        if args.semantic is not None:
            self.body["semantic"] = args.semantic == "on"
        if args.search_fields:
            self.body["search_fields"] = args.search_fields

    def search(self, query: str) -> tuple[list[str], list[str], float | None]:
        body = dict(self.body, query=query)
        request = urllib.request.Request(
            self.url, data=json.dumps(body).encode(),
            headers={"Content-Type": "application/json"}, method="POST",
        )
        with urllib.request.urlopen(request, timeout=self.timeout) as response:
            payload = json.load(response)
        keys = hit_keys(payload, self.match, self.title_field)
        titles = [
            str((hit.get("data") or {}).get(self.title_field, hit.get("id")))
            for hit in hit_list(payload)
        ]
        return keys, titles, server_ms(payload)


# ---------------------------------------------------------------- metrics


def relevant_hits(keys: list[str], grades: dict[str, int]) -> list[tuple[int, int]]:
    """(1-based rank, grade) of every relevant document in the results, each counted once."""
    seen: set[str] = set()
    found = []
    for position, key in enumerate(keys, start=1):
        if key in grades and key not in seen:
            seen.add(key)
            found.append((position, grades[key]))
    return found


def ndcg_at(found: list[tuple[int, int]], grades: dict[str, int], k: int) -> float:
    """nDCG with the judgement grade as gain and a log2 discount, as pytrec_eval computes it."""
    dcg = sum(grade / math.log2(rank + 1) for rank, grade in found if rank <= k)
    ideal_grades = sorted(grades.values(), reverse=True)[:k]
    ideal = sum(grade / math.log2(rank + 1) for rank, grade in enumerate(ideal_grades, start=1))
    return dcg / ideal if ideal else 0.0


def score(record: dict, grades: dict[str, int]) -> None:
    found = [(rank, grade) for rank, grade in record["found"]]
    ranks = [rank for rank, _ in found]
    record["ranks"] = ranks
    record["ndcg10"] = ndcg_at(found, grades, 10)
    record["recall100"] = sum(1 for rank in ranks if rank <= 100) / len(grades) if grades else 0.0
    record["mrr10"] = 1.0 / ranks[0] if ranks and ranks[0] <= 10 else 0.0
    record["hit1"] = 1.0 if ranks and ranks[0] == 1 else 0.0
    record["hit10"] = 1.0 if ranks and ranks[0] <= 10 else 0.0


def percentile(values: list[float], fraction: float) -> float:
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(fraction * len(ordered)))] if ordered else 0.0


def mean(records: list[dict], key: str) -> float:
    return sum(r[key] for r in records) / len(records) if records else 0.0


# ---------------------------------------------------------------- run


def run_one(client: Client, qid: str, text: str, grades: dict[str, int]) -> dict:
    record = {"qid": qid, "query": text, "relevant": len(grades)}
    try:
        keys, titles, took = client.search(text)
        record["found"] = relevant_hits(keys, grades)
        record["server_ms"] = took
        record["returned"] = len(keys)
        record["top"] = titles[:3]
        score(record, grades)
    except (urllib.error.URLError, TimeoutError, ValueError) as err:
        record["error"] = str(err)
    return record


def report(results: list[dict], dataset: str, split: str, total: int, show_misses: int,
           qrels: dict[str, dict[str, int]]) -> None:
    ok = [r for r in results if "error" not in r]
    errors = len(results) - len(ok)
    empty = sum(1 for r in ok if r["returned"] == 0)
    print(f"\n{dataset}: {len(results)} {split} queries, {errors} errors, {empty} with no results\n")
    print(f"  nDCG@10     {mean(ok, 'ndcg10'):.4f}")
    print(f"  Recall@100  {mean(ok, 'recall100'):.4f}")
    print(f"  MRR@10      {mean(ok, 'mrr10'):.4f}")
    print(f"  Hit@1       {mean(ok, 'hit1'):.4f}")
    print(f"  Hit@10      {mean(ok, 'hit10'):.4f}")
    times = [r["server_ms"] for r in ok if r.get("server_ms") is not None]
    if times:
        print(f"  latency     p50 {percentile(times, 0.5):.2f} ms, p95 {percentile(times, 0.95):.2f} ms, "
              f"max {max(times):.2f} ms")
    if split == "test" and dataset in PUBLISHED_BM25:
        print(f"\npublished BM25 nDCG@10 on {dataset} test (BEIR, table 2): {PUBLISHED_BM25[dataset]:.3f}")
        if len(results) < total:
            print(f"  (this run is a sample of {len(results)} of {total:,} queries: compare loosely)")

    misses = [r for r in ok if r["hit10"] == 0.0]
    if misses and show_misses:
        print(f"\nno relevant document in the top 10 (showing {min(show_misses, len(misses))} "
              f"of {len(misses)}):")
        for r in misses[:show_misses]:
            want = max(qrels[r["qid"]].items(), key=lambda item: (item[1], item[0]))[0]
            found = f"rank {r['ranks'][0]}" if r["ranks"] else "not in results"
            print(f"  {r['query']!r}\n      want {want!r} ({found}), got {r['top']}")


def compare(current: list[dict], baseline: dict, settings: dict, show: int) -> None:
    old_settings = baseline.get("settings", {})
    old_match = old_settings.get("match", "title")
    if old_match != settings["match"]:
        print(f"\nWARNING: the baseline matched documents by {old_match}, this run by "
              f"{settings['match']}; rerun one of them with --match {old_match} to compare")
    before = {r["qid"]: r for r in baseline["results"] if "error" not in r}
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
    parser.add_argument("--dataset-dir", "--fever-dir", default=str(default_dataset_dir()),
                        help="a BEIR dataset folder (default: scripts/fever)")
    parser.add_argument("--split", default="test", help="qrels/<split>.tsv: test, dev or train")
    parser.add_argument("--match", default="id", choices=["id", "title"],
                        help="match hits to judged documents by BEIR id (default) or by title")
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

    dataset_dir = Path(args.dataset_dir)
    dataset = dataset_dir.resolve().name
    queries, qrels = load_split(dataset_dir, args.split, args.match)
    ids = sorted(queries)
    total = len(ids)
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

    settings = {k: getattr(args, k) for k in ("split", "limit", "seed", "docs", "match", "match_mode", "semantic")}
    settings["dataset"] = dataset
    if args.save:
        out = Path(args.save)
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(json.dumps({"settings": settings, "results": results}, indent=1), encoding="utf-8")
        print(f"saved {out}", file=sys.stderr)

    report(results, dataset, args.split, total, args.misses, qrels)
    if args.baseline:
        baseline = json.loads(Path(args.baseline).read_text(encoding="utf-8"))
        compare(results, baseline, settings, args.show)


if __name__ == "__main__":
    main()