#!/usr/bin/env python3
"""Ranking test suite for Corelamo: scoring, ranking and relevance on a generated database.

The database is built from scratch so every expectation is known in advance:

  * background: 100,000 documents of made-up filler words ("bzk trv qlm ..."), so word
    frequencies (idf) behave like a real corpus but never collide with the test words
  * fixtures:   small groups of documents for one behaviour each, titled "one", "one two",
    "one two apple", ... with generated text around the words that matter
  * generated:  60 topics of three invented words (each with a title, a longer title, a
    popular body without the title and a keyword-stuffed body) and 40 pairs of invented
    words in order, reversed and apart; every instance is checked, not just one

Every case states what a good search engine should do ("the document titled 'one two' comes
first", "a document with the rare word beats one with the common word"). A failing case is
either a bug or a ranking decision worth looking at.

    python3 ranking_suite.py                      # builds database "ranktest", runs every case
    python3 ranking_suite.py --skip-load          # database already built: only run the cases
    python3 ranking_suite.py --only title sort    # cases whose group starts with these words
    python3 ranking_suite.py --verbose            # print the top results of every case

The database is deleted and created again on each full run (use --db to pick another name).
Documents get automatic ids; cases find them by title. Standard library only.
"""

from __future__ import annotations

import argparse
import json
import random
import sys
import time
import urllib.error
import urllib.request

SEED = 20261016
BACKGROUND_DOCS = 100_000
FILLER_WORDS = 10_000
TOPIC_COUNT = 60
PAIR_COUNT = 40
# english words the fixtures use; invented words must never be one of them
RESERVED = {
    "one", "two", "three", "apple", "pie", "zebra", "okapi", "river", "kumquat", "red", "balloon",
    "violet", "harbor", "melon", "farm", "lantern", "twin", "guide", "notes", "diary", "horse",
    "striped", "page", "both", "words", "rare", "common", "repeated", "adjacent", "distant",
    "reversed", "doomed", "ephemeral", "sentinel", "many", "times", "once", "absolute", "short",
    "long", "swimming", "lessons", "lanterns", "watching", "order", "apart", "stuffed", "body",
}
# the generated families, filled by build_documents() and read by the cases
TOPICS: list[tuple[str, str, str]] = []
STUFFED: list[str] = []
PAIRS: list[tuple[str, str]] = []
GEN = random.Random(SEED + 1)

POLICY = {
    "_id": {"$type": "id_auto"},
    "title": {"$type": "text", "$exact": True, "$stemming": "english", "$weight": {"min": 80, "max": 95}},
    "text": {"$type": "text", "$stemming": "english", "$weight": {"min": 70, "max": 79}},
    "pagerank": {"$type": "int", "$weight": {"min": 1, "max": 1}},
    "group": {"$type": "int", "$weight": {"min": 1, "max": 1}},
}

# ----------------------------------------------------------------------------- the corpus

rng = random.Random(SEED)
CONSONANTS = "bcdfgjklmnpqrstvxz"


def filler_vocabulary(count: int) -> list[str]:
    """Made-up words of consonants only: no stemmer, stopword list or dictionary touches them."""
    words: set[str] = set()
    while len(words) < count:
        words.add("".join(rng.choice(CONSONANTS) for _ in range(rng.randint(3, 6))))
    return sorted(words)


FILLER = filler_vocabulary(FILLER_WORDS)
# Zipf-like: a few filler words are everywhere, most are rare, as in real text
ZIPF = [1.0 / (rank + 1) for rank in range(len(FILLER))]


def filler(n: int) -> list[str]:
    return rng.choices(FILLER, weights=ZIPF, k=n)


def text_with(length: int, placed: dict[int, str] | None = None, sprinkle: dict[str, int] | None = None) -> str:
    """`length` filler words; `placed` puts words at fixed positions, `sprinkle` adds a word
    n times at random positions (replacing filler, so the length stays the same)."""
    words = filler(length)
    for position, word in (placed or {}).items():
        words[position] = word
    taken = set((placed or {}).keys())
    for word, times in (sprinkle or {}).items():
        free = [i for i in range(length) if i not in taken]
        for i in rng.sample(free, times):
            words[i] = word
            taken.add(i)
    return " ".join(words)


def doc(title: str, text: str, pagerank: int = 0, group: int = 0) -> dict:
    return {"title": title, "text": text, "pagerank": pagerank, "group": group}


def invented(count: int, taken: set[str]) -> list[str]:
    """Consonant-vowel words (4 or 6 letters) that no other word in the suite uses."""
    out: list[str] = []
    while len(out) < count:
        word = "".join(GEN.choice(CONSONANTS) + GEN.choice("aeiou") for _ in range(GEN.choice((2, 3))))
        if word not in taken:
            taken.add(word)
            out.append(word)
    return out


def build_documents() -> list[dict]:
    docs: list[dict] = []
    TOPICS.clear()
    STUFFED.clear()
    PAIRS.clear()

    # background; 1,500 of them mention "river" (a common test word), 3 mention "kumquat"
    for i in range(BACKGROUND_DOCS):
        sprinkle = {"river": 1} if i < 1_500 else {}
        if i in (5_000, 5_001, 5_002):
            sprinkle["kumquat"] = 1
        docs.append(doc(" ".join(filler(rng.randint(2, 4))), text_with(rng.randint(30, 80), sprinkle=sprinkle),
                        pagerank=rng.randint(0, 1000)))

    # titles: a ladder of names that contain each other
    for title in ["one", "one two", "one two apple", "one two apple pie", "two", "apple", "two apple",
                  "apple pie", "three one two"]:
        docs.append(doc(title, text_with(40)))

    # title against text, term frequency, length
    docs.append(doc("zebra", text_with(40)))
    docs.append(doc("striped horse notes", text_with(40, sprinkle={"zebra": 1})))
    docs.append(doc("zebra watching diary", text_with(40, sprinkle={"zebra": 5})))
    docs.append(doc("short okapi page", text_with(20, sprinkle={"okapi": 1})))
    docs.append(doc("long okapi page", text_with(200, sprinkle={"okapi": 1})))
    docs.append(doc("okapi many times", text_with(60, sprinkle={"okapi": 6})))
    docs.append(doc("okapi once", text_with(60, sprinkle={"okapi": 1})))

    # rare words count more than common ones; all the words count more than one word repeated
    docs.append(doc("both words page", text_with(50, sprinkle={"river": 1, "kumquat": 1})))
    docs.append(doc("rare word page", text_with(50, sprinkle={"kumquat": 1})))
    docs.append(doc("common word page", text_with(50, sprinkle={"river": 1})))
    docs.append(doc("common word repeated", text_with(50, sprinkle={"river": 10})))

    # words close together
    docs.append(doc("adjacent words", text_with(60, placed={20: "red", 21: "balloon"})))
    docs.append(doc("distant words", text_with(60, placed={5: "red", 50: "balloon"})))
    docs.append(doc("reversed words", text_with(60, placed={20: "balloon", 21: "red"})))

    # stemming and case
    docs.append(doc("lantern maker", text_with(40, sprinkle={"lanterns": 2})))
    docs.append(doc("Swimming Lessons", text_with(40)))

    # a sentence that names a document among others that only mention its words
    docs.append(doc("violet harbor", text_with(60, placed={0: "violet", 1: "harbor"})))
    for i in range(6):
        docs.append(doc(f"harbor guide {i}", text_with(60, sprinkle={"violet": 2, "harbor": 3})))

    # sorting by popularity: equally relevant documents, different pagerank
    for i, pagerank in enumerate([10, 900, 300, 50, 700]):
        docs.append(doc(f"melon {['alpha', 'bravo', 'charlie', 'delta', 'echo'][i]}",
                        text_with(40, placed={3: "melon"}), pagerank=pagerank, group=i % 2 + 1))
    docs.append(doc("melon farm", text_with(40, placed={3: "melon", 4: "farm"}), pagerank=1))

    # identical documents (they land on different shards) must score the same
    twin_text = text_with(40, sprinkle={"twinword": 2})
    docs.append(doc("twin page", twin_text))
    docs.append(doc("twin page", twin_text))

    # generated topics: the exact title must beat the longer title, a popular body that
    # has the three words but not the title (pagerank 1000: default order ignores pagerank)
    # and a body that repeats one word 25 times
    taken = set(RESERVED)
    for _ in range(TOPIC_COUNT):
        a, b, c, stuffed = invented(4, taken)
        TOPICS.append((a, b, c))
        STUFFED.append(stuffed)
        docs.append(doc(f"{a} {b} {c}", text_with(40)))
        docs.append(doc(f"{a} {b} {c} guide", text_with(40)))
        docs.append(doc(invented(1, taken)[0], text_with(60, sprinkle={a: 2, b: 2, c: 2}), pagerank=1000))
        docs.append(doc(stuffed, text_with(60, sprinkle={a: 25})))

    # generated pairs: in order beats reversed, and both beat the same two words far apart
    for _ in range(PAIR_COUNT):
        a, b, x, y, z = invented(5, taken)
        PAIRS.append((a, b, x, y, z))
        p = GEN.randint(5, 25)
        docs.append(doc(f"{x} page", text_with(60, placed={p: a, p + 1: b})))      # in order
        docs.append(doc(f"{y} page", text_with(60, placed={p: b, p + 1: a})))      # reversed
        docs.append(doc(f"{z} page", text_with(60, placed={p: a, p + 30: b})))     # apart

    # a document to delete
    docs.append(doc("doomed page", text_with(30, sprinkle={"ephemeral": 2})))
    # inserted last: once it can be found, everything can
    docs.append(doc("sentinel page", text_with(10, sprinkle={"sentinelword": 1})))
    return docs


# ----------------------------------------------------------------------------- the server

class Server:
    def __init__(self, url: str, db: str):
        self.url = url.rstrip("/")
        self.db = db

    def post(self, path: str, body, method: str = "POST") -> dict:
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        request = urllib.request.Request(f"{self.url}{path}", data=data, method=method)
        try:
            with urllib.request.urlopen(request, timeout=120) as response:
                return json.loads(response.read() or b"{}")
        except urllib.error.HTTPError as err:
            try:
                return json.loads(err.read())
            except ValueError:
                return {"status": err.code}

    def get(self, path: str) -> dict:
        with urllib.request.urlopen(f"{self.url}{path}", timeout=60) as response:
            return json.loads(response.read())

    def search(self, query, docs: int = 10, **extra) -> list[dict]:
        body = {"query": query, "docs": docs, "return_fields": {"text": False}, **extra}
        reply = self.post(f"/api/databases/{self.db}/search", body)
        if reply.get("status", 200) != 200:
            raise RuntimeError(f"search {query!r} failed: {json.dumps(reply)[:300]}")
        return [{"id": hit["id"], "score": hit["score"], **(hit.get("data") or {})} for hit in reply.get("data") or []]

    def load(self, documents: list[dict]) -> None:
        base = f"/api/databases/{self.db}"
        self.post(f"{base}/delete-database", b"", method="DELETE")
        reply = self.post(f"{base}/create-database", {"shard_count": 4})
        if reply.get("status", 200) >= 300:
            raise RuntimeError(f"create-database failed: {json.dumps(reply)[:300]}")
        self.post(f"{base}/start-database", {})
        reply = self.post(f"{base}/set-policy", POLICY)
        if reply.get("status", 200) >= 300:
            raise RuntimeError(f"set-policy failed: {json.dumps(reply)[:300]}")
        for start in range(0, len(documents), 5_000):
            reply = self.post(f"{base}/insert", documents[start:start + 5_000])
            if "inserted" not in (reply.get("data") or {}):
                raise RuntimeError(f"insert failed: {json.dumps(reply)[:300]}")
        # an insert is acknowledged before its documents can be found: wait until every
        # fixture is found by its own title, and report how long that took
        fixtures = sorted({d["title"] for d in documents[BACKGROUND_DOCS:]})
        started = time.monotonic()
        counted = None
        missing = fixtures
        while time.monotonic() - started < 180:
            indexed = self.get(f"{base}/status")["data"]["indexed"]["documents"]
            if counted is None and indexed >= len(documents):
                counted = time.monotonic() - started
            missing = [t for t in missing if t not in titles(self.search(t, docs=20))]
            if counted is not None and not missing:
                break
            time.sleep(0.5)
        searchable = time.monotonic() - started
        print(f"{len(documents):,} documents: counted by status after {counted or 0:.1f}s, "
              f"every fixture searchable after {searchable:.1f}s"
              + (f" (still missing: {missing})" if missing else ""))


# ----------------------------------------------------------------------------- the cases

class Case:
    def __init__(self, group: str, name: str, run):
        self.group, self.name, self.run = group, name, run


def titles(hits: list[dict]) -> list[str]:
    return [hit.get("title") for hit in hits]


def rank(hits: list[dict], title: str) -> int | None:
    for position, hit in enumerate(hits, start=1):
        if hit.get("title") == title:
            return position
    return None


def expect_first(server: Server, query: str, title: str, **extra):
    hits = server.search(query, **extra)
    got = rank(hits, title)
    return got == 1, f"{title!r} at rank {got}", hits


def expect_before(server: Server, query: str, better: str, worse: str, **extra):
    hits = server.search(query, docs=50, **extra)
    a, b = rank(hits, better), rank(hits, worse)
    ok = a is not None and (b is None or a < b)
    detail = f"{better!r} at {a}, {worse!r} at {b}"
    # a tie is not "better": the order would come from document ids
    if ok and b is not None and abs(hits[a - 1]["score"] - hits[b - 1]["score"]) < 1e-3:
        ok, detail = False, detail + " (same score)"
    return ok, detail, hits


def expect_in_top(server: Server, query: str, title: str, k: int, **extra):
    hits = server.search(query, docs=max(k, 10), **extra)
    got = rank(hits, title)
    return got is not None and got <= k, f"{title!r} at rank {got} (want <= {k})", hits


def cases() -> list[Case]:
    out: list[Case] = []
    add = lambda group, name, run: out.append(Case(group, name, run))

    # --- titles
    add("title", "single word title", lambda s: expect_first(s, "one", "one"))
    add("title", "two word title", lambda s: expect_first(s, "one two", "one two"))
    add("title", "three word title", lambda s: expect_first(s, "one two apple", "one two apple"))
    add("title", "four word title", lambda s: expect_first(s, "one two apple pie", "one two apple pie"))
    add("title", "upper case", lambda s: expect_first(s, "ONE TWO APPLE", "one two apple"))
    add("title", "shorter title of the same words first",
        lambda s: expect_before(s, "one two apple", "one two apple", "one two apple pie"))
    add("title", "title containing the query before a longer one",
        lambda s: expect_before(s, "two apple", "two apple", "one two apple"))
    add("title", "words in another order", lambda s: expect_first(s, "two one", "one two"))
    add("title", "a word left out", lambda s: expect_first(s, "one apple", "one two apple"))
    add("title", "title named inside a sentence",
        lambda s: expect_first(s, "one two apple was eaten yesterday by a hungry farmer", "one two apple"))
    add("title", "sentence naming a page beats pages that mention its words",
        lambda s: expect_first(s, "violet harbor was closed for the ferry season", "violet harbor"))
    add("title", "title with a capital letter, typed in lowercase",
        lambda s: expect_first(s, "swimming lessons", "Swimming Lessons"))

    # --- scoring: title, frequency, length
    add("scoring", "word in the title beats word in the text",
        lambda s: expect_before(s, "zebra", "zebra", "striped horse notes"))
    add("scoring", "five mentions beat one (same length)",
        lambda s: expect_before(s, "okapi", "okapi many times", "okapi once"))
    add("scoring", "short text beats long text (one mention each)",
        lambda s: expect_before(s, "okapi", "short okapi page", "long okapi page"))
    add("scoring", "rare word beats common word",
        lambda s: expect_before(s, "river kumquat", "rare word page", "common word page"))
    add("scoring", "every word beats one word",
        lambda s: expect_first(s, "river kumquat", "both words page"))
    add("scoring", "every word beats one word repeated",
        lambda s: expect_before(s, "river kumquat", "both words page", "common word repeated"))
    add("scoring", "identical documents score the same (on different shards)", case_twins)

    # --- words close together
    add("proximity", "adjacent words beat distant ones",
        lambda s: expect_before(s, "red balloon", "adjacent words", "distant words"))
    add("proximity", "phrase in order beats reversed",
        lambda s: expect_before(s, "red balloon", "adjacent words", "reversed words"))
    add("proximity", "phrase operator matches only in order", case_quoted_phrase)

    # --- analysis
    add("analysis", "plural finds singular", lambda s: expect_first(s, "lantern", "lantern maker"))
    add("analysis", "stemmed verb", lambda s: expect_first(s, "swim lesson", "Swimming Lessons"))
    add("analysis", "typo in a word no document has", lambda s: expect_in_top(s, "kumqat", "rare word page", 5))
    add("analysis", "stopwords do not change the result",
        lambda s: expect_first(s, "the one two apple of the", "one two apple"))

    # --- sorting by pagerank
    add("sort", "blend: equal relevance orders by pagerank", case_sort_blend)
    add("sort", "strict ascending is ascending", case_sort_strict)
    add("sort", "sorted search with a word no document has still finds the others", case_sort_multiword)
    add("sort", "exact title stays first when sorted", case_sort_title_first)
    add("sort", "filter keeps only its group", case_filter)

    # --- consistency
    add("consistency", "page 2 continues page 1", case_paging)
    add("consistency", "top 10 does not depend on docs", case_docs_independent)
    add("consistency", "same query, same order", case_deterministic)
    add("consistency", "deleted document is gone", case_delete)

    # --- generated families: each rule is checked on every instance
    add("generated", "exact title first for every generated topic", case_generated_titles)
    add("generated", "keyword stuffing does not beat the exact title", case_generated_stuffing)
    add("generated", "in-order pair first for every generated pair", case_generated_proximity)
    add("generated", "messy query: case, punctuation, extra spaces", case_messy_query)
    add("generated", "long query with stopwords still finds the title", case_long_query)
    add("generated", "uppercase query finds the title", case_upper_only)
    add("generated", "punctuation glued to words still finds the title", case_punctuation_only)
    add("generated", "extra spaces do not change the result", case_extra_spaces_only)
    add("sort", "strict two-field sort: group first, then pagerank", case_sort_strict_two_fields)
    add("consistency", "deep page (offset 40) continues the full result", case_deep_paging)
    return out


def case_generated_titles(s: Server):
    misses = []
    for a, b, c in TOPICS:
        title = f"{a} {b} {c}"
        got = rank(s.search(title, docs=10), title)
        if got != 1:
            misses.append((title, got))
    detail = f"{len(TOPICS) - len(misses)} of {len(TOPICS)} exact titles first"
    return not misses, detail + (f"; missed {misses[:5]}" if misses else ""), []


def case_generated_stuffing(s: Server):
    misses = []
    for (a, b, c), stuffed in zip(TOPICS, STUFFED):
        title = f"{a} {b} {c}"
        hits = s.search(title, docs=50)
        exact, beaten_by = rank(hits, title), rank(hits, stuffed)
        if exact is None or (beaten_by is not None and beaten_by < exact):
            misses.append((title, exact, beaten_by))
    detail = f"{len(TOPICS) - len(misses)} of {len(TOPICS)} topics"
    return not misses, detail + (f"; wrong order {misses[:3]}" if misses else ""), []


def case_generated_proximity(s: Server):
    misses = []
    for a, b, x, y, z in PAIRS:
        hits = s.search(f"{a} {b}", docs=50)
        rx, ry, rz = rank(hits, f"{x} page"), rank(hits, f"{y} page"), rank(hits, f"{z} page")
        if rx is None or any(r is not None and r < rx for r in (ry, rz)):
            misses.append((f"{a} {b}", rx, ry, rz))
            continue
        scores = {h["title"]: h["score"] for h in hits}
        if abs(scores[f"{x} page"] - scores[f"{y} page"]) < 1e-3:
            misses.append((f"{a} {b}", "tie with reversed"))
    detail = f"{len(PAIRS) - len(misses)} of {len(PAIRS)} pairs"
    return not misses, detail + (f"; missed {misses[:5]}" if misses else ""), []

def case_messy_query(s: Server):
    misses = []
    for a, b, c in TOPICS[:20]:
        title = f"{a} {b} {c}"
        got = rank(s.search(f"  {a.upper()}!!  {b},   {c}?  ", docs=10), title)
        if got != 1:
            misses.append((title, got))
    return not misses, f"{20 - len(misses)} of 20 messy queries found the title" + (f"; {misses[:3]}" if misses else ""), []


def case_long_query(s: Server):
    a, b, c = TOPICS[0]
    hits = s.search(f"what is the {a} and {b} of the {c} in a page about the", docs=10)
    got = rank(hits, f"{a} {b} {c}")
    return got == 1, f"title at rank {got}", hits


def case_sort_strict_two_fields(s: Server):
    hits = s.search("melon", docs=20,
                    sort={"mode": "strict", "fields": {"group": {"order": "asc"}, "pagerank": {"order": "desc"}}})
    got = [(h.get("group"), h.get("pagerank")) for h in hits if h.get("title") in MELONS or h.get("title") == "melon farm"]
    # melon farm is group 0; alpha, charlie, echo are group 1; bravo, delta are group 2
    want = [(0, 1), (1, 700), (1, 300), (1, 10), (2, 900), (2, 50)]
    return got == want, f"(group, pagerank) {got} (want {want})", hits


def case_deep_paging(s: Server):
    deep = s.search("river", docs=20, offset=40)
    full = s.search("river", docs=60)[40:60]
    ok = [h["id"] for h in deep] == [h["id"] for h in full] and len(deep) == 20
    return ok, f"offset 40 gave {len(deep)} hits, same as docs=60 slice: {ok}", deep


def case_twins(s: Server):
    hits = [h for h in s.search("twinword", docs=10) if h.get("title") == "twin page"]
    ok = len(hits) == 2 and abs(hits[0]["score"] - hits[1]["score"]) < 1e-3
    return ok, f"twin scores {[round(h['score'], 3) for h in hits]}", hits


def case_quoted_phrase(s: Server):
    hits = s.search({"phrase": "red balloon"}, docs=20)
    got = set(titles(hits)) & {"adjacent words", "distant words", "reversed words"}
    return got == {"adjacent words"}, f"matched {sorted(got)}", hits


MELONS = {"melon alpha": 10, "melon bravo": 900, "melon charlie": 300, "melon delta": 50, "melon echo": 700}


def melon_order(hits):
    return [h.get("title") for h in hits if h.get("title") in MELONS]


def case_sort_blend(s: Server):
    hits = s.search("melon", docs=10, sort={"pagerank": {"order": "desc"}})
    order = melon_order(hits)
    want = sorted(MELONS, key=lambda t: -MELONS[t])
    return order == want, f"order {order} (want {want})", hits


def case_sort_strict(s: Server):
    hits = s.search("melon", docs=10, sort={"mode": "strict", "fields": {"pagerank": {"order": "asc"}}})
    ranks = [h.get("pagerank") for h in hits]
    return ranks == sorted(ranks) and len(ranks) >= 5, f"pageranks {ranks}", hits


def case_sort_multiword(s: Server):
    hits = s.search("melon qqqmissingword", docs=10, sort={"pagerank": {"order": "desc"}})
    return len(melon_order(hits)) >= 5, f"{len(hits)} hits", hits


def case_sort_title_first(s: Server):
    hits = s.search("melon farm", docs=10, sort={"pagerank": {"order": "desc"}})
    got = rank(hits, "melon farm")
    return got == 1, f"'melon farm' (pagerank 1) at rank {got}", hits


def case_filter(s: Server):
    hits = s.search("melon", docs=10, filters={"group": {"range": "1..1"}})
    groups = {h.get("group") for h in hits}
    return groups == {1} and hits, f"groups {sorted(groups)}", hits


def case_paging(s: Server):
    one = s.search("river", docs=10)
    two = s.search("river", docs=10, offset=10)
    both = s.search("river", docs=20)
    ids = [h["id"] for h in one + two]
    ok = ids == [h["id"] for h in both] and len(set(ids)) == len(ids) == 20
    return ok, f"{len(set(ids))} distinct of {len(ids)}, matches docs=20: {ids == [h['id'] for h in both]}", both


def case_docs_independent(s: Server):
    small = [h["id"] for h in s.search("river kumquat", docs=10)]
    large = [h["id"] for h in s.search("river kumquat", docs=100)][:10]
    return small == large, "same top 10" if small == large else "top 10 differs", []


def case_deterministic(s: Server):
    first = [h["id"] for h in s.search("violet harbor ferry", docs=20)]
    second = [h["id"] for h in s.search("violet harbor ferry", docs=20)]
    return first == second, "same" if first == second else "order changed", []


def case_delete(s: Server):
    hits = s.search("ephemeral", docs=5)
    doomed = [h["id"] for h in hits if h.get("title") == "doomed page"]
    if not doomed:
        # an earlier run (--skip-load) deleted it already
        return rank(hits, "doomed page") is None, "already deleted by an earlier run", hits
    reply = s.post(f"/api/databases/{s.db}/delete", doomed, method="DELETE")
    if reply.get("status", 200) >= 300:
        return False, f"delete failed: {json.dumps(reply)[:200]}", hits
    time.sleep(1)
    after = s.search("ephemeral", docs=5)
    return rank(after, "doomed page") is None, f"after delete: {titles(after)}", after
def case_upper_only(s: Server):
    misses = []
    for a, b, c in TOPICS[:20]:
        title = f"{a} {b} {c}"
        if rank(s.search(f"{a.upper()} {b.upper()} {c.upper()}", docs=10), title) != 1:
            misses.append(title)
    return not misses, f"{20 - len(misses)} of 20 uppercase queries", []


def case_punctuation_only(s: Server):
    misses = []
    for a, b, c in TOPICS[:20]:
        title = f"{a} {b} {c}"
        if rank(s.search(f"{a}!! {b}, {c}?", docs=10), title) != 1:
            misses.append(title)
    return not misses, f"{20 - len(misses)} of 20 punctuated queries", []


def case_extra_spaces_only(s: Server):
    misses = []
    for a, b, c in TOPICS[:20]:
        title = f"{a} {b} {c}"
        if rank(s.search(f"   {a}    {b}   {c}  ", docs=10), title) != 1:
            misses.append(title)
    return not misses, f"{20 - len(misses)} of 20 spaced queries", []

# ----------------------------------------------------------------------------- main

def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--url", default="http://127.0.0.1:6006")
    parser.add_argument("--db", default="ranktest")
    parser.add_argument("--skip-load", action="store_true")
    parser.add_argument("--only", nargs="*", help="run only cases whose group starts with these")
    parser.add_argument("--verbose", action="store_true")
    args = parser.parse_args()

    server = Server(args.url, args.db)
    if not args.skip_load:
        server.load(build_documents())

    selected = [c for c in cases() if not args.only or any(c.group.startswith(o) for o in args.only)]
    failed = 0
    for case in selected:
        try:
            ok, detail, hits = case.run(server)
        except Exception as err:  # a failing request is a failing case
            ok, detail, hits = False, f"error: {err}", []
        failed += not ok
        print(f"{'PASS' if ok else 'FAIL'}  {case.group:<12} {case.name:<62} {detail}")
        if (args.verbose or not ok) and hits:
            print("        top:", [f"{h.get('title')} ({h['score']:.0f})" for h in hits[:6]])
    print(f"\n{len(selected) - failed} passed, {failed} failed of {len(selected)}")
    sys.exit(1 if failed else 0)

if __name__ == "__main__":
    main()