#!/usr/bin/env python3
"""Compare Corelamo's top 10 with Wikipedia's own search for ~100 hand-picked queries.

FEVER's corpus is a 2017 Wikipedia dump, so Wikipedia's search (CirrusSearch) is a fair
reference ranking: the same articles, ranked by a mature engine. It is a reference, not the
truth: Wikipedia ranks today's articles (some did not exist in 2017) and boosts popular pages.

    python3 compare_wiki.py --contact "you@example.com"          # first run: fills the cache
    python3 compare_wiki.py --offline                            # later runs: never asks Wikipedia
    python3 compare_wiki.py --offline --only typo --show 15
    python3 compare_wiki.py --offline --save beir_test/wiki-compare.json

Wikipedia's search does not correct spelling by itself: for a typo it returns nothing and a
"did you mean". The script then searches that suggestion (one more polite request), so a typo is
compared with what Wikipedia would show after its correction. A query Wikipedia has no answer
for at all is left out of the scores (nothing to compare with). To fetch again only the cached
queries that had no answer (typos cached before this existed):

    python3 compare_wiki.py --contact "you@example.com" --refetch-empty --only typo

The queries come from wiki_queries.json (built by make_queries.py) plus the built-in list below.
The cache only ever grows: queries already answered are never asked again, so adding queries
and rerunning asks Wikipedia for the new ones only (at most MAX_REQUESTS_PER_RUN per run).

Wikipedia is asked as politely as its API etiquette asks (mediawiki.org/wiki/API:Etiquette):

  * one request at a time, at least MIN_INTERVAL seconds apart, remembered across runs
  * a User-Agent naming this tool and your contact (--contact is required to go online)
  * `maxlag`, so the servers can turn us away when they are busy instead of serving us
  * the first refusal (429, 503, maxlag) stops the run, honours `Retry-After`, and a cooldown
    is stored so a rerun refuses to go online until it has passed
  * at most MAX_REQUESTS_PER_RUN new requests per run
  * every answer is cached at once (wiki_cache.json): a query is asked for only once, ever

Only the standard library is used.
"""

from __future__ import annotations

import argparse
import json
import sys
import time
import unicodedata
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
CACHE_FILE = SCRIPT_DIR / "wiki_cache.json"
QUERIES_FILE = SCRIPT_DIR / "wiki_queries.json"
STATE_FILE = SCRIPT_DIR / "wiki_rate_state.json"
WIKI_API = "https://en.wikipedia.org/w/api.php"

# Seconds between two requests to Wikipedia, also across separate runs.
MIN_INTERVAL = 3.0
# New Wikipedia requests one run may make; the rest wait for the next run (15 min at 3 s).
MAX_REQUESTS_PER_RUN = 300
# Cooldown after any refusal when Wikipedia does not say how long (seconds).
DEFAULT_COOLDOWN = 15 * 60
# Ask Wikipedia to refuse us when its replicas lag more than this many seconds.
MAXLAG = 5

# (kind, query): kinds let you look at one part of the ranking at a time (--only). Used when
# there is no wiki_queries.json (make_queries.py builds a ~1000-query one); merged into it.
QUERIES: list[tuple[str, str]] = [
    # --- one entity, the article itself should win -------------------------------------
    ("entity", "batman"),
    ("entity", "tom holland"),
    ("entity", "world war 2"),
    ("entity", "king of the hill"),
    ("entity", "barack obama"),
    ("entity", "the beatles"),
    ("entity", "mount everest"),
    ("entity", "albert einstein"),
    ("entity", "game of thrones"),
    ("entity", "the godfather"),
    ("entity", "new york city"),
    ("entity", "harry potter"),
    ("entity", "star wars"),
    ("entity", "leonardo da vinci"),
    ("entity", "the who"),
    ("entity", "this is us"),
    ("entity", "her film"),
    ("entity", "it stephen king"),
    ("entity", "there will be blood"),
    ("entity", "doctor who"),
    # --- an entity and what is asked about it --------------------------------------------
    ("entity+context", "spiderman tom holland"),
    ("entity+context", "iphone 14"),
    ("entity+context", "tom holland author"),
    ("entity+context", "michael jordan basketball"),
    ("entity+context", "michael jordan actor"),
    ("entity+context", "paris hilton hotel heiress"),
    ("entity+context", "jaguar car"),
    ("entity+context", "jaguar animal"),
    ("entity+context", "python programming language"),
    ("entity+context", "mercury planet"),
    ("entity+context", "apple company"),
    ("entity+context", "beauty and the beast 2017 film"),
    ("entity+context", "the x factor uk"),
    ("entity+context", "manchester united stadium"),
    ("entity+context", "einstein relativity"),
    # --- typos: the correction should land on the right article ------------------------
    ("typo", "spiderman tom hollland"),
    ("typo", "tom holland spuferman"),
    ("typo", "ipjone 14"),
    ("typo", "barak obama"),
    ("typo", "albert einstien"),
    ("typo", "leonardo dicaprio titanc"),
    ("typo", "micheal jackson thriller"),
    ("typo", "shakespere hamlet"),
    ("typo", "beetles band"),
    ("typo", "mount everst"),
    ("typo", "harry poter"),
    ("typo", "pyhton language"),
    ("typo", "eifel tower"),
    ("typo", "colloseum rome"),
    ("typo", "massachusets"),
    # --- FEVER-style claims ---------------------------------------------------------------
    ("claim", "Nikolaj Coster-Waldau worked with the Fox Broadcasting Company."),
    ("claim", "Robert Downey Jr. was in Iron Man."),
    ("claim", "The X Factor is American."),
    ("claim", "Jesus was a minister."),
    ("claim", "Venus is an astronomical body."),
    ("claim", "Nelson Mandela was from South Africa."),
    ("claim", "Indonesia's capital is Jakarta."),
    ("claim", "Denmark's parliament is not located in Copenhagen."),
    ("claim", "Christian Bale's middle name is Harry."),
    ("claim", "Tennis is an activity."),
    ("claim", "Hawaii has a longer coastline than Florida."),
    ("claim", "The Lakers were defeated in the 1991 NBA Semifinals."),
    ("claim", "Harry Potter is a film series."),
    ("claim", "Anne Hathaway starred in Les Misérables as Fantine."),
    ("claim", "Mel Gibson only had basic high school education."),
    ("claim", "Islam has more than 1.8 billion adherents."),
    ("claim", "Dubai contains the world's tallest building."),
    ("claim", "Doctor Who is made in Britain."),
    ("claim", "Belize is in Central America."),
    ("claim", "An opioid overdose led to the death of Prince."),
    # --- natural-language questions (NQ-style) --------------------------------------------
    ("question", "when is the new tappan zee bridge going to be finished"),
    ("question", "why do red blood cells live for only 120 days"),
    ("question", "who wore number 7 for the green bay packers"),
    ("question", "where does the big sur begin and end"),
    ("question", "how many books are in the one piece series"),
    ("question", "who plays heather in beauty and the beast"),
    ("question", "what molecule in plants captures energy from the sun"),
    ("question", "where does the term helter skelter come from"),
    ("question", "who won the ncaa basketball championship in 1994"),
    ("question", "what is the name of the skin between your nostrils"),
    ("question", "who sang the theme song for dukes of hazzard"),
    ("question", "when were 2 dollar bills stopped being made"),
    ("question", "how many vowels are in the thai alphabet"),
    ("question", "where do they grow hops in the us"),
    ("question", "what is the capital of australia"),
    # --- topics without one obvious article ----------------------------------------------
    ("topic", "english kings"),
    ("topic", "renewable energy sources"),
    ("topic", "history of the internet"),
    ("topic", "causes of the french revolution"),
    ("topic", "machine learning algorithms"),
    ("topic", "climate change effects"),
    ("topic", "ancient egypt pyramids"),
    ("topic", "jazz musicians new orleans"),
    ("topic", "volcanic eruptions iceland"),
    ("topic", "space shuttle disasters"),
    ("topic", "olympic games 2016"),
    ("topic", "italian renaissance painters"),
    ("topic", "cold war arms race"),
    ("topic", "world cup winners"),
    ("topic", "nobel prize physics"),
]


class Refused(Exception):
    """Wikipedia asked us to stop for now; `cooldown` is how long, in seconds."""

    def __init__(self, reason: str, cooldown: float) -> None:
        super().__init__(reason)
        self.cooldown = cooldown


# --------------------------------------------------------------------------------------------
# files that survive between runs
# --------------------------------------------------------------------------------------------

def load_queries(path: Path) -> list[tuple[str, str]]:
    """The query file's queries, then any built-in one it does not have; each text once."""
    from_file = load_json(path, [])
    if not isinstance(from_file, list):
        sys.exit(f"[error] {path} must be a JSON list of [kind, query] pairs")
    queries: list[tuple[str, str]] = []
    seen: set[str] = set()
    for entry in list(from_file) + [list(pair) for pair in QUERIES]:
        if (isinstance(entry, list) and len(entry) == 2
                and all(isinstance(part, str) for part in entry) and entry[1] not in seen):
            seen.add(entry[1])
            queries.append((entry[0], entry[1]))
    return queries


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


# --------------------------------------------------------------------------------------------
# Wikipedia: one polite request at a time
# --------------------------------------------------------------------------------------------

class PoliteWiki:
    def __init__(self, contact: str, timeout: float) -> None:
        self.agent = f"corelamo-ranking-compare/1.1 (offline evaluation script; {contact})"
        self.timeout = timeout
        self.state = load_json(STATE_FILE, {"last_request": 0.0, "cooldown_until": 0.0})
        self.made = 0

    def cooldown_left(self) -> float:
        return max(0.0, float(self.state.get("cooldown_until", 0.0)) - time.time())

    def _remember(self, **changes) -> None:
        self.state.update(changes)
        save_json(STATE_FILE, self.state)

    def _wait_turn(self) -> None:
        since = time.time() - float(self.state.get("last_request", 0.0))
        if since < MIN_INTERVAL:
            time.sleep(MIN_INTERVAL - since)

    def search(self, query: str, limit: int) -> dict:
        """Wikipedia's titles for the query; when it finds nothing but suggests a spelling, the
        titles for that spelling. `{"titles": [...], "suggestion": str | None}`."""
        titles, suggestion = self._search_once(query, limit)
        if titles or not suggestion or suggestion == query:
            return {"titles": titles, "suggestion": None}
        corrected, _ = self._search_once(suggestion, limit)
        return {"titles": corrected, "suggestion": suggestion}

    def _search_once(self, query: str, limit: int) -> tuple[list[str], str | None]:
        if self.cooldown_left() > 0:
            raise Refused("still cooling down from an earlier refusal", self.cooldown_left())
        if self.made >= MAX_REQUESTS_PER_RUN:
            raise Refused(f"reached {MAX_REQUESTS_PER_RUN} requests this run", 0.0)

        self._wait_turn()
        params = urllib.parse.urlencode({
            "action": "query", "list": "search", "srsearch": query, "srlimit": limit,
            "srprop": "", "srinfo": "suggestion", "format": "json", "formatversion": "2",
            "maxlag": MAXLAG,
        })
        request = urllib.request.Request(
            f"{WIKI_API}?{params}",
            headers={"User-Agent": self.agent, "Accept-Encoding": "identity"},
        )
        self.made += 1
        self._remember(last_request=time.time())
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                payload = json.load(response)
        except urllib.error.HTTPError as err:
            self._refuse(f"HTTP {err.code}", err.headers.get("Retry-After"))
        except (urllib.error.URLError, TimeoutError) as err:
            # no answer at all: back off too, rather than retrying into a struggling server
            self._refuse(f"no answer ({err})", None)

        error = payload.get("error") if isinstance(payload, dict) else None
        if error:
            # maxlag (servers busy) and every other API error: stop, try again later
            self._refuse(f"API error {error.get('code', '?')}: {error.get('info', '')}", None)
        found = payload.get("query") or {}
        results = found.get("search") or []
        suggestion = (found.get("searchinfo") or {}).get("suggestion")
        titles = [str(result.get("title", "")) for result in results]
        return titles, suggestion if isinstance(suggestion, str) and suggestion else None

    def _refuse(self, reason: str, retry_after: str | None):
        try:
            cooldown = max(float(retry_after), 60.0) if retry_after else DEFAULT_COOLDOWN
        except ValueError:
            cooldown = DEFAULT_COOLDOWN
        self._remember(cooldown_until=time.time() + cooldown)
        raise Refused(reason, cooldown)


# --------------------------------------------------------------------------------------------
# Corelamo
# --------------------------------------------------------------------------------------------

def corelamo_titles(args: argparse.Namespace, query: str) -> tuple[list[str], float | None, str | None]:
    """Corelamo's top titles, its own time_taken in ms, and the spelling it applied or offered."""
    body = json.dumps({"query": query, "docs": args.k, "return_fields": {"title": True}}).encode()
    request = urllib.request.Request(
        f"{args.url.rstrip('/')}/api/databases/{args.database}/search",
        data=body, method="POST", headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=args.timeout) as response:
        payload = json.load(response)
    if not isinstance(payload, dict):
        return [], None, None
    hits = payload.get("data") or []
    titles = [str((hit.get("data") or {}).get("title", hit.get("id"))) for hit in hits]
    took = None
    raw = payload.get("time_taken")
    if isinstance(raw, str):
        for unit, scale in (("ms", 1.0), ("µs", 1e-3), ("us", 1e-3), ("ns", 1e-6), ("s", 1000.0)):
            if raw.endswith(unit):
                try:
                    took = float(raw[: -len(unit)]) * scale
                except ValueError:
                    took = None
                break
    correction = payload.get("corrected_query") or payload.get("suggested_query")
    return titles, took, correction if isinstance(correction, str) else None


# --------------------------------------------------------------------------------------------
# scoring and printing
# --------------------------------------------------------------------------------------------

def cached_answer(entry) -> tuple[list[str], str | None]:
    """A cache entry as (titles, Wikipedia's suggestion); older entries are plain title lists."""
    if isinstance(entry, dict):
        titles = entry.get("titles") or []
        suggestion = entry.get("suggestion")
        return [str(t) for t in titles], suggestion if isinstance(suggestion, str) else None
    if isinstance(entry, list):
        return [str(t) for t in entry], None
    return [], None


def normalize(title: str) -> str:
    """Titles compared the way beir_eval.py compares them: FEVER's codes undone, case folded."""
    for code, char in (("-LRB-", "("), ("-RRB-", ")"), ("-LSB-", "["), ("-RSB-", "]"),
                       ("-COLON-", ":")):
        title = title.replace(code, char)
    return unicodedata.normalize("NFC", title.replace("_", " ")).strip().casefold()


def rbo(ours: list[str], theirs: list[str], p: float = 0.9) -> float:
    """Rank-biased overlap of two ranked lists (extrapolated to the full depth)."""
    depth = max(len(ours), len(theirs))
    if depth == 0:
        return 1.0
    seen_ours: set[str] = set()
    seen_theirs: set[str] = set()
    total = 0.0
    overlap = 0
    for d in range(1, depth + 1):
        if d <= len(ours):
            item = ours[d - 1]
            overlap += item in seen_theirs
            seen_ours.add(item)
        if d <= len(theirs):
            item = theirs[d - 1]
            overlap += item in seen_ours
            seen_theirs.add(item)
        total += (overlap / d) * p ** (d - 1)
    return (1 - p) * total + (overlap / depth) * p ** depth


def compare(ours: list[str], theirs: list[str]) -> dict | None:
    """Agreement scores, or None when Wikipedia has no answer to compare with."""
    if not theirs:
        return None
    a = [normalize(t) for t in ours]
    b = [normalize(t) for t in theirs]
    top_ours = set(a[:10])
    return {
        "top1": 1.0 if a[:1] and a[:1] == b[:1] else 0.0,
        "at3": sum(t in top_ours for t in b[:3]) / len(b[:3]),
        "at10": sum(t in top_ours for t in b[:10]) / len(b[:10]),
        "rbo": rbo(a[:10], b[:10]),
    }


def show(kind: str, query: str, ours: list[str], theirs: list[str], scores: dict,
         correction: str | None, took: float | None, wiki_fix: str | None = None) -> None:
    fix = f"   [ours: {correction!r}]" if correction else ""
    if wiki_fix:
        fix += f"   [wikipedia: {wiki_fix!r}]"
    ms = f"{took:.1f} ms" if took is not None else "? ms"
    print(f"\n[{kind}] {query!r}   top1 {scores['top1']:.0f}  @3 {scores['at3']:.2f}  "
          f"@10 {scores['at10']:.2f}  rbo {scores['rbo']:.2f}  ({ms}){fix}")
    theirs_set = {normalize(t) for t in theirs}
    ours_set = {normalize(t) for t in ours}
    width = 44
    print(f"    {'corelamo':<{width}}  wikipedia")
    for i in range(max(len(ours), len(theirs))):
        left = ours[i] if i < len(ours) else ""
        right = theirs[i] if i < len(theirs) else ""
        # '=' marks a title the other list also has somewhere in its top 10
        mark_l = "=" if left and normalize(left) in theirs_set else " "
        mark_r = "=" if right and normalize(right) in ours_set else " "
        print(f"  {i + 1:>2} {mark_l}{left[:width - 1]:<{width - 1}}  {mark_r}{right[:width]}")


def summary(rows: list[dict]) -> None:
    kinds = sorted({row["kind"] for row in rows})
    print("\n" + "=" * 72)
    print(f"{'kind':<16}{'queries':>8}{'top1':>8}{'@3':>8}{'@10':>8}{'rbo':>8}{'p50 ms':>9}")
    for kind in kinds + ["ALL"]:
        group = [r for r in rows if kind == "ALL" or r["kind"] == kind]
        if not group:
            continue

        def mean(key: str) -> float:
            return sum(r[key] for r in group) / len(group)

        times = sorted(r["ms"] for r in group if r["ms"] is not None)
        p50 = f"{times[len(times) // 2]:.1f}" if times else "-"
        print(f"{kind:<16}{len(group):>8}{mean('top1'):>8.2f}{mean('at3'):>8.2f}"
              f"{mean('at10'):>8.2f}{mean('rbo'):>8.2f}{p50:>9}")
    worst = sorted(rows, key=lambda r: r["rbo"])[:10]
    print("\nleast agreement (rbo):")
    for r in worst:
        print(f"  {r['rbo']:.2f}  [{r['kind']}] {r['query']!r}")


# --------------------------------------------------------------------------------------------

def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--url", default="http://127.0.0.1:6006")
    parser.add_argument("--database", default="fever")
    parser.add_argument("--k", type=int, default=10, help="results compared per query")
    parser.add_argument("--only", help="one kind: entity, entity+context, typo, claim, question, topic")
    parser.add_argument("--show", type=int, default=30, help="queries printed side by side")
    parser.add_argument("--queries", default=str(QUERIES_FILE),
                        help="query file from make_queries.py (built-in list if missing)")
    parser.add_argument("--contact", help="your email or URL, sent in the User-Agent "
                                          "(required unless --offline)")
    parser.add_argument("--offline", action="store_true",
                        help="never contact Wikipedia: compare only the queries already cached")
    parser.add_argument("--refetch-empty", action="store_true",
                        help="ask Wikipedia again for cached queries that had no results "
                             "(now with its spelling suggestion); everything else stays cached")
    parser.add_argument("--timeout", type=float, default=30.0)
    parser.add_argument("--save", help="write every query's lists and scores to this JSON file")
    args = parser.parse_args()

    if args.k < 1 or args.k > 50:
        sys.exit("--k must be between 1 and 50")

    cache: dict = load_json(CACHE_FILE, {})
    wiki: PoliteWiki | None = None
    if not args.offline:
        if not args.contact or "@" not in args.contact and "://" not in args.contact:
            sys.exit("set --contact to your email or a URL (Wikimedia asks API clients to "
                     "identify themselves), or use --offline to compare cached queries only")
        wiki = PoliteWiki(args.contact, args.timeout)
        left = wiki.cooldown_left()
        if left > 0:
            print(f"[wikipedia] cooling down for {left / 60:.0f} more minute(s): "
                  f"comparing cached queries only", file=sys.stderr)
            wiki = None

    queries = [(k, q) for k, q in load_queries(Path(args.queries)) if not args.only or k == args.only]
    def needs_fetch(query: str) -> bool:
        key = f"{args.k}:{query}"
        if key not in cache:
            return True
        titles, suggestion = cached_answer(cache[key])
        # only answers that were empty and never had a suggestion looked up are asked again
        return args.refetch_empty and not titles and suggestion is None and isinstance(cache[key], list)

    if args.refetch_empty:
        queries = [(k, q) for k, q in queries if needs_fetch(q)]
        print(f"[wikipedia] {len(queries)} cached queries had no answer: only those are compared",
              file=sys.stderr)
    missing = [q for _, q in queries if needs_fetch(q)]
    if wiki is not None and missing:
        estimate = min(len(missing), MAX_REQUESTS_PER_RUN) * MIN_INTERVAL / 60
        print(f"[wikipedia] {len(missing)} queries not cached yet: about {estimate:.0f} "
              f"minute(s), one request every {MIN_INTERVAL:.0f}s", file=sys.stderr)

    rows: list[dict] = []
    skipped = 0
    no_reference = 0
    for index, (kind, query) in enumerate(queries, start=1):
        key = f"{args.k}:{query}"
        if needs_fetch(query):
            if wiki is None:
                skipped += 1
                continue
            try:
                cache[key] = wiki.search(query, args.k)
            except Refused as stop:
                print(f"[wikipedia] stopped: {stop} (no more requests for "
                      f"{stop.cooldown / 60:.0f} minute(s); rerun later to continue)",
                      file=sys.stderr)
                wiki = None
                skipped += 1
                continue
            # every answer is kept at once: an interrupted run never asks again
            save_json(CACHE_FILE, cache)
            if wiki is not None and wiki.made % 10 == 0:
                print(f"[wikipedia] {wiki.made} requests this run", file=sys.stderr, flush=True)

        try:
            ours, took, correction = corelamo_titles(args, query)
        except (urllib.error.URLError, TimeoutError, ValueError) as err:
            print(f"[corelamo] {query!r}: {err}", file=sys.stderr)
            continue
        theirs, wiki_fix = cached_answer(cache[key])
        scores = compare(ours, theirs)
        if scores is None:
            no_reference += 1
            continue
        rows.append({"kind": kind, "query": query, "ms": took, "correction": correction,
                     "wikipedia_correction": wiki_fix, "corelamo": ours, "wikipedia": theirs,
                     **scores})
        if index <= args.show:
            show(kind, query, ours, theirs, scores, correction, took, wiki_fix)

    if skipped:
        print(f"\n[wikipedia] {skipped} queries not compared yet (not cached)", file=sys.stderr)
    if no_reference:
        print(f"[wikipedia] {no_reference} queries left out: Wikipedia had no answer to compare with",
              file=sys.stderr)
    if not rows:
        sys.exit("no query could be compared yet")
    summary(rows)
    if args.save:
        save_json(Path(args.save), rows)
        print(f"\nsaved {args.save}", file=sys.stderr)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit("\ninterrupted: every Wikipedia answer so far is cached")