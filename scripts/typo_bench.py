#!/usr/bin/env python3
"""Typo benchmark for Corelamo on FEVER: how well do search and did-you-mean survive common
misspellings?

The misspellings are real ones, not generated:

  wikipedia   Wikipedia's "Lists of common misspellings/For machines" (about 4,300 pairs
              collected from typos editors actually fixed on Wikipedia, which FEVER comes from)
  birkbeck    the Birkbeck spelling error corpus as published with Peter Norvig's spelling
              corrector (spell-testset1/2: misspellings written by people in tests)

`build` asks the server how many documents contain each correct word and keeps the most popular
ones (top 1,000 pairs by default), so every case is about a word the database actually holds.

Two tests read that list:

  words    did-you-mean on each misspelling alone: is the correct word the first suggestion?
  claims   FEVER dev claims with one popular word replaced by its misspelling, searched as plain
           text the way a user types: is the evidence article still in the top 10? Every claim
           is also searched unchanged, so the drop caused by the typo is measured, not guessed.

    python3 typo_bench.py build  --database fever                       # once, server running
    python3 typo_bench.py words  --database fever --save typo-words-base.json
    python3 typo_bench.py claims --database fever --save typo-claims-base.json
    # ...change the fuzzy code, restart the server, then:
    python3 typo_bench.py words  --database fever --save typo-words-new.json --baseline typo-words-base.json
    python3 typo_bench.py claims --database fever --save typo-claims-new.json --baseline typo-claims-base.json

`words` compares at the level of index terms: the expected suggestion is the correct word as the
analyzer stems it, computed with the same Snowball English stemmer (`pip install snowballstemmer`;
rust-stemmers implements the same algorithm). Everything else uses the standard library only.
"""

from __future__ import annotations

import argparse
import json
import random
import re
import statistics
import sys
import time
import unicodedata
import urllib.error
import urllib.request
from collections import Counter, defaultdict
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
CACHE_DIR = SCRIPT_DIR / "typos"
LIST_FILE = SCRIPT_DIR / "typos.tsv"

SOURCES = {
    "wikipedia": "https://en.wikipedia.org/w/index.php?title=Wikipedia:Lists_of_common_misspellings/For_machines&action=raw",
    "birkbeck1": "https://norvig.com/spell-testset1.txt",
    "birkbeck2": "https://norvig.com/spell-testset2.txt",
}
WORD = re.compile(r"^[a-z]+$")
TOKEN = re.compile(r"[A-Za-z]+")
LENGTH_BUCKETS = [(1, 4, "1-4"), (5, 6, "5-6"), (7, 9, "7-9"), (10, 99, "10+")]
# did-you-mean leaves a word alone when the analyzer removes more than this many characters from
# it (MAX_ANALYSIS_LOSS in shard_manager.rs); words cases that hit the rule are reported apart
MAX_ANALYSIS_LOSS = 1


# ---------------------------------------------------------------- lists


def fetch(name: str, url: str) -> str:
    path = CACHE_DIR / f"{name}.txt"
    if path.exists():
        return path.read_text(encoding="utf-8")
    CACHE_DIR.mkdir(parents=True, exist_ok=True)
    print(f"downloading {url}", file=sys.stderr)
    request = urllib.request.Request(url, headers={"User-Agent": "corelamo-typo-bench/1.0"})
    with urllib.request.urlopen(request, timeout=60) as response:
        text = response.read().decode("utf-8", errors="replace")
    path.write_text(text, encoding="utf-8")
    return text


def parse_wikipedia(text: str) -> list[tuple[str, str]]:
    """`typo->correct` lines; entries with several corrections ("a, b") are ambiguous and skipped."""
    pairs = []
    for line in text.splitlines():
        if "->" not in line:
            continue
        typo, correct = (part.strip().lower() for part in line.split("->", 1))
        if "," in correct or not WORD.match(typo) or not WORD.match(correct):
            continue
        pairs.append((typo, correct))
    return pairs


def parse_birkbeck(text: str) -> list[tuple[str, str]]:
    """`correct: typo typo ...` lines."""
    pairs = []
    for line in text.splitlines():
        if ":" not in line:
            continue
        correct, typos = line.split(":", 1)
        correct = correct.strip().lower()
        if not WORD.match(correct):
            continue
        for typo in typos.split():
            typo = typo.strip().lower()
            if WORD.match(typo):
                pairs.append((typo, correct))
    return pairs


def all_pairs() -> dict[tuple[str, str], str]:
    """(typo, correct) -> source. A typo listed with two different corrections is dropped."""
    found: dict[tuple[str, str], str] = {}
    for name, url in SOURCES.items():
        text = fetch(name, url)
        parsed = parse_wikipedia(text) if name == "wikipedia" else parse_birkbeck(text)
        source = "wikipedia" if name == "wikipedia" else "birkbeck"
        for typo, correct in parsed:
            if typo != correct:
                found.setdefault((typo, correct), source)
    corrections: dict[str, set[str]] = defaultdict(set)
    for typo, correct in found:
        corrections[typo].add(correct)
    return {pair: source for pair, source in found.items() if len(corrections[pair[0]]) == 1}


# ---------------------------------------------------------------- server


def find_key(node: object, key: str) -> object:
    """The first value stored under `key` anywhere in the response."""
    if isinstance(node, dict):
        if key in node:
            return node[key]
        for value in node.values():
            found = find_key(value, key)
            if found is not None:
                return found
    elif isinstance(node, list):
        for item in node:
            found = find_key(item, key)
            if found is not None:
                return found
    return None


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
    def __init__(self, base_url: str, database: str, timeout: float):
        self.base = f"{base_url.rstrip('/')}/api/databases/{database}"
        self.timeout = timeout

    def post(self, endpoint: str, body: dict) -> dict:
        request = urllib.request.Request(
            f"{self.base}/{endpoint}",
            data=json.dumps(body).encode(),
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        with urllib.request.urlopen(request, timeout=self.timeout) as response:
            return json.load(response)

    def documents(self, words: list[str], batch: int = 500) -> dict[str, int]:
        """Documents containing each word, as the server analyzes it (info-words)."""
        out: dict[str, int] = {}
        for start in range(0, len(words), batch):
            chunk = words[start : start + batch]
            stats = find_key(self.post("info-words", {"words": chunk}), "words") or []
            for word, entry in zip(chunk, stats):
                out[word] = int(entry.get("documents", 0)) if isinstance(entry, dict) else 0
            print(f"\r  info-words {min(start + batch, len(words))}/{len(words)}", end="", file=sys.stderr)
        print(file=sys.stderr)
        return out

    def suggestions(self, value: str, count: int) -> tuple[list[dict], float | None]:
        payload = self.post("did-you-mean", {"value": value, "did_you_mean_count": count})
        words = find_key(payload, "suggestions")
        return (words if isinstance(words, list) else []), server_ms(payload)

    def search(self, query: str, docs: int, field: str) -> tuple[list[str], float | None]:
        payload = self.post("search", {"query": query, "docs": docs})
        return hit_titles(payload, field), server_ms(payload)


# ---------------------------------------------------------------- helpers


def normalize_title(title: str) -> str:
    for code, char in (("-LRB-", "("), ("-RRB-", ")"), ("-LSB-", "["), ("-RSB-", "]"), ("-COLON-", ":")):
        title = title.replace(code, char)
    return unicodedata.normalize("NFC", title.replace("_", " ")).strip().casefold()


def length_bucket(word: str) -> str:
    for low, high, name in LENGTH_BUCKETS:
        if low <= len(word) <= high:
            return name
    return "10+"


def percentile(values: list[float], fraction: float) -> float:
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(fraction * len(ordered)))] if ordered else 0.0


def share(part: int, whole: int) -> str:
    return f"{100.0 * part / whole:5.1f}%" if whole else "    - "


def load_list(path: Path) -> list[dict]:
    if not path.exists():
        sys.exit(f"{path} not found; run `typo_bench.py build` first")
    rows = []
    with open(path, encoding="utf-8") as handle:
        header = next(handle).rstrip("\n").split("\t")
        for line in handle:
            row = dict(zip(header, line.rstrip("\n").split("\t")))
            row["correct_docs"] = int(row["correct_docs"])
            row["typo_docs"] = int(row["typo_docs"])
            rows.append(row)
    return rows


def stemmer():
    try:
        import snowballstemmer  # type: ignore
    except ImportError:
        sys.exit("the words test needs the Snowball stemmer: pip install snowballstemmer")
    return snowballstemmer.stemmer("english")


def save_results(path: str | None, payload: dict) -> None:
    if path:
        out = Path(path)
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(json.dumps(payload, indent=1), encoding="utf-8")
        print(f"saved {out}", file=sys.stderr)


def compare(current: list[dict], baseline: list[dict], key: tuple[str, ...], rank: str, show: int) -> None:
    before = {tuple(r[k] for k in key): r for r in baseline if "error" not in r}
    better, worse = [], []
    for record in current:
        old = before.get(tuple(record[k] for k in key))
        if old is None or "error" in record:
            continue
        new_rank, old_rank = record.get(rank) or 10**9, old.get(rank) or 10**9
        if new_rank < old_rank:
            better.append((record, old.get(rank)))
        elif new_rank > old_rank:
            worse.append((record, old.get(rank)))
    print(f"\nvs baseline: {len(better)} improved, {len(worse)} regressed")
    for title, items in (("regressed", worse), ("improved", better)):
        if items:
            print(f"\n{title} (showing {min(show, len(items))}):")
            for record, old_rank in items[:show]:
                label = " / ".join(str(record[k]) for k in key)
                print(f"  {label}: rank {old_rank} -> {record.get(rank)}")


# ---------------------------------------------------------------- build


def command_build(args: argparse.Namespace) -> None:
    pairs = all_pairs()
    print(f"  {len(pairs):,} unambiguous misspelling pairs", file=sys.stderr)
    client = Client(args.url, args.database, args.timeout)
    words = sorted({word for pair in pairs for word in pair})
    documents = client.documents(words)

    by_word: dict[str, list[tuple[str, str]]] = defaultdict(list)
    for (typo, correct), source in sorted(pairs.items()):
        if documents.get(correct, 0) > 0:
            by_word[correct].append((typo, source))
    # most popular correct words first; within a word, real human errors (birkbeck) first
    ranked = sorted(by_word, key=lambda word: (-documents[word], word))
    rows = []
    for correct in ranked:
        typos = sorted(by_word[correct], key=lambda item: (item[1] != "birkbeck", item[0]))
        for typo, source in typos[: args.per_word]:
            rows.append((typo, correct, documents[correct], documents.get(typo, 0), source))
            if len(rows) >= args.top:
                break
        if len(rows) >= args.top:
            break

    out = Path(args.list)
    with open(out, "w", encoding="utf-8") as handle:
        handle.write("rank\ttypo\tcorrect\tcorrect_docs\ttypo_docs\tsource\n")
        for rank, (typo, correct, correct_docs, typo_docs, source) in enumerate(rows, start=1):
            handle.write(f"{rank}\t{typo}\t{correct}\t{correct_docs}\t{typo_docs}\t{source}\n")
    real_words = sum(1 for row in rows if row[3] > 0)
    print(
        f"wrote {out}: {len(rows):,} pairs for {len({row[1] for row in rows}):,} words; "
        f"{real_words:,} misspellings are themselves in the database",
        file=sys.stderr,
    )


# ---------------------------------------------------------------- words


def command_words(args: argparse.Namespace) -> None:
    rows = load_list(Path(args.list))[: args.limit or None]
    stem = stemmer()
    client = Client(args.url, args.database, args.timeout)
    results = []
    started = time.monotonic()
    for index, row in enumerate(rows, start=1):
        typo, correct = row["typo"], row["correct"]
        record = {"typo": typo, "correct": correct, "expected": stem.stemWord(correct),
                  "typo_in_db": row["typo_docs"] > 0, "length": length_bucket(typo),
                  # the analyzer alone already turns these into the right term
                  "same_stem": stem.stemWord(typo) == stem.stemWord(correct)}
        try:
            words, took = client.suggestions(typo, args.count)
            record["server_ms"] = took
            suggested = []
            if words and isinstance(words[0], dict):
                suggested = [s for s in words[0].get("suggestions", []) if isinstance(s, str)]
            record["suggestions"] = suggested
            record["rank"] = suggested.index(record["expected"]) + 1 if record["expected"] in suggested else None
            if not words and record["same_stem"]:
                # nothing to correct: the analyzer already turns the typo into the right term
                record["rank"] = 1
                record["outcome"] = "fixed_by_stemming"
            elif not words:
                stemmed = stem.stemWord(typo)
                lost = len(typo) - len(stemmed)
                record["outcome"] = "skipped_by_loss_rule" if lost > MAX_ANALYSIS_LOSS else "no_suggestion"
            elif record["rank"] == 1:
                record["outcome"] = "first"
            elif record["rank"]:
                record["outcome"] = "lower"
            else:
                record["outcome"] = "missing"
        except (urllib.error.URLError, TimeoutError, ValueError) as err:
            record["error"] = str(err)
        results.append(record)
        if index % 50 == 0 or index == len(rows):
            print(f"\r  {index}/{len(rows)} misspellings", end="", file=sys.stderr)
    print(f"\n  done in {time.monotonic() - started:.1f}s", file=sys.stderr)

    save_results(args.save, {"test": "words", "count": args.count, "results": results})
    report_words(results, args.misses)
    if args.baseline:
        baseline = json.loads(Path(args.baseline).read_text(encoding="utf-8"))["results"]
        compare(results, baseline, ("typo", "correct"), "rank", args.show)


def report_words(results: list[dict], misses: int) -> None:
    ok = [r for r in results if "error" not in r]
    errors = len(results) - len(ok)
    print(f"\n{len(results)} misspellings, {errors} errors\n")
    print(f"{'group':<26}{'cases':>7}{'first':>9}{'top 3':>9}{'listed':>9}{'none':>9}")

    def line(name: str, group: list[dict]) -> None:
        n = len(group)
        first = sum(1 for r in group if r.get("rank") == 1)
        top3 = sum(1 for r in group if r.get("rank") and r["rank"] <= 3)
        listed = sum(1 for r in group if r.get("rank"))
        print(f"{name:<26}{n:>7}{share(first, n):>9}{share(top3, n):>9}{share(listed, n):>9}{share(n - listed, n):>9}")

    line("all", ok)
    for _, _, bucket in LENGTH_BUCKETS:
        line(f"  {bucket} letters", [r for r in ok if r["length"] == bucket])
    line("  typo not in db", [r for r in ok if not r["typo_in_db"]])
    line("  typo is in db", [r for r in ok if r["typo_in_db"]])
    line("  stems to correct term", [r for r in ok if r["same_stem"]])
    line("  real corrections", [r for r in ok if not r["same_stem"]])

    outcomes = Counter(r["outcome"] for r in ok)
    print("\noutcomes: " + ", ".join(f"{name} {count}" for name, count in outcomes.most_common()))
    if outcomes.get("skipped_by_loss_rule"):
        print("  skipped_by_loss_rule: did-you-mean returned nothing and stemming shortened the "
              "misspelling by more than 1 letter (MAX_ANALYSIS_LOSS skips these words)")
    if outcomes.get("fixed_by_stemming"):
        print("  fixed_by_stemming: nothing returned, and none needed: the typo stems to the right term")
    times = [r["server_ms"] for r in ok if r.get("server_ms") is not None]
    if times:
        print(f"server time: p50 {percentile(times, 0.5):.2f} ms, p95 {percentile(times, 0.95):.2f} ms, "
              f"max {max(times):.2f} ms")

    wrong = [r for r in ok if r.get("rank") != 1 and r["outcome"] not in ("skipped_by_loss_rule",)]
    if wrong and misses:
        print(f"\nnot first (showing {min(misses, len(wrong))}):")
        for r in wrong[:misses]:
            got = ", ".join(r.get("suggestions", [])[:3]) or "-"
            print(f"  {r['typo']:<18} want {r['expected']:<16} got {got}")


# ---------------------------------------------------------------- claims


def load_claims(fever_dir: Path, split: str) -> tuple[dict[str, str], dict[str, set[str]]]:
    qrels: dict[str, set[str]] = defaultdict(set)
    with open(fever_dir / "qrels" / f"{split}.tsv", encoding="utf-8") as handle:
        next(handle, None)
        for line in handle:
            parts = line.rstrip("\n").split("\t")
            if len(parts) >= 3 and parts[2] != "0":
                qrels[parts[0]].add(normalize_title(parts[1]))
    claims: dict[str, str] = {}
    with open(fever_dir / "queries.jsonl", encoding="utf-8") as handle:
        for line in handle:
            entry = json.loads(line)
            if entry["_id"] in qrels:
                claims[entry["_id"]] = entry["text"]
    return claims, qrels


def with_case(typo: str, original: str) -> str:
    if original.isupper() and len(original) > 1:
        return typo.upper()
    if original[0].isupper():
        return typo[0].upper() + typo[1:]
    return typo


def build_cases(claims: dict[str, str], qrels: dict[str, set[str]], rows: list[dict], limit: int, seed: int) -> list[dict]:
    """One misspelled word per claim. Claims are taken so that as many different words as
    possible are covered before any word is used twice."""
    typos: dict[str, list[dict]] = defaultdict(list)
    for row in rows:
        typos[row["correct"]].append(row)
    rng = random.Random(seed)
    ids = sorted(claims)
    rng.shuffle(ids)

    options: list[tuple[str, list[re.Match]]] = []
    for qid in ids:
        matches = [m for m in TOKEN.finditer(claims[qid]) if m.group(0).lower() in typos]
        if matches:
            options.append((qid, matches))

    cases: list[dict] = []
    used_words: Counter = Counter()
    used_claims: set[str] = set()
    rounds = 0
    while True:
        # round r takes words used fewer than r times, so popular words do not crowd out the rest
        rounds += 1
        progress = False
        for qid, matches in options:
            if len(cases) >= limit:
                return cases
            if qid in used_claims:
                continue
            fresh = [m for m in matches if used_words[m.group(0).lower()] < rounds]
            if not fresh:
                continue
            match = rng.choice(fresh)
            word = match.group(0)
            row = rng.choice(typos[word.lower()])
            text = claims[qid]
            typo_text = text[: match.start()] + with_case(row["typo"], word) + text[match.end():]
            cases.append({
                "qid": qid, "query": text, "typo_query": typo_text, "word": word.lower(),
                "typo": row["typo"], "typo_in_db": row["typo_docs"] > 0, "targets": sorted(qrels[qid]),
            })
            used_words[word.lower()] += 1
            used_claims.add(qid)
            progress = True
        if not progress:
            break
    return cases


def rank_in(titles: list[str], targets: set[str]) -> int | None:
    for position, title in enumerate(titles, start=1):
        if normalize_title(title) in targets:
            return position
    return None


def default_fever_dir() -> Path:
    for candidate in (SCRIPT_DIR / "fever", SCRIPT_DIR.parent / "fever"):
        if (candidate / "queries.jsonl").exists():
            return candidate
    return SCRIPT_DIR.parent / "fever"


def command_claims(args: argparse.Namespace) -> None:
    rows = load_list(Path(args.list))
    claims, qrels = load_claims(Path(args.fever_dir), args.split)
    cases = build_cases(claims, qrels, rows, args.limit, args.seed)
    print(f"  {len(cases)} claims with a misspelled word ({len({c['word'] for c in cases})} different words)",
          file=sys.stderr)
    client = Client(args.url, args.database, args.timeout)
    started = time.monotonic()
    for index, case in enumerate(cases, start=1):
        targets = set(case["targets"])
        try:
            titles, took = client.search(case["query"], args.docs, args.title_field)
            case["rank"], case["server_ms"] = rank_in(titles, targets), took
            titles, took = client.search(case["typo_query"], args.docs, args.title_field)
            case["typo_rank"], case["typo_server_ms"] = rank_in(titles, targets), took
            case["typo_top"] = titles[:3]
        except (urllib.error.URLError, TimeoutError, ValueError) as err:
            case["error"] = str(err)
        if index % 25 == 0 or index == len(cases):
            print(f"\r  {index}/{len(cases)} claims", end="", file=sys.stderr)
    print(f"\n  done in {time.monotonic() - started:.1f}s", file=sys.stderr)

    save_results(args.save, {"test": "claims", "docs": args.docs, "results": cases})
    report_claims(cases, args.misses)
    if args.baseline:
        baseline = json.loads(Path(args.baseline).read_text(encoding="utf-8"))["results"]
        compare(cases, baseline, ("qid", "typo_query"), "typo_rank", args.show)


def report_claims(cases: list[dict], misses: int) -> None:
    ok = [c for c in cases if "error" not in c]
    print(f"\n{len(cases)} claims, {len(cases) - len(ok)} errors\n")
    print(f"{'group':<22}{'cases':>7}{'orig @1':>10}{'typo @1':>10}{'orig @10':>10}{'typo @10':>10}{'kept':>9}")

    def line(name: str, group: list[dict]) -> None:
        n = len(group)
        o1 = sum(1 for c in group if c.get("rank") == 1)
        t1 = sum(1 for c in group if c.get("typo_rank") == 1)
        o10 = [c for c in group if c.get("rank") and c["rank"] <= 10]
        t10 = sum(1 for c in group if c.get("typo_rank") and c["typo_rank"] <= 10)
        kept = sum(1 for c in o10 if c.get("typo_rank") and c["typo_rank"] <= 10)
        print(f"{name:<22}{n:>7}{share(o1, n):>10}{share(t1, n):>10}{share(len(o10), n):>10}"
              f"{share(t10, n):>10}{share(kept, len(o10)):>9}")

    line("all", ok)
    for _, _, bucket in LENGTH_BUCKETS:
        line(f"  {bucket} letters", [c for c in ok if length_bucket(c["typo"]) == bucket])
    line("  typo not in db", [c for c in ok if not c["typo_in_db"]])
    line("  typo is in db", [c for c in ok if c["typo_in_db"]])
    print("\nkept: of the claims whose article was in the top 10 unchanged, the share still in the "
          "top 10 with the typo")
    if any(c["typo_in_db"] for c in ok):
        print("typo is in db: the misspelling exists in some document, so the query has no unknown "
              "word and the fuzzy tier may never run")

    for label, key in (("unchanged", "server_ms"), ("with typo", "typo_server_ms")):
        times = [c[key] for c in ok if c.get(key) is not None]
        if times:
            print(f"server time {label:<10}: p50 {percentile(times, 0.5):.2f} ms, "
                  f"p95 {percentile(times, 0.95):.2f} ms, max {max(times):.2f} ms")

    lost = [c for c in ok if c.get("rank") and c["rank"] <= 10 and not (c.get("typo_rank") and c["typo_rank"] <= 10)]
    if lost and misses:
        print(f"\nlost to the typo (showing {min(misses, len(lost))}):")
        for c in lost[:misses]:
            print(f"  {c['word']} -> {c['typo']}: {c['typo_query']!r}")
            print(f"      want {c['targets'][0]!r} (rank {c['rank']} -> {c.get('typo_rank')}), "
                  f"got {c.get('typo_top', [])}")


# ---------------------------------------------------------------- cli


def main() -> None:
    # options every command takes, written after the command name: `build --database fever`
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--url", default="http://127.0.0.1:6006")
    common.add_argument("--database", default="fever")
    common.add_argument("--timeout", type=float, default=30.0)
    common.add_argument("--list", default=str(LIST_FILE), help="the pair list build writes and the tests read")

    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)

    build = sub.add_parser("build", parents=[common], help="download the misspelling lists and keep the most popular pairs")
    build.add_argument("--top", type=int, default=1000, help="pairs to keep")
    build.add_argument("--per-word", type=int, default=3, help="most misspellings kept for one correct word")

    words = sub.add_parser("words", parents=[common], help="did-you-mean on each misspelling")
    words.add_argument("--count", type=int, default=5, help="suggestions asked per word")
    words.add_argument("--limit", type=int, default=0, help="test only the first N pairs (0 = all)")

    claims = sub.add_parser("claims", parents=[common], help="FEVER claims searched with one word misspelled")
    claims.add_argument("--fever-dir", default=str(default_fever_dir()))
    claims.add_argument("--split", default="dev", choices=["dev", "test", "train"])
    claims.add_argument("--limit", type=int, default=1000, help="claims to test")
    claims.add_argument("--docs", type=int, default=10, help="results read per search")
    claims.add_argument("--title-field", default="title")
    claims.add_argument("--seed", type=int, default=20261023)

    for command in (words, claims):
        command.add_argument("--save", help="write every result to this JSON file")
        command.add_argument("--baseline", help="a saved JSON file to compare with")
        command.add_argument("--misses", type=int, default=20, help="failures to print")
        command.add_argument("--show", type=int, default=20, help="changes vs baseline to print")

    args = parser.parse_args()
    {"build": command_build, "words": command_words, "claims": command_claims}[args.command](args)


if __name__ == "__main__":
    main()