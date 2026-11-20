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

then where the first relevant document lands (rank buckets), the metrics per kind of query
(length, negation, numbers, how many documents are relevant), why queries miss (see "Miss
categories" below) and the documents that are hardest to find.

    python3 beir_eval.py --database fever --save fever-base.json                       # FEVER test
    python3 beir_eval.py --database scifact --dataset-dir beir/scifact --save scifact-base.json
    python3 beir_eval.py --database nq --dataset-dir beir/nq --limit 1000
    python3 beir_eval.py --database fever --split train --limit 2000                   # tune on train
    # ...change the ranking, restart the server, then:
    python3 beir_eval.py --database fever --save fever-new.json --baseline fever-base.json
    # the hard cases as separate files (see "Analysis files" below):
    python3 beir_eval.py --database fever --split train --limit 5000 --analysis-dir analysis/run1

Miss categories (for every query whose first relevant document is not at rank 1). They lean on
titles, so they mean most for FEVER, where a document's id is its title:

    not_found           no relevant document anywhere in the returned results
    same_name_above     a page with the same base title ("Macbeth (album)" for "Macbeth") ranks
                        above it: a disambiguation problem
    title_not_in_query  the relevant title's words are not all in the query (e.g. "Island" for
                        a query about Bali): it can only be found through its text
    title_in_query      the title is in the query, yet other pages rank above it
    other               no title to compare (opaque ids)

Analysis files (`--analysis-dir DIR`, tab separated, open in any spreadsheet):

    not_found.tsv       queries with no relevant document in the results at all
    ranked_low.tsv      queries whose first relevant document is found but below the top 10
    outranked.tsv       queries whose first relevant document is at ranks 2-10
    hard_documents.tsv  per relevant document: how often it was wanted, found first, found in the
                        top 10, not found at all; most-missed first
    summary.txt         the printed report, as a file

Tune constants on a train or dev split and report test once at the end, or the test numbers
stop meaning anything (nq has only a test split: use a fixed --limit sample for tuning and the
full split once). Only the standard library is used.
"""

from __future__ import annotations

import argparse
import contextlib
import csv
import io
import json
import math
import random
import re
import statistics
import sys
import time
import unicodedata
import urllib.error
import urllib.request
from collections import Counter, defaultdict
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
PUBLISHED_BM25 = {
    "fever": 0.753, "climate-fever": 0.213, "scifact": 0.665, "nq": 0.329, "hotpotqa": 0.603,
    "fiqa": 0.236, "trec-covid": 0.656, "nfcorpus": 0.325, "quora": 0.789, "dbpedia-entity": 0.313,
    "scidocs": 0.158, "arguana": 0.315, "webis-touche2020": 0.367, "msmarco": 0.228,
}

# hits kept per query in the saved results and the analysis files
TOP_KEPT = 10
RANK_BUCKETS = (("1", 1, 1), ("2-3", 2, 3), ("4-10", 4, 10), ("11-20", 11, 20),
                ("21-50", 21, 50), ("51-100", 51, 100), ("101+", 101, 10**9))
MISS_CATEGORIES = ("not_found", "same_name_above", "title_not_in_query", "title_in_query", "other")
NEGATION = re.compile(r"\b(not|never|no|none|nobody|nothing|neither|nor|without|only)\b|n't\b",
                      re.IGNORECASE)
NUMBER = re.compile(r"\d")
WORD = re.compile(r"\w+", re.UNICODE)
PARENTHETICAL = re.compile(r"\s*\([^)]*\)\s*$")
# ids that are hashes or numbers carry no title to compare with
OPAQUE_ID = re.compile(r"^[0-9a-f\-]+$|^\d+$", re.IGNORECASE)


def default_dataset_dir() -> Path:
    for candidate in (SCRIPT_DIR / "fever", SCRIPT_DIR.parent / "fever"):
        if (candidate / "queries.jsonl").exists():
            return candidate
    return SCRIPT_DIR.parent / "fever"


def normalize_title(title: str) -> str:
    for code, char in (("-LRB-", "("), ("-RRB-", ")"), ("-LSB-", "["), ("-RSB-", "]"), ("-COLON-", ":")):
        title = title.replace(code, char)
    return unicodedata.normalize("NFC", title.replace("_", " ")).strip().casefold()


def base_title(title: str) -> str:
    """The title without a trailing "(...)": "macbeth (album)" -> "macbeth"."""
    return PARENTHETICAL.sub("", normalize_title(title))


def words(text: str) -> set[str]:
    return {w.casefold() for w in WORD.findall(unicodedata.normalize("NFC", text))}


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


def hit_keys(hits: list[dict], match: str, title_field: str) -> list[str]:
    """Each hit's id, or its title (normalized), in response order."""
    keys = []
    for hit in hits:
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


def as_float(value: object) -> float | None:
    try:
        return float(value)  # type: ignore[arg-type]
    except (TypeError, ValueError):
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

    def search(self, query: str) -> tuple[list[str], list[str], list[float | None], float | None]:
        """The hits' keys, titles and scores in response order, and the server's time."""
        body = dict(self.body, query=query)
        request = urllib.request.Request(
            self.url, data=json.dumps(body).encode(),
            headers={"Content-Type": "application/json"}, method="POST",
        )
        with urllib.request.urlopen(request, timeout=self.timeout) as response:
            payload = json.load(response)
        hits = hit_list(payload)
        keys = hit_keys(hits, self.match, self.title_field)
        titles = [str((hit.get("data") or {}).get(self.title_field, hit.get("id"))) for hit in hits]
        scores = [as_float(hit.get("score")) for hit in hits]
        return keys, titles, scores, server_ms(payload)


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


# ---------------------------------------------------------------- query analysis


def wanted_key(record: dict, grades: dict[str, int]) -> str:
    """The relevant document the query is judged by: the one found first, else the best graded."""
    if record.get("first_relevant_key"):
        return record["first_relevant_key"]
    return max(grades.items(), key=lambda item: (item[1], item[0]))[0]


def has_title(key: str) -> bool:
    return bool(key) and not OPAQUE_ID.match(key)


def miss_category(record: dict, want: str) -> str:
    """Why the first relevant document is not at rank 1 (see the module docs)."""
    if not record["ranks"]:
        return "not_found"
    if not has_title(want):
        return "other"
    want_base = base_title(want)
    first = record["ranks"][0]
    above = record.get("titles_above", [])
    if any(base_title(title) == want_base for title in above[: first - 1]):
        return "same_name_above"
    if not words(want_base) <= words(record["query"]):
        return "title_not_in_query"
    return "title_in_query"


def annotate(record: dict, grades: dict[str, int], keys: list[str], titles: list[str],
             scores: list[float | None]) -> None:
    """Query features, the hits around the first relevant document and the miss category."""
    text = record["query"]
    record["query_words"] = len(WORD.findall(text))
    record["negation"] = bool(NEGATION.search(text))
    record["has_number"] = bool(NUMBER.search(text))
    record["top_hits"] = [[titles[i], scores[i]] for i in range(min(TOP_KEPT, len(titles)))]
    record["top_score"] = scores[0] if scores else None
    ranks = record["ranks"]
    if ranks:
        first = ranks[0]
        record["first_relevant_key"] = keys[first - 1]
        record["first_relevant_title"] = titles[first - 1]
        record["first_relevant_score"] = scores[first - 1]
        # titles ranked above it (at most TOP_KEPT), for the same-name check
        record["titles_above"] = titles[: min(first - 1, TOP_KEPT)]
        top, own = scores[0], scores[first - 1]
        record["score_gap"] = (top - own) if top is not None and own is not None else None
    want = wanted_key(record, grades)
    record["want"] = want
    record["want_title"] = normalize_title(want) if has_title(want) else want
    record["category"] = None if record["hit1"] == 1.0 else miss_category(record, want)


def length_bucket(n: int) -> str:
    if n <= 4:
        return "1-4 words"
    if n <= 8:
        return "5-8 words"
    if n <= 12:
        return "9-12 words"
    return "13+ words"


def relevant_bucket(n: int) -> str:
    return "1 relevant" if n == 1 else "2-3 relevant" if n <= 3 else "4+ relevant"


def rank_bucket(rank: int | None) -> str:
    if rank is None:
        return "not found"
    for name, low, high in RANK_BUCKETS:
        if low <= rank <= high:
            return name
    return "101+"


# ---------------------------------------------------------------- run


def run_one(client: Client, qid: str, text: str, grades: dict[str, int]) -> dict:
    record = {"qid": qid, "query": text, "relevant": len(grades)}
    try:
        keys, titles, scores, took = client.search(text)
        record["found"] = relevant_hits(keys, grades)
        record["server_ms"] = took
        record["returned"] = len(keys)
        record["top"] = titles[:3]
        score(record, grades)
        annotate(record, grades, keys, titles, scores)
    except (urllib.error.URLError, TimeoutError, ValueError) as err:
        record["error"] = str(err)
    return record


def print_table(title: str, groups: dict[str, list[dict]], order: list[str] | None = None) -> None:
    total = sum(len(v) for v in groups.values())
    if not total:
        return
    print(f"\n{title}:")
    print(f"  {'':<16} {'queries':>8} {'share':>7} {'nDCG@10':>8} {'Hit@1':>7} {'Hit@10':>7} {'R@100':>7}")
    for name in order or sorted(groups):
        rows = groups.get(name, [])
        if not rows:
            continue
        print(f"  {name:<16} {len(rows):>8} {len(rows) / total:>7.1%} {mean(rows, 'ndcg10'):>8.4f} "
              f"{mean(rows, 'hit1'):>7.4f} {mean(rows, 'hit10'):>7.4f} {mean(rows, 'recall100'):>7.4f}")


def hard_documents(ok: list[dict]) -> list[dict]:
    """Per wanted document: how its queries went, most-missed first."""
    per_doc: dict[str, dict] = {}
    for r in ok:
        doc = per_doc.setdefault(r["want"], {
            "document": r["want_title"], "queries": 0, "hit1": 0, "hit10": 0, "not_found": 0,
            "ranks": [], "beaten_by": Counter(), "example_query": r["query"],
        })
        doc["queries"] += 1
        doc["hit1"] += int(r["hit1"])
        doc["hit10"] += int(r["hit10"])
        if not r["ranks"]:
            doc["not_found"] += 1
        else:
            doc["ranks"].append(r["ranks"][0])
        if r["hit1"] == 0.0 and r["top"]:
            doc["beaten_by"][r["top"][0]] += 1
            if doc["queries"] - doc["hit1"] == 1:
                doc["example_query"] = r["query"]
    rows = []
    for doc in per_doc.values():
        missed = doc["queries"] - doc["hit10"]
        rows.append({
            "document": doc["document"],
            "queries": doc["queries"],
            "hit1_rate": doc["hit1"] / doc["queries"],
            "hit10_rate": doc["hit10"] / doc["queries"],
            "missed_top10": missed,
            "not_found": doc["not_found"],
            "median_rank_when_found": statistics.median(doc["ranks"]) if doc["ranks"] else None,
            "most_often_ranked_first_instead": ", ".join(
                f"{title} ({n})" for title, n in doc["beaten_by"].most_common(3)),
            "example_missed_query": doc["example_query"],
        })
    rows.sort(key=lambda row: (-row["missed_top10"], -row["not_found"], row["hit1_rate"], row["document"]))
    return rows


def report(results: list[dict], dataset: str, split: str, total: int, show_misses: int,
           qrels: dict[str, dict[str, int]], show_hard: int) -> None:
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
    if not ok:
        return

    # where the first relevant document lands
    buckets = Counter(rank_bucket(r["ranks"][0] if r["ranks"] else None) for r in ok)
    print("\nfirst relevant document at rank:")
    running = 0
    for name in [b[0] for b in RANK_BUCKETS] + ["not found"]:
        n = buckets.get(name, 0)
        if name != "not found":
            running += n
        cumulative = f"  (cumulative {running / len(ok):.1%})" if name != "not found" else ""
        print(f"  {name:<10} {n:>8} {n / len(ok):>7.1%}{cumulative}")

    # metrics per kind of query
    print_table("by query length", {b: [r for r in ok if length_bucket(r["query_words"]) == b]
                                    for b in ("1-4 words", "5-8 words", "9-12 words", "13+ words")},
                ["1-4 words", "5-8 words", "9-12 words", "13+ words"])
    print_table("by negation (not, never, no, only, n't ...)",
                {"negated": [r for r in ok if r["negation"]], "plain": [r for r in ok if not r["negation"]]},
                ["plain", "negated"])
    print_table("by numbers in the query",
                {"with number": [r for r in ok if r["has_number"]],
                 "no number": [r for r in ok if not r["has_number"]]},
                ["no number", "with number"])
    print_table("by relevant documents per query",
                {b: [r for r in ok if relevant_bucket(r["relevant"]) == b]
                 for b in ("1 relevant", "2-3 relevant", "4+ relevant")},
                ["1 relevant", "2-3 relevant", "4+ relevant"])

    # why queries miss
    not_first = [r for r in ok if r["hit1"] == 0.0]
    if not_first:
        not_top10 = [r for r in not_first if r["hit10"] == 0.0]
        print(f"\nmiss categories ({len(not_first)} queries not at rank 1, {len(not_top10)} of them "
              f"not in the top 10):")
        print(f"  {'':<20} {'not @1':>8} {'share':>7} {'not @10':>8} {'share':>7}")
        for category in MISS_CATEGORIES:
            a = sum(1 for r in not_first if r["category"] == category)
            b = sum(1 for r in not_top10 if r["category"] == category)
            if a:
                print(f"  {category:<20} {a:>8} {a / len(not_first):>7.1%} {b:>8} "
                      f"{(b / len(not_top10) if not_top10 else 0):>7.1%}")
        gaps = [r["score_gap"] for r in not_first if r.get("score_gap") is not None and r["top_score"]]
        if gaps:
            relative = [r["score_gap"] / r["top_score"] for r in not_first
                        if r.get("score_gap") is not None and r["top_score"]]
            print(f"  score gap to rank 1 when found: median {statistics.median(gaps):.1f} "
                  f"({statistics.median(relative):.1%} of the top score); "
                  f"within 5% of it: {sum(1 for x in relative if x <= 0.05) / len(relative):.1%}")

    misses = [r for r in ok if r["hit10"] == 0.0]
    if misses and show_misses:
        print(f"\nno relevant document in the top 10 (showing {min(show_misses, len(misses))} "
              f"of {len(misses)}):")
        for r in misses[:show_misses]:
            found = f"rank {r['ranks'][0]}" if r["ranks"] else "not in results"
            gap = (f", {r['score_gap']:.1f} below the top" if r.get("score_gap") is not None else "")
            print(f"  {r['query']!r}  [{r['category']}]\n"
                  f"      want {r['want_title']!r} ({found}{gap}), got {r['top']}")

    hard = hard_documents(ok)
    hard = [row for row in hard if row["missed_top10"] > 0]
    if hard and show_hard:
        print(f"\nhardest documents to find (showing {min(show_hard, len(hard))} of {len(hard)} "
              f"missed at least once):")
        print(f"  {'document':<40} {'queries':>7} {'@1':>6} {'@10':>6} {'missing':>7}  ranked first instead")
        for row in hard[:show_hard]:
            print(f"  {row['document'][:40]:<40} {row['queries']:>7} {row['hit1_rate']:>6.0%} "
                  f"{row['hit10_rate']:>6.0%} {row['not_found']:>7}  "
                  f"{row['most_often_ranked_first_instead'][:60]}")


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

    def first(record: dict) -> int | None:
        return record["ranks"][0] if record["ranks"] else None

    # how the first relevant document moved
    moves = Counter()
    for new, old in pairs:
        a, b = first(old), first(new)
        if a == 1 and b != 1:
            moves["lost rank 1"] += 1
        if a != 1 and b == 1:
            moves["gained rank 1"] += 1
        if a is not None and a <= 10 and (b is None or b > 10):
            moves["fell out of top 10"] += 1
        if (a is None or a > 10) and b is not None and b <= 10:
            moves["entered top 10"] += 1
        if a is not None and b is None:
            moves["no longer found"] += 1
        if a is None and b is not None:
            moves["newly found"] += 1
    print("  movement: " + ", ".join(f"{name} {moves[name]}" for name in (
        "gained rank 1", "lost rank 1", "entered top 10", "fell out of top 10", "newly found",
        "no longer found")))

    # rank bucket transitions (rows: baseline, columns: this run)
    names = [b[0] for b in RANK_BUCKETS] + ["not found"]
    matrix = Counter((rank_bucket(first(old)), rank_bucket(first(new))) for new, old in pairs)
    print("\n  first relevant rank, baseline (rows) -> this run (columns):")
    print("  " + f"{'':<10}" + "".join(f"{n:>10}" for n in names))
    for row in names:
        if not any(matrix[(row, col)] for col in names):
            continue
        print("  " + f"{row:<10}" + "".join(f"{matrix[(row, col)]:>10}" for col in names))

    # what kind of query regressed (only when this run carries the categories)
    if worse and "category" in worse[0][0]:
        kinds = Counter(new.get("category") or "rank 1" for new, _ in worse)
        print("\n  regressed queries by miss category now: " + ", ".join(
            f"{name} {n}" for name, n in kinds.most_common()))
        negated = sum(1 for new, _ in worse if new.get("negation"))
        print(f"  regressed queries with negation: {negated} ({negated / len(worse):.1%})")

    for title, items in (("regressed", worse), ("improved", better)):
        if items and show:
            print(f"\n{title} (showing {min(show, len(items))}):")
            for new, old in items[:show]:
                extra = ""
                if new.get("category"):
                    extra = f"  [{new['category']}] now first: {new['top'][:1]}"
                print(f"  {new['query']!r}: first relevant at {first(old)} -> {first(new)}{extra}")


# ---------------------------------------------------------------- analysis files


QUERY_COLUMNS = ("qid", "query", "category", "want", "first_relevant_rank", "first_relevant_score",
                 "top_score", "score_gap", "relevant", "query_words", "negation", "has_number")


def query_row(r: dict) -> dict:
    row = {
        "qid": r["qid"], "query": r["query"], "category": r.get("category") or "",
        "want": r["want_title"], "first_relevant_rank": r["ranks"][0] if r["ranks"] else "",
        "first_relevant_score": r.get("first_relevant_score", ""), "top_score": r.get("top_score", ""),
        "score_gap": r.get("score_gap", ""), "relevant": r["relevant"],
        "query_words": r["query_words"], "negation": int(r["negation"]),
        "has_number": int(r["has_number"]),
    }
    for i in range(TOP_KEPT):
        hit = r["top_hits"][i] if i < len(r["top_hits"]) else None
        row[f"hit{i + 1}"] = f"{hit[0]} ({hit[1]:.1f})" if hit and hit[1] is not None else (hit[0] if hit else "")
    return row


def write_tsv(path: Path, rows: list[dict], columns: list[str]) -> None:
    with open(path, "w", encoding="utf-8", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=columns, delimiter="\t", extrasaction="ignore")
        writer.writeheader()
        for row in rows:
            writer.writerow({k: ("" if v is None else v) for k, v in row.items()})


def write_analysis(directory: Path, results: list[dict], summary: str) -> None:
    directory.mkdir(parents=True, exist_ok=True)
    ok = [r for r in results if "error" not in r]
    columns = list(QUERY_COLUMNS) + [f"hit{i + 1}" for i in range(TOP_KEPT)]

    def first(r: dict) -> int:
        return r["ranks"][0] if r["ranks"] else 10**9

    not_found = [r for r in ok if not r["ranks"]]
    ranked_low = sorted((r for r in ok if r["ranks"] and r["ranks"][0] > 10), key=first)
    outranked = sorted((r for r in ok if r["ranks"] and 1 < r["ranks"][0] <= 10), key=first)
    write_tsv(directory / "not_found.tsv", [query_row(r) for r in not_found], columns)
    write_tsv(directory / "ranked_low.tsv", [query_row(r) for r in ranked_low], columns)
    write_tsv(directory / "outranked.tsv", [query_row(r) for r in outranked], columns)
    hard = hard_documents(ok)
    write_tsv(directory / "hard_documents.tsv", hard, list(hard[0].keys()) if hard else ["document"])
    (directory / "summary.txt").write_text(summary, encoding="utf-8")
    print(f"analysis: {len(not_found)} not found, {len(ranked_low)} ranked below 10, "
          f"{len(outranked)} at ranks 2-10, {len(hard)} documents -> {directory}", file=sys.stderr)


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
    parser.add_argument("--hard", type=int, default=15, help="hardest documents to print")
    parser.add_argument("--analysis-dir",
                        help="write not_found.tsv, ranked_low.tsv, outranked.tsv, hard_documents.tsv "
                             "and summary.txt into this folder")
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

    # print the report and keep a copy for summary.txt
    captured = io.StringIO()

    class Tee(io.TextIOBase):
        def write(self, text: str) -> int:
            captured.write(text)
            return sys.__stdout__.write(text)

        def flush(self) -> None:
            sys.__stdout__.flush()

    with contextlib.redirect_stdout(Tee()):
        report(results, dataset, args.split, total, args.misses, qrels, args.hard)
        if args.baseline:
            baseline = json.loads(Path(args.baseline).read_text(encoding="utf-8"))
            compare(results, baseline, settings, args.show)

    if args.analysis_dir:
        write_analysis(Path(args.analysis_dir), results, captured.getvalue())


if __name__ == "__main__":
    main()