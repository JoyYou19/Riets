#!/usr/bin/env python3
"""Which top 10 is more accurate, Corelamo's or Wikipedia's? An LLM grades every result.

compare_wiki.py measures how much the two lists agree; this script measures how good each one is.
For every query it pools both top 10s, has DeepSeek grade each pooled title on its own (blind:
shuffled, with no word on which engine found it), and scores both lists with nDCG@10 and P@3 from
the same grades, the way TREC judges systems.

    python3 compare_wiki.py --offline --save beir_test/wiki-compare.json
    DEEPSEEK_API_KEY=... python3 judge_wiki.py beir_test/wiki-compare.json
    DEEPSEEK_API_KEY=... python3 judge_wiki.py beir_test/wiki-compare.json --only typo --show 10

Before trusting the judge, check it against FEVER's own answers (BEIR files):

    DEEPSEEK_API_KEY=... python3 judge_wiki.py --validate fever/queries.jsonl fever/qrels/train.tsv

Grades: 3 the page the query is after, 2 closely related, 1 loosely related, 0 unrelated. Each
title is graded with the start of its article, read from Corelamo by exact title. A Wikipedia
title Corelamo does not have (an article newer than the 2017 dump) is graded on its title alone
and counted apart: Corelamo could never have returned it.

Queries are left out when either list is empty: there is nothing to weigh one engine against.

Every grade is cached (judge_cache.json) by query and title, so a rerun after a ranking change
only pays for titles it has not seen. The key is read from DEEPSEEK_API_KEY, never written down.
One request at a time; the first error stops the run with everything so far kept.

Only the standard library is used.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import random
import sys
import time
import unicodedata
import urllib.error
import urllib.request
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
CACHE_FILE = SCRIPT_DIR / "judge_cache.json"
DEEPSEEK_URL = "https://api.deepseek.com/chat/completions"
DEEPSEEK_MODEL = "deepseek-chat"

# Characters of article text shown to the judge per title.
SNIPPET_CHARS = 400
# Seconds between two judge requests.
MIN_INTERVAL = 0.5
# Judge requests one run may make (one per query); the rest wait for the next run.
MAX_REQUESTS_PER_RUN = 500
# The same seed shuffles a query's titles the same way on every run.
SHUFFLE_SEED = 17

PROMPT = """You grade search results for a Wikipedia search engine.

Query: {query}

The query may contain typos or be phrased as a claim or a question. Grade every result for how
well it answers what the person was looking for, as they most likely meant it:
  3 = the page the query is after
  2 = closely related, useful for this query
  1 = loosely related
  0 = unrelated
Grade each result on its own; several can share a grade.

Results:
{results}

Answer with JSON only: {{"grades": {{"<number>": <grade>, ...}}}} with one entry per result."""


class Stop(Exception):
    """The judge or Corelamo failed: end the run, keep the cache."""


class Skip(Exception):
    """One query could not be graded completely: leave it out, go on with the next."""


# --------------------------------------------------------------------------------------------
# files
# --------------------------------------------------------------------------------------------

def load_json(path: Path, default):
    try:
        return json.loads(path.read_text())
    except FileNotFoundError:
        return default
    except (OSError, json.JSONDecodeError) as err:
        sys.exit(f"[error] {path} is unreadable ({err}); fix or delete it")


def save_json(path: Path, value) -> None:
    """Writes to a temporary file first, so an interrupted run never leaves half a file."""
    partial = path.with_suffix(path.suffix + ".tmp")
    partial.write_text(json.dumps(value, indent=1, ensure_ascii=False))
    partial.replace(path)


def normalize(title: str) -> str:
    """Titles compared the way compare_wiki.py and beir_eval.py compare them."""
    for code, char in (("-LRB-", "("), ("-RRB-", ")"), ("-LSB-", "["), ("-RSB-", "]"),
                       ("-COLON-", ":")):
        title = title.replace(code, char)
    return unicodedata.normalize("NFC", title.replace("_", " ")).strip().casefold()


# --------------------------------------------------------------------------------------------
# Corelamo: the start of an article, by exact title
# --------------------------------------------------------------------------------------------

class Corelamo:
    def __init__(self, args: argparse.Namespace) -> None:
        self.url = f"{args.url.rstrip('/')}/api/databases/{args.database}/search"
        self.title_field = args.title_field
        self.text_field = args.text_field
        self.timeout = args.timeout
        self.texts: dict[str, str | None] = {}

    def _post(self, body: dict) -> dict:
        request = urllib.request.Request(
            self.url, data=json.dumps(body).encode(), method="POST",
            headers={"Content-Type": "application/json"},
        )
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                payload = json.load(response)
        except (urllib.error.URLError, TimeoutError, ValueError) as err:
            raise Stop(f"corelamo: {err}") from err
        return payload if isinstance(payload, dict) else {}

    def text_of(self, title: str) -> str | None:
        """The start of the article named `title`, or None when the corpus has no such title."""
        key = normalize(title)
        if key in self.texts:
            return self.texts[key]
        # FEVER's ids write the title with underscores and bracket codes; ask for it as written
        readable = title.replace("_", " ")
        for code, char in (("-LRB-", "("), ("-RRB-", ")"), ("-LSB-", "["), ("-RSB-", "]"),
                           ("-COLON-", ":")):
            readable = readable.replace(code, char)
        payload = self._post({
            "query": {"exact": readable},
            "search_fields": [self.title_field],
            "docs": 5,
            "return_fields": {self.title_field: True, self.text_field: True},
        })
        found = None
        for hit in payload.get("data") or []:
            data = hit.get("data") or {}
            if normalize(str(data.get(self.title_field, ""))) == key:
                text = " ".join(str(data.get(self.text_field, "")).split())
                found = text[:SNIPPET_CHARS]
                break
        self.texts[key] = found
        return found

    def top_titles(self, query: str, k: int) -> list[str]:
        payload = self._post({"query": query, "docs": k,
                              "return_fields": {self.title_field: True}})
        return [str((hit.get("data") or {}).get(self.title_field, hit.get("id")))
                for hit in payload.get("data") or []]


# --------------------------------------------------------------------------------------------
# the judge
# --------------------------------------------------------------------------------------------

class Judge:
    def __init__(self, key: str, timeout: float) -> None:
        self.key = key
        self.timeout = timeout
        self.cache: dict = load_json(CACHE_FILE, {})
        self.made = 0
        self.last = 0.0

    def grades(self, query: str, titles: list[str], corelamo: Corelamo) -> dict[str, int]:
        """A grade for every title (by normalized title), asking only for the uncached ones."""
        graded = self.cache.setdefault(query, {})
        # one title per page: FEVER's ids ("Papua_New_Guinea") and Corelamo's titles ("Papua New
        # Guinea") name the same page and must not be sent, or graded, twice
        unique: dict[str, str] = {}
        for title in titles:
            unique.setdefault(normalize(title), title)
        # a title the judge skipped is asked about once more, on its own
        for _ in range(2):
            todo = [title for key, title in unique.items() if key not in graded]
            if not todo:
                break
            graded.update(self._ask(query, todo, corelamo))
            save_json(CACHE_FILE, self.cache)
        missing = [title for key, title in unique.items() if key not in graded]
        if missing:
            raise Skip(f"no grade for {len(missing)} result(s) of {query!r}")
        return {key: graded[key] for key in unique}

    def _ask(self, query: str, titles: list[str], corelamo: Corelamo) -> dict[str, int]:
        if self.made >= MAX_REQUESTS_PER_RUN:
            raise Stop(f"reached {MAX_REQUESTS_PER_RUN} judge requests this run")
        # blind: a fixed shuffle per query, no word on which engine found a title
        order = sorted(titles, key=normalize)
        random.Random(f"{SHUFFLE_SEED}:{query}").shuffle(order)
        lines = []
        for number, title in enumerate(order, start=1):
            text = corelamo.text_of(title)
            about = text if text else "(no article text available)"
            lines.append(f"{number}. {title}\n   {about}")
        prompt = PROMPT.format(query=query, results="\n".join(lines))

        since = time.time() - self.last
        if since < MIN_INTERVAL:
            time.sleep(MIN_INTERVAL - since)
        body = json.dumps({
            "model": DEEPSEEK_MODEL,
            "messages": [{"role": "user", "content": prompt}],
            "temperature": 0,
            "response_format": {"type": "json_object"},
        }).encode()
        request = urllib.request.Request(
            DEEPSEEK_URL, data=body, method="POST",
            headers={"Content-Type": "application/json", "Authorization": f"Bearer {self.key}"},
        )
        self.made += 1
        self.last = time.time()
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                payload = json.load(response)
            content = payload["choices"][0]["message"]["content"]
            answer = json.loads(content).get("grades") or {}
        except urllib.error.HTTPError as err:
            raise Stop(f"deepseek: HTTP {err.code} {err.read()[:200]!r}") from err
        except (urllib.error.URLError, TimeoutError, KeyError, IndexError, ValueError,
                AttributeError) as err:
            raise Stop(f"deepseek: unusable answer ({err})") from err

        # what the judge graded; a skipped result is asked about again by `grades`
        out: dict[str, int] = {}
        for number, title in enumerate(order, start=1):
            grade = answer.get(str(number))
            if isinstance(grade, (int, float)) and 0 <= grade <= 3:
                out[normalize(title)] = int(grade)
        return out


# --------------------------------------------------------------------------------------------
# scores
# --------------------------------------------------------------------------------------------

def dcg(gains: list[int]) -> float:
    return sum((2 ** g - 1) / math.log2(i + 2) for i, g in enumerate(gains))


def ndcg(ranked: list[str], grades: dict[str, int], k: int = 10) -> float:
    """nDCG@k of `ranked`, the ideal order taken from every graded title of the query (pooled)."""
    ideal = dcg(sorted(grades.values(), reverse=True)[:k])
    if ideal == 0:
        return 0.0
    return dcg([grades.get(normalize(t), 0) for t in ranked[:k]]) / ideal


def precision(ranked: list[str], grades: dict[str, int], k: int = 3, at_least: int = 2) -> float:
    top = ranked[:k]
    return sum(grades.get(normalize(t), 0) >= at_least for t in top) / k if top else 0.0


def mean(values: list[float]) -> float:
    return sum(values) / len(values) if values else 0.0


# --------------------------------------------------------------------------------------------
# modes
# --------------------------------------------------------------------------------------------

def judge_compare(args: argparse.Namespace, judge: Judge, corelamo: Corelamo) -> None:
    rows = load_json(Path(args.compared), None)
    if not isinstance(rows, list):
        sys.exit(f"[error] {args.compared} is not a compare_wiki.py --save file")
    if args.only:
        rows = [r for r in rows if r.get("kind") == args.only]

    results: list[dict] = []
    left_out = 0
    try:
        for row in rows:
            ours = [str(t) for t in row.get("corelamo") or []][:10]
            theirs = [str(t) for t in row.get("wikipedia") or []][:10]
            # nothing to weigh one engine against
            if not ours or not theirs:
                left_out += 1
                continue
            query = str(row["query"])
            try:
                grades = judge.grades(query, ours + theirs, corelamo)
            except Skip as skip:
                print(f"[judge] skipped: {skip}", file=sys.stderr)
                left_out += 1
                continue
            # Wikipedia titles Corelamo cannot return: graded, but counted apart
            not_in_corpus = [t for t in theirs if corelamo.text_of(t) is None]
            results.append({
                "kind": row.get("kind", "?"), "query": query,
                "corelamo": ours, "wikipedia": theirs, "grades": grades,
                "ndcg_ours": ndcg(ours, grades), "ndcg_wiki": ndcg(theirs, grades),
                "p3_ours": precision(ours, grades), "p3_wiki": precision(theirs, grades),
                "wiki_not_in_corpus": not_in_corpus,
                "wiki_good_not_in_corpus": sum(
                    grades.get(normalize(t), 0) >= 2 for t in not_in_corpus),
            })
            if len(results) % 10 == 0:
                print(f"[judge] {len(results)} queries graded", file=sys.stderr, flush=True)
    except Stop as stop:
        print(f"[judge] stopped: {stop} (grades so far are cached; rerun to continue)",
              file=sys.stderr)

    if left_out:
        print(f"[judge] {left_out} queries left out: one engine had no results", file=sys.stderr)
    if not results:
        sys.exit("no query could be graded yet")

    for r in sorted(results, key=lambda r: r["ndcg_ours"] - r["ndcg_wiki"])[:args.show]:
        show(r)
    summary(results)
    if args.save:
        save_json(Path(args.save), results)
        print(f"\nsaved {args.save}", file=sys.stderr)


def show(r: dict) -> None:
    print(f"\n[{r['kind']}] {r['query']!r}   nDCG@10 corelamo {r['ndcg_ours']:.2f}  "
          f"wikipedia {r['ndcg_wiki']:.2f}   P@3 {r['p3_ours']:.2f} / {r['p3_wiki']:.2f}")
    width = 44
    print(f"    {'corelamo':<{width}}  wikipedia")
    missing = {normalize(t) for t in r["wiki_not_in_corpus"]}
    for i in range(max(len(r["corelamo"]), len(r["wikipedia"]))):
        left = r["corelamo"][i] if i < len(r["corelamo"]) else ""
        right = r["wikipedia"][i] if i < len(r["wikipedia"]) else ""
        g_left = r["grades"].get(normalize(left), "") if left else ""
        g_right = r["grades"].get(normalize(right), "") if right else ""
        # '*' marks a Wikipedia title the 2017 corpus does not have
        star = "*" if right and normalize(right) in missing else " "
        print(f"  {i + 1:>2} {g_left!s:>1} {left[:width - 3]:<{width - 3}}  "
              f"{g_right!s:>1}{star}{right[:width]}")


def summary(results: list[dict]) -> None:
    kinds = sorted({r["kind"] for r in results})
    print("\n" + "=" * 86)
    print(f"{'kind':<16}{'queries':>8}{'nDCG ours':>11}{'nDCG wiki':>11}{'P@3 ours':>10}"
          f"{'P@3 wiki':>10}{'wins':>7}{'losses':>8}")
    for kind in kinds + ["ALL"]:
        group = [r for r in results if kind == "ALL" or r["kind"] == kind]
        if not group:
            continue
        wins = sum(r["ndcg_ours"] > r["ndcg_wiki"] + 0.05 for r in group)
        losses = sum(r["ndcg_ours"] < r["ndcg_wiki"] - 0.05 for r in group)
        print(f"{kind:<16}{len(group):>8}"
              f"{mean([r['ndcg_ours'] for r in group]):>11.3f}"
              f"{mean([r['ndcg_wiki'] for r in group]):>11.3f}"
              f"{mean([r['p3_ours'] for r in group]):>10.2f}"
              f"{mean([r['p3_wiki'] for r in group]):>10.2f}"
              f"{wins:>7}{losses:>8}")
    good_missing = sum(r["wiki_good_not_in_corpus"] for r in results)
    print(f"\nwins/losses: nDCG@10 apart by more than 0.05. Wikipedia results graded 2 or 3 that "
          f"the 2017 corpus does not have: {good_missing}")


def validate(args: argparse.Namespace, judge: Judge, corelamo: Corelamo) -> None:
    """How often the judge gives FEVER's own right page a 3 (and at least a 2)."""
    queries_path, qrels_path = map(Path, args.validate)
    texts: dict[str, str] = {}
    for line in queries_path.read_text().splitlines():
        if line.strip():
            entry = json.loads(line)
            texts[str(entry["_id"])] = str(entry["text"])
    gold: dict[str, list[str]] = {}
    for line in qrels_path.read_text().splitlines()[1:]:
        parts = line.split("\t")
        if len(parts) >= 3 and parts[2].strip() not in ("", "0") and parts[0] in texts:
            gold.setdefault(parts[0], []).append(parts[1])

    ids = sorted(gold)
    random.Random(SHUFFLE_SEED).shuffle(ids)
    ids = ids[:args.sample]

    top3 = at_least2 = counted = 0
    try:
        for qid in ids:
            query = texts[qid]
            # graded among Corelamo's own results, as in a real comparison
            pool = corelamo.top_titles(query, 10) + gold[qid]
            try:
                grades = judge.grades(query, pool, corelamo)
            except Skip as skip:
                print(f"[judge] skipped: {skip}", file=sys.stderr)
                continue
            best = max(grades.get(normalize(t), 0) for t in gold[qid])
            counted += 1
            top3 += best == 3
            at_least2 += best >= 2
    except Stop as stop:
        print(f"[judge] stopped: {stop}", file=sys.stderr)
    if not counted:
        sys.exit("no query could be graded")
    print(f"\n{counted} FEVER queries: the right page got a 3 in {top3 / counted:.1%}, "
          f"at least a 2 in {at_least2 / counted:.1%}")
    print("Above ~90% for 'at least a 2', the judge's grades are worth trusting.")


# --------------------------------------------------------------------------------------------

def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("compared", nargs="?", help="a compare_wiki.py --save file")
    parser.add_argument("--validate", nargs=2, metavar=("QUERIES_JSONL", "QRELS_TSV"),
                        help="check the judge against FEVER's answers instead")
    parser.add_argument("--sample", type=int, default=200, help="queries for --validate")
    parser.add_argument("--only", help="one kind: entity, entity+context, typo, claim, ...")
    parser.add_argument("--show", type=int, default=15,
                        help="queries printed side by side, worst for Corelamo first")
    parser.add_argument("--save", help="write every query's grades and scores to this JSON file")
    parser.add_argument("--url", default="http://127.0.0.1:6006")
    parser.add_argument("--database", default="fever")
    parser.add_argument("--title-field", default="title")
    parser.add_argument("--text-field", default="text")
    parser.add_argument("--timeout", type=float, default=60.0)
    args = parser.parse_args()

    if not args.compared and not args.validate:
        parser.error("give a compare_wiki.py --save file, or --validate QUERIES QRELS")
    key = os.environ.get("DEEPSEEK_API_KEY", "").strip()
    if not key:
        sys.exit("set DEEPSEEK_API_KEY")

    judge = Judge(key, args.timeout)
    corelamo = Corelamo(args)
    if args.validate:
        validate(args, judge, corelamo)
    else:
        judge_compare(args, judge, corelamo)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit("\ninterrupted: every grade so far is cached")