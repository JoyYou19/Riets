#!/usr/bin/env python3
"""
Runs the analyzer/search test queries against a Corelamo database and checks
that each one returns the expected documents.

    python3 check_searches.py                       # localhost:6006, db "movies"
    python3 check_searches.py --db mytest -v        # other db, show every query
    python3 check_searches.py -k stemming           # only cases whose group/name contains "stemming"
    python3 check_searches.py --list                # just list the cases

Result codes:
    PASS  got exactly what was expected
    FAIL  wrong result (counts toward the exit code)
    WARN  an "uncertain" case (soft=True) differed - expectation is a guess from the docs
    ERR   HTTP / connection error
Needs only the Python 3 standard library.
"""
import argparse
import json
import sys
import urllib.error
import urllib.request

# ---------------------------------------------------------------- documents
D = {
    1: "doc-001-spiderman",
    2: "doc-002-search-engine",
    3: "doc-003-legacy-notes",
    4: "doc-004-code-review",
    5: "doc-005-misc-edge-cases",
}


def E(*nums):
    """Expected doc numbers -> list of ids."""
    return [D[n] for n in nums]


# ---------------------------------------------------------------- case model
CASES = []
_group = ["general"]


def group(name):
    _group[0] = name


def add(name, body, expect, ordered=False, soft=False, extra=(), err_ok=False, no_field=None, has_field=None):
    body = dict(body)
    body.setdefault("docs", 20)
    CASES.append(dict(group=_group[0], name=name, body=body, expect=list(expect),
                      ordered=ordered, soft=soft, extra=set(extra),
                      err_ok=err_ok, no_field=no_field, has_field=has_field))


def S(field, text, expect, **kw):
    """Query restricted to one field."""
    add(f"{field}: {text}", {"query": text,
        "search_fields": [field]}, expect, **kw)


def X(field, text, expect, **kw):
    """Exact whole-value query on a field."""
    add(f"{field} exact: {text}", {"query": {"exact": text},
        "search_fields": [field]}, expect, **kw)


def P(field, text, expect, **kw):
    """Phrase query on a field."""
    add(f"{field} phrase: {text}", {"query": {"phrase": text},
        "search_fields": [field]}, expect, **kw)


def FT(text, expect, **kw):
    """Free-text query across all searchable fields."""
    add(f"free-text: {text}", {"query": text}, expect, **kw)


def F(field, value, expect, **kw):
    """Filter-only query."""
    add(f"filter {field} = {json.dumps(value)}", {
        "filters": {field: value}}, expect, **kw)


def SORT(label, sort, expect, **kw):
    add(f"sort {label}", {"sort": sort}, expect, ordered=True, **kw)


# ================================================================ the cases
group("sanity")
FT("jumps", E(1, 3))
FT("fox", E(3), extra=E(1))  # D1 comes in as a semantic (vector) neighbour
S("body", "fox", E(3))
FT("spider-man", E(1))

group("stemming / default analyzer")
S("body", "run", E(1, 3))
S("body", "runner", E(1))
S("title", "running", E(1))
S("title", "run", [])
S("no_original_field", "running", E(1))
S("no_original_field", "run", [])
S("empty_field", "running", E(1))
S("empty_field", "run", [])
S("tags", "run", E(1, 3))
S("tags", "animal", E(3))
S("body", "spiderman", E(1))
S("body", "supercalifragilisticexpialidocious", E(5))

group("ascii folding / $exact")
S("author", "Janis", E(1))
S("author", "Berzins", E(1))
S("author", "Nunez", E(2))
S("author", "Muller", E(3))
S("author", "Lei", E(5))
X("author", "jānis bērziņš", E(1))
S("address/city", "riga", E(1, 4), soft=True)
S("address/city", "zurich", E(3), soft=True)
S("address/city", "cafe", E(2), soft=True)
X("address/city", "rīga", E(1, 4), soft=True)
S("orders/ship_to/city", "zurich", [], soft=True)
S("orders/ship_to/city", "zürich", E(3), soft=True)
X("title", "the quick brown fox", E(3))
X("title", "CODE REVIEW CHECKLIST FOR DEVELOPERS", E(4))
X("url", "https://www.example.com/tutorials/spider-man?ref=home", E(1))

group("keyword / whitespace / standard tokenizers")
S("kw_field", "spider", [])
S("kw_field", "fox", [])
P("kw_field", "spider-man origins", E(1), soft=True)
S("tag", "spider-man", [])
P("tag", "SPIDER-MAN ORIGINS", E(1), soft=True)
S("ws_field", "spider-man", E(1))
S("ws_field", "spider", [])
S("ws_field", "v2.0", E(2))
S("std_field", "spider", E(1))
S("std_field", "man", E(1))
S("std_field", "spiderman", [])
S("std_field", "v2", E(2))

group("url / code / filepath tokenizers")
S("url", "github.com", E(2))
S("url", "www", E(1, 4))
S("url", "tutorials", E(1))
S("url", "munchen", E(5))
S("source_code", "HTTP", E(1, 5))
S("source_code", "Http", E(4))
# snake_case IS split (parse_http_request); uppercase HTTP must not match
S("source_code", "http", E(4))
S("source_code", "Response", E(1))
# query splits into get/HTTP/Response and is OR-ed; D5 has HTTP
S("source_code", "getHTTPResponse", E(1), extra=E(5))
S("source_code", "Manager", E(3))
S("source_code", "Parser", E(2))
S("repo_path", "src", E(1, 2, 4))
S("repo_path", "rs", E(1, 2))
S("repo_path", "shard_db", E(1))
S("repo_path", "shard", [])
S("repo_path", "lib", E(3, 5))
S("repo_path", "ts", E(5))

group("char filters")
S("readme", "fast", E(1))
S("readme", "run", E(1))
for w in ("b", "div", "table", "intro"):
    S("readme", w, [])
S("readme", "documents", E(2))
S("readme", "checklist", E(4))
S("notes_md", "bold", E(1))
S("notes_md", "index", E(2))
S("notes_md", "inline", E(2))
S("notes_md", "lint", E(4))
S("notes_md", "emphasis", E(3))
S("notes_md", "strikethrough", E(5))
S("tracking_url", "spiderman", E(1))
for w in ("utm_source", "newsletter", "launch"):
    S("tracking_url", w, [])
S("tracking_url", "blog", E(2))
S("tracking_url", "checklist", E(4))
S("chained", "public", E(1))
S("chained", "review", E(4))
# [REDACTED] is removed; D3 may still hit because "redactions" stems to the same root
S("chained", "redacted", [], extra=E(3))

group("stopwords / lengths / literal / case")
S("stop_en_field", "question", E(2))
S("stop_en_field", "the", [], err_ok=True)
S("stop_en_field", "over", [], err_ok=True)
S("stop_en_field", "jumping", E(1))
S("stop_en_field", "apple", E(3))
S("stop_en_field", "filter", E(4))
for w in ("foo", "bar", "baz"):
    S("stop_custom_field", w, [], err_ok=True)
S("stop_custom_field", "qux", E(1, 5))
S("stop_custom_field", "quux", E(2))
S("stop_custom_field", "and", E(1))
S("stop_custom_field", "remove", [])
S("lengths_field", "abcd", E(1))
S("lengths_field", "abc", [], err_ok=True)
S("lengths_field", "abcdefghij", E(1))
S("lengths_field", "abcdefghijk", [], err_ok=True)
S("lengths_field", "deployment", E(4))
S("lengths_field", "elephants", E(3))
S("lengths_field", "hippopotamus", [], err_ok=True)
S("lengths_field", "cat", [], err_ok=True)
S("empty_field", "a", E(1, 2))
S("empty_field", "q", E(5))
S("empty_field", "x1", E(1))
S("empty_field", "i", E(1, 2))
S("empty_field", "o", E(2))
S("literal_field", "snake_case", E(1))
S("literal_field", "snake", [], soft=True)
S("literal_field", "user_name", E(3))
S("literal_field", "user@example", E(1))
S("literal_field", "mixed", E(1))
S("literal_field", "dash", E(1))
S("literal_field", "mixeddash", E(1))
S("case_field", "Example", E(1))
S("case_field", "example", [])
S("case_field", "UPPER", E(5))
S("case_field", "upper", [])
X("case_field", "camelcase example", E(1))

group("german stemming + folding")
S("german_field", "haus", E(1))
S("german_field", "Häuser", E(1))
S("german_field", "munchen", E(1))
S("german_field", "dokument", E(2))
S("german_field", "faul", E(3))
S("german_field", "prüfen", E(4))
S("german_field", "strasse", E(5))
S("german_field", "fussball", E(5))
S("german_field", "die", E(1, 2))

group("repeated fields / structs")
S("tags", "running", E(1, 3))
S("tags", "index", E(2))
S("tags", "edge-case", E(5))
S("tags", "case", E(5))
S("tags", "aou", [])
S("items/sku", "web", E(1))
S("items/sku", "1", E(2, 3, 4), soft=True)
F("items/qty", "1..3", E(1, 2, 4), soft=True)
F("items/price", ">=20", E(2), soft=True)
F("items/price", "<1", E(3, 4), soft=True)
S("address/street", "baker", E(1), soft=True)
S("address/street", "paix", E(2), soft=True)
F("address/zip", ">=8000", E(2, 3, 5), soft=True)
F("address/geo/lat", ">56", E(1, 4), soft=True)
F("address/geo/lon", "24..25", E(1, 4), soft=True)
S("orders/sku", "ord", E(1, 2, 3, 4), soft=True)
S("orders/sku", "20", E(3), soft=True)
S("orders/ship_to/city", "riga", E(1, 4), soft=True)
S("orders/ship_to/city", "tallinn", E(1), soft=True)
F("orders/ship_to/zip", ">=10000", E(1, 2), soft=True)

group("diagnostics (struct paths + folding)")
# is address.street indexed at all?
FT("baker", E(1), soft=True)
add("stored: address returned on all docs", {"return_fields": {"slug": True, "address": True}},
    E(1, 2, 3, 4, 5), has_field="address", soft=True)   # stored but not indexed?
# exact number instead of a range
F("address/zip", 1010, E(1), soft=True)
# diacritics in the query itself
S("author", "Bērziņš", E(1), soft=True)
# must NOT hit: tokenizer shouldn't split at the ā in Jānis
S("author", "nis", [], soft=True)
# plain-ASCII baseline for the german analyzer
S("german_field", "Fuchs", E(3), soft=True)
S("german_field", "hauser", E(1), soft=True)       # pre-folded form of Häuser

group("scalar filters")
F("year", ">=2023", E(1, 2, 5))
F("year", "<2000", E(3))
F("count", ">0", E(1, 2, 4))
F("count", "<0", E(5))
F("count", 0, E(3))
F("rating", ">=4.5", E(1, 2))
F("rating", "<1", E(5))
F("is_public", False, E(3))
F("is_public", True, E(1, 2, 4, 5))
F("years", "2024", E(1, 4), soft=True)
F("years", "1", E(5), soft=True)

group("sort (strict, match-all)")
SORT("year desc", {"mode": "strict", "year": {
     "order": "desc"}}, E(5, 1, 2, 4, 3))
SORT("count asc", {"mode": "strict", "count": {
     "order": "asc"}}, E(5, 3, 4, 1, 2))
SORT("rating desc", {"mode": "strict", "rating": {
     "order": "desc"}}, E(2, 1, 4, 3, 5))

group("special fields")
add("slug exact filter", {"filters": {
    "slug": {"exact": "doc-003-legacy-notes"}}}, E(3), soft=True)
FT("confidential", [])
FT("hidden-note-alpha", [])
# server answers 400 "not searchable"
S("secret_note", "confidential", [], err_ok=True)
FT("debug-info-one", [])
S("debug", "debug-info-one", [], err_ok=True)
add("internal_id filter (and hidden from results)", {"filters": {"internal_id": 1003}}, E(3),
    soft=True, no_field="internal_id")


# ================================================================ runner
def post(url, body, timeout=10):
    req = urllib.request.Request(url, data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"}, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, json.loads(r.read().decode() or "{}")
    except urllib.error.HTTPError as e:
        raw = e.read().decode(errors="replace")
        try:
            return e.code, json.loads(raw)
        except ValueError:
            return e.code, {"raw": raw}


def main():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--url", default="http://localhost:6006",
                    help="server base url")
    ap.add_argument("--db", default="movies", help="database name")
    ap.add_argument(
        "-k", default="", help="only run cases whose group or name contains this text")
    ap.add_argument("-v", "--verbose", action="store_true",
                    help="print passing cases too")
    ap.add_argument("--list", action="store_true", help="list cases and exit")
    args = ap.parse_args()

    cases = [c for c in CASES if args.k.lower() in (
        c["group"] + " " + c["name"]).lower()]
    if args.list:
        for c in cases:
            print(f"[{c['group']}] {c['name']}  ->  {c['expect']
                  or 'none'}{'  (soft)' if c['soft'] else ''}")
        print(f"\n{len(cases)} cases")
        return 0

    color = sys.stdout.isatty()
    def paint(code, s): return f"\033[{code}m{s}\033[0m" if color else s
    tags = {"PASS": paint(32, "PASS"), "FAIL": paint(31, "FAIL"),
            "WARN": paint(33, "WARN"), "ERR ": paint(31, "ERR ")}

    endpoint = f"{args.url.rstrip('/')}/api/databases/{args.db}/search"
    counts = {"PASS": 0, "FAIL": 0, "WARN": 0, "ERR ": 0}
    current_group = None

    for c in cases:
        if c["group"] != current_group:
            current_group = c["group"]
            print(f"\n== {current_group}")
        try:
            status, resp = post(endpoint, c["body"])
        except (urllib.error.URLError, OSError) as e:
            print(f"{tags['ERR ']} cannot reach {endpoint}: {e}")
            return 2

        got, detail = [], ""
        if status != 200:
            if c["err_ok"] and not c["expect"]:
                code, detail = "PASS", f"(HTTP {status}, accepted as empty)"
            else:
                code = "WARN" if c["soft"] else "ERR "
                detail = f"HTTP {status}: {json.dumps(resp)[:200]}"
        else:
            data = resp.get("data") or []
            got = [d.get("id") for d in data]
            expect = c["expect"]
            if c["ordered"]:
                ok = got == expect
            else:
                ok = set(expect) <= set(got) and (
                    set(got) - set(expect)) <= c["extra"]
            if ok and c["no_field"]:
                leaked = [d.get("id") for d in data if c["no_field"]
                          in (d.get("data") or {})]
                if leaked:
                    ok, detail = False, f"field '{
                        c['no_field']}' present in results for {leaked}"
            if ok and c["has_field"]:
                absent = [d.get("id") for d in data if c["has_field"]
                          not in (d.get("data") or {})]
                if absent:
                    ok, detail = False, f"field '{
                        c['has_field']}' missing from stored docs {absent}"
            code = "PASS" if ok else ("WARN" if c["soft"] else "FAIL")
            if not ok and not detail:
                missing = [i for i in expect if i not in got]
                unexpected = [
                    i for i in got if i not in expect and i not in c["extra"]]
                detail = f"missing={missing} unexpected={unexpected}"
                if c["ordered"]:
                    detail = f"expected order {expect}, got {got}"
        counts[code] += 1
        if code != "PASS" or args.verbose:
            def short(ids): return [i.split("-")[1]
                                    for i in ids]  # doc-003-x -> "003"
            print(f"{tags[code]} {c['name']}  expected={
                  short(c['expect']) or 'none'} got={short(got) or 'none'}")
            if code != "PASS":
                print(f"       {detail}")
                print(f"       request: {json.dumps(
                    c['body'], ensure_ascii=False)}")

    total = sum(counts.values())
    print(f"\n{total} cases: {counts['PASS']} passed, {counts['FAIL']} failed, "
          f"{counts['WARN']} warnings (uncertain expectations), {counts['ERR ']} errors")
    return 1 if counts["FAIL"] or counts["ERR "] else 0


if __name__ == "__main__":
    sys.exit(main())
