#!/usr/bin/env python3
"""Build (or grow) wiki_queries.json, the query list compare_wiki.py runs.

    python3 make_queries.py                    # ~1000 queries: entities, typos, claims, questions
    python3 make_queries.py --fever 500 --nq 500 --typos 300
    python3 make_queries.py --contact you@example.com --popular 2017-06 2017-11 --popular-count 400

`--popular` adds what people really looked up: Wikipedia's most-viewed articles of each given
month (the official Pageviews API, one request per month, cached in wiki_top_cache.json),
turned into the query someone would type ("Tom Holland (actor)" -> "tom holland"). Wikipedia does
not publish the search box's own queries; the most-viewed pages are the closest public record.
2017 months match the FEVER dump, so the pages exist in the corpus.

Nothing already in wiki_queries.json is removed or changed: new queries are appended, and a
query already there (same text) is skipped. So running it again, or with bigger numbers, only
adds; compare_wiki.py then asks Wikipedia for the new ones only.

Real queries are sampled from the BEIR folders load_beir.py unpacked (beir/fever, beir/nq), with a
fixed seed, so the same command always picks the same queries. Only the standard library is used.
"""

from __future__ import annotations

import argparse
import json
import random
import re
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
OUT_FILE = SCRIPT_DIR / "wiki_queries.json"
TOP_CACHE = SCRIPT_DIR / "wiki_top_cache.json"
TOP_API = ("https://wikimedia.org/api/rest_v1/metrics/pageviews/top/"
           "en.wikipedia/all-access/{year}/{month}/all-days")
# Seconds between two Pageviews API calls (one call covers a whole month).
TOP_INTERVAL = 3.0
# Pages in the top lists that are not articles anyone searches for.
NOT_ARTICLES = re.compile(
    r"^(Main_Page|Special:|Wikipedia:|Portal:|File:|Help:|Talk:|User:|Template:|Category:|"
    r"Search|-$|Undefined$|XHamster|Xxx|Pornhub|XVideos|Bible$)",
    re.IGNORECASE,
)

# Well-known articles, each one unambiguous enough that "the right first hit" is clear.
ENTITIES = [
    "batman", "superman", "spider-man", "iron man", "wonder woman", "the avengers",
    "tom holland", "tom hanks", "tom cruise", "brad pitt", "leonardo dicaprio", "meryl streep",
    "scarlett johansson", "robert downey jr.", "keanu reeves", "denzel washington",
    "morgan freeman", "natalie portman", "johnny depp", "angelina jolie", "will smith",
    "barack obama", "donald trump", "abraham lincoln", "george washington", "winston churchill",
    "napoleon", "julius caesar", "cleopatra", "queen victoria", "elizabeth ii", "nelson mandela",
    "mahatma gandhi", "martin luther king jr.", "albert einstein", "isaac newton",
    "charles darwin", "marie curie", "nikola tesla", "galileo galilei", "stephen hawking",
    "leonardo da vinci", "michelangelo", "vincent van gogh", "pablo picasso", "claude monet",
    "william shakespeare", "charles dickens", "jane austen", "mark twain", "j. k. rowling",
    "stephen king", "ernest hemingway", "george orwell", "the beatles", "the rolling stones",
    "queen band", "pink floyd", "led zeppelin", "nirvana band", "michael jackson", "madonna",
    "elvis presley", "taylor swift", "beyonce", "eminem", "bob dylan", "david bowie",
    "game of thrones", "breaking bad", "the simpsons", "friends tv series", "the office",
    "stranger things", "the sopranos", "doctor who", "star trek", "star wars", "harry potter",
    "the lord of the rings", "the godfather", "titanic film", "jurassic park", "the matrix",
    "pulp fiction", "forrest gump", "the dark knight", "inception", "avatar film",
    "new york city", "london", "paris", "tokyo", "berlin", "rome", "moscow", "beijing",
    "sydney", "los angeles", "chicago", "barcelona", "amsterdam", "riga", "latvia", "estonia",
    "lithuania", "germany", "france", "japan", "china", "india", "brazil", "canada",
    "australia", "egypt", "south africa", "mount everest", "amazon river", "sahara",
    "pacific ocean", "grand canyon", "eiffel tower", "colosseum", "great wall of china",
    "statue of liberty", "taj mahal", "world war i", "world war ii", "cold war",
    "french revolution", "american civil war", "roman empire", "renaissance",
    "industrial revolution", "apple inc.", "microsoft", "google", "amazon company", "tesla inc.",
    "facebook", "netflix", "coca-cola", "nike", "toyota", "python programming language",
    "linux", "internet", "bitcoin", "artificial intelligence", "photosynthesis", "dna",
    "black hole", "solar system", "mars", "jupiter", "moon", "football", "basketball",
    "tennis", "olympic games", "fifa world cup", "manchester united", "real madrid",
    "lionel messi", "cristiano ronaldo", "michael jordan", "serena williams", "usain bolt",
]

KEYBOARD_NEIGHBOURS = {
    "a": "qs", "b": "vn", "c": "xv", "d": "sf", "e": "wr", "f": "dg", "g": "fh", "h": "gj",
    "i": "uo", "j": "hk", "k": "jl", "l": "k", "m": "n", "n": "bm", "o": "ip", "p": "o",
    "q": "w", "r": "et", "s": "ad", "t": "ry", "u": "yi", "v": "cb", "w": "qe", "x": "zc",
    "y": "tu", "z": "x",
}


def typo(word: str, rng: random.Random) -> str:
    """One realistic slip in a word of 4+ letters: neighbour key, swap, drop or double."""
    letters = [i for i, c in enumerate(word) if c.isalpha()]
    if len(word) < 4 or len(letters) < 3:
        return word
    i = rng.choice(letters[1:])  # keep the first letter, as people mostly do
    kind = rng.choice(("neighbour", "swap", "drop", "double"))
    if kind == "neighbour" and word[i] in KEYBOARD_NEIGHBOURS:
        return word[:i] + rng.choice(KEYBOARD_NEIGHBOURS[word[i]]) + word[i + 1:]
    if kind == "swap" and i + 1 < len(word) and word[i + 1].isalpha():
        return word[:i] + word[i + 1] + word[i] + word[i + 2:]
    if kind == "drop":
        return word[:i] + word[i + 1:]
    return word[:i] + word[i] + word[i:]


def typo_query(entity: str, rng: random.Random) -> str | None:
    words = entity.split()
    candidates = [i for i, w in enumerate(words) if len(w) >= 4 and w.isalpha()]
    if not candidates:
        return None
    i = rng.choice(candidates)
    changed = typo(words[i], rng)
    if changed == words[i]:
        return None
    words[i] = changed
    return " ".join(words)


def popular_titles(months: list[str], contact: str | None) -> dict[str, list[str]]:
    """Each month's most-viewed article titles, best first, from the cache or the API."""
    cache: dict[str, list[str]] = {}
    if TOP_CACHE.exists():
        try:
            cache = json.loads(TOP_CACHE.read_text())
        except json.JSONDecodeError as err:
            sys.exit(f"[error] {TOP_CACHE} is not valid JSON ({err}); fix or delete it")
    wanted = [month for month in months if month not in cache]
    if wanted and not (contact and ("@" in contact or "://" in contact)):
        sys.exit("--popular needs --contact (your email or URL) to ask the Pageviews API for "
                 f"{', '.join(wanted)}; months already in {TOP_CACHE.name} need nothing")
    for number, month in enumerate(wanted):
        match = re.fullmatch(r"(\d{4})-(\d{2})", month)
        if not match:
            sys.exit(f"[error] month {month!r} must look like 2017-06")
        if number:
            time.sleep(TOP_INTERVAL)
        url = TOP_API.format(year=match.group(1), month=match.group(2))
        request = urllib.request.Request(
            url, headers={"User-Agent": f"corelamo-ranking-compare/1.1 (query list builder; {contact})"})
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                payload = json.load(response)
        except urllib.error.HTTPError as err:
            # a refusal or a month without data: stop asking, keep what is cached
            print(f"[warn] Pageviews API {month}: HTTP {err.code}; not asking again this run",
                  file=sys.stderr)
            break
        except (urllib.error.URLError, TimeoutError) as err:
            print(f"[warn] Pageviews API {month}: {err}; not asking again this run", file=sys.stderr)
            break
        items = (payload.get("items") or [{}])[0].get("articles") or []
        cache[month] = [str(item.get("article", "")) for item in sorted(items, key=lambda i: i.get("rank", 0))]
        partial = TOP_CACHE.with_suffix(".json.tmp")
        partial.write_text(json.dumps(cache, indent=1, ensure_ascii=False))
        partial.replace(TOP_CACHE)
    return {month: cache[month] for month in months if month in cache}


def as_typed(title: str) -> str | None:
    """The query someone would type for an article title: no qualifier, spaces, lower case."""
    if NOT_ARTICLES.search(title):
        return None
    text = title.replace("_", " ")
    text = re.sub(r"\s*\([^)]*\)\s*$", "", text)  # "Tom Holland (actor)" -> "Tom Holland"
    text = text.strip().lower()
    return text if len(text) >= 2 else None


def sample_queries(path: Path, count: int, rng: random.Random) -> list[str]:
    """`count` query texts from a BEIR queries.jsonl, chosen with the fixed seed."""
    if count <= 0:
        return []
    if not path.exists():
        print(f"[warn] {path} not found (run load_beir.py for it first); skipped", file=sys.stderr)
        return []
    texts: list[str] = []
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                text = json.loads(line).get("text", "")
            except json.JSONDecodeError:
                continue
            # the API caps search text at 300 characters; very long ones are not typical anyway
            if 3 <= len(text) <= 200:
                texts.append(text)
    texts.sort()  # the same picks whatever order the file is in
    return rng.sample(texts, min(count, len(texts)))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--entities", type=int, default=len(ENTITIES))
    parser.add_argument("--typos", type=int, default=150)
    parser.add_argument("--fever", type=int, default=350, help="FEVER claims to sample")
    parser.add_argument("--nq", type=int, default=350, help="NQ questions to sample")
    parser.add_argument("--fever-queries", default=str(SCRIPT_DIR / "beir/fever/queries.jsonl"))
    parser.add_argument("--nq-queries", default=str(SCRIPT_DIR / "beir/nq/queries.jsonl"))
    parser.add_argument("--popular", nargs="*", default=[], metavar="YYYY-MM",
                        help="months whose most-viewed articles to add, e.g. 2017-06 2017-11")
    parser.add_argument("--popular-count", type=int, default=300,
                        help="popular queries to add in total, best-viewed first, months interleaved")
    parser.add_argument("--contact", help="your email or URL for the Pageviews API User-Agent")
    parser.add_argument("--seed", type=int, default=20261105)
    args = parser.parse_args()

    existing: list[list[str]] = []
    if OUT_FILE.exists():
        try:
            existing = json.loads(OUT_FILE.read_text())
        except json.JSONDecodeError as err:
            sys.exit(f"[error] {OUT_FILE} is not valid JSON ({err}); fix or move it first")
    seen = {query for _, query in existing}
    added: list[list[str]] = []

    def add(kind: str, query: str | None) -> None:
        if query and query not in seen:
            seen.add(query)
            added.append([kind, query])

    if args.popular:
        by_month = popular_titles(args.popular, args.contact)
        # the target is a total: a rerun with the same numbers adds nothing
        have = sum(1 for kind, _ in existing if kind == "popular")
        lists = [titles for titles in by_month.values()]
        for rank in range(max((len(t) for t in lists), default=0)):
            if have >= args.popular_count:
                break
            for titles in lists:  # interleave months, so each one's top pages come first
                if rank < len(titles) and have < args.popular_count:
                    before = len(added)
                    add("popular", as_typed(titles[rank]))
                    have += len(added) - before
    for entity in ENTITIES[: args.entities]:
        add("entity", entity)
    typo_rng = random.Random(args.seed + 1)
    # the target is a total: a rerun with the same numbers adds nothing
    made = sum(1 for kind, _ in existing if kind == "typo")
    for _ in range(args.typos * 5):  # a few tries each: short names cannot be misspelled
        if made >= args.typos:
            break
        query = typo_query(typo_rng.choice(ENTITIES), typo_rng)
        before = len(added)
        add("typo", query)
        made += len(added) - before
    for text in sample_queries(Path(args.fever_queries), args.fever, random.Random(args.seed + 2)):
        add("claim", text)
    for text in sample_queries(Path(args.nq_queries), args.nq, random.Random(args.seed + 3)):
        add("question", text)

    result = existing + added
    partial = OUT_FILE.with_suffix(".json.tmp")
    partial.write_text(json.dumps(result, indent=1, ensure_ascii=False))
    partial.replace(OUT_FILE)
    kinds: dict[str, int] = {}
    for kind, _ in result:
        kinds[kind] = kinds.get(kind, 0) + 1
    print(f"{OUT_FILE.name}: {len(result)} queries ({len(added)} new): "
          + ", ".join(f"{k} {n}" for k, n in sorted(kinds.items())))


if __name__ == "__main__":
    main()